//! Milestone 1 Part 2 — Stage B: Two `libprojectM 4.1.8` Decks Driven by Live
//! Windows WASAPI Loopback Audio via a Lock-Free Ring Buffer.
//!
//! Threading model:
//! - **Background thread (`wasapi-loopback-capture`):** Owns `WasapiLoopbackSource`,
//!   reads native Windows loopback packets, converts/downmixes to stereo `f32`, and
//!   pushes timestamped frames into a lock-free SPSC ring buffer (`RingProducer`).
//! - **Main thread:** Owns all OpenGL contexts (`Deck A`, `Deck B`, `main window`),
//!   pops audio from `RingConsumer` without blocking, measures the age of the oldest
//!   sample reaching `libprojectM`, feeds both decks, and composites side by side.
//!
//! CLI flags:
//! - `--audio-source live|test`   Select Live WASAPI Loopback (default) or Synthetic Test Audio
//! - `--audio-device <filter>`    Select output device by 1-based index or name substring
//! - `--deck-size WxH`            Internal render resolution per deck (default `1920x1080`)
//! - `--measure-gpu`              Diagnostic `glFinish()` split timing (OFF in live runs)
//! - `--soak-minutes N`           Active soak with 10s preset rotation & 30s full telemetry
//! - `--log <path>`               Custom log file path
//!
//! Interactive keys:
//! - `L`          Toggle Live WASAPI Loopback vs. Synthetic Test Audio
//! - `R`          Reconnect Live WASAPI Loopback to current Windows default output device
//! - `1` / `2`    Deck A active + Deck B silent (Independence) / Both decks active
//! - `S`          Swap presets between Deck A and Deck B
//! - `E`          Load broken shader preset onto Deck A
//! - `P`          Capture 1080p PNGs to `docs/spike/`
//! - `Left/Right` Cycle Deck A preset | `Up/Down` Cycle Deck B preset | `Esc` Clean exit

mod compositor;
mod gl_log;
mod gl_state;
mod metrics;
mod render_deck;

use milkdrop_vj::audio::capture_thread::LiveAudioCapture;
use milkdrop_vj::audio::wasapi_loopback::WasapiLoopbackSource;
use milkdrop_vj::audio::AudioSource;
use milkdrop_vj::deck::{
    init_gl_loader_for_projectm, max_pcm_samples_per_channel, projectm_version_string,
};
use milkdrop_vj::test_audio::TestAudioGenerator;

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
use crate::gl_log::GlErrorLogger;
use crate::metrics::{
    current_process_memory_mb, FpsBucketTracker, GpuTelemetry, GpuTelemetryMonitor,
    MemorySnapshotMb, TimingStats,
};
use crate::render_deck::RenderDeck;

/// Threshold for counting slow frames: 1 / 55 FPS = 18.1818 ms.
const FRAME_BELOW_55_FPS_MS: f64 = 1000.0 / 55.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioMode {
    LiveWasapi,
    SyntheticTest,
}

#[derive(Clone, Debug)]
enum RunMode {
    Interactive,
    ActiveSoak { minutes: f64 },
}

#[derive(Clone, Debug)]
struct CliConfig {
    mode: RunMode,
    audio_mode: AudioMode,
    audio_device_filter: Option<String>,
    deck_width: u32,
    deck_height: u32,
    measure_gpu: bool,
    log_path: PathBuf,
}

struct ActiveSoakRunner {
    target_duration: Duration,
    last_switch_time: Instant,
    switch_count: usize,
    next_preset_cursor: usize,
    last_telemetry_time: Instant,
    last_1s_time: Instant,
    last_1s_frames: u64,
    fps_window_30s: FpsBucketTracker,
    fps_all_1s: FpsBucketTracker,
    frames_below_55: u64,
    min_gpu_clock: Option<u32>,
    max_gpu_clock: Option<u32>,
    max_gpu_temp: Option<u32>,
    gpu_monitor: GpuTelemetryMonitor,
}

struct StageBState {
    config: CliConfig,
    deck_a: RenderDeck,
    deck_b: RenderDeck,
    compositor: SideBySideCompositor,
    window: Window,
    main_surface: Surface<WindowSurface>,
    main_context: PossiblyCurrentContext,
    main_gl: glow::Context,

    // Swappable live vs. synthetic audio pipeline.
    audio_mode: AudioMode,
    live_capture: Option<LiveAudioCapture>,
    last_wasapi_packets: u64,
    audio_age_stats: TimingStats,
    audio_a_gen: TestAudioGenerator,
    max_pcm_samples: usize,
    live_pcm_buf: Vec<f32>,
    synth_pcm_buf: Vec<f32>,
    silence_pcm_buf: Vec<f32>,
    deck_a_audio: bool,
    deck_b_audio: bool,
    device_lost_logged: bool,

