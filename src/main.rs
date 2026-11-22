//! Stage 3 of the Two-Instance Spike: Two Independent `libprojectM 4.1.8` Decks
//! Side by Side at 1920x1080 Internal Resolution per Deck (Pattern 1).
//!
//! Why ALL rendering stays strictly on ONE thread (the main thread):
//! - OpenGL is a thread-local state machine: an OpenGL context can only be current
//!   on one thread at a time. Moving contexts across threads or sharing textures
//!   without explicit fences easily causes driver stalls, black frames, or crashes.
//! - `libprojectM` is not designed for concurrent cross-thread calls, and on Windows
//!   it shares global `glew32.dll` state.
//! - Switching between Deck A's shared context, Deck B's shared context, and the main
//!   window context sequentially on ONE thread is simple, deterministic, and easily
//!   fast enough for 60 FPS at 1080p x 2 on the Quadro RTX 3000.
//!
//! Modes:
//! - Interactive mode (default):
//!     `.\target\release\milkdrop-vj.exe`
//!     Keys:
//!       `1`        = Test 1 (Independence: Deck A = test audio, Deck B = silence)
//!       `2`        = Both decks = test audio
//!       `A` / `B`  = Toggle Deck A / Deck B audio individually
//!       `S`        = Test 2 (No bleed: swap presets between Deck A and Deck B)
//!       `E`        = Test 3 (Bad preset: load deliberately corrupt preset onto Deck A)
//!       `P`        = Capture PNGs of Deck A & Deck B to `docs/spike/deck_a.png` & `deck_b.png`
//!       `Left/Right` = Cycle Deck A preset
//!       `Up/Down`    = Cycle Deck B preset
//!       `Esc` / `X`  = Clean exit with full report
//!
//! - Automated soak mode (runs Tests 1, 2, 3, PNG capture, and timed stability test):
//!     `.\target\release\milkdrop-vj.exe --soak-minutes 10`

mod compositor;
mod deck;
mod gl_log;
mod gl_state;
mod metrics;
mod render_deck;
mod test_audio;

use std::ffi::CStr;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use glow::HasContext;
use glutin::config::{ConfigSurfaceTypes, ConfigTemplateBuilder};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version,
};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::compositor::SideBySideCompositor;
use crate::deck::{
    init_gl_loader_for_projectm, max_pcm_samples_per_channel, projectm_version_string,
};
use crate::gl_log::GlErrorLogger;
use crate::metrics::{current_process_memory_mb, MemorySnapshotMb, TimingStats};
use crate::render_deck::RenderDeck;
use crate::test_audio::TestAudioGenerator;

/// Internal rendering resolution per deck (1920x1080 = Full HD for BOTH decks).
const DECK_WIDTH: u32 = 1920;
const DECK_HEIGHT: u32 = 1080;

/// Automated soak-test phases when `--soak-minutes N` is passed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SoakPhase {
    Off,
    Independence,
    Swapped,
    BadPreset,
    SteadySoak,
}

struct Stage3State {
    deck_a: RenderDeck,
    deck_b: RenderDeck,
    compositor: SideBySideCompositor,
    window: Window,
    main_surface: Surface<WindowSurface>,
    main_context: PossiblyCurrentContext,
    main_gl: glow::Context,

    // Independent audio generators so Deck A and Deck B never share audio state.
    audio_a: TestAudioGenerator,
    audio_b: TestAudioGenerator,
    pcm_a: Vec<f32>,
    pcm_b: Vec<f32>,
    deck_a_audio: bool,
    deck_b_audio: bool,

    // Presets and bad-preset file path.
    presets: Vec<PathBuf>,
    bad_preset_path: PathBuf,
    deck_a_preset_idx: usize,
    deck_b_preset_idx: usize,

    // Timing, FPS, and memory tracking.
    start_time: Instant,
    last_report_time: Instant,
    last_report_frames: u64,
    frame_count: u64,
    timing_a: TimingStats,
    timing_b: TimingStats,
    timing_total: TimingStats,
    start_memory: Option<MemorySnapshotMb>,

    // Optional automated soak-test configuration.
    soak_duration: Option<Duration>,
    soak_phase: SoakPhase,
    png_captured: bool,
}

struct Stage3App {
    state: Option<Stage3State>,
    logger: GlErrorLogger,
    presets: Vec<PathBuf>,
    bad_preset_path: PathBuf,
    soak_duration: Option<Duration>,
}

