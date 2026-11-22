//! Stage 2 of the Two-Instance Spike: One `libprojectM` Instance at 60 FPS.
//!
//! What this program does:
//! 1. Opens a 1280x720 OpenGL 3.3 Core window on the NVIDIA Quadro RTX 3000.
//! 2. Creates one safe `Deck` wrapper owning a `libprojectM 4.1.8` instance.
//! 3. Loads `.milk` presets from `C:\dev\vj\presets\` (official projectM packs).
//! 4. Feeds generated test audio (sine sweep + 120 BPM bass pulse + subtle noise).
//! 5. Renders live visuals at 60 FPS (VSync) and checks `glGetError()` every frame.
//!
//! Controls while watching:
//! - `Right Arrow` or `N`: Load next preset (smooth transition)
//! - `Left Arrow` or `P`:  Load previous preset (smooth transition)
//! - `Space`:              Toggle generated test audio vs. silence
//! - `Esc` or `X`:         Close cleanly

mod deck;
mod gl_log;
mod test_audio;

use std::ffi::CStr;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Instant;

use glow::HasContext;
use glutin::config::ConfigTemplateBuilder;
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

use crate::deck::{
    init_gl_loader_for_projectm, max_pcm_samples_per_channel, projectm_version_string, Deck,
};
use crate::gl_log::GlErrorLogger;
use crate::test_audio::TestAudioGenerator;

/// Holds the active window, OpenGL context, and our single `Deck` instance.
///
/// Rust drop-order note: Rust drops struct fields from TOP to BOTTOM.
/// Declaring `deck` BEFORE `gl_context` and `gl_surface` guarantees that
/// `Deck::drop()` (which calls `projectm_destroy()`) runs while the OpenGL
/// context is still alive and current.
struct Stage2State {
    deck: Deck,
    window: Window,
    gl_surface: Surface<WindowSurface>,
    gl_context: PossiblyCurrentContext,
    gl: glow::Context,
    audio_gen: TestAudioGenerator,
    pcm_buffer: Vec<f32>,
    audio_enabled: bool,
    presets: Vec<PathBuf>,
    current_preset_idx: usize,
    start_time: Instant,
    last_report_time: Instant,
    last_report_frames: u64,
    frame_count: u64,
}

struct Stage2App {
    state: Option<Stage2State>,
    logger: GlErrorLogger,
    presets: Vec<PathBuf>,
}

impl Stage2App {
    fn new(logger: GlErrorLogger, presets: Vec<PathBuf>) -> Self {
        Self {
            state: None,
            logger,
            presets,
        }
    }