    // Presets
    presets: Vec<PathBuf>,
    bad_shader_path: PathBuf,
    deck_a_preset_idx: usize,
    deck_b_preset_idx: usize,

    // Timing, FPS, and memory tracking
    start_time: Instant,
    last_frame_instant: Instant,
    last_report_time: Instant,
    last_report_frames: u64,
    frame_count: u64,
    timing_a_cpu: TimingStats,
    timing_a_gpu: TimingStats,
    timing_a: TimingStats,
    timing_b_cpu: TimingStats,
    timing_b_gpu: TimingStats,
    timing_b: TimingStats,
    timing_total: TimingStats,
    frames_below_55: u64,
    start_memory: Option<MemorySnapshotMb>,

    soak_runner: Option<ActiveSoakRunner>,
}

struct StageBApp {
    state: Option<StageBState>,
    logger: GlErrorLogger,
    presets: Vec<PathBuf>,
    config: CliConfig,
}

impl StageBApp {
    fn new(logger: GlErrorLogger, presets: Vec<PathBuf>, config: CliConfig) -> Self {
        Self {
            state: None,
            logger,
            presets,
            config,
        }
    }

    fn init_all(&mut self, event_loop: &ActiveEventLoop) {
        let mode_label = match self.config.mode {
            RunMode::Interactive => {
                "Interactive (L:Live/Test, R:Reconnect, 1/2:Audio, S:Swap, Left/Right/Up/Down:Preset, Esc:Exit)"
                    .to_string()
            }
            RunMode::ActiveSoak { minutes } => format!("Live Audio Soak ({minutes:.1} min)"),
        };
        let title = format!(
            "MilkDrop VJ — Stage B ({}x{} x2) — {}",
            self.config.deck_width, self.config.deck_height, mode_label
        );

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

        let max_pcm_samples = max_pcm_samples_per_channel();
        let header = format!(
            "=== Stage B Ready | deck_size={}x{} | audio={:?} | max_pcm_samples={} | mode={:?} ===",
            self.config.deck_width,
            self.config.deck_height,
            self.config.audio_mode,
            max_pcm_samples,
            self.config.mode
        );
        println!("{header}");
        println!("GL_VENDOR:   {vendor}");
        println!("GL_RENDERER: {renderer}");
        println!("GL_VERSION:  {version}");
        self.logger.log_info(&header);
        self.logger.log_info(&format!("GL_VENDOR={vendor}"));
        self.logger.log_info(&format!("GL_RENDERER={renderer}"));
        self.logger.log_info(&format!("GL_VERSION={version}"));

        init_gl_loader_for_projectm().expect("Failed to initialize GLEW for libprojectM");
        self.logger.log_info("GLEW initialized for libprojectM.");

        let compositor =
            SideBySideCompositor::new(&main_gl).expect("Failed to create side-by-side compositor");
        self.logger.check_frame(&main_gl, 0, "main-init");

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

        let deck_a = RenderDeck::new(
            "DeckA",
            &gl_display,
            &pbuffer_config,
            &main_context,
            self.config.deck_width,
            self.config.deck_height,
            &mut self.logger,
        )
        .expect("Failed to create Deck A");

        let deck_b = RenderDeck::new(
            "DeckB",
            &gl_display,
            &pbuffer_config,
            &main_context,
            self.config.deck_width,
            self.config.deck_height,
            &mut self.logger,
        )
        .expect("Failed to create Deck B");

        let deck_a_idx = 0;
        let deck_b_idx = 1.min(self.presets.len().saturating_sub(1));
        deck_a
            .load_preset(&self.presets[deck_a_idx], false)
            .unwrap_or_else(|e| self.logger.log_info(&format!("Deck A init err: {e}")));
        deck_b
            .load_preset(&self.presets[deck_b_idx], false)
            .unwrap_or_else(|e| self.logger.log_info(&format!("Deck B init err: {e}")));

        main_context
            .make_current(&main_surface)
            .expect("Failed to restore main context after deck init");

        // Start the background WASAPI loopback capture thread if requested (or when Live mode is on).
        let selected_device_id =
            resolve_device_id(self.config.audio_device_filter.as_deref(), &mut self.logger);
        let live_capture = match LiveAudioCapture::start(selected_device_id) {
            Ok(cap) => {
                let fmt = cap.format();
                let msg = format!(
                    "Live WASAPI Capture Started: `{}` | {} | Native: {} Hz, {} ch, {} -> Output: {} Hz stereo f32",
                    fmt.device_name,
                    fmt.capture_mode_label,
                    fmt.native_sample_rate,
                    fmt.native_channels,
                    fmt.native_sample_type,
                    fmt.output_sample_rate
                );
                println!("{msg}");
                self.logger.log_info(&msg);
                Some(cap)
            }
            Err(err) => {
                let msg = format!(
                    "WARNING: Could not start Live WASAPI Loopback ({err}). Falling back to Synthetic Test Audio."
                );
                eprintln!("{msg}");
                self.logger.log_info(&msg);
                None
            }
        };

        let active_audio_mode = if live_capture.is_some() {
            self.config.audio_mode
        } else {
            AudioMode::SyntheticTest
        };

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

        let now = Instant::now();
        let soak_runner = match self.config.mode {
            RunMode::ActiveSoak { minutes } => {
                let gpu_monitor = GpuTelemetryMonitor::start(Duration::from_secs(30));
                if let Some(gpu) = gpu_monitor.latest() {
                    let msg = format!(
                        "Start GPU (nvidia-smi): VRAM={}/{} MiB, Temp={}C, Clock={} MHz",
                        gpu.vram_used_mib, gpu.vram_total_mib, gpu.temp_c, gpu.graphics_clock_mhz
                    );
                    println!("{msg}");
                    self.logger.log_info(&msg);
                }
                let msg = format!(
                    "[Stage B Soak] Running {:.1} minutes with {:?} audio, 10s preset rotation, 30s telemetry...",
                    minutes, active_audio_mode
                );
                println!("{msg}");
                self.logger.log_info(&msg);
                Some(ActiveSoakRunner {
                    target_duration: Duration::from_secs_f64(minutes * 60.0),
                    last_switch_time: now,
                    switch_count: 0,
                    next_preset_cursor: 2,
                    last_telemetry_time: now,
                    last_1s_time: now,
                    last_1s_frames: 0,
                    fps_window_30s: FpsBucketTracker::default(),
                    fps_all_1s: FpsBucketTracker::default(),
                    frames_below_55: 0,
                    min_gpu_clock: None,
                    max_gpu_clock: None,
                    max_gpu_temp: None,
                    gpu_monitor,
                })
            }
            RunMode::Interactive => None,
        };

        let synth_frames = max_pcm_samples.min(800);
        self.state = Some(StageBState {
            config: self.config.clone(),
            deck_a,
            deck_b,
            compositor,
            window,
            main_surface,
            main_context,
            main_gl,
            audio_mode: active_audio_mode,
            live_capture,
            last_wasapi_packets: 0,
            audio_age_stats: TimingStats::default(),
            audio_a_gen: TestAudioGenerator::new(48_000.0),
            max_pcm_samples,
            live_pcm_buf: Vec::with_capacity(max_pcm_samples * 2),
            synth_pcm_buf: vec![0.0_f32; synth_frames * 2],
            silence_pcm_buf: vec![0.0_f32; 512 * 2],
            deck_a_audio: true,
            deck_b_audio: true,
            device_lost_logged: false,
            presets: self.presets.clone(),
            bad_shader_path: PathBuf::from("presets/_bad_shader_test.milk"),
            deck_a_preset_idx: deck_a_idx,
            deck_b_preset_idx: deck_b_idx,
            start_time: now,
            last_frame_instant: now,
            last_report_time: now,
            last_report_frames: 0,
            frame_count: 0,
            timing_a_cpu: TimingStats::default(),
            timing_a_gpu: TimingStats::default(),
            timing_a: TimingStats::default(),
            timing_b_cpu: TimingStats::default(),
            timing_b_gpu: TimingStats::default(),
            timing_b: TimingStats::default(),
            timing_total: TimingStats::default(),
            frames_below_55: 0,
            start_memory,
            soak_runner,
        });
    }

