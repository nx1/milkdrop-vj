//! Stage 3 of the Two-Instance Spike: Two Independent `libprojectM 4.1.8` Decks
//! Side by Side (Pattern 1) with Diagnostic Benchmarks and Active Soak Runner.
//!
//! Supported command-line options:
//! - Default (no flags): Interactive side-by-side mode (`1920x1080` per deck)
//! - `--deck-size WxH`          Set internal render resolution per deck (default `1920x1080`,
//!                              for example `--deck-size 1280x720`)
//! - `--measure-gpu`            Diagnostic mode ONLY: calls `glFinish()` per deck so timers
//!                              report true render cost without VSync wait mixed in
//! - `--bad-preset-test`        Runs 3 bad-preset cases on Deck A (missing file, empty file,
//!                              broken `shader_body`) while Deck B runs at 60 FPS (~15s)
//! - `--preset-load-bench`      Measures cold and warm load + first-frame compile time (ms)
//!                              for all 10 presets in `presets/` (~10s)
//! - `--heavy-test-seconds N`   Loads `08_Reaction` (Deck A) and `09_Drawing` (Deck B) at once,
//!                              reports first 5s warmup separately from N seconds steady state
//! - `--soak-minutes N`         Active soak: swaps a preset every 10s (alternating decks,
//!                              including heavy and bad presets), logs RAM + FPS min/avg/max
//!                              + `nvidia-smi` GPU telemetry every 30s, reports frames < 55 FPS
//! - `--log <path>`             Write log output to a specific file path

mod compositor;
mod gl_log;
mod gl_state;
mod metrics;
mod render_deck;

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

#[derive(Clone, Debug)]
enum RunMode {
    Interactive,
    BadPresetTest,
    PresetLoadBench,
    HeavyTest { steady_secs: u64 },
    ActiveSoak { minutes: f64 },
}

#[derive(Clone, Debug)]
struct CliConfig {
    mode: RunMode,
    deck_width: u32,
    deck_height: u32,
    measure_gpu: bool,
    deck_a_preset_override: Option<PathBuf>,
    deck_b_preset_override: Option<PathBuf>,
    log_path: PathBuf,
}

/// Tracks the 3 sequential cases inside `--bad-preset-test`.
struct BadPresetRunner {
    step: usize, // 0=baseline(3s), 1=missing(4s), 2=empty(4s), 3=broken_shader(4s), 4=done
    step_start: Instant,
    step_frames: u64,
}

/// Tracks steady-state vs. warmup inside `--heavy-test-seconds`.
struct HeavyTestRunner {
    warmup_done: bool,
    warmup_frames: u64,
    warmup_start: Instant,
    warmup_worst_ms: f64,
    steady_start: Option<Instant>,
    steady_frames: u64,
    steady_below_55: u64,
    steady_a_cpu: TimingStats,
    steady_a_gpu: TimingStats,
    steady_a_total: TimingStats,
    steady_b_cpu: TimingStats,
    steady_b_gpu: TimingStats,
    steady_b_total: TimingStats,
    steady_timing_total: TimingStats,
    steady_fps_buckets: FpsBucketTracker,
    last_bucket_time: Instant,
    last_bucket_frames: u64,
}

/// Tracks active preset rotation and 30-second telemetry inside `--soak-minutes`.
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

struct Stage3State {
    config: CliConfig,
    deck_a: RenderDeck,
    deck_b: RenderDeck,
    compositor: SideBySideCompositor,
    window: Window,
    main_surface: Surface<WindowSurface>,
    main_context: PossiblyCurrentContext,
    main_gl: glow::Context,

    audio_a: TestAudioGenerator,
    audio_b: TestAudioGenerator,
    pcm_a: Vec<f32>,
    pcm_b: Vec<f32>,
    deck_a_audio: bool,
    deck_b_audio: bool,

    presets: Vec<PathBuf>,
    bad_missing_path: PathBuf,
    bad_empty_path: PathBuf,
    bad_shader_path: PathBuf,
    deck_a_preset_idx: usize,
    deck_b_preset_idx: usize,

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

    bad_runner: Option<BadPresetRunner>,
    heavy_runner: Option<HeavyTestRunner>,
    soak_runner: Option<ActiveSoakRunner>,
    load_bench_done: bool,
}