    fn init_window_and_deck(&mut self, event_loop: &ActiveEventLoop) {
        let window_attrs = Window::default_attributes()
            .with_title("MilkDrop VJ — Stage 2 Single Deck (Left/Right: preset, Space: audio, Esc: close)")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));

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

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(Some(raw_window_handle));

        let not_current_context = unsafe {
            gl_display
                .create_context(&gl_config, &context_attributes)
                .expect("Failed to create OpenGL 3.3 Core context")
        };

        let surface_attrs = window
            .build_surface_attributes(Default::default())
            .expect("Failed to build window surface attributes");

        let gl_surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .expect("Failed to create OpenGL window surface")
        };

        let gl_context = not_current_context
            .make_current(&gl_surface)
            .expect("Failed to make OpenGL context current");

        if let Err(err) =
            gl_surface.set_swap_interval(&gl_context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()))
        {
            eprintln!("Warning: Could not enable VSync: {err}");
            self.logger.log_info(&format!("Warning: VSync failed: {err}"));
        }

        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|symbol: &CStr| {
                gl_display.get_proc_address(symbol) as *const _
            })
        };

        let vendor = unsafe { gl.get_parameter_string(glow::VENDOR) };
        let renderer = unsafe { gl.get_parameter_string(glow::RENDERER) };
        let version = unsafe { gl.get_parameter_string(glow::VERSION) };

        println!("=== Stage 2 OpenGL Context Ready ===");
        println!("GL_VENDOR:   {vendor}");
        println!("GL_RENDERER: {renderer}");
        println!("GL_VERSION:  {version}");
        self.logger.log_info(&format!("GL_VENDOR={vendor}"));
        self.logger.log_info(&format!("GL_RENDERER={renderer}"));
        self.logger.log_info(&format!("GL_VERSION={version}"));

        window.set_visible(true);
        window.focus_window();

        let size = window.inner_size();
        let width = size.width.max(1) as usize;
        let height = size.height.max(1) as usize;

        // Initialize GLEW on Windows before creating the first libprojectM instance.
        init_gl_loader_for_projectm().expect("Failed to initialize GLEW for libprojectM");
        println!("GLEW initialized for libprojectM.");
        self.logger.log_info("GLEW initialized for libprojectM.");

        // Clear any benign GL_INVALID_ENUM that legacy GLEW probes may leave on Core profiles,
        // and log if one occurred so nothing is hidden.
        let glew_probe_errs = self.logger.check_frame(&gl, 0, "post-glew-init");
        if glew_probe_errs > 0 {
            self.logger.log_info(&format!(
                "Note: drained {glew_probe_errs} GL flag(s) left by glewInit() probe on Core profile."
            ));
        }

        // Create our safe Deck wrapper on the active OpenGL context.
        let deck = Deck::new(width, height).expect("Failed to create libprojectM Deck");

        // Load the first preset from C:\dev\vj\presets\.
        let first_preset = &self.presets[0];
        match deck.load_preset(first_preset, false) {
            Ok(()) => {
                let name = preset_label(first_preset);
                println!("Loaded preset [1/{}]: {name}", self.presets.len());
                self.logger.log_info(&format!("Loaded initial preset: {name}"));
            }
            Err(err) => {
                eprintln!("Initial preset load warning: {err}");
                self.logger.log_info(&format!("Initial preset load warning: {err}"));
            }
        }

        // Check GL errors right after deck init and preset load.
        self.logger.check_frame(&gl, 0, "stage2-init");

        // Allocate a stereo PCM buffer sized to libprojectM's max sample count per frame.
        let max_samples = max_pcm_samples_per_channel();
        let pcm_buffer = vec![0.0_f32; max_samples * 2];

        let now = Instant::now();
        self.state = Some(Stage2State {
            deck,
            window,
            gl_surface,
            gl_context,
            gl,
            audio_gen: TestAudioGenerator::new(48_000.0),
            pcm_buffer,
            audio_enabled: true,
            presets: self.presets.clone(),
            current_preset_idx: 0,
            start_time: now,
            last_report_time: now,
            last_report_frames: 0,
            frame_count: 0,
        });
    }

    fn switch_preset(&mut self, delta: isize) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let count = state.presets.len() as isize;
        if count == 0 {
            return;
        }
        let next_idx = ((state.current_preset_idx as isize + delta).rem_euclid(count)) as usize;
        state.current_preset_idx = next_idx;

        let path = &state.presets[next_idx];
        let label = preset_label(path);
        match state.deck.load_preset(path, true) {
            Ok(()) => {
                println!("Switched to preset [{}/{}]: {label}", next_idx + 1, count);
                self.logger.log_info(&format!("Switched preset: {label}"));
            }
            Err(err) => {
                eprintln!("Preset load error: {err}");
                self.logger.log_info(&format!("Preset load error: {err}"));
            }
        }
    }

    fn render_frame(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        state.frame_count += 1;

        // 1. Generate and feed stereo test audio (or silence if toggled off).
        state
            .audio_gen
            .fill_stereo(&mut state.pcm_buffer, state.audio_enabled);
        state.deck.add_pcm_stereo(&state.pcm_buffer);

        // 2. Render one MilkDrop frame via libprojectM.
        state.deck.render_frame();

        // 3. Check for OpenGL errors on every frame.
        self.logger
            .check_frame(&state.gl, state.frame_count, "stage2-frame");

        // 4. Present the frame to the window (VSync-paced).
        state
            .gl_surface
            .swap_buffers(&state.gl_context)
            .expect("Failed to swap OpenGL buffers");

        // 5. Print a 1-second FPS heartbeat.
        let since_report = state.last_report_time.elapsed().as_secs_f64();
        if since_report >= 1.0 {
            let delta_frames = state.frame_count - state.last_report_frames;
            let fps = delta_frames as f64 / since_report;
            state.deck.set_fps(fps.round() as i32);
            let mode = if state.audio_enabled { "TEST_AUDIO" } else { "SILENCE" };
            println!(
                "[Stage 2] frames={}, fps={:.1}, audio={}, preset=[{}/{}], gl_errors={}",
                state.frame_count,
                fps,
                mode,
                state.current_preset_idx + 1,
                state.presets.len(),
                self.logger.total_errors()
            );
            state.last_report_time = Instant::now();
            state.last_report_frames = state.frame_count;
        }

        state.window.request_redraw();
    }

    fn shutdown(&mut self) {
        if let Some(state) = self.state.take() {
            let secs = state.start_time.elapsed().as_secs_f64();
            let avg_fps = if secs > 0.0 {
                state.frame_count as f64 / secs
            } else {
                0.0
            };
            let summary = format!(
                "Stage 2 closed cleanly after {} frames in {:.2}s (avg {:.1} FPS). Total GL errors: {}",
                state.frame_count,
                secs,
                avg_fps,
                self.logger.total_errors()
            );
            println!("{summary}");
            println!("GL error log saved to: {}", self.logger.path().display());
            self.logger.log_info(&summary);
            // `state` drops here: `Deck::drop()` calls `projectm_destroy()` first,
            // while `gl_context` is still alive and current.
            drop(state);
        }
    }
}