    fn reconnect_live_audio(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        println!("Reconnecting Live WASAPI Loopback to current Windows default output...");
        state.live_capture.take(); // Drop old thread cleanly first.
        match LiveAudioCapture::start(None) {
            Ok(cap) => {
                let msg = format!(
                    "Reconnected Live WASAPI -> `{}` ({} Hz, {} ch, {})",
                    cap.format().device_name,
                    cap.format().native_sample_rate,
                    cap.format().native_channels,
                    cap.format().native_sample_type
                );
                println!("{msg}");
                self.logger.log_info(&msg);
                state.live_capture = Some(cap);
                state.audio_mode = AudioMode::LiveWasapi;
                state.device_lost_logged = false;
                state.last_wasapi_packets = 0;
            }
            Err(err) => {
                let msg = format!("Reconnect failed: {err}");
                eprintln!("{msg}");
                self.logger.log_info(&msg);
            }
        }
    }

    fn render_frame(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let now = Instant::now();
        let frame_delta_ms = now.duration_since(state.last_frame_instant).as_secs_f64() * 1000.0;
        state.last_frame_instant = now;

        state.frame_count += 1;
        let frame_idx = state.frame_count;
        if frame_idx > 5 && frame_delta_ms > FRAME_BELOW_55_FPS_MS {
            state.frames_below_55 += 1;
        }

        // 0. Pull the latest stereo audio from either the lock-free WASAPI ring buffer
        //    or our swappable Synthetic Test Audio generator.
        let active_pcm_slice: &[f32] = match state.audio_mode {
            AudioMode::LiveWasapi => {
                if let Some(cap) = state.live_capture.as_mut() {
                    if !state.device_lost_logged && cap.device_lost_events() > 0 {
                        state.device_lost_logged = true;
                        let err_txt = cap.last_error().unwrap_or_else(|| "unknown error".into());
                        let msg = format!(
                            "[AUDIO EVENT] Device lost ({err_txt}). Visuals continue on silence; press `R` to reconnect."
                        );
                        println!("{msg}");
                        self.logger.log_info(&msg);
                    }

                    // Drain ALL newly arrived frames from the ring buffer on each video frame
                    // (at 48 kHz / 60 FPS, ~800 frames arrive per tick, which is larger than
                    // `projectm_pcm_get_max_samples()` = 576). `Deck::add_pcm_stereo` feeds
                    // them in chronological chunks of `<= 576` so the ring never backs up.
                    let pop = cap.pop_stereo(&mut state.live_pcm_buf, 4096);
                    let cur_pkts = cap.packets_received();
                    let got_live_packet = cur_pkts > state.last_wasapi_packets;
                    state.last_wasapi_packets = cur_pkts;

                    // Record oldest-sample age only when real live audio packets arrived
                    // (not during 20 ms silence-timeout padding between songs).
                    if got_live_packet {
                        if let Some(age_ms) = pop.oldest_sample_age_ms {
                            if frame_idx > 10 {
                                state.audio_age_stats.record_ms(age_ms);
                            }
                        }
                    }

                    if pop.frames_read > 0 {
                        &state.live_pcm_buf
                    } else {
                        &state.silence_pcm_buf
                    }
                } else {
                    &state.silence_pcm_buf
                }
            }
            AudioMode::SyntheticTest => {
                // Keep draining the live ring buffer silently if running so it never overruns
                // while temporarily switched to Synthetic Test mode.
                if let Some(cap) = state.live_capture.as_mut() {
                    let _ = cap.pop_stereo(&mut state.live_pcm_buf, 4096);
                }
                state.audio_a_gen.fill_stereo(&mut state.synth_pcm_buf, true);
                &state.synth_pcm_buf
            }
        };

        let pcm_for_a: &[f32] = if state.deck_a_audio {
            active_pcm_slice
        } else {
            &state.silence_pcm_buf
        };
        let pcm_for_b: &[f32] = if state.deck_b_audio {
            active_pcm_slice
        } else {
            &state.silence_pcm_buf
        };

        let measure_gpu = state.config.measure_gpu;
        let work_start = Instant::now();

        // 1. Render Deck A (strictly on the main thread).
        let timing_a_frame = state
            .deck_a
            .render_to_texture(pcm_for_a, frame_idx, measure_gpu, &mut self.logger)
            .expect("Deck A render failed");
        if frame_idx > 5 {
            state.timing_a_cpu.record_ms(timing_a_frame.cpu_submit_ms);
            state.timing_a_gpu.record_ms(timing_a_frame.gpu_wait_ms);
            state.timing_a.record_ms(timing_a_frame.total_ms());
        }

        // 2. Render Deck B (strictly on the main thread).
        let timing_b_frame = state
            .deck_b
            .render_to_texture(pcm_for_b, frame_idx, measure_gpu, &mut self.logger)
            .expect("Deck B render failed");
        if frame_idx > 5 {
            state.timing_b_cpu.record_ms(timing_b_frame.cpu_submit_ms);
            state.timing_b_gpu.record_ms(timing_b_frame.gpu_wait_ms);
            state.timing_b.record_ms(timing_b_frame.total_ms());
        }

        // 3. Composite both decks side by side on the main window context.
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

        let total_work_ms = if measure_gpu {
            unsafe {
                state.main_gl.finish();
            }
            work_start.elapsed().as_secs_f64() * 1000.0
        } else {
            0.0
        };

        state
            .main_surface
            .swap_buffers(&state.main_context)
            .expect("Failed to swap main window buffers");

        let total_recorded_ms = if measure_gpu {
            unsafe {
                state.main_gl.finish();
            }
            total_work_ms
        } else {
            work_start.elapsed().as_secs_f64() * 1000.0
        };
        if frame_idx > 5 {
            state.timing_total.record_ms(total_recorded_ms);
        }

        // 4. Active 10-minute soak schedule (with live music playing).
        let mut should_exit = false;
        if let Some(soak) = state.soak_runner.as_mut() {
            if frame_idx > 5 && frame_delta_ms > FRAME_BELOW_55_FPS_MS {
                soak.frames_below_55 += 1;
            }

            let sec_dt = soak.last_1s_time.elapsed().as_secs_f64();
            if sec_dt >= 1.0 {
                let df = state.frame_count - soak.last_1s_frames;
                let fps = df as f64 / sec_dt;
                soak.fps_window_30s.push(fps);
                soak.fps_all_1s.push(fps);
                state.deck_a.set_fps(fps.round() as i32);
                state.deck_b.set_fps(fps.round() as i32);
                soak.last_1s_time = Instant::now();
                soak.last_1s_frames = state.frame_count;
            }

            // Switch a preset every 10 seconds, alternating Deck A and Deck B.
            if soak.last_switch_time.elapsed() >= Duration::from_secs(10) {
                soak.last_switch_time = Instant::now();
                soak.switch_count += 1;
                let use_deck_a = soak.switch_count % 2 == 1;
                let deck_label = if use_deck_a { "DeckA" } else { "DeckB" };
                let idx = soak.next_preset_cursor % state.presets.len();
                soak.next_preset_cursor += 1;
                let target_path = state.presets[idx].clone();
                let label = preset_label(&target_path);

                let res = if use_deck_a {
                    state.deck_a.load_preset(&target_path, false)
                } else {
                    state.deck_b.load_preset(&target_path, false)
                };
                match res {
                    Ok(()) => self.logger.log_info(&format!(
                        "[Soak switch #{}] {} -> `{}` (ok)",
                        soak.switch_count, deck_label, label
                    )),
                    Err(e) => self.logger.log_info(&format!(
                        "[Soak switch #{}] {} -> `{}` (err: {})",
                        soak.switch_count, deck_label, label, e
                    )),
                }
            }

            // Every 30 seconds: log FPS, RAM, GPU telemetry, and live audio latency/health.
            if soak.last_telemetry_time.elapsed() >= Duration::from_secs(30) {
                soak.last_telemetry_time = Instant::now();
                let elapsed_s = state.start_time.elapsed().as_secs();
                let mem = current_process_memory_mb();
                let fps_str = match soak.fps_window_30s.summary_and_clear() {
                    Some((mn, av, mx)) => format!("fps={mn:.1}/{av:.1}/{mx:.1}"),
                    None => "fps=n/a".to_string(),
                };
                let gpu_str = match soak.gpu_monitor.latest() {
                    Some(g) => {
                        update_gpu_extremes(soak, g);
                        format!(
                            "GPU(vram={}MiB, {}C, {}MHz)",
                            g.vram_used_mib, g.temp_c, g.graphics_clock_mhz
                        )
                    }
                    None => "GPU=n/a".to_string(),
                };
                let audio_str = match &state.live_capture {
                    Some(c) => format!(
                        "audio_age={:.1}ms(max {:.1}) | ovr={} und={} pkts={}",
                        state.audio_age_stats.avg_ms(),
                        state.audio_age_stats.max_ms(),
                        c.overrun_frames(),
                        c.underrun_reads(),
                        c.packets_received()
                    ),
                    None => "audio=SYNTH".to_string(),
                };
                let line = format!(
                    "[Soak {:>3}s] {} | {} | {} | {} | below55={} | gl_err={}",
                    elapsed_s,
                    fps_str,
                    format_mem_short(mem),
                    gpu_str,
                    audio_str,
                    soak.frames_below_55,
                    self.logger.total_errors()
                );
                println!("{line}");
                self.logger.log_info(&line);
            }

            if state.start_time.elapsed() >= soak.target_duration {
                should_exit = true;
            }
        } else {
            // Interactive heartbeat every 3 seconds.
            let since_report = state.last_report_time.elapsed().as_secs_f64();
            if since_report >= 3.0 {
                let df = state.frame_count - state.last_report_frames;
                let fps = df as f64 / since_report;
                state.deck_a.set_fps(fps.round() as i32);
                state.deck_b.set_fps(fps.round() as i32);
                let audio_str = match (&state.audio_mode, &state.live_capture) {
                    (AudioMode::LiveWasapi, Some(c)) => format!(
                        "LIVE age={:.1}ms(max {:.1}) ovr={} und={} pkts={}",
                        state.audio_age_stats.avg_ms(),
                        state.audio_age_stats.max_ms(),
                        c.overrun_frames(),
                        c.underrun_reads(),
                        c.packets_received()
                    ),
                    _ => "SYNTH_TEST".to_string(),
                };
                let line = format!(
                    "[Stage B | {:.0}s] fps={:.1} | {} | {} | gl_err={}",
                    state.start_time.elapsed().as_secs_f64(),
                    fps,
                    audio_str,
                    format_mem_short(current_process_memory_mb()),
                    self.logger.total_errors()
                );
                println!("{line}");
                self.logger.log_info(&line);
                state.last_report_time = Instant::now();
                state.last_report_frames = state.frame_count;
            }
        }

        if should_exit {
            self.shutdown();
            event_loop.exit();
            return;
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

            let audio_summary = match &state.live_capture {
                Some(c) => format!(
                    "Audio device:     {}\n\
                     Audio age (in):   avg {:.2} ms | worst {:.2} ms ({} live frames)\n\
                     Ring overruns:    {} dropped stereo frames\n\
                     Ring underruns:   {} empty reads (incl. silence between tracks)\n\
                     WASAPI packets:   {} | silence injections: {} | device-lost: {}",
                    c.format().device_name,
                    state.audio_age_stats.avg_ms(),
                    state.audio_age_stats.max_ms(),
                    state.audio_age_stats.samples(),
                    c.overrun_frames(),
                    c.underrun_reads(),
                    c.packets_received(),
                    c.silence_injections(),
                    c.device_lost_events()
                ),
                None => "Audio device:     Synthetic Test Signal (no live capture)".to_string(),
            };

            let mut summary = format!(
                "=== Stage B Summary ({:?}) ===\n\
                 Duration:         {:.2}s ({} frames, overall avg {:.2} FPS)\n\
                 Deck resolution:  {}x{} per deck (x2) | max_pcm_samples={}\n\
                 {audio_summary}\n\
                 Frames < 55 FPS:  {}\n\
                 Start memory:     {}\n\
                 End memory:       {}\n\
                 Total GL errors:  {}",
                state.config.mode,
                secs,
                state.frame_count,
                avg_fps,
                state.config.deck_width,
                state.config.deck_height,
                state.max_pcm_samples,
                state.frames_below_55,
                format_mem(state.start_memory),
                format_mem(end_mem),
                self.logger.total_errors()
            );

            if let Some(mut soak) = state.soak_runner.take() {
                if let Some(g) = soak.gpu_monitor.latest() {
                    update_gpu_extremes(&mut soak, g);
                }
                let fps_all = match soak.fps_all_1s.summary_and_clear() {
                    Some((mn, av, mx)) => format!("min={mn:.1} / avg={av:.1} / max={mx:.1}"),
                    None => "n/a".to_string(),
                };
                summary.push_str(&format!(
                    "\n--- Stage B Soak Totals ---\n\
                     Preset switches:  {}\n\
                     1s FPS overall:   {}\n\
                     Frames < 55 FPS:  {}\n\
                     GPU Clock range:  {:?}..={:?} MHz | Peak GPU Temp: {:?} C",
                    soak.switch_count,
                    fps_all,
                    soak.frames_below_55,
                    soak.min_gpu_clock,
                    soak.max_gpu_clock,
                    soak.max_gpu_temp
                ));
            }

            println!("\n{summary}");
            println!("Log file saved to: {}", self.logger.path().display());
            self.logger.log_info(&summary);

            // Stop capture thread cleanly before tearing down OpenGL.
            state.live_capture.take();
            state.deck_a.destroy();
            state.deck_b.destroy();
            if state.main_context.make_current(&state.main_surface).is_ok() {
                state.compositor.destroy(&state.main_gl);
            }
            drop(state);
        }
    }