impl Stage3App {
    fn new(
        logger: GlErrorLogger,
        presets: Vec<PathBuf>,
        bad_preset_path: PathBuf,
        soak_duration: Option<Duration>,
    ) -> Self {
        Self {
            state: None,
            logger,
            presets,
            bad_preset_path,
            soak_duration,
        }
    }

    fn init_all(&mut self, event_loop: &ActiveEventLoop) {
        let title = if let Some(dur) = self.soak_duration {
            format!(
                "MilkDrop VJ — Stage 3 Two-Deck Soak ({:.1} min @ 1080p x2, Esc to abort)",
                dur.as_secs_f64() / 60.0
            )
        } else {
            "MilkDrop VJ — Stage 3 Two Decks @ 1080p x2 (1:Indep, S:Swap, E:BadPreset, P:PNG, Esc:Exit)".to_string()
        };

        // 1444x405 shows two exact 16:9 halves (720x405 each) plus a 4px divider.
        let window_attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(1444.0, 405.0));

        let template = ConfigTemplateBuilder::new().with_alpha_size(8);
        let display_builder = DisplayBuilder::new().with_window_attributes(Some(window_attrs));

        let (window, gl_config) = display_builder
            .build(event_loop, template, |configs| {
                configs
                    .reduce(|accum, config| {
                        if config.num_samples() < accum.num_samples() {
                            config
                        } else {
                            accum
                        }
                    })
                    .expect("No compatible OpenGL pixel format found")
            })
            .expect("Failed to create winit window and glutin GL display");

        let window = window.expect("DisplayBuilder did not return a window");
        let raw_window_handle = window
            .window_handle()
            .expect("Failed to obtain native window handle")
            .as_raw();
        let gl_display = gl_config.display();

        // Main window OpenGL 3.3 Core context.
        let main_ctx_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(Some(raw_window_handle));

        let main_not_current = unsafe {
            gl_display
                .create_context(&gl_config, &main_ctx_attrs)
                .expect("Failed to create main OpenGL 3.3 Core context")
        };

        let surface_attrs = window
            .build_surface_attributes(Default::default())
            .expect("Failed to build window surface attributes");