struct Stage3App {
    state: Option<Stage3State>,
    logger: GlErrorLogger,
    presets: Vec<PathBuf>,
    config: CliConfig,
}

impl Stage3App {
    fn new(logger: GlErrorLogger, presets: Vec<PathBuf>, config: CliConfig) -> Self {
        Self {
            state: None,
            logger,
            presets,
            config,
        }
    }

    fn init_all(&mut self, event_loop: &ActiveEventLoop) {
        let mode_label = match &self.config.mode {
            RunMode::Interactive => "Interactive (1:Indep, S:Swap, E:BadPreset, P:PNG, Esc:Exit)".to_string(),
            RunMode::BadPresetTest => "Test 3: Bad-Preset Cases (~15s)".to_string(),
            RunMode::PresetLoadBench => "Preset Load-Time Benchmark (~10s)".to_string(),
            RunMode::HeavyTest { steady_secs } => {
                format!("Heavy Preset Test: 08+09 ({steady_secs}s steady)")
            }
            RunMode::ActiveSoak { minutes } => format!("Active Soak ({minutes:.1} min)"),
        };
        let gpu_tag = if self.config.measure_gpu { " [measure-gpu]" } else { "" };
        let title = format!(
            "MilkDrop VJ — Stage 3 ({}x{} x2){} — {}",
            self.config.deck_width, self.config.deck_height, gpu_tag, mode_label
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

        let header = format!(
            "=== Stage 3 Ready | deck_size={}x{} | measure_gpu={} | mode={:?} ===",
            self.config.deck_width, self.config.deck_height, self.config.measure_gpu, self.config.mode
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

        // WGL pbuffer config bound to our real native window's HDC.
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

        // Pick initial presets based on run mode (with optional CLI overrides for Runs 3D/3E).
        let (deck_a_idx, deck_b_idx) = match self.config.mode {
            RunMode::HeavyTest { .. } => {
                let idx_08 = find_preset_by_prefix(&self.presets, "08_").unwrap_or(0);
                let idx_09 = find_preset_by_prefix(&self.presets, "09_").unwrap_or(1);
                (idx_08, idx_09)
            }
            _ => (0, 1.min(self.presets.len().saturating_sub(1))),
        };

        let init_path_a = self
            .config
            .deck_a_preset_override
            .clone()
            .unwrap_or_else(|| self.presets[deck_a_idx].clone());
        let init_path_b = self
            .config
            .deck_b_preset_override
            .clone()
            .unwrap_or_else(|| self.presets[deck_b_idx].clone());

        let label_a = preset_label(&init_path_a);
        let label_b = preset_label(&init_path_b);
        deck_a
            .load_preset(&init_path_a, false)
            .unwrap_or_else(|e| self.logger.log_info(&format!("Deck A init err: {e}")));
        deck_b
            .load_preset(&init_path_b, false)
            .unwrap_or_else(|e| self.logger.log_info(&format!("Deck B init err: {e}")));

        println!("Deck A initial preset: {label_a}");
        println!("Deck B initial preset: {label_b}");
        self.logger.log_info(&format!("Deck A initial preset: {label_a}"));
        self.logger.log_info(&format!("Deck B initial preset: {label_b}"));

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

        let now = Instant::now();
        let bad_runner = matches!(self.config.mode, RunMode::BadPresetTest).then(|| {
            println!("[Bad-Preset Test] Step 0/3: 3s baseline with valid presets on both decks...");
            self.logger
                .log_info("[Bad-Preset Test] Step 0/3: 3s baseline started");
            BadPresetRunner {
                step: 0,
                step_start: now,
                step_frames: 0,
            }
        });

        let heavy_runner = matches!(self.config.mode, RunMode::HeavyTest { .. }).then(|| {
            println!("[Heavy Test] First 5s = warmup/shader compile (tracked separately from steady state)...");
            self.logger.log_info(&format!(
                "[Heavy Test] Warmup 5s started (Deck A=`{label_a}`, Deck B=`{label_b}`)"
            ));
            HeavyTestRunner {
                warmup_done: false,
                warmup_frames: 0,
                warmup_start: now,
                warmup_worst_ms: 0.0,
                steady_start: None,
                steady_frames: 0,
                steady_below_55: 0,
                steady_a_cpu: TimingStats::default(),
                steady_a_gpu: TimingStats::default(),
                steady_a_total: TimingStats::default(),
                steady_b_cpu: TimingStats::default(),
                steady_b_gpu: TimingStats::default(),
                steady_b_total: TimingStats::default(),
                steady_timing_total: TimingStats::default(),
                steady_fps_buckets: FpsBucketTracker::default(),
                last_bucket_time: now,
                last_bucket_frames: 0,
            }
        });

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
                println!(
                    "[Active Soak] Running for {:.1} minutes: rotating presets every 10s (alternating decks, incl. heavy + bad), telemetry every 30s...",
                    minutes
                );
                self.logger.log_info(&format!(
                    "[Active Soak] Started ({:.1} minutes, 10s preset rotation, 30s telemetry)",
                    minutes
                ));
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
            _ => None,
        };

        let max_samples = max_pcm_samples_per_channel();
        self.state = Some(Stage3State {
            config: self.config.clone(),
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
            deck_b_audio: true,
            presets: self.presets.clone(),
            bad_missing_path: PathBuf::from("presets/_does_not_exist.milk"),
            bad_empty_path: PathBuf::from("presets/_bad_empty_test.milk"),
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
            bad_runner,
            heavy_runner,
            soak_runner,
            load_bench_done: false,
        });
    }

    /// Runs `--preset-load-bench` once after 10 warmup frames, measuring cold and warm
    /// `load_preset()` duration + first-frame compile duration for each of the 10 presets.
    fn run_preset_load_benchmark(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.load_bench_done || state.frame_count < 10 {
            return;
        }
        state.load_bench_done = true;

        let header = "=== Preset Load-Time Benchmark (Deck A @ 1920x1080 or --deck-size) ===";
        println!("\n{header}");
        self.logger.log_info(header);
        let col_hdr = format!(
            "{:<4} | {:<42} | {:>10} | {:>11} | {:>10} | {:>11}",
            "#", "Preset", "ColdLoadMs", "Cold1stFrMs", "WarmLoadMs", "Warm1stFrMs"
        );
        println!("{col_hdr}");
        self.logger.log_info(&col_hdr);

        let presets = state.presets.clone();
        let mut cold_loads = Vec::with_capacity(presets.len());
        let mut cold_firsts = Vec::with_capacity(presets.len());

        // Pass 1 (Cold load for each preset)
        for path in &presets {
            let t0 = Instant::now();
            let load_res = state.deck_a.load_preset(path, false);
            let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
            if let Err(e) = load_res {
                self.logger
                    .log_info(&format!("Bench cold load err for {}: {e}", preset_label(path)));
            }

            state.audio_a.fill_stereo(&mut state.pcm_a, true);
            let t1 = Instant::now();
            let _ = state
                .deck_a
                .render_to_texture(&state.pcm_a, state.frame_count, true, &mut self.logger);
            let first_ms = t1.elapsed().as_secs_f64() * 1000.0;

            cold_loads.push(load_ms);
            cold_firsts.push(first_ms);
        }

        // Pass 2 (Warm load for each preset — tests NVIDIA driver shader cache)
        for (i, path) in presets.iter().enumerate() {
            let t0 = Instant::now();
            let _ = state.deck_a.load_preset(path, false);
            let warm_load_ms = t0.elapsed().as_secs_f64() * 1000.0;

            state.audio_a.fill_stereo(&mut state.pcm_a, true);
            let t1 = Instant::now();
            let _ = state
                .deck_a
                .render_to_texture(&state.pcm_a, state.frame_count, true, &mut self.logger);
            let warm_first_ms = t1.elapsed().as_secs_f64() * 1000.0;

            let mut name = preset_label(path);
            if name.len() > 42 {
                name.truncate(39);
                name.push_str("...");
            }
            let row = format!(
                "{:<4} | {:<42} | {:>8.2}ms | {:>9.2}ms | {:>8.2}ms | {:>9.2}ms",
                i + 1,
                name,
                cold_loads[i],
                cold_firsts[i],
                warm_load_ms,
                warm_first_ms
            );
            println!("{row}");
            self.logger.log_info(&row);
        }

        println!("=== Benchmark complete. Closing cleanly. ===");
        self.logger.log_info("=== Preset Load-Time Benchmark complete ===");
    }

    /// Drives the 3-case `--bad-preset-test` sequence. Returns `true` when finished.
    fn step_bad_preset_test(&mut self) -> bool {
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        let Some(runner) = state.bad_runner.as_mut() else {
            return false;
        };

        runner.step_frames += 1;
        let elapsed = runner.step_start.elapsed();
        let target_secs = if runner.step == 0 { 3.0 } else { 4.0 };
        if elapsed.as_secs_f64() < target_secs {
            return false;
        }

        let step_fps = runner.step_frames as f64 / elapsed.as_secs_f64();
        let brightness_a = state
            .deck_a
            .sample_center_brightness(&mut self.logger)
            .unwrap_or(-1.0);
        let prev_step = runner.step;
        let msg = format!(
            "[Bad-Preset Test] Step {prev_step} window complete: avg_fps={step_fps:.1} (Deck B live), DeckA_center_luma={brightness_a:.1}/255"
        );
        println!("{msg}");
        self.logger.log_info(&msg);

        runner.step += 1;
        runner.step_start = Instant::now();
        runner.step_frames = 0;

        let next_step = runner.step;
        let (case_name, test_path) = match next_step {
            1 => ("Case 1/3: Missing file path", state.bad_missing_path.clone()),
            2 => ("Case 2/3: Empty 0-byte file", state.bad_empty_path.clone()),
            3 => (
                "Case 3/3: Broken shader_body",
                state.bad_shader_path.clone(),
            ),
            _ => return true,
        };

        // Reload valid preset 01 before each case so we know what was on Deck A beforehand.
        let valid_preset = state.presets[0].clone();
        let _ = state.deck_a.load_preset(&valid_preset, false);

        println!("\n[Bad-Preset Test] Triggering {case_name}: `{}`", test_path.display());
        match state.deck_a.load_preset(&test_path, false) {
            Ok(()) => {
                let line = format!(
                    "[Bad-Preset Test] {case_name} -> callback_fired=NO | error_text=<none> | no crash"
                );
                println!("{line}");
                self.logger.log_info(&line);
            }
            Err(err) => {
                let line = format!(
                    "[Bad-Preset Test] {case_name} -> callback_fired=YES | error_text=`{err}` | no crash"
                );
                println!("{line}");
                self.logger.log_info(&line);
            }
        }
        false
    }

    fn render_frame(&mut self, event_loop: &ActiveEventLoop) {
        // Handle one-shot preset load benchmark mode.
        if matches!(self.config.mode, RunMode::PresetLoadBench) {
            self.run_preset_load_benchmark();
            if self.state.as_ref().map(|s| s.load_bench_done).unwrap_or(false) {
                self.shutdown();
                event_loop.exit();
                return;
            }
        }

        // Handle 3-case bad-preset test progression.
        if self.step_bad_preset_test() {
            self.shutdown();
            event_loop.exit();
            return;
        }

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

        let measure_gpu = state.config.measure_gpu;

        // If `--measure-gpu` is active, start the total work timer AFTER the previous
        // frame's VSync wait has already finished (at the end of swap_buffers below).
        let work_start = Instant::now();

        // 1. Render Deck A inside Deck A's shared OpenGL context.
        state
            .audio_a
            .fill_stereo(&mut state.pcm_a, state.deck_a_audio);
        let timing_a_frame = state
            .deck_a
            .render_to_texture(&state.pcm_a, frame_idx, measure_gpu, &mut self.logger)
            .expect("Deck A render failed");
        if frame_idx > 5 {
            state.timing_a_cpu.record_ms(timing_a_frame.cpu_submit_ms);
            state.timing_a_gpu.record_ms(timing_a_frame.gpu_wait_ms);
            state.timing_a.record_ms(timing_a_frame.total_ms());
        }

        // 2. Render Deck B inside Deck B's shared OpenGL context.
        state
            .audio_b
            .fill_stereo(&mut state.pcm_b, state.deck_b_audio);
        let timing_b_frame = state
            .deck_b
            .render_to_texture(&state.pcm_b, frame_idx, measure_gpu, &mut self.logger)
            .expect("Deck B render failed");
        if frame_idx > 5 {
            state.timing_b_cpu.record_ms(timing_b_frame.cpu_submit_ms);
            state.timing_b_gpu.record_ms(timing_b_frame.gpu_wait_ms);
            state.timing_b.record_ms(timing_b_frame.total_ms());
        }

        // 3. Composite side-by-side on the main window context.
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
            // Measure pure GPU+CPU work before VSync swap.
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

        // In `--measure-gpu` ONLY: drain VSync right here *outside* Deck A's timer so the
        // next frame's Deck A timer measures pure Deck A work instead of idle VSync wait.
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

        // 4. Update Heavy-Test tracker if running `--heavy-test-seconds`.
        let mut should_exit = false;
        if let (RunMode::HeavyTest { steady_secs }, Some(heavy)) =
            (&state.config.mode, state.heavy_runner.as_mut())
        {
            if !heavy.warmup_done {
                heavy.warmup_frames += 1;
                if frame_delta_ms > heavy.warmup_worst_ms {
                    heavy.warmup_worst_ms = frame_delta_ms;
                }
                if heavy.warmup_start.elapsed() >= Duration::from_secs(5) {
                    let secs = heavy.warmup_start.elapsed().as_secs_f64();
                    let w_fps = heavy.warmup_frames as f64 / secs;
                    let msg = format!(
                        "[Heavy Test] Warmup (first {:.2}s) done: {} frames, avg {:.1} FPS, worst frame delta {:.2} ms. Starting {}s steady-state window...",
                        secs, heavy.warmup_frames, w_fps, heavy.warmup_worst_ms, steady_secs
                    );
                    println!("{msg}");
                    self.logger.log_info(&msg);
                    heavy.warmup_done = true;
                    heavy.steady_start = Some(Instant::now());
                    heavy.last_bucket_time = Instant::now();
                    heavy.last_bucket_frames = 0;
                }
            } else if let Some(steady_start) = heavy.steady_start {
                heavy.steady_frames += 1;
                if frame_delta_ms > FRAME_BELOW_55_FPS_MS {
                    heavy.steady_below_55 += 1;
                }
                heavy.steady_a_cpu.record_ms(timing_a_frame.cpu_submit_ms);
                heavy.steady_a_gpu.record_ms(timing_a_frame.gpu_wait_ms);
                heavy.steady_a_total.record_ms(timing_a_frame.total_ms());
                heavy.steady_b_cpu.record_ms(timing_b_frame.cpu_submit_ms);
                heavy.steady_b_gpu.record_ms(timing_b_frame.gpu_wait_ms);
                heavy.steady_b_total.record_ms(timing_b_frame.total_ms());
                heavy.steady_timing_total.record_ms(total_recorded_ms);

                let b_secs = heavy.last_bucket_time.elapsed().as_secs_f64();
                if b_secs >= 1.0 {
                    let df = heavy.steady_frames - heavy.last_bucket_frames;
                    let fps = df as f64 / b_secs;
                    heavy.steady_fps_buckets.push(fps);
                    heavy.last_bucket_time = Instant::now();
                    heavy.last_bucket_frames = heavy.steady_frames;
                }

                if steady_start.elapsed() >= Duration::from_secs(*steady_secs) {
                    should_exit = true;
                }
            }
        }

        // 5. Update Active-Soak tracker if running `--soak-minutes`.
        if let Some(soak) = state.soak_runner.as_mut() {
            if frame_idx > 5 && frame_delta_ms > FRAME_BELOW_55_FPS_MS {
                soak.frames_below_55 += 1;
            }

            // 1-second FPS bucket update.
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

            // Rotate preset every 10 seconds, alternating Deck A and Deck B.
            // Every 6th rotation injects the bad shader preset to prove continuous resilience.
            if soak.last_switch_time.elapsed() >= Duration::from_secs(10) {
                soak.last_switch_time = Instant::now();
                soak.switch_count += 1;
                let use_deck_a = soak.switch_count % 2 == 1;
                let deck_label = if use_deck_a { "DeckA" } else { "DeckB" };

                let target_path = if soak.switch_count % 6 == 0 {
                    state.bad_shader_path.clone()
                } else {
                    let idx = soak.next_preset_cursor % state.presets.len();
                    soak.next_preset_cursor += 1;
                    state.presets[idx].clone()
                };

                let label = preset_label(&target_path);
                let res = if use_deck_a {
                    state.deck_a.load_preset(&target_path, false)
                } else {
                    state.deck_b.load_preset(&target_path, false)
                };
                match res {
                    Ok(()) => {
                        self.logger.log_info(&format!(
                            "[Soak switch #{}] {} -> `{}` (ok)",
                            soak.switch_count, deck_label, label
                        ));
                    }
                    Err(e) => {
                        self.logger.log_info(&format!(
                            "[Soak switch #{}] {} -> `{}` (caught error, no crash: {})",
                            soak.switch_count, deck_label, label, e
                        ));
                    }
                }
            }

            // Every 30 seconds: log RAM, FPS min/avg/max, and nvidia-smi GPU telemetry.
            if soak.last_telemetry_time.elapsed() >= Duration::from_secs(30) {
                soak.last_telemetry_time = Instant::now();
                let elapsed_s = state.start_time.elapsed().as_secs();
                let mem = current_process_memory_mb();
                let fps_str = match soak.fps_window_30s.summary_and_clear() {
                    Some((mn, av, mx)) => format!("fps(min/avg/max)={mn:.1}/{av:.1}/{mx:.1}"),
                    None => "fps=n/a".to_string(),
                };
                let gpu_str = match soak.gpu_monitor.latest() {
                    Some(g) => {
                        update_gpu_extremes(soak, g);
                        format!(
                            "GPU(vram={}/{}MiB, temp={}C, clk={}MHz)",
                            g.vram_used_mib, g.vram_total_mib, g.temp_c, g.graphics_clock_mhz
                        )
                    }
                    None => "GPU=n/a".to_string(),
                };
                let line = format!(
                    "[Soak {:>3}s] {} | {} | {} | below55={} | gl_err={}",
                    elapsed_s,
                    fps_str,
                    format_mem_short(mem),
                    gpu_str,
                    soak.frames_below_55,
                    self.logger.total_errors()
                );
                println!("{line}");
                self.logger.log_info(&line);
            }

            if state.start_time.elapsed() >= soak.target_duration {
                should_exit = true;
            }
        }

        // 6. Standard periodic console heartbeat for Interactive / Heavy modes.
        if state.soak_runner.is_none() && state.bad_runner.is_none() {
            let since_report = state.last_report_time.elapsed().as_secs_f64();
            if since_report >= 5.0 {
                let df = state.frame_count - state.last_report_frames;
                let fps = df as f64 / since_report;
                state.deck_a.set_fps(fps.round() as i32);
                state.deck_b.set_fps(fps.round() as i32);
                let line = format!(
                    "[Stage 3 | {:.0}s] fps={:.1} | A={:.2}ms(max {:.2}) | B={:.2}ms(max {:.2}) | total={:.2}ms(max {:.2}) | {} | below55={} | gl_err={}",
                    state.start_time.elapsed().as_secs_f64(),
                    fps,
                    state.timing_a.avg_ms(),
                    state.timing_a.max_ms(),
                    state.timing_b.avg_ms(),
                    state.timing_b.max_ms(),
                    state.timing_total.avg_ms(),
                    state.timing_total.max_ms(),
                    format_mem_short(current_process_memory_mb()),
                    state.frames_below_55,
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

            let mut summary = format!(
                "=== Stage 3 Summary ({:?}) ===\n\
                 Duration:         {:.2}s ({} frames, overall avg {:.2} FPS)\n\
                 Deck resolution:  {}x{} per deck (x2) | measure_gpu={}\n\
                 Deck A render:    avg {:.2} ms (cpu_submit {:.2} ms + gpu_wait {:.2} ms) | worst {:.2} ms ({} frames)\n\
                 Deck B render:    avg {:.2} ms (cpu_submit {:.2} ms + gpu_wait {:.2} ms) | worst {:.2} ms ({} frames)\n\
                 Total work/frame: avg {:.2} ms | worst {:.2} ms\n\
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
                state.config.measure_gpu,
                state.timing_a.avg_ms(),
                state.timing_a_cpu.avg_ms(),
                state.timing_a_gpu.avg_ms(),
                state.timing_a.max_ms(),
                state.timing_a.samples(),
                state.timing_b.avg_ms(),
                state.timing_b_cpu.avg_ms(),
                state.timing_b_gpu.avg_ms(),
                state.timing_b.max_ms(),
                state.timing_b.samples(),
                state.timing_total.avg_ms(),
                state.timing_total.max_ms(),
                state.frames_below_55,
                format_mem(state.start_memory),
                format_mem(end_mem),
                self.logger.total_errors()
            );

            if let Some(mut heavy) = state.heavy_runner.take() {
                let steady_secs = heavy
                    .steady_start
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                let steady_avg_fps = if steady_secs > 0.0 {
                    heavy.steady_frames as f64 / steady_secs
                } else {
                    0.0
                };
                let bucket_line = match heavy.steady_fps_buckets.summary_and_clear() {
                    Some((mn, av, mx)) => format!("min={mn:.1} / avg={av:.1} / max={mx:.1}"),
                    None => "n/a".to_string(),
                };
                summary.push_str(&format!(
                    "\n--- Heavy-Preset Steady State (AFTER 5s warmup) ---\n\
                     Steady duration:  {:.2}s ({} frames, avg {:.2} FPS | 1s buckets: {})\n\
                     Steady Deck A:    total avg {:.2} ms (max {:.2}) | cpu_submit avg {:.2} ms (max {:.2}) | gpu_wait avg {:.2} ms (max {:.2})\n\
                     Steady Deck B:    total avg {:.2} ms (max {:.2}) | cpu_submit avg {:.2} ms (max {:.2}) | gpu_wait avg {:.2} ms (max {:.2})\n\
                     Steady Total:     avg {:.2} ms | worst {:.2} ms\n\
                     Steady < 55 FPS:  {} frames",
                    steady_secs,
                    heavy.steady_frames,
                    steady_avg_fps,
                    bucket_line,
                    heavy.steady_a_total.avg_ms(),
                    heavy.steady_a_total.max_ms(),
                    heavy.steady_a_cpu.avg_ms(),
                    heavy.steady_a_cpu.max_ms(),
                    heavy.steady_a_gpu.avg_ms(),
                    heavy.steady_a_gpu.max_ms(),
                    heavy.steady_b_total.avg_ms(),
                    heavy.steady_b_total.max_ms(),
                    heavy.steady_b_cpu.avg_ms(),
                    heavy.steady_b_cpu.max_ms(),
                    heavy.steady_b_gpu.avg_ms(),
                    heavy.steady_b_gpu.max_ms(),
                    heavy.steady_timing_total.avg_ms(),
                    heavy.steady_timing_total.max_ms(),
                    heavy.steady_below_55
                ));
            }

            if let Some(mut soak) = state.soak_runner.take() {
                if let Some(g) = soak.gpu_monitor.latest() {
                    update_gpu_extremes(&mut soak, g);
                }
                let fps_all = match soak.fps_all_1s.summary_and_clear() {
                    Some((mn, av, mx)) => format!("min={mn:.1} / avg={av:.1} / max={mx:.1}"),
                    None => "n/a".to_string(),
                };
                summary.push_str(&format!(
                    "\n--- Active Soak Totals ---\n\
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

            state.deck_a.destroy();
            state.deck_b.destroy();
            if state.main_context.make_current(&state.main_surface).is_ok() {
                state.compositor.destroy(&state.main_gl);
            }
            drop(state);
        }
    }

    fn interactive_key(&mut self, key: Key) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if !matches!(state.config.mode, RunMode::Interactive) {
            return;
        }
        match key {
            Key::Character(ref ch) if ch == "1" => {
                state.deck_a_audio = true;
                state.deck_b_audio = false;
                println!("Test 1 (Independence): Deck A = AUDIO, Deck B = SILENCE");
            }
            Key::Character(ref ch) if ch == "2" => {
                state.deck_a_audio = true;
                state.deck_b_audio = true;
                println!("Both decks = AUDIO");
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
                let _ = state
                    .deck_a
                    .load_preset(&state.presets[state.deck_a_preset_idx], true);
            }
            Key::Named(NamedKey::ArrowLeft) => {
                state.deck_a_preset_idx =
                    (state.deck_a_preset_idx + state.presets.len() - 1) % state.presets.len();
                let _ = state
                    .deck_a
                    .load_preset(&state.presets[state.deck_a_preset_idx], true);
            }
            Key::Named(NamedKey::ArrowUp) => {
                state.deck_b_preset_idx = (state.deck_b_preset_idx + 1) % state.presets.len();
                let _ = state
                    .deck_b
                    .load_preset(&state.presets[state.deck_b_preset_idx], true);
            }
            Key::Named(NamedKey::ArrowDown) => {
                state.deck_b_preset_idx =
                    (state.deck_b_preset_idx + state.presets.len() - 1) % state.presets.len();
                let _ = state
                    .deck_b
                    .load_preset(&state.presets[state.deck_b_preset_idx], true);
            }
            _ => {}
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
            } => {
                if matches!(logical_key, Key::Named(NamedKey::Escape)) {
                    self.shutdown();
                    event_loop.exit();
                } else {
                    self.interactive_key(logical_key);
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
        Some(m) => format!("WS={:.1}MiB/Priv={:.1}MiB", m.working_set_mb, m.private_commit_mb),
        None => "mem=n/a".to_string(),
    }
}

fn find_preset_by_prefix(presets: &[PathBuf], prefix: &str) -> Option<usize> {
    presets.iter().position(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with(prefix))
            .unwrap_or(false)
    })
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
    let mut deck_width = 1920_u32;
    let mut deck_height = 1080_u32;
    let mut measure_gpu = false;
    let mut deck_a_preset_override: Option<PathBuf> = None;
    let mut deck_b_preset_override: Option<PathBuf> = None;
    let mut custom_log: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--measure-gpu" => measure_gpu = true,
            "--bad-preset-test" => mode = RunMode::BadPresetTest,
            "--preset-load-bench" => mode = RunMode::PresetLoadBench,
            "--deck-a-preset" => {
                i += 1;
                let val = args.get(i).expect("--deck-a-preset requires a file path");
                deck_a_preset_override = Some(PathBuf::from(val));
            }
            "--deck-b-preset" => {
                i += 1;
                let val = args.get(i).expect("--deck-b-preset requires a file path");
                deck_b_preset_override = Some(PathBuf::from(val));
            }
            "--deck-size" => {
                i += 1;
                let val = args.get(i).expect("--deck-size requires WxH (e.g. 1280x720)");
                let (w, h) = parse_deck_size(val).expect("Invalid --deck-size");
                deck_width = w;
                deck_height = h;
            }
            "--heavy-test-seconds" => {
                i += 1;
                let val = args
                    .get(i)
                    .expect("--heavy-test-seconds requires a positive integer");
                let secs: u64 = val
                    .parse()
                    .expect("--heavy-test-seconds must be a positive integer");
                mode = RunMode::HeavyTest { steady_secs: secs };
            }
            "--soak-minutes" => {
                i += 1;
                let val = args
                    .get(i)
                    .expect("--soak-minutes requires a positive number");
                let mins: f64 = val
                    .parse()
                    .expect("--soak-minutes must be a positive number");
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

    let default_log = match &mode {
        RunMode::Interactive => PathBuf::from("logs/stage3_spike.log"),
        RunMode::BadPresetTest => PathBuf::from("logs/test_bad_presets.log"),
        RunMode::PresetLoadBench => PathBuf::from("logs/test_preset_loads.log"),
        RunMode::HeavyTest { .. } => {
            PathBuf::from(format!("logs/test_heavy_{deck_width}x{deck_height}.log"))
        }
        RunMode::ActiveSoak { .. } => {
            PathBuf::from(format!("logs/test_soak_{deck_width}x{deck_height}.log"))
        }
    };

    CliConfig {
        mode,
        deck_width,
        deck_height,
        measure_gpu,
        deck_a_preset_override,
        deck_b_preset_override,
        log_path: custom_log.unwrap_or(default_log),
    }
}

fn main() {
    let pm_version = projectm_version_string();
    let config = parse_cli();

    println!("libprojectM runtime version: {pm_version}");
    let presets = discover_valid_presets(Path::new("presets"))
        .expect("Failed to load presets from C:\\dev\\vj\\presets\\");
    println!(
        "Loaded {} valid presets | log file: {}",
        presets.len(),
        config.log_path.display()
    );

    let logger = GlErrorLogger::open(&config.log_path)
        .expect("Could not initialize Stage 3 log file");

    let event_loop = EventLoop::new().expect("Failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = Stage3App::new(logger, presets, config);
    event_loop
        .run_app(&mut app)
        .expect("Event loop exited with an error");
}