    fn handle_key(&mut self, key: Key) {
        if matches!(&key, Key::Character(ch) if ch.eq_ignore_ascii_case("r")) {
            self.reconnect_live_audio();
            return;
        }

        let Some(state) = self.state.as_mut() else {
            return;
        };
        match key {
            Key::Character(ref ch) if ch.eq_ignore_ascii_case("l") => {
                state.audio_mode = match state.audio_mode {
                    AudioMode::LiveWasapi => AudioMode::SyntheticTest,
                    AudioMode::SyntheticTest => {
                        if state.live_capture.is_some() {
                            AudioMode::LiveWasapi
                        } else {
                            println!("No live WASAPI capture active — press `R` to connect.");
                            AudioMode::SyntheticTest
                        }
                    }
                };
                let msg = format!("Audio source switched -> {:?}", state.audio_mode);
                println!("{msg}");
                self.logger.log_info(&msg);
            }
            Key::Character(ref ch) if ch == "1" => {
                state.deck_a_audio = true;
                state.deck_b_audio = false;
                println!("Audio routing: Deck A = ACTIVE, Deck B = SILENCE");
            }
            Key::Character(ref ch) if ch == "2" => {
                state.deck_a_audio = true;
                state.deck_b_audio = true;
                println!("Audio routing: Both Deck A and Deck B = ACTIVE");
            }
            Key::Character(ref ch) if ch.eq_ignore_ascii_case("s") => {
                std::mem::swap(&mut state.deck_a_preset_idx, &mut state.deck_b_preset_idx);
                let _ = state
                    .deck_a
                    .load_preset(&state.presets[state.deck_a_preset_idx], false);
                let _ = state
                    .deck_b
                    .load_preset(&state.presets[state.deck_b_preset_idx], false);
                println!("Swapped presets between Deck A and Deck B.");
            }
            Key::Character(ref ch) if ch.eq_ignore_ascii_case("e") => {
                let bad = state.bad_shader_path.clone();
                match state.deck_a.load_preset(&bad, false) {
                    Ok(()) => println!("Bad shader preset returned Ok (no crash)."),
                    Err(e) => println!("Bad shader preset caught error (no crash): {e}"),
                }
            }
            Key::Character(ref ch) if ch.eq_ignore_ascii_case("p") => {
                let _ = state
                    .deck_a
                    .capture_png(Path::new("docs/spike/deck_a.png"), &mut self.logger);
                let _ = state
                    .deck_b
                    .capture_png(Path::new("docs/spike/deck_b.png"), &mut self.logger);
                println!("Captured docs/spike/deck_a.png and deck_b.png");
            }
            Key::Named(NamedKey::ArrowRight) => {
                state.deck_a_preset_idx = (state.deck_a_preset_idx + 1) % state.presets.len();
                let p = state.presets[state.deck_a_preset_idx].clone();
                let _ = state.deck_a.load_preset(&p, true);
                println!("Deck A -> {}", preset_label(&p));
            }
            Key::Named(NamedKey::ArrowLeft) => {
                state.deck_a_preset_idx =
                    (state.deck_a_preset_idx + state.presets.len() - 1) % state.presets.len();
                let p = state.presets[state.deck_a_preset_idx].clone();
                let _ = state.deck_a.load_preset(&p, true);
                println!("Deck A -> {}", preset_label(&p));
            }
            Key::Named(NamedKey::ArrowUp) => {
                state.deck_b_preset_idx = (state.deck_b_preset_idx + 1) % state.presets.len();
                let p = state.presets[state.deck_b_preset_idx].clone();
                let _ = state.deck_b.load_preset(&p, true);
                println!("Deck B -> {}", preset_label(&p));
            }
            Key::Named(NamedKey::ArrowDown) => {
                state.deck_b_preset_idx =
                    (state.deck_b_preset_idx + state.presets.len() - 1) % state.presets.len();
                let p = state.presets[state.deck_b_preset_idx].clone();
                let _ = state.deck_b.load_preset(&p, true);
                println!("Deck B -> {}", preset_label(&p));
            }
            _ => {}
        }
    }
}