        let main_surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .expect("Failed to create main window surface")
        };

        let main_context = main_not_current
            .make_current(&main_surface)
            .expect("Failed to make main OpenGL context current");

        if let Err(err) = main_surface.set_swap_interval(
            &main_context,
            SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
        ) {
            self.logger.log_info(&format!("Warning: VSync failed: {err}"));
        }

        let main_gl = unsafe {
            glow::Context::from_loader_function_cstr(|symbol: &CStr| {
                gl_display.get_proc_address(symbol) as *const _
            })
        };

        let vendor = unsafe { main_gl.get_parameter_string(glow::VENDOR) };
        let renderer = unsafe { main_gl.get_parameter_string(glow::RENDERER) };
        let version = unsafe { main_gl.get_parameter_string(glow::VERSION) };

        println!("=== Stage 3 OpenGL Ready (Internal: {DECK_WIDTH}x{DECK_HEIGHT} per deck) ===");
        println!("GL_VENDOR:   {vendor}");
        println!("GL_RENDERER: {renderer}");
        println!("GL_VERSION:  {version}");
        self.logger.log_info(&format!("GL_VENDOR={vendor}"));
        self.logger.log_info(&format!("GL_RENDERER={renderer}"));
        self.logger.log_info(&format!("GL_VERSION={version}"));

        // Initialize GLEW once on Windows before creating any libprojectM deck.
        init_gl_loader_for_projectm().expect("Failed to initialize GLEW for libprojectM");
        self.logger.log_info("GLEW initialized for libprojectM.");

        let compositor =
            SideBySideCompositor::new(&main_gl).expect("Failed to create side-by-side compositor");
        self.logger.check_frame(&main_gl, 0, "main-init");

        // Find a WGL config supporting off-screen pbuffers (`WGL_ARB_pbuffer`).
        // IMPORTANT (Windows WGL): `.compatible_with_native_window(raw_window_handle)` is
        // required so glutin calls `GetDC(hwnd)` on our real window instead of `GetDC(0)`
        // (desktop screen DC), which otherwise causes `wglCreatePbufferARB` to fail with
        // OS error `0xC0070591` (ERROR_DC_NOT_FOUND = 1425).
        let pbuffer_template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_surface_type(ConfigSurfaceTypes::PBUFFER)
            .compatible_with_native_window(raw_window_handle)
            .build();
        let pbuffer_config = unsafe {
            gl_display
                .find_configs(pbuffer_template)
                .expect("Failed to query pbuffer OpenGL configs")
                .reduce(|accum, config| {
                    if config.num_samples() < accum.num_samples() {
                        config
                    } else {
                        accum
                    }
                })
                .expect("No pbuffer-capable OpenGL config found on this GPU")
        };

        // Pattern 1: create TWO independent RenderDecks, each with its own shared GL context.
        let deck_a = RenderDeck::new(
            "DeckA",
            &gl_display,
            &pbuffer_config,
            &main_context,
            DECK_WIDTH,
            DECK_HEIGHT,
            &mut self.logger,
        )
        .expect("Failed to create Deck A");

        let deck_b = RenderDeck::new(
            "DeckB",
            &gl_display,
            &pbuffer_config,
            &main_context,
            DECK_WIDTH,
            DECK_HEIGHT,
            &mut self.logger,
        )
        .expect("Failed to create Deck B");

        // Load two very different initial presets (index 0 = Fractal, index 1 = Geometric).
        let deck_a_idx = 0;
        let deck_b_idx = 1.min(self.presets.len().saturating_sub(1));

        let label_a = preset_label(&self.presets[deck_a_idx]);
        let label_b = preset_label(&self.presets[deck_b_idx]);
        deck_a
            .load_preset(&self.presets[deck_a_idx], false)
            .unwrap_or_else(|e| self.logger.log_info(&format!("Deck A initial load err: {e}")));
        deck_b
            .load_preset(&self.presets[deck_b_idx], false)
            .unwrap_or_else(|e| self.logger.log_info(&format!("Deck B initial load err: {e}")));

        println!("Deck A initial preset: {label_a}");
        println!("Deck B initial preset: {label_b}");
        self.logger.log_info(&format!("Deck A initial preset: {label_a}"));
        self.logger.log_info(&format!("Deck B initial preset: {label_b}"));

        // Return focus to the main context and show the window.
        main_context
            .make_current(&main_surface)
            .expect("Failed to restore main context after deck init");

        window.set_visible(true);
        window.focus_window();

        let start_memory = current_process_memory_mb();
        if let Some(mem) = start_memory {
            let msg = format!(
                "Start memory: WorkingSet={:.1} MiB, PrivateCommit={:.1} MiB",
                mem.working_set_mb, mem.private_commit_mb
            );
            println!("{msg}");
            self.logger.log_info(&msg);
        }

        let max_samples = max_pcm_samples_per_channel();
        let initial_phase = if self.soak_duration.is_some() {
            println!("[Soak Test 1/4] INDEPENDENCE: Deck A = TEST_AUDIO, Deck B = SILENCE (10s)...");
            self.logger.log_info("[Soak Test 1/4] INDEPENDENCE started: Deck A=AUDIO, Deck B=SILENCE");
            SoakPhase::Independence
        } else {
            SoakPhase::Off
        };
        let deck_b_starts_silent = initial_phase == SoakPhase::Independence;

        let now = Instant::now();
        self.state = Some(Stage3State {
            deck_a,
            deck_b,
            compositor,
            window,
            main_surface,
            main_context,
            main_gl,
            audio_a: TestAudioGenerator::new(48_000.0),
            audio_b: TestAudioGenerator::new(48_000.0),
            pcm_a: vec![0.0_f32; max_samples * 2],
            pcm_b: vec![0.0_f32; max_samples * 2],
            deck_a_audio: true,
            deck_b_audio: !deck_b_starts_silent,
            presets: self.presets.clone(),
            bad_preset_path: self.bad_preset_path.clone(),
            deck_a_preset_idx: deck_a_idx,
            deck_b_preset_idx: deck_b_idx,
            start_time: now,
            last_report_time: now,
            last_report_frames: 0,
            frame_count: 0,
            timing_a: TimingStats::default(),
            timing_b: TimingStats::default(),
            timing_total: TimingStats::default(),
            start_memory,
            soak_duration: self.soak_duration,
            soak_phase: initial_phase,
            png_captured: false,
        });
    }

    fn set_independence_mode(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.deck_a_audio = true;
        state.deck_b_audio = false;
        let msg = "Test 1 (Independence): Deck A = TEST_AUDIO, Deck B = SILENCE";
        println!("{msg}");
        self.logger.log_info(msg);
    }

    fn swap_presets(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        std::mem::swap(&mut state.deck_a_preset_idx, &mut state.deck_b_preset_idx);
        let path_a = state.presets[state.deck_a_preset_idx].clone();
        let path_b = state.presets[state.deck_b_preset_idx].clone();
        let label_a = preset_label(&path_a);
        let label_b = preset_label(&path_b);

        if let Err(err) = state.deck_a.load_preset(&path_a, false) {
            eprintln!("Deck A swap load error: {err}");
            self.logger.log_info(&format!("Deck A swap load error: {err}"));
        }
        if let Err(err) = state.deck_b.load_preset(&path_b, false) {
            eprintln!("Deck B swap load error: {err}");
            self.logger.log_info(&format!("Deck B swap load error: {err}"));
        }

        let msg = format!("Test 2 (Swap): Deck A -> `{label_a}` | Deck B -> `{label_b}`");
        println!("{msg}");
        self.logger.log_info(&msg);
    }

    fn trigger_bad_preset_on_deck_a(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let bad_path = state.bad_preset_path.clone();
        println!(
            "Test 3 (Bad preset): Loading deliberately broken preset `{}` onto Deck A...",
            bad_path.display()
        );
        match state.deck_a.load_preset(&bad_path, false) {
            Ok(()) => {
                let msg = "Test 3 result: libprojectM accepted the broken/empty preset without firing an error callback (no crash; Deck B unaffected).";
                println!("{msg}");
                self.logger.log_info(msg);
            }
            Err(err) => {
                let msg = format!("Test 3 result (PASS - caught cleanly, no crash): {err}");
                println!("{msg}");
                self.logger.log_info(&msg);
            }
        }
    }

    fn capture_both_pngs(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let path_a = Path::new("docs/spike/deck_a.png");
        let path_b = Path::new("docs/spike/deck_b.png");

        match state.deck_a.capture_png(path_a, &mut self.logger) {
            Ok(()) => {
                let msg = format!("Saved Deck A PNG capture -> {}", path_a.display());
                println!("{msg}");
                self.logger.log_info(&msg);
            }
            Err(err) => {
                eprintln!("Deck A PNG capture error: {err}");
                self.logger.log_info(&format!("Deck A PNG capture error: {err}"));
            }
        }

        match state.deck_b.capture_png(path_b, &mut self.logger) {
            Ok(()) => {
                let msg = format!("Saved Deck B PNG capture -> {}", path_b.display());
                println!("{msg}");
                self.logger.log_info(&msg);
            }
            Err(err) => {
                eprintln!("Deck B PNG capture error: {err}");
                self.logger.log_info(&format!("Deck B PNG capture error: {err}"));
            }
        }

        state.png_captured = true;
    }

    fn cycle_preset(&mut self, is_deck_a: bool, delta: isize) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let count = state.presets.len() as isize;
        if count == 0 {
            return;
        }
        let idx = if is_deck_a {
            state.deck_a_preset_idx =
                ((state.deck_a_preset_idx as isize + delta).rem_euclid(count)) as usize;
            state.deck_a_preset_idx
        } else {
            state.deck_b_preset_idx =
                ((state.deck_b_preset_idx as isize + delta).rem_euclid(count)) as usize;
            state.deck_b_preset_idx
        };

        let path = state.presets[idx].clone();
        let label = preset_label(&path);
        let deck_name = if is_deck_a { "Deck A" } else { "Deck B" };
        let res = if is_deck_a {
            state.deck_a.load_preset(&path, true)
        } else {
            state.deck_b.load_preset(&path, true)
        };
        match res {
            Ok(()) => {
                let msg = format!("{deck_name} preset -> [{}/{}] {label}", idx + 1, count);
                println!("{msg}");
                self.logger.log_info(&msg);
            }
            Err(err) => {
                eprintln!("{deck_name} preset load error: {err}");
                self.logger.log_info(&format!("{deck_name} preset load error: {err}"));
            }
        }
    }

    /// Drives the automated soak sequence when `--soak-minutes` is active.
    /// Returns `true` when the soak timer has completed and the app should exit.
    fn advance_soak_schedule(&mut self) -> bool {
        let (elapsed, target_dur, phase, png_captured) = {
            let Some(state) = &self.state else {
                return false;
            };
            let Some(dur) = state.soak_duration else {
                return false;
            };
            (state.start_time.elapsed(), dur, state.soak_phase, state.png_captured)
        };

        if elapsed >= target_dur {
            return true;
        }

        match phase {
            SoakPhase::Independence if elapsed >= Duration::from_secs(5) && !png_captured => {
                // Capture PNG evidence mid-way through Test 1 so both distinct presets are vivid.
                self.capture_both_pngs();
            }
            SoakPhase::Independence if elapsed >= Duration::from_secs(10) => {
                if let Some(s) = self.state.as_mut() {
                    s.deck_b_audio = true;
                    s.soak_phase = SoakPhase::Swapped;
                }
                println!("[Soak Test 2/4] NO BLEED (SWAP): Both decks active, swapping presets (10s)...");
                self.logger.log_info("[Soak Test 2/4] NO BLEED (SWAP) started");
                self.swap_presets();
            }
            SoakPhase::Swapped if elapsed >= Duration::from_secs(20) => {
                if let Some(s) = self.state.as_mut() {
                    s.soak_phase = SoakPhase::BadPreset;
                }
                println!("[Soak Test 3/4] BAD PRESET: Loading broken preset onto Deck A (10s)...");
                self.logger.log_info("[Soak Test 3/4] BAD PRESET started");
                self.trigger_bad_preset_on_deck_a();
            }
            SoakPhase::BadPreset if elapsed >= Duration::from_secs(30) => {
                if let Some(s) = self.state.as_mut() {
                    s.soak_phase = SoakPhase::SteadySoak;
                }
                println!(
                    "[Soak Test 4/4] STEADY STABILITY SOAK: Both decks rendering @ {DECK_WIDTH}x{DECK_HEIGHT} until {:.1} min...",
                    target_dur.as_secs_f64() / 60.0
                );
                self.logger.log_info("[Soak Test 4/4] STEADY STABILITY SOAK started");
                // Reload a valid preset onto Deck A for the long soak.
                self.cycle_preset(true, 1);
            }
            _ => {}
        }

        false
    }

    fn render_frame(&mut self, event_loop: &ActiveEventLoop) {
        if self.advance_soak_schedule() {
            self.shutdown();
            event_loop.exit();
            return;
        }

        let Some(state) = self.state.as_mut() else {
            return;
        };

        state.frame_count += 1;
        let frame_idx = state.frame_count;
        let frame_start = Instant::now();

        // 1. Render Deck A (1920x1080) inside Deck A's shared OpenGL context.
        state
            .audio_a
            .fill_stereo(&mut state.pcm_a, state.deck_a_audio);
        let t_a = Instant::now();
        state
            .deck_a
            .render_to_texture(&state.pcm_a, frame_idx, &mut self.logger)
            .expect("Deck A render failed");
        let a_ms = t_a.elapsed().as_secs_f64() * 1000.0;
        state.timing_a.record_ms(a_ms);

        // 2. Render Deck B (1920x1080) inside Deck B's shared OpenGL context.
        state
            .audio_b
            .fill_stereo(&mut state.pcm_b, state.deck_b_audio);
        let t_b = Instant::now();
        state
            .deck_b
            .render_to_texture(&state.pcm_b, frame_idx, &mut self.logger)
            .expect("Deck B render failed");
        let b_ms = t_b.elapsed().as_secs_f64() * 1000.0;
        state.timing_b.record_ms(b_ms);

        // 3. Switch back to the main window context and draw Deck A (left) and Deck B (right).
        state
            .main_context
            .make_current(&state.main_surface)
            .expect("Failed to make main context current for compositing");

        let win_size = state.window.inner_size();
        state.compositor.draw(
            &state.main_gl,
            win_size.width.max(1) as i32,
            win_size.height.max(1) as i32,
            state.deck_a.output_texture(),
            state.deck_b.output_texture(),
        );
        self.logger
            .check_frame(&state.main_gl, frame_idx, "main-composite");

        state
            .main_surface
            .swap_buffers(&state.main_context)
            .expect("Failed to swap main window buffers");

        let total_ms = frame_start.elapsed().as_secs_f64() * 1000.0;
        // Skip first 5 warmup frames when recording worst-case spikes (shader compile warmup).
        if frame_idx > 5 {
            state.timing_total.record_ms(total_ms);
        }

        // 4. Print a 1-second (or 15-second in soak mode) heartbeat.
        let report_interval = if state.soak_duration.is_some() { 15.0 } else { 2.0 };
        let since_report = state.last_report_time.elapsed().as_secs_f64();
        if since_report >= report_interval {
            let delta_frames = state.frame_count - state.last_report_frames;
            let fps = delta_frames as f64 / since_report;
            state.deck_a.set_fps(fps.round() as i32);
            state.deck_b.set_fps(fps.round() as i32);

            let mem_str = current_process_memory_mb()
                .map(|m| format!("mem={:.1}MiB", m.working_set_mb))
                .unwrap_or_else(|| "mem=n/a".to_string());

            let line = format!(
                "[Stage 3 | {:.0}s] fps={:.1} | A={:.2}ms(max {:.2}) | B={:.2}ms(max {:.2}) | total={:.2}ms(max {:.2}) | {} | gl_err={}",
                state.start_time.elapsed().as_secs_f64(),
                fps,
                state.timing_a.avg_ms(),
                state.timing_a.max_ms(),
                state.timing_b.avg_ms(),
                state.timing_b.max_ms(),
                state.timing_total.avg_ms(),
                state.timing_total.max_ms(),
                mem_str,
                self.logger.total_errors()
            );
            println!("{line}");
            self.logger.log_info(&line);
            state.last_report_time = Instant::now();
            state.last_report_frames = state.frame_count;
        }

        state.window.request_redraw();
    }

    fn shutdown(&mut self) {
        if let Some(mut state) = self.state.take() {
            let secs = state.start_time.elapsed().as_secs_f64();
            let avg_fps = if secs > 0.0 {
                state.frame_count as f64 / secs
            } else {
                0.0
            };
            let end_mem = current_process_memory_mb();

            let summary = format!(
                "=== Stage 3 Summary ===\n\
                 Duration:        {:.2}s ({} frames, avg {:.2} FPS)\n\
                 Resolution:      {DECK_WIDTH}x{DECK_HEIGHT} per deck (x2)\n\
                 Deck A render:   avg {:.2} ms | worst {:.2} ms ({} frames)\n\
                 Deck B render:   avg {:.2} ms | worst {:.2} ms ({} frames)\n\
                 Total frame:     avg {:.2} ms | worst {:.2} ms (after 5-frame warmup)\n\
                 Start memory:    {}\n\
                 End memory:      {}\n\
                 Total GL errors: {}",
                secs,
                state.frame_count,
                avg_fps,
                state.timing_a.avg_ms(),
                state.timing_a.max_ms(),
                state.timing_a.samples(),
                state.timing_b.avg_ms(),
                state.timing_b.max_ms(),
                state.timing_b.samples(),
                state.timing_total.avg_ms(),
                state.timing_total.max_ms(),
                format_mem(state.start_memory),
                format_mem(end_mem),
                self.logger.total_errors()
            );
            println!("\n{summary}");
            println!("Log file saved to: {}", self.logger.path().display());
            self.logger.log_info(&summary);

            // Destroy decks while their own contexts are current, then compositor on main context.
            state.deck_a.destroy();
            state.deck_b.destroy();
            if state.main_context.make_current(&state.main_surface).is_ok() {
                state.compositor.destroy(&state.main_gl);
            }
            drop(state);
        }
    }
}

