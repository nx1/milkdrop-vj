//! Milestone 2 — Stage `2A-1`: Dark-Venue `egui` Console, Control Bus (`mappings.json`),
//! OpenGL 5.3 Cross-Context Sync, Async Blank-Deck Warning, and Preset-Load Stall Logging.
//!
//! Architecture:
//! - **Control Bus (`src/control/mod.rs`):** Normalized `0.0..=1.0` parameter store and trigger
//!   queue backed by `mappings.json`, built first so `egui` widgets (`2A-1`), GPU mixer (`2A-2`),
//!   remappable keyboard shortcuts (`2A-3`), and future MIDI (Milestone 4) share one bus.
//! - **Background thread (`wasapi-loopback-capture`):** Streams native Windows WASAPI loopback
//!   audio into a lock-free SPSC ring buffer (or falls back to Synthetic Test Audio via CLI).
//! - **Main thread (single-threaded OpenGL):**
//!   1. Renders Deck A and Deck B in their shared OpenGL 3.3 Core pbuffer contexts (`1280x720`
//!      default per Light-Load Rules).
//!   2. Each deck inserts a `glFenceSync` + `glFlush()` after `glCopyTexSubImage2D` and runs an
//!      asynchronous `32x18` PBO blank-deck luma sample twice per second (zero stall).
//!   3. Main window context executes GPU-only `glWaitSync` + `glDeleteSync` for both decks
//!      (OpenGL 3.3 Core Spec, Chapter 5 / Section 5.3) before `egui_glow` draws Deck A and Deck B.
//!
//! CLI flags:
//! - `--audio-source live|test`   Select Live WASAPI Loopback (default) or Synthetic Test Audio
//! - `--audio-device <filter>`    Select output device by 1-based index or name substring
//! - `--deck-size WxH`            Internal render resolution per deck (default `1280x720`)
//! - `--measure-gpu`              Diagnostic `glFinish()` split timing (OFF in normal runs)
//! - `--log <path>`               Custom log file path (default `logs/stage_2a1.log`)

mod egui_renderer;
mod gl_log;
mod gl_state;
mod metrics;
mod mixer;
mod projector;
mod render_deck;
mod ui;

use std::collections::VecDeque;
use std::ffi::CStr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glow::HasContext;
use glutin::config::{Config, ConfigSurfaceTypes, ConfigTemplateBuilder};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version,
};
use glutin::display::{Display, GetGlDisplay};
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use milkdrop_vj::audio::beat_pulse::BeatPulseEngine;
use milkdrop_vj::audio::capture_thread::{
    AsyncAudioSwitch, AsyncDeviceListRefresh, AudioSwitchOutcome, LiveAudioCapture,
};
use milkdrop_vj::audio::visual_eq::{VisualEqParams, VisualEqProcessor};
use milkdrop_vj::audio::wasapi_loopback::WasapiLoopbackSource;
use milkdrop_vj::audio::{AudioDeviceInfo, AudioSource};
use milkdrop_vj::control::{
    normalize_modifiers, param, ControlBus, MappingMode, MappingsConfig, BLEND_MODE_COUNT,
};
use milkdrop_vj::deck::{
    init_gl_loader_for_projectm, max_pcm_samples_per_channel, projectm_version_string,
};
use milkdrop_vj::hot_banks::HotBankStore;
use milkdrop_vj::playlists::{CollectionFilter, PlaylistStore};
use milkdrop_vj::preset_browser::{
    discover_immediate_starter_presets, filter_preset_indices, PresetEntry, PresetScannerHandle,
    ScanStatus, ScanUpdate, WeightFilter,
};
use milkdrop_vj::preset_meta::PresetComplexity;
use milkdrop_vj::test_audio::TestAudioGenerator;