impl ApplicationHandler for Stage2App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            self.init_window_and_deck(event_loop);
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
                Key::Named(NamedKey::ArrowRight) => self.switch_preset(1),
                Key::Named(NamedKey::ArrowLeft) => self.switch_preset(-1),
                Key::Named(NamedKey::Space) => {
                    if let Some(state) = self.state.as_mut() {
                        state.audio_enabled = !state.audio_enabled;
                        let mode = if state.audio_enabled { "TEST_AUDIO" } else { "SILENCE" };
                        println!("Audio toggled -> {mode}");
                        self.logger.log_info(&format!("Audio toggled -> {mode}"));
                    }
                }
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("n") => self.switch_preset(1),
                Key::Character(ref ch) if ch.eq_ignore_ascii_case("p") => self.switch_preset(-1),
                _ => {}
            },
            WindowEvent::Resized(new_size) => {
                if let Some(state) = &self.state {
                    if let (Some(w), Some(h)) = (
                        NonZeroU32::new(new_size.width),
                        NonZeroU32::new(new_size.height),
                    ) {
                        state.gl_surface.resize(&state.gl_context, w, h);
                        state
                            .deck
                            .resize(new_size.width as usize, new_size.height as usize);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.render_frame();
            }
            _ => {}
        }
    }
}

/// Finds all `.milk` preset files in `dir`, sorted alphabetically.
fn discover_presets(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Could not read preset folder `{}`: {e}", dir.display()))?;

    let mut presets = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Error reading preset directory: {e}"))?;
        let path = entry.path();
        let is_milk = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("milk"))
            .unwrap_or(false);
        if is_milk {
            presets.push(path);
        }
    }
    presets.sort();

    if presets.is_empty() {
        return Err(format!("No .milk presets found in `{}`", dir.display()));
    }
    Ok(presets)
}

fn preset_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

fn main() {
    let pm_version = projectm_version_string();
    println!("libprojectM runtime version: {pm_version}");

    let presets = discover_presets(Path::new("presets"))
        .expect("Failed to load presets from C:\\dev\\vj\\presets\\");
    println!("Found {} .milk presets in presets/.", presets.len());
    println!("Controls: Left/Right (or P/N) = switch preset | Space = toggle test audio/silence | Esc = close");

    let logger = GlErrorLogger::open("logs/gl_errors.log")
        .expect("Could not initialize OpenGL error log file");

    let event_loop = EventLoop::new().expect("Failed to create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = Stage2App::new(logger, presets);
    event_loop
        .run_app(&mut app)
        .expect("Event loop exited with an error");
}