impl ApplicationHandler for Stage3App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            self.init_all(event_loop);
            if let Some(state) = &self.state {
                state.window.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.shutdown();
                event_loop.exit();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => match logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.shutdown();
                    event_loop.exit();
                }
                Key::Named(NamedKey::ArrowRight) => self.cycle_preset(true, 1),
                Key::Named(NamedKey::ArrowLeft) => self.cycle_preset(true, -1),
                Key::Named(NamedKey::ArrowUp) => self.cycle_preset(false, 1),
                Key::Named(NamedKey::ArrowDown) => self.cycle_preset(false, -1),
                Key::Character(ref ch) if ch == "1" => self.set_independence_mode(),
                Key::Character(ref ch) if ch == "2" => {
                    if let Some(s) = self.state.as_mut() {
                        s.deck_a_audio = true;
                        s.deck_b_audio = true;
                        println!("Audio: Both Deck A and Deck B = TEST_AUDIO");
                        self.logger.log_info("Audio: Both decks = TEST_AUDIO");
                    }
                }
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("a") => {
                    if let Some(s) = self.state.as_mut() {
                        s.deck_a_audio = !s.deck_a_audio;
                        println!("Deck A audio -> {}", s.deck_a_audio);
                    }
                }
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("b") => {
                    if let Some(s) = self.state.as_mut() {
                        s.deck_b_audio = !s.deck_b_audio;
                        println!("Deck B audio -> {}", s.deck_b_audio);
                    }
                }
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("s") => self.swap_presets(),
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("e") => {
                    self.trigger_bad_preset_on_deck_a()
                }
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("p") => self.capture_both_pngs(),
                _ => {}
            },
            WindowEvent::Resized(new_size) => {
                if let Some(state) = &self.state {
                    if let (Some(w), Some(h)) = (
                        NonZeroU32::new(new_size.width),
                        NonZeroU32::new(new_size.height),
                    ) {
                        if state.main_context.make_current(&state.main_surface).is_ok() {
                            state.main_surface.resize(&state.main_context, w, h);
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.render_frame(event_loop);
            }
            _ => {}
        }
    }
}