use crate::egui_renderer::EguiGlRenderer;
use crate::gl_log::GlErrorLogger;
use crate::metrics::{current_process_memory_mb, MemorySnapshotMb, TimingStats};
use crate::mixer::{
    compute_strobe_flash, step_blackout_alpha, BlendMode, DeckAirState, FxRoutingState,
    FxUnitRoute, GpuMixer, MasterFxParams,
};
use crate::projector::{
    enumerate_monitors, DisplayMonitorInfo, ProjectorTarget, ProjectorWindow,
};
use crate::render_deck::RenderDeck;
use crate::ui::{
    apply_dark_venue_style, draw_console_ui, AudioBadgeState, ConsoleUiState, DeckUiSnapshot,
    HotBankAction, LastLoadSummary, LoadGuardState, PlaylistAction, TopBarAction,
    UiInteractionState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioMode {
    LiveWasapi,
    SyntheticTest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectorCommand {
    Open(ProjectorTarget),
    Close,
    RefreshMonitors,
}

#[derive(Clone, Debug)]
struct CliConfig {
    audio_mode: AudioMode,
    audio_device_filter: Option<String>,
    deck_width: u32,
    deck_height: u32,
    measure_gpu: bool,
    auto_exit_secs: Option<f64>,
    test_res_toggles: u32,
    test_projector_windowed: bool,
    log_path: PathBuf,
}

/// Tracks the preset-load frame (`+0`) and the next two frames (`+1`, `+2`) for Risk 8 logging.
#[derive(Clone, Debug)]
struct PendingStallTrace {
    deck_label: String,
    preset_name: String,
    load_call_ms: f64,
    smooth: bool,
    step: u8,
}

struct ConsoleState {
    config: CliConfig,
    // Drop order matters: destroy `egui_renderer`, `deck_a`, `deck_b` explicitly in `shutdown()`
    // while their respective OpenGL contexts are current.
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: EguiGlRenderer,
    mixer: GpuMixer,
    master_tex_id: egui::TextureId,
    deck_a_tex_id: egui::TextureId,
    deck_b_tex_id: egui::TextureId,
    deck_a: RenderDeck,
    deck_b: RenderDeck,
    projector: Option<ProjectorWindow>,
    monitors: Vec<DisplayMonitorInfo>,
    pending_projector_cmd: Option<ProjectorCommand>,
    master_fence: Option<glow::NativeFence>,
    retired_master_fences: VecDeque<glow::NativeFence>,
    test_projector_opened: bool,
    gl_display: Display,
    pbuffer_config: Config,
    window: Window,
    main_surface: Surface<WindowSurface>,
    main_context: PossiblyCurrentContext,
    main_gl: Arc<glow::Context>,

    // Unified Control Bus + mappings.json + 2-click Load Safety guard + keyboard/focus state
    bus: ControlBus,
    load_guard: LoadGuardState,
    interact: UiInteractionState,
    modifiers: ModifiersState,
    held_momentary_targets: Vec<String>,
    blackout_alpha: f32,
    mappings_status: String,

    // GPU info & automated resolution-toggle test counter
    gpu_label: String,
    gpu_is_intel: bool,
    res_toggles_done: u32,
    last_res_toggle_test_at: Instant,

    // Audio pipeline (non-blocking async device switcher & enumerator for Stage 2A-5)
    audio_mode: AudioMode,
    live_capture: Option<LiveAudioCapture>,
    selected_audio_device_id: Option<String>,
    audio_devices: Vec<AudioDeviceInfo>,
    audio_switch_handle: Option<AsyncAudioSwitch>,
    audio_switch_seq: u64,
    audio_device_refresh: Option<AsyncDeviceListRefresh>,
    last_device_refresh_at: Instant,
    audio_device_name: String,
    audio_sample_rate: u32,
    last_wasapi_packets: u64,
    last_packet_time: Instant,
    audio_peak_db: f32,
    audio_age_stats: TimingStats,
    synth_gen: TestAudioGenerator,
    live_pcm_buf: Vec<f32>,
    synth_pcm_buf: Vec<f32>,
    silence_pcm_buf: Vec<f32>,
    eq_deck_a: VisualEqProcessor,
    eq_deck_b: VisualEqProcessor,
    eq_buf_a: Vec<f32>,
    eq_buf_b: Vec<f32>,
    beat_pulse: BeatPulseEngine,
    device_lost_logged: bool,

    // Preset Library (background scanner + disk cache + favorites/playlists + live filtered view)
    presets_root: PathBuf,
    preset_cache_path: PathBuf,
    playlists_path: PathBuf,
    playlists: PlaylistStore,
    playlists_revision: usize,
    hot_banks_path: PathBuf,
    hot_banks: HotBankStore,
    presets: Vec<PresetEntry>,
    folders: Vec<String>,
    filtered_indices: Vec<usize>,
    last_filter_sig: (String, WeightFilter, Option<String>, CollectionFilter, usize, usize),
    library_revision: usize,
    scanner: Option<PresetScannerHandle>,
    scan_status: ScanStatus,
    bad_shader_path: PathBuf,
    deck_a_current: PresetEntry,
    deck_b_current: PresetEntry,
    deck_a_override_name: Option<String>,
    last_load: Option<LastLoadSummary>,
    pending_stall_traces: Vec<PendingStallTrace>,

    // Timing, FPS, and telemetry
    start_time: Instant,
    last_frame_instant: Instant,
    last_fps_sample_time: Instant,
    last_fps_sample_frames: u64,
    global_fps: f64,
    deck_a_ms_ema: f64,
    deck_b_ms_ema: f64,
    frame_count: u64,
    timing_a_cpu: TimingStats,
    timing_b_cpu: TimingStats,
    timing_a_gpu_q: TimingStats,
    timing_b_gpu_q: TimingStats,
    timing_frame_dt: TimingStats,
    blank_check_stats: TimingStats,
    start_memory: Option<MemorySnapshotMb>,
    last_periodic_log: Instant,
    logger: GlErrorLogger,
    shut_down: bool,
}

impl ConsoleState {
    fn initialize(event_loop: &ActiveEventLoop, config: CliConfig) -> Result<Self, String> {
        let mut logger = GlErrorLogger::open(&config.log_path)?;
        logger.log_info(
            "Milestone 3 Stage 3D: Hot-Preset Banks (1-4 Deck A, 5-8 Deck B, Load Safety & hot_banks.json)",
        );
        logger.log_info(&format!(
            "Deck render resolution: {}x{} (default 1280x720 per Light-Load Rules) | measure_gpu={}",
            config.deck_width, config.deck_height, config.measure_gpu
        ));
        logger.log_info(&format!("libprojectM runtime version: {}", projectm_version_string()));

        // 1. Control Bus FIRST (`mappings.json` loaded or created before any UI widgets exist).
        let mappings_path = Path::new("mappings.json");
        let (mappings_cfg, created_new) = MappingsConfig::load_or_create_default(mappings_path)?;
        let mappings_status = if created_new {
            format!(
                "Bus: mappings.json created ({} keys)",
                mappings_cfg.keyboard.len()
            )
        } else {
            format!(
                "Bus: mappings.json loaded ({} keys)",
                mappings_cfg.keyboard.len()
            )
        };
        logger.log_info(&mappings_status);
        let bus = ControlBus::new(mappings_cfg);

        // 1b. Load Favorites & Curated Playlists (`playlists.json`, gitignored).
        let playlists_path = PathBuf::from("playlists.json");
        let (playlists, playlists_existed) = PlaylistStore::load_or_empty(&playlists_path)?;
        logger.log_info(&format!(
            "Playlists ({}): {} favorites, {} curated lists",
            if playlists_existed {
                "loaded playlists.json"
            } else {
                "new empty store"
            },
            playlists.favorite_count(),
            playlists.playlists().len()
        ));

        // 1c. Load Hot-Preset Banks (`hot_banks.json`, gitignored).
        let hot_banks_path = PathBuf::from("hot_banks.json");
        let (hot_banks, hot_banks_existed) = HotBankStore::load_or_default(&hot_banks_path)?;
        logger.log_info(&format!(
            "Hot-Preset Banks ({}): {} slot(s) assigned",
            if hot_banks_existed {
                "loaded hot_banks.json"
            } else {
                "new empty banks"
            },
            hot_banks.assigned_count()
        ));

        // 2. Discover immediate top-level starter presets for instant boot, then start the
        //    non-blocking background scanner for the full `presets\` tree + `preset_cache.json`.
        let presets_root = PathBuf::from("presets");
        let preset_cache_path = PathBuf::from("preset_cache.json");
        let presets = discover_immediate_starter_presets(&presets_root)?;
        let folders = vec!["Starter".to_string()];
        let filtered_indices: Vec<usize> = (0..presets.len()).collect();
        let bad_shader_path = presets_root.join("_bad_shader_test.milk");
        logger.log_info(&format!(
            "Bootstrapped {} starter presets immediately; spawning background preset-scanner thread...",
            presets.len()
        ));
        let scanner = Some(PresetScannerHandle::spawn(
            presets_root.clone(),
            preset_cache_path.clone(),
        ));
        let scan_status = ScanStatus {
            scanning: true,
            scanned: 0,
            total: 0,
            cache_hits: 0,
            parsed: 0,
            elapsed_ms: 0.0,
            last_error: None,
        };

        // 3. Create the main winit window with minimum 1280x720 pt logical size (Section 13).
        let monitors: Vec<DisplayMonitorInfo> = enumerate_monitors(event_loop)
            .into_iter()
            .map(|(info, _)| info)
            .collect();
        logger.log_info(&format!("Detected {} Windows display(s):", monitors.len()));
        for m in &monitors {
            logger.log_info(&format!(
                "  - {}: `{}` pos=({}, {}) size={}x{} scale={:.2}",
                m.short_label(),
                m.name,
                m.pos_x,
                m.pos_y,
                m.width,
                m.height,
                m.scale_factor
            ));
        }

        let window_attrs = Window::default_attributes()
            .with_title(format!(
                "MilkDrop VJ — Stage 3D Console, FX Racks & Hot Banks ({}x{} decks)",
                config.deck_width, config.deck_height
            ))
            .with_inner_size(LogicalSize::new(1440.0, 860.0))
            .with_min_inner_size(LogicalSize::new(1280.0, 720.0));

        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_depth_size(24);

        let (window_opt, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(window_attrs))
            .build(event_loop, template, |configs| {
                configs
                    .reduce(|best, candidate| {
                        if candidate.num_samples() < best.num_samples() {
                            candidate
                        } else {
                            best
                        }
                    })
                    .expect("No suitable OpenGL pixel format found")
            })
            .map_err(|e| format!("Failed to create main OpenGL window: {e}"))?;

        let window = window_opt.ok_or_else(|| "DisplayBuilder returned no window".to_string())?;
        let raw_window_handle = window
            .window_handle()
            .map_err(|e| format!("Failed to get raw window handle: {e}"))?
            .as_raw();
        let gl_display = gl_config.display();

        // 4. Create the main OpenGL 3.3 Core context and window surface.
        let ctx_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(Some(raw_window_handle));
        let not_current = unsafe {
            gl_display
                .create_context(&gl_config, &ctx_attrs)
                .map_err(|e| format!("Failed to create main OpenGL 3.3 Core context: {e}"))?
        };
        let surface_attrs = window
            .build_surface_attributes(Default::default())
            .map_err(|e| format!("Failed to build main window surface attributes: {e}"))?;
        let main_surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .map_err(|e| format!("Failed to create main window surface: {e}"))?
        };
        let main_context = not_current
            .make_current(&main_surface)
            .map_err(|e| format!("Failed to make main context current: {e}"))?;

        // Single-window mode: VSync `Wait(1)` on the main console window.
        let _ = main_surface.set_swap_interval(
            &main_context,
            SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
        );

        let main_gl = Arc::new(unsafe {
            glow::Context::from_loader_function_cstr(|symbol: &CStr| {
                gl_display.get_proc_address(symbol) as *const _
            })
        });

        let vendor = unsafe { main_gl.get_parameter_string(glow::VENDOR) };
        let renderer = unsafe { main_gl.get_parameter_string(glow::RENDERER) };
        let version = unsafe { main_gl.get_parameter_string(glow::VERSION) };
        let gpu_is_intel = renderer.to_lowercase().contains("intel")
            || vendor.to_lowercase().contains("intel");
        let gpu_label = shorten_gpu_name(&renderer);
        logger.log_info(&format!("GL_VENDOR:   {vendor}"));
        logger.log_info(&format!("GL_RENDERER: {renderer}"));
        logger.log_info(&format!("GL_VERSION:  {version}"));
        logger.log_info(&format!(
            "Window scale_factor={:.2} | initial inner_size={}x{} px",
            window.scale_factor(),
            window.inner_size().width,
            window.inner_size().height
        ));

        // Required on Windows before any `Deck::new()` so `projectM-4.dll`'s GLEW pointers load.
        init_gl_loader_for_projectm()?;
        logger.check_frame(&main_gl, 0, "main-init");

        // 5. Find a WGL pbuffer config compatible with the native window DC (Stage 3 fix).
        let pbuffer_template = ConfigTemplateBuilder::new()
            .with_surface_type(ConfigSurfaceTypes::PBUFFER)
            .compatible_with_native_window(raw_window_handle)
            .with_alpha_size(8)
            .with_depth_size(24)
            .build();
        let pbuffer_config = unsafe {
            gl_display
                .find_configs(pbuffer_template)
                .map_err(|e| format!("Failed to query pbuffer configs: {e}"))?
                .next()
                .ok_or_else(|| "No WGL pbuffer OpenGL config found".to_string())?
        };

        // 6. Create Deck A and Deck B in shared OpenGL 3.3 Core contexts.
        let mut deck_a = RenderDeck::new(
            "Deck A",
            &gl_display,
            &pbuffer_config,
            &main_context,
            config.deck_width,
            config.deck_height,
            &mut logger,
        )?;
        let mut deck_b = RenderDeck::new(
            "Deck B",
            &gl_display,
            &pbuffer_config,
            &main_context,
            config.deck_width,
            config.deck_height,
            &mut logger,
        )?;

        let deck_a_current = presets[0].clone();
        let deck_b_current = presets[1usize.min(presets.len().saturating_sub(1))].clone();
        deck_a.load_preset(&deck_a_current.path, false)?;
        deck_b.load_preset(&deck_b_current.path, false)?;

        // 7. Re-bind main window context and initialize `egui 0.37` + zero-reallocation GL renderer.
        main_context
            .make_current(&main_surface)
            .map_err(|e| format!("Failed to re-bind main context after deck init: {e}"))?;

        let mut egui_renderer = EguiGlRenderer::new(Arc::clone(&main_gl))?;
        let mixer = GpuMixer::new(Arc::clone(&main_gl), config.deck_width, config.deck_height)?;
        let egui_ctx = egui::Context::default();
        apply_dark_venue_style(&egui_ctx);
        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            event_loop.system_theme(),
            Some(egui_renderer.max_texture_side()),
        );

        let master_tex_id = egui_renderer.register_native_texture(mixer.master_texture());
        // Register post-FX deck textures so Deck A and Deck B previews show cue FX live!
        let deck_a_tex_id = egui_renderer.register_native_texture(mixer.deck_a_texture());
        let deck_b_tex_id = egui_renderer.register_native_texture(mixer.deck_b_texture());
        logger.check_frame(&main_gl, 0, "egui-and-mixer-init");

        // 8. Start Live WASAPI Loopback capture (or Synthetic Test audio if requested),
        //    and spawn a background Windows output device list refresh for the Top Bar dropdown.
        let mut selected_audio_device_id: Option<String> = None;
        let (audio_mode, live_capture, audio_device_name, audio_sample_rate) =
            match config.audio_mode {
                AudioMode::LiveWasapi => {
                    let chosen_id = resolve_device_filter(config.audio_device_filter.as_deref())?;
                    selected_audio_device_id = chosen_id.clone();
                    match LiveAudioCapture::start(chosen_id) {
                        Ok(cap) => {
                            let fmt = cap.format().clone();
                            logger.log_info(&format!(
                                "WASAPI Loopback active: `{}` ({} Hz, {} ch native -> {} Hz stereo f32)",
                                fmt.device_name,
                                fmt.native_sample_rate,
                                fmt.native_channels,
                                fmt.output_sample_rate
                            ));
                            (AudioMode::LiveWasapi, Some(cap), fmt.device_name, fmt.output_sample_rate)
                        }
                        Err(err) => {
                            logger.log_info(&format!(
                                "WASAPI Loopback unavailable ({err}); falling back to Synthetic Test Audio"
                            ));
                            (
                                AudioMode::SyntheticTest,
                                None,
                                "Synthetic Test Signal".to_string(),
                                48_000,
                            )
                        }
                    }
                }
                AudioMode::SyntheticTest => {
                    logger.log_info("Audio mode: Synthetic Test Audio (--audio-source test)");
                    (
                        AudioMode::SyntheticTest,
                        None,
                        "Synthetic Test Signal".to_string(),
                        48_000,
                    )
                }
            };
        let audio_device_refresh = Some(AsyncDeviceListRefresh::spawn());

        let max_pcm = max_pcm_samples_per_channel();
        logger.log_info(&format!("projectm_pcm_get_max_samples() = {max_pcm}"));

        let start_memory = current_process_memory_mb();
        if let Some(m) = start_memory {
            logger.log_info(&format!(
                "Startup memory: WorkingSet={:.1} MiB | PrivateCommit={:.1} MiB",
                m.working_set_mb, m.private_commit_mb
            ));
        }

        let now = Instant::now();
        Ok(Self {
            config,
            egui_ctx,
            egui_winit,
            egui_renderer,
            mixer,
            master_tex_id,
            deck_a_tex_id,
            deck_b_tex_id,
            deck_a,
            deck_b,
            projector: None,
            monitors,
            pending_projector_cmd: None,
            master_fence: None,
            retired_master_fences: VecDeque::with_capacity(8),
            test_projector_opened: false,
            gl_display,
            pbuffer_config,
            window,
            main_surface,
            main_context,
            main_gl,
            bus,
            load_guard: LoadGuardState::default(),
            interact: UiInteractionState::default(),
            modifiers: ModifiersState::empty(),
            held_momentary_targets: Vec::new(),
            blackout_alpha: 0.0,
            mappings_status,
            gpu_label,
            gpu_is_intel,
            res_toggles_done: 0,
            last_res_toggle_test_at: now,
            audio_mode,
            live_capture,
            selected_audio_device_id,
            audio_devices: Vec::new(),
            audio_switch_handle: None,
            audio_switch_seq: 0,
            audio_device_refresh,
            last_device_refresh_at: now,
            audio_device_name,
            audio_sample_rate,
            last_wasapi_packets: 0,
            last_packet_time: now,
            audio_peak_db: -100.0,
            audio_age_stats: TimingStats::default(),
            synth_gen: TestAudioGenerator::new(48_000.0),
            live_pcm_buf: Vec::with_capacity(4096 * 2),
            synth_pcm_buf: vec![0.0_f32; 800 * 2],
            silence_pcm_buf: vec![0.0_f32; 800 * 2],
            eq_deck_a: VisualEqProcessor::new(audio_sample_rate),
            eq_deck_b: VisualEqProcessor::new(audio_sample_rate),
            eq_buf_a: Vec::with_capacity(4096 * 2),
            eq_buf_b: Vec::with_capacity(4096 * 2),
            beat_pulse: BeatPulseEngine::new(audio_sample_rate),
            device_lost_logged: false,
            presets_root,
            preset_cache_path,
            playlists_path,
            playlists,
            playlists_revision: 0,
            hot_banks_path,
            hot_banks,
            presets,
            folders,
            filtered_indices,
            last_filter_sig: (
                String::new(),
                WeightFilter::All,
                None,
                CollectionFilter::All,
                0,
                0,
            ),
            library_revision: 0,
            scanner,
            scan_status,
            bad_shader_path,
            deck_a_current,
            deck_b_current,
            deck_a_override_name: None,
            last_load: None,
            pending_stall_traces: Vec::new(),
            start_time: now,
            last_frame_instant: now,
            last_fps_sample_time: now,
            last_fps_sample_frames: 0,
            global_fps: 60.0,
            deck_a_ms_ema: 2.0,
            deck_b_ms_ema: 2.0,
            frame_count: 0,
            timing_a_cpu: TimingStats::default(),
            timing_b_cpu: TimingStats::default(),
            timing_a_gpu_q: TimingStats::default(),
            timing_b_gpu_q: TimingStats::default(),
            timing_frame_dt: TimingStats::default(),
            blank_check_stats: TimingStats::default(),
            start_memory,
            last_periodic_log: now,
            logger,
            shut_down: false,
        })
    }

    /// Polls non-blocking background audio device enumeration and WASAPI switching workers (`2A-5`).
    fn poll_audio_background_tasks(&mut self) {
        if let Some(ref refresh) = self.audio_device_refresh {
            if let Some(result) = refresh.poll() {
                self.audio_device_refresh = None;
                match result {
                    Ok(devices) => {
                        let count = devices.len();
                        self.audio_devices = devices;
                        self.logger.log_info(&format!(
                            "[AUDIO] Refreshed active Windows output devices ({count} found)"
                        ));
                    }
                    Err(err) => {
                        self.logger
                            .log_info(&format!("[AUDIO-ERROR] Device refresh failed: {err}"));
                    }
                }
            }
        }

        if let Some(ref switcher) = self.audio_switch_handle {
            if let Some(outcome) = switcher.poll() {
                self.audio_switch_handle = None;
                match outcome {
                    AudioSwitchOutcome::LiveReady {
                        request_id,
                        requested_device_id,
                        capture,
                    } => {
                        if request_id != self.audio_switch_seq
                            || self.audio_mode != AudioMode::LiveWasapi
                        {
                            // Stale switch completed after a newer user choice; stop cleanly off-thread.
                            AsyncAudioSwitch::stop_in_background(Some(capture));
                            return;
                        }
                        let fmt = capture.format().clone();
                        self.audio_device_name = fmt.device_name.clone();
                        self.audio_sample_rate = fmt.output_sample_rate;
                        self.selected_audio_device_id = requested_device_id;
                        self.last_wasapi_packets = 0;
                        self.last_packet_time = Instant::now();
                        self.device_lost_logged = false;
                        self.live_capture = Some(capture);
                        let msg = format!(
                            "Audio active: {} ({} Hz)",
                            fmt.device_name, fmt.output_sample_rate
                        );
                        self.logger.log_info(&format!(
                            "[AUDIO] Live WASAPI switched cleanly: `{}` ({} Hz, {} ch -> {} Hz stereo)",
                            fmt.device_name,
                            fmt.native_sample_rate,
                            fmt.native_channels,
                            fmt.output_sample_rate
                        ));
                        self.interact.show_toast(msg);
                    }
                    AudioSwitchOutcome::LiveFailed {
                        request_id,
                        error,
                        ..
                    } => {
                        if request_id != self.audio_switch_seq
                            || self.audio_mode != AudioMode::LiveWasapi
                        {
                            return;
                        }
                        self.live_capture = None;
                        self.device_lost_logged = true;
                        self.logger
                            .log_info(&format!("[AUDIO-ERROR] Live WASAPI switch failed: {error}"));
                        self.interact
                            .show_toast(format!("Audio switch failed: {error}"));
                        // Refresh device list so an unplugged endpoint disappears cleanly.
                        self.trigger_device_list_refresh(true);
                    }
                }
            }
        }
    }

    fn trigger_device_list_refresh(&mut self, force: bool) {
        if self.audio_device_refresh.is_some() {
            return;
        }
        if !force && self.last_device_refresh_at.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_device_refresh_at = Instant::now();
        self.audio_device_refresh = Some(AsyncDeviceListRefresh::spawn());
    }

    fn start_async_live_audio_switch(&mut self, device_id: Option<String>, label: String) {
        self.audio_mode = AudioMode::LiveWasapi;
        self.audio_switch_seq = self.audio_switch_seq.wrapping_add(1);
        let old = self.live_capture.take();
        self.audio_device_name = label.clone();
        self.device_lost_logged = false;
        self.audio_switch_handle = Some(AsyncAudioSwitch::spawn_live_switch(
            old,
            device_id,
            self.audio_switch_seq,
        ));
        self.logger
            .log_info(&format!("[AUDIO] Switching Live WASAPI -> `{label}` (async)..."));
    }

    /// Executes Top Bar actions (audio source switch, device switch, refresh, resolution toggle).
    fn process_top_bar_actions(&mut self) {
        if self.interact.top_bar_actions.is_empty() {
            return;
        }
        let actions: Vec<TopBarAction> = std::mem::take(&mut self.interact.top_bar_actions);
        for action in actions {
            match action {
                TopBarAction::SetAudioSourceLive(true) => {
                    let label = self
                        .selected_audio_device_id
                        .as_deref()
                        .and_then(|id| self.audio_devices.iter().find(|d| d.id == id))
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "System Default".to_string());
                    self.start_async_live_audio_switch(
                        self.selected_audio_device_id.clone(),
                        label,
                    );
                }
                TopBarAction::SetAudioSourceLive(false) => {
                    self.audio_mode = AudioMode::SyntheticTest;
                    self.audio_switch_seq = self.audio_switch_seq.wrapping_add(1);
                    self.audio_switch_handle = None;
                    AsyncAudioSwitch::stop_in_background(self.live_capture.take());
                    self.audio_device_name = "Synthetic Test Signal".to_string();
                    self.audio_sample_rate = 48_000;
                    self.device_lost_logged = false;
                    self.logger
                        .log_info("[AUDIO] Switched source -> Synthetic Test Audio (48000 Hz)");
                    self.interact
                        .show_toast("Audio source: Synthetic Test Signal (48 kHz)");
                }
                TopBarAction::SelectAudioDevice { device_id, label } => {
                    self.selected_audio_device_id = device_id.clone();
                    self.start_async_live_audio_switch(device_id, label);
                }
                TopBarAction::RefreshAudioDevices => {
                    self.trigger_device_list_refresh(false);
                }
                TopBarAction::ReconnectAudio => {
                    self.trigger_device_list_refresh(true);
                    let label = self
                        .selected_audio_device_id
                        .as_deref()
                        .and_then(|id| self.audio_devices.iter().find(|d| d.id == id))
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "System Default".to_string());
                    self.start_async_live_audio_switch(
                        self.selected_audio_device_id.clone(),
                        label,
                    );
                }
                TopBarAction::ToggleDeckResolution => {
                    let (new_w, new_h) = if self.config.deck_height >= 1080 {
                        (1280, 720)
                    } else {
                        (1920, 1080)
                    };
                    self.resize_decks_and_mixer(new_w, new_h);
                }
                TopBarAction::OpenProjector(target) => {
                    self.pending_projector_cmd = Some(ProjectorCommand::Open(target));
                }
                TopBarAction::CloseProjector => {
                    self.pending_projector_cmd = Some(ProjectorCommand::Close);
                }
                TopBarAction::RefreshMonitors => {
                    self.pending_projector_cmd = Some(ProjectorCommand::RefreshMonitors);
                }
            }
        }
    }

    /// Executes queued Projector window open/close/refresh commands that require `&ActiveEventLoop`,
    /// enforcing the Single-VSync swap-interval rule (`Projector=Wait(1), Console=DontWait`).
    fn apply_pending_projector_command(&mut self, event_loop: &ActiveEventLoop) {
        // Optional automated windowed Projector test (`--test-projector-windowed`).
        if self.config.test_projector_windowed
            && !self.test_projector_opened
            && self.frame_count >= 10
        {
            self.test_projector_opened = true;
            self.pending_projector_cmd =
                Some(ProjectorCommand::Open(ProjectorTarget::WindowedPreview));
        }

        let Some(cmd) = self.pending_projector_cmd.take() else {
            return;
        };
        match cmd {
            ProjectorCommand::RefreshMonitors => {
                self.monitors = enumerate_monitors(event_loop)
                    .into_iter()
                    .map(|(info, _)| info)
                    .collect();
                self.logger.log_info(&format!(
                    "[PROJ] Refreshed Windows displays ({} connected)",
                    self.monitors.len()
                ));
            }
            ProjectorCommand::Open(target) => {
                // Close any existing projector cleanly before opening on a new target.
                if self.projector.is_some() {
                    self.close_projector();
                }
                // Refresh monitor snapshot so indices match current Windows topology.
                self.monitors = enumerate_monitors(event_loop)
                    .into_iter()
                    .map(|(info, _)| info)
                    .collect();

                match ProjectorWindow::open(
                    event_loop,
                    &self.gl_display,
                    &self.main_context,
                    target,
                    &mut self.logger,
                ) {
                    Ok(proj) => {
                        let lbl = proj.target_label().to_string();
                        self.projector = Some(proj);
                        // Re-bind main console context and disable VSync on Console so ONLY
                        // the Projector waits for VSync (prevents 30 FPS halving!).
                        if self.main_context.make_current(&self.main_surface).is_ok() {
                            let _ = self
                                .main_surface
                                .set_swap_interval(&self.main_context, SwapInterval::DontWait);
                        }
                        self.logger.log_info(
                            "[PROJ] Single-VSync active: Projector=Wait(1), Control Console=DontWait",
                        );
                        self.window.focus_window();
                        self.interact
                            .show_toast(format!("Projector ON: {lbl}"));
                    }
                    Err(err) => {
                        let _ = self.main_context.make_current(&self.main_surface);
                        self.logger
                            .log_info(&format!("[PROJ-ERROR] Failed to open Projector: {err}"));
                        self.interact
                            .show_toast(format!("Projector failed: {err}"));
                    }
                }
            }
            ProjectorCommand::Close => {
                self.close_projector();
            }
        }
    }

    /// Closes the Projector Output window cleanly and restores `Wait(1)` VSync on Control Console.
    fn close_projector(&mut self) {
        if let Some(mut proj) = self.projector.take() {
            let lbl = proj.target_label().to_string();
            proj.destroy();
            drop(proj);
            if self.main_context.make_current(&self.main_surface).is_ok() {
                let _ = self.main_surface.set_swap_interval(
                    &self.main_context,
                    SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
                );
            }
            self.logger.log_info(&format!(
                "[PROJ] Closed Projector (`{lbl}`); restored Control Console VSync=Wait(1)"
            ));
            self.interact.show_toast("Projector Output closed");
        }
    }

    /// Runtime resolution toggle (`1280x720` <-> `1920x1080`) for Deck A, Deck B, and Master FBO.
    fn resize_decks_and_mixer(&mut self, new_w: u32, new_h: u32) {
        if self.config.deck_width == new_w && self.config.deck_height == new_h {
            return;
        }
        let t0 = Instant::now();
        let mem_before = current_process_memory_mb();

        if let Err(err) = self.deck_a.resize(
            new_w,
            new_h,
            &self.gl_display,
            &self.pbuffer_config,
            &mut self.logger,
        ) {
            self.logger
                .log_info(&format!("[RES-ERROR] Deck A resize failed: {err}"));
            self.interact
                .show_toast(format!("Resolution resize error (Deck A): {err}"));
            let _ = self.main_context.make_current(&self.main_surface);
            return;
        }
        if let Err(err) = self.deck_b.resize(
            new_w,
            new_h,
            &self.gl_display,
            &self.pbuffer_config,
            &mut self.logger,
        ) {
            self.logger
                .log_info(&format!("[RES-ERROR] Deck B resize failed: {err}"));
            self.interact
                .show_toast(format!("Resolution resize error (Deck B): {err}"));
            let _ = self.main_context.make_current(&self.main_surface);
            return;
        }

        if let Err(err) = self.main_context.make_current(&self.main_surface) {
            self.logger.log_info(&format!(
                "[RES-ERROR] Main context make_current failed after deck resize: {err}"
            ));
            return;
        }
        if let Err(err) = self.mixer.resize(new_w, new_h) {
            self.logger
                .log_info(&format!("[RES-ERROR] GPU mixer resize failed: {err}"));
            self.interact
                .show_toast(format!("Resolution resize error (Mixer): {err}"));
            return;
        }
        self.logger.check_frame(
            &self.main_gl,
            self.frame_count,
            &format!("main-mixer-resize-{new_w}x{new_h}"),
        );

        self.config.deck_width = new_w;
        self.config.deck_height = new_h;
        self.window.set_title(&format!(
            "MilkDrop VJ — Stage 3D Console, FX Racks & Hot Banks ({new_w}x{new_h} decks)"
        ));

        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mem_after = current_process_memory_mb();
        let mem_note = match (mem_before, mem_after) {
            (Some(b), Some(a)) => format!(
                "WS {:.1}->{:.1} MiB PC {:.1}->{:.1} MiB",
                b.working_set_mb, a.working_set_mb, b.private_commit_mb, a.private_commit_mb
            ),
            _ => "mem=n/a".to_string(),
        };
        let line = format!(
            "[RES] Toggled decks + Master -> {new_w}x{new_h} in {elapsed_ms:.2} ms | GL_err={} | {mem_note}",
            self.logger.total_errors()
        );
        println!("{line}");
        self.logger.log_info(&line);
        self.interact
            .show_toast(format!("Deck resolution switched to {new_w}x{new_h} ({elapsed_ms:.1} ms)"));
    }

    /// Polls the background preset-scanner thread without ever blocking the render loop.
    fn poll_preset_scanner(&mut self) {
        if self.interact.request_rescan && self.scanner.is_none() {
            self.interact.request_rescan = false;
            self.scanner = Some(PresetScannerHandle::spawn(
                self.presets_root.clone(),
                self.preset_cache_path.clone(),
            ));
            self.scan_status.scanning = true;
            self.scan_status.scanned = 0;
            self.scan_status.total = 0;
            self.scan_status.last_error = None;
            self.logger
                .log_info("[SCAN] User triggered background preset library rescan...");
        }

        let Some(handle) = self.scanner.as_ref() else {
            return;
        };
        let updates = handle.poll_updates();
        let mut finished = false;

        for update in updates {
            match update {
                ScanUpdate::Progress {
                    scanned,
                    total,
                    cache_hits,
                    parsed,
                } => {
                    self.scan_status.scanning = true;
                    self.scan_status.scanned = scanned;
                    self.scan_status.total = total;
                    self.scan_status.cache_hits = cache_hits;
                    self.scan_status.parsed = parsed;
                }
                ScanUpdate::Finished(report) => {
                    finished = true;
                    if !report.entries.is_empty() {
                        self.presets = report.entries;
                        self.folders = report.folders;
                        // Clear folder filter if the selected folder disappeared after a rescan.
                        if let Some(ref f) = self.interact.folder_filter {
                            if !self.folders.iter().any(|x| x == f) {
                                self.interact.folder_filter = None;
                            }
                        }
                        self.library_revision = self.library_revision.wrapping_add(1);
                    }
                    self.scan_status = ScanStatus {
                        scanning: false,
                        scanned: report.total,
                        total: report.total,
                        cache_hits: report.cache_hits,
                        parsed: report.parsed,
                        elapsed_ms: report.elapsed_ms,
                        last_error: report.error.clone(),
                    };
                    let msg = format!(
                        "[SCAN] Complete (provisional, laptop-limited): total={} | folders={} | cache_hits={} | parsed={} | elapsed={:.2} ms | cache_saved={}",
                        report.total,
                        self.folders.len(),
                        report.cache_hits,
                        report.parsed,
                        report.elapsed_ms,
                        report.cache_saved
                    );
                    println!("{msg}");
                    self.logger.log_info(&msg);
                    if let Some(err) = report.error {
                        self.logger.log_info(&format!("[SCAN-ERROR] {err}"));
                        self.interact
                            .show_toast(format!("Preset scan warning: {err}"));
                    }
                }
            }
        }

        if finished {
            self.scanner = None;
        }
    }

    /// Applies queued Favorite & Playlist actions from the UI and saves `playlists.json` atomically.
    fn process_playlist_actions(&mut self) {
        if self.interact.playlist_actions.is_empty() {
            return;
        }
        let actions: Vec<PlaylistAction> = std::mem::take(&mut self.interact.playlist_actions);
        let mut dirty = false;

        for action in actions {
            match action {
                PlaylistAction::ToggleFavorite {
                    rel_path,
                    display_name,
                } => {
                    let now_fav = self.playlists.toggle_favorite(&rel_path);
                    dirty = true;
                    let msg = if now_fav {
                        format!("Starred `{display_name}` in Favorites")
                    } else {
                        format!("Removed `{display_name}` from Favorites")
                    };
                    self.interact.show_toast(msg);
                }
                PlaylistAction::ToggleInPlaylist {
                    playlist_name,
                    rel_path,
                    display_name,
                } => {
                    let added = self
                        .playlists
                        .toggle_in_playlist(&playlist_name, &rel_path);
                    dirty = true;
                    let msg = if added {
                        format!("Added `{display_name}` to playlist `{playlist_name}`")
                    } else {
                        format!("Removed `{display_name}` from playlist `{playlist_name}`")
                    };
                    self.interact.show_toast(msg);
                }
                PlaylistAction::MoveInPlaylist {
                    playlist_name,
                    rel_path,
                    delta,
                } => {
                    if self
                        .playlists
                        .move_in_playlist(&playlist_name, &rel_path, delta)
                    {
                        dirty = true;
                    }
                }
                PlaylistAction::RemoveFromPlaylist {
                    playlist_name,
                    rel_path,
                } => {
                    if self
                        .playlists
                        .remove_from_playlist(&playlist_name, &rel_path)
                    {
                        dirty = true;
                        self.interact
                            .show_toast(format!("Removed preset from playlist `{playlist_name}`"));
                    }
                }
                PlaylistAction::CreatePlaylist { name, also_add } => {
                    match self.playlists.create_playlist(&name) {
                        Ok(created_name) => {
                            if let Some((rel, _disp)) = also_add {
                                self.playlists.toggle_in_playlist(&created_name, &rel);
                            }
                            dirty = true;
                            let msg = format!("Created playlist `{created_name}`");
                            self.interact.playlist_modal_feedback = Some(msg.clone());
                            self.interact.show_toast(msg);
                            self.logger
                                .log_info(&format!("[PLAYLISTS] Created `{created_name}`"));
                        }
                        Err(e) => {
                            self.interact.playlist_modal_feedback = Some(e);
                        }
                    }
                }
                PlaylistAction::RenamePlaylist { idx, new_name } => {
                    match self.playlists.rename_playlist(idx, &new_name) {
                        Ok((old_name, final_name)) => {
                            if let CollectionFilter::Playlist(ref mut cur) =
                                self.interact.collection_filter
                            {
                                if cur.eq_ignore_ascii_case(&old_name) {
                                    *cur = final_name.clone();
                                }
                            }
                            dirty = true;
                            let msg = format!("Renamed `{old_name}` -> `{final_name}`");
                            self.interact.playlist_modal_feedback = Some(msg.clone());
                            self.interact.show_toast(msg);
                        }
                        Err(e) => {
                            self.interact.playlist_modal_feedback = Some(e);
                        }
                    }
                }
                PlaylistAction::DeletePlaylist { idx } => {
                    if let Some(removed) = self.playlists.delete_playlist(idx) {
                        if let CollectionFilter::Playlist(ref cur) = self.interact.collection_filter
                        {
                            if cur.eq_ignore_ascii_case(&removed) {
                                self.interact.collection_filter = CollectionFilter::All;
                            }
                        }
                        dirty = true;
                        let msg = format!("Deleted playlist `{removed}`");
                        self.interact.playlist_modal_feedback = Some(msg.clone());
                        self.interact.show_toast(msg);
                        self.logger
                            .log_info(&format!("[PLAYLISTS] Deleted `{removed}`"));
                    }
                }
            }
        }

        if dirty {
            self.playlists_revision = self.playlists_revision.wrapping_add(1);
            if let Err(e) = self.playlists.save_atomic(&self.playlists_path) {
                let err_msg = format!("Failed to save playlists.json: {e}");
                self.logger.log_info(&format!("[ERROR] {err_msg}"));
                self.interact.playlist_modal_feedback = Some(err_msg.clone());
                self.interact.show_toast(err_msg);
            }
        }
    }

    /// Recomputes `filtered_indices` ONLY when search, Weight, Folder, Collection, playlists, or library change.
    fn refresh_filtered_presets_if_needed(&mut self) {
        let sig = (
            self.interact.search_query.clone(),
            self.interact.weight_filter,
            self.interact.folder_filter.clone(),
            self.interact.collection_filter.clone(),
            self.library_revision,
            self.playlists_revision,
        );
        if sig == self.last_filter_sig {
            return;
        }
        self.last_filter_sig = sig;
        self.filtered_indices = filter_preset_indices(
            &self.presets,
            &self.interact.search_query,
            self.interact.weight_filter,
            self.interact.folder_filter.as_deref(),
            &self.interact.collection_filter,
            &self.playlists,
        );
        if self.filtered_indices.is_empty() {
            self.interact.selected_preset_idx = 0;
        } else if self.interact.selected_preset_idx >= self.filtered_indices.len() {
            self.interact.selected_preset_idx = self.filtered_indices.len() - 1;
        }
    }

    /// Computes the next or previous entry index for `< Prev` / `Next >`, stepping through the
    /// currently filtered list (or falling back to the full list if the filter matches 0 rows).
    fn step_deck_in_filtered_list(&self, current_path: &Path, forward: bool) -> Option<usize> {
        if !self.filtered_indices.is_empty() {
            let len = self.filtered_indices.len();
            if let Some(pos) = self
                .filtered_indices
                .iter()
                .position(|&entry_idx| self.presets[entry_idx].path == current_path)
            {
                let next_pos = if forward {
                    (pos + 1) % len
                } else if pos == 0 {
                    len - 1
                } else {
                    pos - 1
                };
                return Some(self.filtered_indices[next_pos]);
            }
            // Current deck preset is outside the active filter: jump into the filtered list cleanly.
            return Some(if forward {
                self.filtered_indices[0]
            } else {
                self.filtered_indices[len - 1]
            });
        }

        // Fallback if the active filter has 0 matches: step across the full library.
        if self.presets.is_empty() {
            return None;
        }
        let len = self.presets.len();
        let pos = self
            .presets
            .iter()
            .position(|p| p.path == current_path)
            .unwrap_or(0);
        let next_pos = if forward {
            (pos + 1) % len
        } else if pos == 0 {
            len - 1
        } else {
            pos - 1
        };
        Some(next_pos)
    }

    /// Executes any queued Control Bus triggers (preset switching, keyboard/mouse loads, test-blank).
    fn process_bus_triggers(&mut self) {
        if self.interact.request_reset_defaults {
            self.interact.request_reset_defaults = false;
            self.bus.mappings = MappingsConfig::default();
            match self.bus.mappings.save(Path::new("mappings.json")) {
                Ok(()) => {
                    self.mappings_status = format!(
                        "Bus: mappings.json reset ({} keys)",
                        self.bus.mappings.keyboard.len()
                    );
                    self.interact.remap_feedback =
                        Some("Restored default key bindings and saved mappings.json.".to_string());
                    self.logger.log_info("Mappings reset to defaults and saved to mappings.json");
                }
                Err(e) => {
                    self.interact.remap_feedback = Some(format!("Save failed: {e}"));
                    self.logger.log_info(&format!("[ERROR] Failed to save mappings.json: {e}"));
                }
            }
        }

        if self.interact.request_clear_tempo {
            self.interact.request_clear_tempo = false;
            self.beat_pulse.clear_tempo();
            self.bus.set(param::MASTER_BEAT_AUTO, 0.0);
            self.interact.show_toast("Cleared Tap Tempo & Auto-Pulse");
        }

        let triggers = self.bus.drain_triggers();
        let xfade = self.bus.get(param::MIXER_CROSSFADER);
        let air_a = DeckAirState::from_weight(1.0 - xfade);
        let air_b = DeckAirState::from_weight(xfade);
        let selected_entry_idx = self
            .filtered_indices
            .get(self.interact.selected_preset_idx)
            .copied();

        for trig in triggers {
            match trig.target.as_str() {
                param::DECK_A_PREV_PRESET => {
                    if let Some(next_idx) =
                        self.step_deck_in_filtered_list(&self.deck_a_current.path, false)
                    {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_A_TRANSITION_SMOOTH);
                        self.load_deck_preset(true, next_idx, smooth);
                    }
                }
                param::DECK_A_NEXT_PRESET => {
                    if let Some(next_idx) =
                        self.step_deck_in_filtered_list(&self.deck_a_current.path, true)
                    {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_A_TRANSITION_SMOOTH);
                        self.load_deck_preset(true, next_idx, smooth);
                    }
                }
                param::DECK_B_PREV_PRESET => {
                    if let Some(next_idx) =
                        self.step_deck_in_filtered_list(&self.deck_b_current.path, false)
                    {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_B_TRANSITION_SMOOTH);
                        self.load_deck_preset(false, next_idx, smooth);
                    }
                }
                param::DECK_B_NEXT_PRESET => {
                    if let Some(next_idx) =
                        self.step_deck_in_filtered_list(&self.deck_b_current.path, true)
                    {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_B_TRANSITION_SMOOTH);
                        self.load_deck_preset(false, next_idx, smooth);
                    }
                }
                param::DECK_A_TEST_BLANK => {
                    self.load_bad_shader_on_deck_a();
                }
                param::BROWSER_LOAD_OFF_AIR => {
                    let Some(sel_idx) = selected_entry_idx else {
                        self.interact
                            .show_toast("No presets match the current filter — press Esc to clear");
                        continue;
                    };
                    // Section 13.3 Load Safety: `Enter` loads only into a `CUE` deck (<= 5% weight).
                    if !air_b.is_protected() {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_B_TRANSITION_SMOOTH);
                        self.load_deck_preset(false, sel_idx, smooth);
                    } else if !air_a.is_protected() {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_A_TRANSITION_SMOOTH);
                        self.load_deck_preset(true, sel_idx, smooth);
                    } else {
                        self.interact.show_toast(
                            "Both decks are live (MIXED) — cue a deck (<=5%) or press Ctrl+A / Ctrl+B to force",
                        );
                    }
                }
                param::DECK_A_LOAD_SELECTED => {
                    let Some(sel_idx) = selected_entry_idx else {
                        self.interact
                            .show_toast("No presets match the current filter — press Esc to clear");
                        continue;
                    };
                    if air_a.is_protected() && !trig.force_on_air {
                        self.interact.show_toast(
                            "Deck A is ON-AIR/MIXED — press Ctrl+A to force load",
                        );
                    } else {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_A_TRANSITION_SMOOTH);
                        self.load_deck_preset(true, sel_idx, smooth);
                    }
                }
                param::DECK_B_LOAD_SELECTED => {
                    let Some(sel_idx) = selected_entry_idx else {
                        self.interact
                            .show_toast("No presets match the current filter — press Esc to clear");
                        continue;
                    };
                    if air_b.is_protected() && !trig.force_on_air {
                        self.interact.show_toast(
                            "Deck B is ON-AIR/MIXED — press Ctrl+B to force load",
                        );
                    } else {
                        let smooth =
                            !trig.hard_cut && self.bus.get_bool(param::DECK_B_TRANSITION_SMOOTH);
                        self.load_deck_preset(false, sel_idx, smooth);
                    }
                }
                param::MASTER_BEAT_PULSE => {
                    let strength = self.bus.get(param::MASTER_BEAT_STRENGTH);
                    self.beat_pulse.trigger_pulse(strength);
                }
                param::MASTER_TAP_TEMPO => {
                    if let Some(bpm) = self.beat_pulse.record_tap() {
                        self.interact
                            .show_toast(format!("Tap Tempo: {bpm:.1} BPM"));
                    }
                }
                other => {
                    if let Some((is_deck_a, slot)) = param::parse_hot_bank_target(other) {
                        let air = if is_deck_a { air_a } else { air_b };
                        self.trigger_hot_bank_slot(
                            is_deck_a,
                            slot,
                            trig.hard_cut,
                            trig.force_on_air,
                            air,
                        );
                    }
                }
            }
        }
    }

    /// Saves or clears Hot-Preset Bank slots triggered by the UI and persists `hot_banks.json`.
    fn process_hot_bank_actions(&mut self) {
        if self.interact.hot_bank_actions.is_empty() {
            return;
        }
        let actions: Vec<HotBankAction> = self.interact.hot_bank_actions.drain(..).collect();
        let mut changed = false;
        for action in actions {
            match action {
                HotBankAction::Assign {
                    is_deck_a,
                    slot,
                    rel_path,
                    display_name,
                } => {
                    let target = param::hot_bank_target(is_deck_a, slot).unwrap_or("");
                    let hint = self
                        .bus
                        .mappings
                        .hint_for_target(target, MappingMode::Trigger);
                    let deck_lbl = if is_deck_a { "Deck A" } else { "Deck B" };
                    if self
                        .hot_banks
                        .assign(is_deck_a, slot, rel_path, display_name.clone())
                    {
                        changed = true;
                        let msg = format!("Saved `{display_name}` to {deck_lbl} Hot Bank [{hint}]");
                        self.logger.log_info(&format!("[BANKS] {msg}"));
                        self.interact.show_toast(msg);
                    }
                }
                HotBankAction::Clear { is_deck_a, slot } => {
                    let target = param::hot_bank_target(is_deck_a, slot).unwrap_or("");
                    let hint = self
                        .bus
                        .mappings
                        .hint_for_target(target, MappingMode::Trigger);
                    let deck_lbl = if is_deck_a { "Deck A" } else { "Deck B" };
                    if self.hot_banks.clear(is_deck_a, slot) {
                        changed = true;
                        let msg = format!("Cleared {deck_lbl} Hot Bank [{hint}]");
                        self.logger.log_info(&format!("[BANKS] {msg}"));
                        self.interact.show_toast(msg);
                    }
                }
            }
        }
        if changed {
            if let Err(err) = self.hot_banks.save(&self.hot_banks_path) {
                self.logger
                    .log_info(&format!("[ERROR] Failed to save hot_banks.json: {err}"));
                self.interact
                    .show_toast(format!("Failed to save hot_banks.json: {err}"));
            }
        }
    }

    /// Loads a preset assigned to a Hot-Preset Bank slot, respecting Load Safety and Smooth/Cut mode.
    fn trigger_hot_bank_slot(
        &mut self,
        is_deck_a: bool,
        slot: usize,
        hard_cut: bool,
        force_on_air: bool,
        air_state: DeckAirState,
    ) {
        let deck_lbl = if is_deck_a { "Deck A" } else { "Deck B" };
        let target = param::hot_bank_target(is_deck_a, slot).unwrap_or("");
        let hint = self
            .bus
            .mappings
            .hint_for_target(target, MappingMode::Trigger);

        let Some(assigned) = self.hot_banks.get(is_deck_a, slot).cloned() else {
            self.interact.show_toast(format!(
                "{deck_lbl} Hot Bank [{hint}] is empty — click pad to save current preset"
            ));
            return;
        };

        if air_state.is_protected() && !force_on_air {
            self.interact.show_toast(format!(
                "{deck_lbl} is ON-AIR/MIXED — press Ctrl+{hint} to force load Hot Bank [{hint}]"
            ));
            return;
        }

        let smooth_param = if is_deck_a {
            param::DECK_A_TRANSITION_SMOOTH
        } else {
            param::DECK_B_TRANSITION_SMOOTH
        };
        let smooth = !hard_cut && self.bus.get_bool(smooth_param);

        // Prefer the indexed entry from `self.presets` (full complexity metadata).
        if let Some(idx) = self
            .presets
            .iter()
            .position(|p| p.rel_path == assigned.rel_path)
        {
            self.load_deck_preset(is_deck_a, idx, smooth);
            return;
        }

        // Fallback: load directly from `presets/<rel_path>` so Hot Banks work even during the first
        // second of startup before the background library scan finishes!
        let direct_path = self.presets_root.join(&assigned.rel_path);
        if direct_path.exists() {
            let folder = Path::new(&assigned.rel_path)
                .parent()
                .and_then(|p| p.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("Starter")
                .to_string();
            let entry = PresetEntry {
                path: direct_path,
                rel_path: assigned.rel_path.clone(),
                display_name: assigned.display_name.clone(),
                search_haystack: format!("{} {}", assigned.display_name, folder).to_lowercase(),
                folder,
                complexity: PresetComplexity {
                    enabled_shape_instances: 0,
                    tier: milkdrop_vj::preset_meta::ComplexityTier::Light,
                },
            };
            self.load_deck_preset_entry(is_deck_a, entry, smooth);
        } else {
            self.interact.show_toast(format!(
                "Hot Bank [{hint}] file missing on disk: {}",
                assigned.rel_path
            ));
        }
    }

    /// Handles `winit` keyboard events according to the Section 13 Focus Model & `mappings.json`.
    /// Returns `true` if this event was consumed by our keyboard router (should NOT be sent to egui).
    fn handle_keyboard_event(&mut self, event: &KeyEvent) -> bool {
        let pressed = event.state == ElementState::Pressed;
        let PhysicalKey::Code(code) = event.physical_key else {
            return false;
        };

        let shift = self.modifiers.shift_key();
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();

        // 1. Keys Remapping Modal active: capture replacement key or close on Esc; pause all live keys.
        if self.interact.keys_modal_open {
            if !pressed {
                return true;
            }
            if let Some(idx) = self.interact.remapping_idx {
                if code == KeyCode::Escape {
                    self.interact.remapping_idx = None;
                    self.interact.remap_feedback = Some("Remap cancelled.".to_string());
                    return true;
                }
                if let Some(key_name) = keycode_to_mapping_name(code, &event.logical_key) {
                    let mods = normalize_modifiers(shift, ctrl, alt);
                    let conflict = self.bus.mappings.remap_keyboard(idx, key_name, mods);
                    let badge = self
                        .bus
                        .mappings
                        .keyboard
                        .get(idx)
                        .map(|m| m.short_key_badge())
                        .unwrap_or_default();
                    let action = self
                        .bus
                        .mappings
                        .keyboard
                        .get(idx)
                        .map(|m| m.action_description())
                        .unwrap_or_default();
                    match self.bus.mappings.save(Path::new("mappings.json")) {
                        Ok(()) => {
                            let conflict_note = conflict
                                .and_then(|ci| self.bus.mappings.keyboard.get(ci))
                                .map(|other| {
                                    format!(" (WARNING: also bound to `{}`)", other.action_description())
                                })
                                .unwrap_or_default();
                            let msg = format!("Saved `{action}` -> [{badge}]{conflict_note}");
                            self.logger.log_info(&format!("[KEYS] {msg}"));
                            self.interact.remap_feedback = Some(msg);
                            self.mappings_status = format!(
                                "Bus: mappings.json saved ({} keys)",
                                self.bus.mappings.keyboard.len()
                            );
                        }
                        Err(e) => {
                            let msg = format!("Failed to save mappings.json: {e}");
                            self.logger.log_info(&format!("[ERROR] {msg}"));
                            self.interact.remap_feedback = Some(msg);
                        }
                    }
                    self.interact.remapping_idx = None;
                }
                return true;
            } else if code == KeyCode::Escape {
                self.interact.keys_modal_open = false;
                return true;
            }
            return false;
        }

        // 1b. Playlists Manager Modal active: pause all live performance keys; Esc closes modal.
        if self.interact.playlists_modal_open {
            if pressed && code == KeyCode::Escape {
                self.interact.playlists_modal_open = false;
                return true;
            }
            return false;
        }

        // 2. Typing Mode (Search box has focus):
        //    - Esc exits Typing Mode immediately (keeping search text).
        //    - Enter exits Typing Mode first WITHOUT triggering a live load.
        //    - Up/Down still step through the Preset Browser list below.
        //    - All other keys (including Left/Right cursor and Backspace) go to egui TextEdit!
        if self.interact.search_focused {
            if !pressed {
                return false;
            }
            match code {
                KeyCode::Escape | KeyCode::Enter | KeyCode::NumpadEnter => {
                    self.interact.request_blur_search = true;
                    return true;
                }
                KeyCode::ArrowUp => {
                    self.step_browser_selection(-1);
                    return true;
                }
                KeyCode::ArrowDown => {
                    self.step_browser_selection(1);
                    return true;
                }
                _ => return false,
            }
        }

        // 3. Performance / List Mode (default):
        //    Fixed UI focus keys first, then remappable Control Bus mappings from `mappings.json`.
        if pressed {
            match code {
                KeyCode::Slash if !shift && !ctrl && !alt => {
                    self.interact.request_focus_search = true;
                    return true;
                }
                KeyCode::Escape => {
                    // Second Esc clears search text; Esc NEVER quits the app accidentally.
                    if !self.interact.search_query.is_empty() {
                        self.interact.search_query.clear();
                    }
                    return true;
                }
                KeyCode::ArrowUp => {
                    self.step_browser_selection(-1);
                    return true;
                }
                KeyCode::ArrowDown => {
                    self.step_browser_selection(1);
                    return true;
                }
                _ => {}
            }
        }

        let Some(key_name) = keycode_to_mapping_name(code, &event.logical_key) else {
            return false;
        };

        let mut matches = self
            .bus
            .mappings
            .find_keyboard_matches(&key_name, shift, ctrl, alt);

        // Convenience: allow Ctrl+Shift+<force-load key> to combine Force On-Air + Hard Cut,
        // and allow Shift / Ctrl / Ctrl+Shift with Hot-Preset Bank keys (`1`-`8`) for Hard Cut / Force On-Air.
        if matches.is_empty() && !alt {
            if ctrl && shift {
                for mut m in self
                    .bus
                    .mappings
                    .find_keyboard_matches(&key_name, false, true, false)
                {
                    if m.force_on_air {
                        m.hard_cut = true;
                        matches.push(m);
                    }
                }
            }
            if matches.is_empty() && (ctrl || shift) {
                for mut m in self
                    .bus
                    .mappings
                    .find_keyboard_matches(&key_name, false, false, false)
                {
                    if param::parse_hot_bank_target(&m.target).is_some() {
                        m.hard_cut = shift;
                        m.force_on_air = ctrl;
                        matches.push(m);
                    }
                }
            }
        }

        if matches.is_empty() {
            return false;
        }

        for mapping in matches {
            // Allow key repeat ONLY for Nudge (smooth crossfader sliding); ignore repeat on Toggle/Trigger/Cycle/Set/Momentary.
            if event.repeat && mapping.mode != MappingMode::Nudge {
                continue;
            }
            if mapping.mode == MappingMode::Momentary {
                if pressed {
                    if !self.held_momentary_targets.contains(&mapping.target) {
                        self.held_momentary_targets.push(mapping.target.clone());
                    }
                } else {
                    self.held_momentary_targets
                        .retain(|t| t != &mapping.target);
                }
            }
            self.bus.apply_keyboard_mapping(&mapping, pressed);
        }
        true
    }

    /// Safely releases any momentary keyboard FX (`Z`/`X`/`C`/`V`) if the window loses focus
    /// while a key is held down, without disturbing FX latched with the mouse.
    fn release_held_momentary_keys(&mut self) {
        for target in self.held_momentary_targets.drain(..) {
            self.bus.set(&target, 0.0);
        }
    }

    fn step_browser_selection(&mut self, delta: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        let next = (self.interact.selected_preset_idx as isize + delta).clamp(0, (len - 1) as isize)
            as usize;
        self.interact.selected_preset_idx = next;
        self.interact.scroll_to_selected = true;
    }

    fn load_deck_preset(&mut self, is_deck_a: bool, idx: usize, smooth: bool) {
        let Some(entry) = self.presets.get(idx).cloned() else {
            return;
        };
        self.load_deck_preset_entry(is_deck_a, entry, smooth);
    }

    fn load_deck_preset_entry(&mut self, is_deck_a: bool, entry: PresetEntry, smooth: bool) {
        let deck_label = if is_deck_a { "Deck A" } else { "Deck B" };
        let t0 = Instant::now();
        let result = if is_deck_a {
            self.deck_a.load_preset(&entry.path, smooth)
        } else {
            self.deck_b.load_preset(&entry.path, smooth)
        };
        let load_ms = t0.elapsed().as_secs_f64() * 1000.0;

        match result {
            Ok(()) => {
                if is_deck_a {
                    self.deck_a_current = entry.clone();
                    self.deck_a_override_name = None;
                } else {
                    self.deck_b_current = entry.clone();
                }
                self.last_load = Some(LastLoadSummary {
                    deck_label: deck_label.to_string(),
                    preset_name: entry.display_name.clone(),
                    load_ms,
                    error: None,
                });
                self.pending_stall_traces.push(PendingStallTrace {
                    deck_label: deck_label.to_string(),
                    preset_name: entry.display_name,
                    load_call_ms: load_ms,
                    smooth,
                    step: 0,
                });
            }
            Err(err) => {
                self.logger
                    .log_info(&format!("[LOAD-ERROR] {deck_label}: {err}"));
                self.last_load = Some(LastLoadSummary {
                    deck_label: deck_label.to_string(),
                    preset_name: entry.display_name,
                    load_ms,
                    error: Some(err),
                });
            }
        }
    }

    fn load_bad_shader_on_deck_a(&mut self) {
        let t0 = Instant::now();
        let res = self.deck_a.load_preset(&self.bad_shader_path, false);
        let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let label = "_bad_shader_test.milk".to_string();
        match res {
            Ok(()) => {
                self.deck_a_override_name = Some(label.clone());
                self.last_load = Some(LastLoadSummary {
                    deck_label: "Deck A".to_string(),
                    preset_name: label.clone(),
                    load_ms,
                    error: None,
                });
                self.pending_stall_traces.push(PendingStallTrace {
                    deck_label: "Deck A".to_string(),
                    preset_name: label,
                    load_call_ms: load_ms,
                    smooth: false,
                    step: 0,
                });
            }
            Err(err) => {
                self.logger
                    .log_info(&format!("[LOAD-ERROR] Deck A blank test: {err}"));
                self.last_load = Some(LastLoadSummary {
                    deck_label: "Deck A".to_string(),
                    preset_name: label,
                    load_ms,
                    error: Some(err),
                });
            }
        }
    }

    fn render_frame(&mut self) {
        if self.shut_down {
            return;
        }

        let frame_start = Instant::now();
        let frame_dt_ms = frame_start
            .duration_since(self.last_frame_instant)
            .as_secs_f64()
            * 1000.0;
        self.last_frame_instant = frame_start;
        if self.frame_count > 0 {
            self.timing_frame_dt.record_ms(frame_dt_ms);
        }
        self.frame_count += 1;

        // 0. Poll non-blocking background workers (preset scanner, audio switch/enum),
        //    apply Top Bar & playlist actions, and refresh filtered preset view if needed.
        self.poll_preset_scanner();
        self.poll_audio_background_tasks();
        self.process_top_bar_actions();
        self.process_playlist_actions();
        self.process_hot_bank_actions();
        self.refresh_filtered_presets_if_needed();

        // Optional automated resolution-toggle verification (`--test-res-toggles N`).
        if self.res_toggles_done < self.config.test_res_toggles
            && self.last_res_toggle_test_at.elapsed() >= Duration::from_secs(1)
        {
            self.last_res_toggle_test_at = Instant::now();
            self.res_toggles_done += 1;
            let (next_w, next_h) = if self.config.deck_height >= 1080 {
                (1280, 720)
            } else {
                (1920, 1080)
            };
            self.resize_decks_and_mixer(next_w, next_h);
        }

        // 1. Execute any Control Bus triggers queued on the previous UI frame.
        self.process_bus_triggers();

        // 2. Gather live audio (non-blocking ring drain) or synthetic test audio.
        let (audio_slice_len, is_live_buf, audio_badge) = self.collect_audio_for_frame();
        let pcm_slice: &[f32] = if is_live_buf {
            &self.live_pcm_buf[..audio_slice_len]
        } else if self.audio_mode == AudioMode::SyntheticTest {
            &self.synth_pcm_buf
        } else {
            &self.silence_pcm_buf
        };

        // Update smooth top-bar peak level meter (`dBFS`).
        let frame_peak = pcm_slice
            .iter()
            .fold(0.0_f32, |acc, &s| acc.max(s.abs()));
        let instant_db = if frame_peak > 1e-5 {
            20.0 * frame_peak.log10()
        } else {
            -100.0
        };
        if instant_db > self.audio_peak_db {
            self.audio_peak_db = instant_db;
        } else {
            // Smooth ~40 dB/sec decay at 60 FPS so the meter stays readable.
            self.audio_peak_db = (self.audio_peak_db - 0.7).max(instant_db);
        }

        // 2b. Shape independent read-only copies of stereo PCM for Deck A & Deck B Visual EQ
        //     (speakers are never touched; zero per-frame allocations via reusable scratch buffers).
        let eq_params_a = VisualEqParams {
            bass: self.bus.get(param::DECK_A_EQ_BASS),
            mid: self.bus.get(param::DECK_A_EQ_MID),
            treble: self.bus.get(param::DECK_A_EQ_TREBLE),
            bass_kill: self.bus.get_bool(param::DECK_A_EQ_BASS_KILL),
            mid_kill: self.bus.get_bool(param::DECK_A_EQ_MID_KILL),
            treble_kill: self.bus.get_bool(param::DECK_A_EQ_TREBLE_KILL),
        };
        let eq_params_b = VisualEqParams {
            bass: self.bus.get(param::DECK_B_EQ_BASS),
            mid: self.bus.get(param::DECK_B_EQ_MID),
            treble: self.bus.get(param::DECK_B_EQ_TREBLE),
            bass_kill: self.bus.get_bool(param::DECK_B_EQ_BASS_KILL),
            mid_kill: self.bus.get_bool(param::DECK_B_EQ_MID_KILL),
            treble_kill: self.bus.get_bool(param::DECK_B_EQ_TREBLE_KILL),
        };
        self.eq_deck_a.process_stereo(
            pcm_slice,
            &mut self.eq_buf_a,
            self.audio_sample_rate,
            eq_params_a,
        );
        self.eq_deck_b.process_stereo(
            pcm_slice,
            &mut self.eq_buf_b,
            self.audio_sample_rate,
            eq_params_b,
        );

        // 2c. Advance Tap Tempo Auto-Pulse clock and mix any active 70 Hz visual beat pulse
        //     after Visual EQ so manual `Space` hits work even during silence or Bass Kill.
        let beat_dt_secs = (frame_dt_ms / 1000.0) as f32;
        self.beat_pulse.update_clock(
            frame_start,
            beat_dt_secs,
            self.bus.get_bool(param::MASTER_BEAT_AUTO),
            self.bus.get(param::MASTER_BEAT_STRENGTH),
        );
        self.beat_pulse
            .mix_into_decks(&mut self.eq_buf_a, &mut self.eq_buf_b, self.audio_sample_rate);

        // 3. Render Deck A and Deck B in their isolated shared OpenGL contexts.
        let timing_a = match self.deck_a.render_to_texture(
            &self.eq_buf_a,
            self.frame_count,
            self.config.measure_gpu,
            &mut self.logger,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.logger.log_info(&format!("[ERROR] Deck A render failed: {e}"));
                eprintln!("[ERROR] Deck A render failed: {e}");
                return;
            }
        };
        let timing_b = match self.deck_b.render_to_texture(
            &self.eq_buf_b,
            self.frame_count,
            self.config.measure_gpu,
            &mut self.logger,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.logger.log_info(&format!("[ERROR] Deck B render failed: {e}"));
                eprintln!("[ERROR] Deck B render failed: {e}");
                return;
            }
        };

        self.timing_a_cpu.record_ms(timing_a.cpu_submit_ms);
        self.timing_b_cpu.record_ms(timing_b.cpu_submit_ms);
        if let Some(gpu_ms) = timing_a.gpu_timer_ms {
            self.timing_a_gpu_q.record_ms(gpu_ms);
            self.deck_a_ms_ema = self.deck_a_ms_ema * 0.88 + gpu_ms * 0.12;
        }
        if let Some(gpu_ms) = timing_b.gpu_timer_ms {
            self.timing_b_gpu_q.record_ms(gpu_ms);
            self.deck_b_ms_ema = self.deck_b_ms_ema * 0.88 + gpu_ms * 0.12;
        }

        if let Some(ms) = timing_a.blank_check_ms {
            self.blank_check_stats.record_ms(ms);
        }
        if let Some(ms) = timing_b.blank_check_ms {
            self.blank_check_stats.record_ms(ms);
        }

        // 4. Switch back to main window context and issue OpenGL 3.3 Core Section 5.3
        // GPU-only `glWaitSync` on both decks' fences before sampling their shared textures.
        if let Err(e) = self.main_context.make_current(&self.main_surface) {
            self.logger
                .log_info(&format!("[ERROR] Main context make_current failed: {e}"));
            return;
        }
        for fence in [self.deck_a.pending_sync(), self.deck_b.pending_sync()]
            .into_iter()
            .flatten()
        {
            unsafe {
                self.main_gl.wait_sync(fence, 0, glow::TIMEOUT_IGNORED);
            }
        }

        // 4b. Step smooth Emergency Blackout fade (`0.4s`) and render Master Output FBO.
        // Retire previously signaled Master cross-context fences on the main context.
        if let Some(prev_f) = self.master_fence.take() {
            self.retired_master_fences.push_back(prev_f);
        }
        while let Some(&oldest) = self.retired_master_fences.front() {
            let status = unsafe { self.main_gl.client_wait_sync(oldest, 0, 0) };
            if status == glow::ALREADY_SIGNALED || status == glow::CONDITION_SATISFIED {
                self.retired_master_fences.pop_front();
                unsafe {
                    self.main_gl.delete_sync(oldest);
                }
            } else {
                break;
            }
        }

        let dt_secs = (frame_dt_ms / 1000.0) as f32;
        let blackout_on = self.bus.get_bool(param::MASTER_BLACKOUT);
        self.blackout_alpha = step_blackout_alpha(self.blackout_alpha, blackout_on, dt_secs);
        let xfade = self.bus.get(param::MIXER_CROSSFADER);
        let blend_idx = self
            .bus
            .get_discrete_index(param::MIXER_BLEND_MODE, BLEND_MODE_COUNT);
        let blend_mode = BlendMode::from_index(blend_idx);
        let strobe_flash = compute_strobe_flash(
            self.bus.get(param::MASTER_FX_STROBE),
            self.start_time.elapsed().as_secs_f64(),
            self.beat_pulse.bpm(),
        );
        let zoom_punch = if self.bus.get_bool(param::MASTER_FX_ZOOM_PUNCH) {
            self.beat_pulse.ui_flash()
        } else {
            0.0
        };
        let fx = MasterFxParams {
            hue_shift: self.bus.get(param::MASTER_FX_HUE),
            kaleidoscope: self.bus.get(param::MASTER_FX_KALEIDOSCOPE),
            invert: self.bus.get(param::MASTER_FX_INVERT),
            strobe_flash,
            freeze: self.bus.get_bool(param::MASTER_FX_FREEZE),
            zoom_punch,
        };
        let routing = FxRoutingState {
            fx1: FxUnitRoute {
                deck_a: self.bus.get_bool(param::MASTER_FX1_ROUTE_A),
                deck_b: self.bus.get_bool(param::MASTER_FX1_ROUTE_B),
                master: self.bus.get_bool(param::MASTER_FX1_ROUTE_MASTER),
            },
            fx2: FxUnitRoute {
                deck_a: self.bus.get_bool(param::MASTER_FX2_ROUTE_A),
                deck_b: self.bus.get_bool(param::MASTER_FX2_ROUTE_B),
                master: self.bus.get_bool(param::MASTER_FX2_ROUTE_MASTER),
            },
        };
        self.mixer.render(
            self.deck_a.output_texture(),
            self.deck_b.output_texture(),
            xfade,
            blend_mode,
            self.blackout_alpha,
            fx,
            routing,
        );
        // OpenGL 3.3 Core Spec (Section 5.3): if the Projector window is open, insert a fence
        // + glFlush() after writing `master_texture` so the Projector context can call GPU-only
        // `glWaitSync` before sampling it.
        if self.projector.is_some() {
            match unsafe {
                self.main_gl
                    .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
            } {
                Ok(fence) => {
                    unsafe {
                        self.main_gl.flush();
                    }
                    self.master_fence = Some(fence);
                }
                Err(e) => {
                    self.logger
                        .log_info(&format!("[ERROR] Master glFenceSync failed: {e}"));
                }
            }
        }
        self.logger
            .check_frame(&self.main_gl, self.frame_count, "main-mixer");

        // 5. Update rolling 0.5-second global FPS readout.
        let fps_elapsed = self.last_fps_sample_time.elapsed().as_secs_f64();
        if fps_elapsed >= 0.5 {
            let df = self.frame_count - self.last_fps_sample_frames;
            self.global_fps = df as f64 / fps_elapsed;
            self.last_fps_sample_time = Instant::now();
            self.last_fps_sample_frames = self.frame_count;
            let fps_i32 = self.global_fps.round().clamp(15.0, 240.0) as i32;
            self.deck_a.set_fps(fps_i32);
            self.deck_b.set_fps(fps_i32);
        }

        // 6. Clear main window and render the `egui 0.37` dark-venue console.
        let win_size = self.window.inner_size();
        unsafe {
            self.main_gl
                .viewport(0, 0, win_size.width as i32, win_size.height as i32);
            self.main_gl.clear_color(0.055, 0.063, 0.078, 1.0);
            self.main_gl.clear(glow::COLOR_BUFFER_BIT);
        }

        let deck_a_entry = &self.deck_a_current;
        let deck_b_entry = &self.deck_b_current;
        let deck_a_name = self
            .deck_a_override_name
            .as_deref()
            .unwrap_or(&deck_a_entry.display_name);
        let deck_a_path: &Path = if self.deck_a_override_name.is_some() {
            &self.bad_shader_path
        } else {
            &deck_a_entry.path
        };
        let deck_a_comp = if self.deck_a_override_name.is_some() {
            PresetComplexity {
                enabled_shape_instances: 0,
                tier: milkdrop_vj::preset_meta::ComplexityTier::Light,
            }
        } else {
            deck_a_entry.complexity
        };

        let deck_a_rel = if self.deck_a_override_name.is_some() {
            None
        } else {
            Some(deck_a_entry.rel_path.as_str())
        };

        let deck_res_label = format!("{}p", self.config.deck_height);
        let audio_source_label = match self.audio_mode {
            AudioMode::LiveWasapi => "Live WASAPI",
            AudioMode::SyntheticTest => "Test Audio",
        };

        let ui_state = ConsoleUiState {
            audio_is_live: self.audio_mode == AudioMode::LiveWasapi,
            audio_source_label,
            audio_device_name: &self.audio_device_name,
            audio_selected_device_id: self.selected_audio_device_id.as_deref(),
            audio_devices: &self.audio_devices,
            audio_devices_refreshing: self.audio_device_refresh.is_some(),
            audio_sample_rate: self.audio_sample_rate,
            audio_peak_db: self.audio_peak_db,
            audio_state: audio_badge,
            deck_res_label: &deck_res_label,
            deck_is_1080p: self.config.deck_height >= 1080,
            projector_active_label: self.projector.as_ref().map(|p| p.target_label()),
            projector_renderer: self.projector.as_ref().map(|p| p.renderer_name()),
            monitors: &self.monitors,
            gpu_label: &self.gpu_label,
            gpu_is_intel: self.gpu_is_intel,
            global_fps: self.global_fps,
            gl_errors: self.logger.total_errors(),
            blank_check_avg_us: self.blank_check_stats.avg_ms() * 1000.0,
            mappings_status: &self.mappings_status,
            last_load: self.last_load.as_ref(),
            master_texture_id: self.master_tex_id,
            blackout_alpha: self.blackout_alpha,
            beat_bpm: self.beat_pulse.bpm(),
            beat_flash: self.beat_pulse.ui_flash(),
            deck_a: DeckUiSnapshot {
                texture_id: self.deck_a_tex_id,
                preset_name: deck_a_name,
                preset_path: deck_a_path,
                preset_rel_path: deck_a_rel,
                complexity: deck_a_comp,
                render_ms_avg: self.deck_a_ms_ema,
                blank: self.deck_a.blank_status(),
            },
            deck_b: DeckUiSnapshot {
                texture_id: self.deck_b_tex_id,
                preset_name: &deck_b_entry.display_name,
                preset_path: &deck_b_entry.path,
                preset_rel_path: Some(deck_b_entry.rel_path.as_str()),
                complexity: deck_b_entry.complexity,
                render_ms_avg: self.deck_b_ms_ema,
                blank: self.deck_b.blank_status(),
            },
            all_presets: &self.presets,
            filtered_indices: &self.filtered_indices,
            folders: &self.folders,
            scan_status: &self.scan_status,
            playlists: &self.playlists,
            hot_banks: &self.hot_banks,
        };

        let bus_ref = &mut self.bus;
        let guard_ref = &mut self.load_guard;
        let interact_ref = &mut self.interact;
        let raw_input = self.egui_winit.take_egui_input(&self.window);
        let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            draw_console_ui(ui, &ui_state, bus_ref, guard_ref, interact_ref);
        });
        self.egui_winit
            .handle_platform_output(&self.window, full_output.platform_output);
        let clipped_primitives = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let dimensions: [u32; 2] = [win_size.width, win_size.height];
        self.egui_renderer.paint(
            dimensions,
            full_output.pixels_per_point,
            &clipped_primitives,
            &full_output.textures_delta,
        );

        self.logger
            .check_frame(&self.main_gl, self.frame_count, "main-egui");

        if let Err(e) = self.main_surface.swap_buffers(&self.main_context) {
            self.logger
                .log_info(&format!("[ERROR] swap_buffers failed: {e}"));
        }

        // 6b. If Projector Output is open, blit `master_texture` with aspect-preserving letterbox
        //     after GPU-only `glWaitSync(master_fence)` and swap with VSync `Wait(1)`.
        if let Some(ref proj) = self.projector {
            let src_aspect = self.config.deck_width as f32 / self.config.deck_height.max(1) as f32;
            if let Err(e) = proj.render_and_swap(
                self.mixer.master_texture(),
                self.master_fence,
                src_aspect,
                self.frame_count,
                &mut self.logger,
            ) {
                self.logger
                    .log_info(&format!("[PROJ-ERROR] Projector render failed: {e}"));
            }
        }

        // 7. Log preset-load frame (+0) and next two frames (+1, +2) for Risk 8 stall visibility.
        let total_frame_work_ms = frame_start.elapsed().as_secs_f64() * 1000.0;
        self.record_pending_stall_traces(
            frame_dt_ms,
            total_frame_work_ms,
            timing_a.cpu_submit_ms,
            timing_b.cpu_submit_ms,
        );

        // 8. Periodic 5-second health log line.
        if self.last_periodic_log.elapsed() >= Duration::from_secs(5) {
            self.last_periodic_log = Instant::now();
            let mem_str = current_process_memory_mb()
                .map(|m| format!("WS={:.1}MiB PC={:.1}MiB", m.working_set_mb, m.private_commit_mb))
                .unwrap_or_else(|| "mem=n/a".to_string());
            let proj_str = if self.projector.is_some() { "ON" } else { "OFF" };
            self.logger.log_info(&format!(
                "t={:5.1}s | FPS={:5.1} | DeckA={:4.2}ms | DeckB={:4.2}ms | Proj={} | BlankPBO={:5.1}us | GL_err={} | {}",
                self.start_time.elapsed().as_secs_f64(),
                self.global_fps,
                self.deck_a_ms_ema,
                self.deck_b_ms_ema,
                proj_str,
                self.blank_check_stats.avg_ms() * 1000.0,
                self.logger.total_errors(),
                mem_str
            ));
        }
    }

    /// Drains newly arrived WASAPI frames (non-blocking) or generates synthetic test audio.
    fn collect_audio_for_frame(&mut self) -> (usize, bool, AudioBadgeState) {
        match self.audio_mode {
            AudioMode::LiveWasapi => {
                if self.audio_switch_handle.is_some() {
                    return (0, false, AudioBadgeState::Switching);
                }
                if let Some(cap) = self.live_capture.as_mut() {
                    let pop = cap.pop_stereo(&mut self.live_pcm_buf, 4096);
                    if let Some(age_ms) = pop.oldest_sample_age_ms {
                        self.audio_age_stats.record_ms(age_ms);
                    }
                    let pkts = cap.packets_received();
                    if pkts > self.last_wasapi_packets {
                        self.last_wasapi_packets = pkts;
                        self.last_packet_time = Instant::now();
                    }
                    let mut newly_lost = false;
                    let badge = if cap.device_lost_events() > 0 {
                        if !self.device_lost_logged {
                            self.device_lost_logged = true;
                            newly_lost = true;
                            let err = cap.last_error().unwrap_or_else(|| "unknown".to_string());
                            self.logger
                                .log_info(&format!("[AUDIO] DEVICE LOST detected: {err}"));
                        }
                        AudioBadgeState::DeviceLost
                    } else if self.last_packet_time.elapsed() < Duration::from_millis(250) {
                        AudioBadgeState::Live
                    } else {
                        AudioBadgeState::Silence
                    };

                    if newly_lost {
                        self.trigger_device_list_refresh(true);
                    }

                    if pop.frames_read > 0 {
                        (pop.frames_read * 2, true, badge)
                    } else {
                        (0, false, badge)
                    }
                } else {
                    (0, false, AudioBadgeState::DeviceLost)
                }
            }
            AudioMode::SyntheticTest => {
                self.synth_gen.fill_stereo(&mut self.synth_pcm_buf, true);
                (self.synth_pcm_buf.len(), false, AudioBadgeState::Test)
            }
        }
    }

    fn record_pending_stall_traces(
        &mut self,
        frame_dt_ms: f64,
        frame_work_ms: f64,
        deck_a_ms: f64,
        deck_b_ms: f64,
    ) {
        if self.pending_stall_traces.is_empty() {
            return;
        }
        let mut keep = Vec::new();
        for mut trace in self.pending_stall_traces.drain(..) {
            let tag = match trace.step {
                0 => "+0 (LOAD)",
                1 => "+1       ",
                _ => "+2       ",
            };
            let mode_str = if trace.smooth { "smooth" } else { "hard" };
            let line = format!(
                "[LOAD-STALL] {tag} | {} -> `{}` ({mode_str}, load_call={:.2} ms) | frame_work={:.2} ms | prev_frame_dt={:.2} ms | DeckA_cpu={:.2} ms | DeckB_cpu={:.2} ms",
                trace.deck_label,
                trace.preset_name,
                trace.load_call_ms,
                frame_work_ms,
                frame_dt_ms,
                deck_a_ms,
                deck_b_ms
            );
            println!("{line}");
            self.logger.log_info(&line);
            trace.step += 1;
            if trace.step < 3 {
                keep.push(trace);
            }
        }
        self.pending_stall_traces = keep;
    }

    fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;

        let elapsed = self.start_time.elapsed().as_secs_f64();
        let avg_fps = if elapsed > 0.0 {
            self.frame_count as f64 / elapsed
        } else {
            0.0
        };
        let end_mem = current_process_memory_mb();

        self.logger.log_info("=== Stage 3D Shutdown Summary (provisional, laptop-limited) ===");
        self.logger.log_info(&format!(
            "Elapsed: {:.2}s | Frames: {} | Avg FPS: {:.2} | Total GL errors: {}",
            elapsed,
            self.frame_count,
            avg_fps,
            self.logger.total_errors()
        ));
        self.logger.log_info(&format!(
            "Deck A GL_TIME_ELAPSED: avg {:.2} ms (max {:.2} ms) | Deck B GL_TIME_ELAPSED: avg {:.2} ms (max {:.2} ms)",
            self.timing_a_gpu_q.avg_ms(),
            self.timing_a_gpu_q.max_ms(),
            self.timing_b_gpu_q.avg_ms(),
            self.timing_b_gpu_q.max_ms()
        ));
        self.logger.log_info(&format!(
            "Deck A CPU submit (incl. VSync wait): avg {:.2} ms (max {:.2} ms) | Deck B CPU submit: avg {:.2} ms (max {:.2} ms)",
            self.timing_a_cpu.avg_ms(),
            self.timing_a_cpu.max_ms(),
            self.timing_b_cpu.avg_ms(),
            self.timing_b_cpu.max_ms()
        ));
        self.logger.log_info(&format!(
            "Async 32x18 Blank-Deck PBO check ({} samples): avg {:.1} us ({:.4} ms) | max {:.1} us ({:.4} ms)",
            self.blank_check_stats.samples(),
            self.blank_check_stats.avg_ms() * 1000.0,
            self.blank_check_stats.avg_ms(),
            self.blank_check_stats.max_ms() * 1000.0,
            self.blank_check_stats.max_ms()
        ));
        if self.audio_age_stats.samples() > 0 {
            self.logger.log_info(&format!(
                "WASAPI internal audio_age (from packet arrival): avg {:.2} ms | max {:.2} ms",
                self.audio_age_stats.avg_ms(),
                self.audio_age_stats.max_ms()
            ));
        }
        if let (Some(s), Some(e)) = (self.start_memory, end_mem) {
            self.logger.log_info(&format!(
                "Memory: WorkingSet {:.1} -> {:.1} MiB | PrivateCommit {:.1} -> {:.1} MiB",
                s.working_set_mb, e.working_set_mb, s.private_commit_mb, e.private_commit_mb
            ));
        }

        // Clean GPU teardown: destroy Projector first, then main context fences/mixer/egui, then decks.
        if let Some(mut proj) = self.projector.take() {
            proj.destroy();
        }
        if self.main_context.make_current(&self.main_surface).is_ok() {
            if let Some(fence) = self.master_fence.take() {
                unsafe {
                    self.main_gl.delete_sync(fence);
                }
            }
            while let Some(fence) = self.retired_master_fences.pop_front() {
                unsafe {
                    self.main_gl.delete_sync(fence);
                }
            }
            self.mixer.destroy();
            self.egui_renderer.destroy();
        }
        self.deck_a.destroy();
        self.deck_b.destroy();
        self.live_capture.take();

        println!(
            "Stage 3D closed cleanly. Frames={}, Avg FPS={:.2}, Presets={}, BlankPBO avg={:.1} us, GL errors={}, log={}",
            self.frame_count,
            avg_fps,
            self.presets.len(),
            self.blank_check_stats.avg_ms() * 1000.0,
            self.logger.total_errors(),
            self.logger.path().display()
        );
    }
}