impl ApplicationHandler for StageBApp {
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
            } => {
                if matches!(logical_key, Key::Named(NamedKey::Escape)) {
                    self.shutdown();
                    event_loop.exit();
                } else {
                    self.handle_key(logical_key);
                }
            }
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

fn resolve_device_id(filter: Option<&str>, logger: &mut GlErrorLogger) -> Option<String> {
    let filter = filter?;
    let src = WasapiLoopbackSource::new().ok()?;
    let devices = src.list_devices().ok()?;
    if let Ok(idx_1based) = filter.parse::<usize>() {
        if let Some(dev) = devices.get(idx_1based.saturating_sub(1)) {
            logger.log_info(&format!(
                "Selected audio device #{idx_1based}: `{}`",
                dev.name
            ));
            return Some(dev.id.clone());
        }
    }
    let needle = filter.to_ascii_lowercase();
    for dev in &devices {
        if dev.name.to_ascii_lowercase().contains(&needle) {
            logger.log_info(&format!(
                "Selected audio device matching `{filter}`: `{}`",
                dev.name
            ));
            return Some(dev.id.clone());
        }
    }
    logger.log_info(&format!(
        "Warning: No active output device matched `{filter}`; using Windows default."
    ));
    None
}

fn update_gpu_extremes(soak: &mut ActiveSoakRunner, g: GpuTelemetry) {
    soak.min_gpu_clock = Some(match soak.min_gpu_clock {
        Some(prev) => prev.min(g.graphics_clock_mhz),
        None => g.graphics_clock_mhz,
    });
    soak.max_gpu_clock = Some(match soak.max_gpu_clock {
        Some(prev) => prev.max(g.graphics_clock_mhz),
        None => g.graphics_clock_mhz,
    });
    soak.max_gpu_temp = Some(match soak.max_gpu_temp {
        Some(prev) => prev.max(g.temp_c),
        None => g.temp_c,
    });
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

fn format_mem_short(mem: Option<MemorySnapshotMb>) -> String {
    match mem {
        Some(m) => format!(
            "WS={:.1}MiB/Priv={:.1}MiB",
            m.working_set_mb, m.private_commit_mb
        ),
        None => "mem=n/a".to_string(),
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
        if is_milk && !name.starts_with('_') {
            presets.push(path);
        }
    }
    presets.sort();

    if presets.len() < 2 {
        return Err(format!(
            "Need at least 2 valid .milk presets in `{}`",
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

fn parse_deck_size(val: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = val.split(['x', 'X']).collect();
    if parts.len() != 2 {
        return Err(format!("Invalid --deck-size `{val}` (expected WxH like 1280x720)"));
    }
    let w: u32 = parts[0]
        .parse()
        .map_err(|_| format!("Invalid width in --deck-size `{val}`"))?;
    let h: u32 = parts[1]
        .parse()
        .map_err(|_| format!("Invalid height in --deck-size `{val}`"))?;
    if w < 64 || h < 64 || w > 4096 || h > 4096 {
        return Err(format!("--deck-size `{val}` out of safe range (64..=4096)"));
    }
    Ok((w, h))
}

fn parse_cli() -> CliConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut mode = RunMode::Interactive;
    let mut audio_mode = AudioMode::LiveWasapi;
    let mut audio_device_filter: Option<String> = None;
    let mut deck_width = 1920_u32;
    let mut deck_height = 1080_u32;
    let mut measure_gpu = false;
    let mut custom_log: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--measure-gpu" => measure_gpu = true,
            "--audio-source" => {
                i += 1;
                let val = args.get(i).expect("--audio-source requires `live` or `test`");
                audio_mode = match val.to_ascii_lowercase().as_str() {
                    "live" | "wasapi" => AudioMode::LiveWasapi,
                    "test" | "synth" => AudioMode::SyntheticTest,
                    other => panic!("Invalid --audio-source `{other}` (expected `live` or `test`)"),
                };
            }
            "--audio-device" => {
                i += 1;
                let val = args
                    .get(i)
                    .expect("--audio-device requires a 1-based index or name substring");
                audio_device_filter = Some(val.clone());
            }
            "--deck-size" => {
                i += 1;
                let val = args.get(i).expect("--deck-size requires WxH");
                let (w, h) = parse_deck_size(val).expect("Invalid --deck-size");
                deck_width = w;
                deck_height = h;
            }
            "--soak-minutes" => {
                i += 1;
                let val = args.get(i).expect("--soak-minutes requires a number");
                let mins: f64 = val.parse().expect("--soak-minutes must be a number");
                mode = RunMode::ActiveSoak { minutes: mins };
            }
            "--log" => {
                i += 1;
                let val = args.get(i).expect("--log requires a file path");
                custom_log = Some(PathBuf::from(val));
            }
            other => panic!("Unknown CLI flag: `{other}`"),
        }
        i += 1;
    }

    let default_log = match mode {
        RunMode::Interactive => PathBuf::from("logs/stage_b_live.log"),
        RunMode::ActiveSoak { .. } => PathBuf::from("logs/stage_b_soak_10m.log"),
    };

    CliConfig {
        mode,
        audio_mode,
        audio_device_filter,
        deck_width,
        deck_height,
        measure_gpu,
        log_path: custom_log.unwrap_or(default_log),
    }
}

fn main() {
    let pm_version = projectm_version_string();
    let max_pcm = max_pcm_samples_per_channel();
    let config = parse_cli();

    println!(
        "libprojectM runtime version: {pm_version} | projectm_pcm_get_max_samples() = {max_pcm}"
    );
    let presets = discover_valid_presets(Path::new("presets"))
        .expect("Failed to load presets from C:\\dev\\vj\\presets\\");
    println!(
        "Loaded {} valid presets | log file: {}",
        presets.len(),
        config.log_path.display()
    );
    println!("Keys: L = Live/Test audio | R = Reconnect WASAPI | 1/2 = Independence/Both | S = Swap | Left/Right/Up/Down = Presets | Esc = Exit");

    let logger = GlErrorLogger::open(&config.log_path)
        .expect("Could not initialize Stage B log file");

    let event_loop = EventLoop::new().expect("Failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = StageBApp::new(logger, presets, config);
    event_loop
        .run_app(&mut app)
        .expect("Event loop exited with an error");
}