fn format_mem(mem: Option<MemorySnapshotMb>) -> String {
    match mem {
        Some(m) => format!(
            "WorkingSet={:.1} MiB, PrivateCommit={:.1} MiB",
            m.working_set_mb, m.private_commit_mb
        ),
        None => "unavailable".to_string(),
    }
}

fn discover_valid_presets(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Could not read preset folder `{}`: {e}", dir.display()))?;

    let mut presets = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Error reading preset directory: {e}"))?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_milk = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("milk"))
            .unwrap_or(false);
        // Skip our deliberately broken test preset (prefixed with `_bad_`) from the normal playlist.
        if is_milk && !name.starts_with("_bad_") {
            presets.push(path);
        }
    }
    presets.sort();

    if presets.len() < 2 {
        return Err(format!(
            "Need at least 2 valid .milk presets in `{}` for Stage 3",
            dir.display()
        ));
    }
    Ok(presets)
}

fn preset_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

/// Parses optional `--soak-minutes <float>` from command-line arguments.
fn parse_soak_minutes() -> Option<Duration> {
    let args: Vec<String> = std::env::args().collect();
    for i in 1..args.len() {
        if args[i] == "--soak-minutes" {
            if let Some(val) = args.get(i + 1) {
                if let Ok(mins) = val.parse::<f64>() {
                    if mins > 0.0 {
                        return Some(Duration::from_secs_f64(mins * 60.0));
                    }
                }
            }
        }
    }
    None
}