struct App {
    config: CliConfig,
    state: Option<ConsoleState>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        match ConsoleState::initialize(event_loop, self.config.clone()) {
            Ok(state) => {
                self.state = Some(state);
            }
            Err(err) => {
                eprintln!("[FATAL] Stage 2A-1 initialization failed: {err}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        // Handle Projector Output window events separately so closing the Projector never quits
        // the Control Console, while live performance hotkeys (like Backspace blackout) still work!
        let is_projector_window = state
            .projector
            .as_ref()
            .map(|p| p.window_id() == window_id)
            .unwrap_or(false);
        if is_projector_window {
            match event {
                WindowEvent::ModifiersChanged(new_mods) => {
                    state.modifiers = new_mods.state();
                }
                WindowEvent::KeyboardInput {
                    event: key_event, ..
                } => {
                    state.handle_keyboard_event(&key_event);
                }
                WindowEvent::Resized(new_size) => {
                    if let Some(ref proj) = state.projector {
                        proj.resize(new_size.width, new_size.height);
                    }
                }
                WindowEvent::Focused(false) => {
                    state.release_held_momentary_keys();
                }
                WindowEvent::CloseRequested => {
                    state.close_projector();
                }
                _ => {}
            }
            return;
        }

        if window_id != state.window.id() {
            return;
        }

        if let WindowEvent::ModifiersChanged(new_mods) = &event {
            state.modifiers = new_mods.state();
        }

        // Route keyboard events through our Section 13 Focus Model & Control Bus first so
        // `/` doesn't type a slash into Search and performance shortcuts never hijack Typing Mode.
        let consumed_by_router = match &event {
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => state.handle_keyboard_event(key_event),
            _ => false,
        };

        if !consumed_by_router {
            let _ = state.egui_winit.on_window_event(&state.window, &event);
        }

        match event {
            WindowEvent::Focused(false) => {
                state.release_held_momentary_keys();
            }
            WindowEvent::CloseRequested => {
                state.shutdown();
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                if let (Some(w), Some(h)) = (
                    NonZeroU32::new(new_size.width),
                    NonZeroU32::new(new_size.height),
                ) {
                    if state
                        .main_context
                        .make_current(&state.main_surface)
                        .is_ok()
                    {
                        state.main_surface.resize(&state.main_context, w, h);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                state.apply_pending_projector_command(event_loop);
                state.render_frame();
                if let Some(limit_s) = state.config.auto_exit_secs {
                    if state.start_time.elapsed().as_secs_f64() >= limit_s {
                        state.shutdown();
                        event_loop.exit();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_ref() {
            if !state.shut_down {
                state.window.request_redraw();
            }
        }
    }
}

fn resolve_device_filter(filter: Option<&str>) -> Result<Option<String>, String> {
    let Some(f) = filter else {
        return Ok(None);
    };
    let src = WasapiLoopbackSource::new()?;
    let devices = src.list_devices()?;
    if let Ok(idx) = f.parse::<usize>() {
        if idx >= 1 && idx <= devices.len() {
            return Ok(Some(devices[idx - 1].id.clone()));
        }
    }
    let needle = f.to_lowercase();
    for dev in devices {
        if dev.name.to_lowercase().contains(&needle) {
            return Ok(Some(dev.id));
        }
    }
    Err(format!("No active Windows output device matched `--audio-device {f}`"))
}

fn keycode_to_mapping_name(code: KeyCode, logical_key: &Key) -> Option<String> {
    let s = match code {
        KeyCode::KeyA => "A",
        KeyCode::KeyB => "B",
        KeyCode::KeyC => "C",
        KeyCode::KeyD => "D",
        KeyCode::KeyE => "E",
        KeyCode::KeyF => "F",
        KeyCode::KeyG => "G",
        KeyCode::KeyH => "H",
        KeyCode::KeyI => "I",
        KeyCode::KeyJ => "J",
        KeyCode::KeyK => "K",
        KeyCode::KeyL => "L",
        KeyCode::KeyM => "M",
        KeyCode::KeyN => "N",
        KeyCode::KeyO => "O",
        KeyCode::KeyP => "P",
        KeyCode::KeyQ => "Q",
        KeyCode::KeyR => "R",
        KeyCode::KeyS => "S",
        KeyCode::KeyT => "T",
        KeyCode::KeyU => "U",
        KeyCode::KeyV => "V",
        KeyCode::KeyW => "W",
        KeyCode::KeyX => "X",
        KeyCode::KeyY => "Y",
        KeyCode::KeyZ => "Z",
        KeyCode::Digit0 => "0",
        KeyCode::Digit1 => "1",
        KeyCode::Digit2 => "2",
        KeyCode::Digit3 => "3",
        KeyCode::Digit4 => "4",
        KeyCode::Digit5 => "5",
        KeyCode::Digit6 => "6",
        KeyCode::Digit7 => "7",
        KeyCode::Digit8 => "8",
        KeyCode::Digit9 => "9",
        KeyCode::Numpad0 => "Numpad0",
        KeyCode::Numpad1 => "Numpad1",
        KeyCode::Numpad2 => "Numpad2",
        KeyCode::Numpad3 => "Numpad3",
        KeyCode::Numpad4 => "Numpad4",
        KeyCode::Numpad5 => "Numpad5",
        KeyCode::Numpad6 => "Numpad6",
        KeyCode::Numpad7 => "Numpad7",
        KeyCode::Numpad8 => "Numpad8",
        KeyCode::Numpad9 => "Numpad9",
        KeyCode::F1 => "F1",
        KeyCode::F2 => "F2",
        KeyCode::F3 => "F3",
        KeyCode::F4 => "F4",
        KeyCode::F5 => "F5",
        KeyCode::F6 => "F6",
        KeyCode::F7 => "F7",
        KeyCode::F8 => "F8",
        KeyCode::F9 => "F9",
        KeyCode::F10 => "F10",
        KeyCode::F11 => "F11",
        KeyCode::F12 => "F12",
        KeyCode::BracketLeft => "BracketLeft",
        KeyCode::BracketRight => "BracketRight",
        // UK / European ISO keyboards place `\` between Left Shift and `Z` (`IntlBackslash`),
        // while the physical key next to Enter (`Backslash` on US ANSI) produces `#` / `~`.
        KeyCode::IntlBackslash => "Backslash",
        KeyCode::Backslash => {
            if matches!(logical_key, Key::Character(c) if c.as_str() == "#" || c.as_str() == "~") {
                "Hash"
            } else {
                "Backslash"
            }
        }
        KeyCode::Backquote => "Backquote",
        KeyCode::ArrowLeft => "ArrowLeft",
        KeyCode::ArrowRight => "ArrowRight",
        KeyCode::ArrowUp => "ArrowUp",
        KeyCode::ArrowDown => "ArrowDown",
        KeyCode::Backspace => "Backspace",
        KeyCode::Delete => "Delete",
        KeyCode::Insert => "Insert",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::Enter | KeyCode::NumpadEnter => "Enter",
        KeyCode::Space => "Space",
        KeyCode::Tab => "Tab",
        KeyCode::Slash => "Slash",
        KeyCode::Minus => "Minus",
        KeyCode::Equal => "Equal",
        KeyCode::Comma => "Comma",
        KeyCode::Period => "Period",
        KeyCode::Semicolon => "Semicolon",
        KeyCode::Quote => "Quote",
        _ => return None,
    };
    Some(s.to_string())
}

fn shorten_gpu_name(renderer: &str) -> String {
    let trimmed = renderer.split('/').next().unwrap_or(renderer).trim();
    trimmed
        .replace("NVIDIA ", "")
        .replace("Quadro ", "")
}

fn parse_cli_args() -> Result<CliConfig, String> {
    let mut cfg = CliConfig {
        audio_mode: AudioMode::LiveWasapi,
        audio_device_filter: None,
        // Light-Load Rules: default to 1280x720 per deck during development.
        deck_width: 1280,
        deck_height: 720,
        measure_gpu: false,
        auto_exit_secs: None,
        test_res_toggles: 0,
        test_projector_windowed: false,
        log_path: PathBuf::from("logs").join("stage_3d.log"),
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--audio-source" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--audio-source requires `live` or `test`".to_string())?;
                cfg.audio_mode = match val.to_lowercase().as_str() {
                    "live" => AudioMode::LiveWasapi,
                    "test" => AudioMode::SyntheticTest,
                    other => {
                        return Err(format!(
                            "Invalid --audio-source `{other}` (expected `live` or `test`)"
                        ));
                    }
                };
            }
            "--audio-device" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--audio-device requires an index or name".to_string())?;
                cfg.audio_device_filter = Some(val);
            }
            "--deck-size" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--deck-size requires WxH (e.g. 1280x720)".to_string())?;
                let (w_str, h_str) = val
                    .split_once('x')
                    .or_else(|| val.split_once('X'))
                    .ok_or_else(|| format!("Invalid --deck-size `{val}` (expected WxH)"))?;
                cfg.deck_width = w_str
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid width `{w_str}`"))?;
                cfg.deck_height = h_str
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid height `{h_str}`"))?;
            }
            "--measure-gpu" => cfg.measure_gpu = true,
            "--auto-exit-secs" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--auto-exit-secs requires a number of seconds".to_string())?;
                cfg.auto_exit_secs = Some(
                    val.parse::<f64>()
                        .map_err(|_| format!("Invalid --auto-exit-secs `{val}`"))?,
                );
            }
            "--test-res-toggles" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--test-res-toggles requires a count (e.g. 5)".to_string())?;
                cfg.test_res_toggles = val
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid --test-res-toggles `{val}`"))?;
            }
            "--test-projector-windowed" => cfg.test_projector_windowed = true,
            "--log" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--log requires a file path".to_string())?;
                cfg.log_path = PathBuf::from(val);
            }
            other => {
                return Err(format!("Unknown CLI argument `{other}`"));
            }
        }
    }
    Ok(cfg)
}

fn main() {
    let config = match parse_cli_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ERROR] {e}");
            std::process::exit(1);
        }
    };

    let event_loop = EventLoop::new().expect("Failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        config,
        state: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[ERROR] Event loop exited with error: {e}");
        std::process::exit(1);
    }
}