fn main() {
    let pm_version = projectm_version_string();
    println!("libprojectM runtime version: {pm_version}");

    let presets = discover_valid_presets(Path::new("presets"))
        .expect("Failed to load presets from C:\\dev\\vj\\presets\\");
    let bad_preset_path = PathBuf::from("presets/_bad_corrupt_test.milk");

    let soak_duration = parse_soak_minutes();
    println!(
        "Loaded {} valid presets. Bad preset test file: {}",
        presets.len(),
        bad_preset_path.display()
    );
    if let Some(dur) = soak_duration {
        println!(
            "Mode: AUTOMATED SOAK ({:.2} minutes). Will run Tests 1-3, save PNGs to docs/spike/, and measure stability.",
            dur.as_secs_f64() / 60.0
        );
    } else {
        println!("Mode: INTERACTIVE.");
        println!("  1 = Test 1 Independence (Deck A audio, Deck B silence) | 2 = Both audio");
        println!("  S = Test 2 Swap presets | E = Test 3 Bad preset on Deck A | P = Save PNGs");
        println!("  Left/Right = Deck A preset | Up/Down = Deck B preset | Esc = Exit");
    }

    let logger = GlErrorLogger::open("logs/stage3_spike.log")
        .expect("Could not initialize Stage 3 log file");

    let event_loop = EventLoop::new().expect("Failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = Stage3App::new(logger, presets, bad_preset_path, soak_duration);
    event_loop
        .run_app(&mut app)
        .expect("Event loop exited with an error");
}
