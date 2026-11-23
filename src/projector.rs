//! Milestone 2 — Stage `2B`: Borderless Fullscreen Projector Output Window.
//!
//! Architecture:
//! - Owns a second `winit` window (`Window`), its own OpenGL 3.3 Core `PossiblyCurrentContext`
//!   created with `.with_sharing(&main_context)` so it directly samples `GpuMixer::master_texture()`,
//!   and a zero-VBO fullscreen-triangle aspect-preserving blit shader.
//! - **Cross-context sync (OpenGL 3.3 Core Spec, Section 5.3):**
//!   Main context inserts `glFenceSync` + `glFlush` after `GpuMixer::render()`; this window calls
//!   GPU-only `glWaitSync` on its own context before sampling `master_texture` (zero CPU stall).
//! - **Single-VSync swap-interval rule:**
//!   While `ProjectorWindow` is open, `ProjectorWindow` uses `SwapInterval::Wait(1)` (tear-free
//!   audience output) and the Control Console switches to `SwapInterval::DontWait` so the
//!   single-threaded loop never blocks on two consecutive VSyncs (preventing 30 FPS halving).
//! - **Aspect-ratio letterboxing:**
//!   If the external projector/monitor is not `16:9` (e.g., `16:10` or `4:3`), black bars are
//!   drawn cleanly around `master_texture` so visuals are never stretched.

use std::ffi::CStr;
use std::num::NonZeroU32;

use glow::HasContext;
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{
    ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version,
};
use glutin::display::Display;
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};
use glutin_winit::GlWindow;
use raw_window_handle::HasWindowHandle;
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowId};

use crate::gl_log::GlErrorLogger;

/// Metadata describing one connected Windows monitor/display for the Top Bar selector.
#[derive(Debug, Clone)]
pub struct DisplayMonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub pos_x: i32,
    pub pos_y: i32,
    pub scale_factor: f64,
    pub is_primary: bool,
}

impl DisplayMonitorInfo {
    pub fn short_label(&self) -> String {
        let role = if self.is_primary { "Primary" } else { "External" };
        format!(
            "Display {} ({role}, {}x{})",
            self.index + 1,
            self.width,
            self.height
        )
    }
}

/// Target mode for opening the Projector Output window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectorTarget {
    /// Borderless fullscreen on monitor `index` from `available_monitors()`.
    Monitor(usize),
    /// Resizable `1280x720` preview window (for testing on a single-screen laptop).
    WindowedPreview,
}

/// Enumerates all connected Windows displays via `winit`.
pub fn enumerate_monitors(event_loop: &ActiveEventLoop) -> Vec<(DisplayMonitorInfo, MonitorHandle)> {
    let primary_pos = event_loop.primary_monitor().map(|m| m.position());
    let mut out = Vec::new();
    for (index, handle) in event_loop.available_monitors().enumerate() {
        let pos = handle.position();
        let size = handle.size();
        let name = handle
            .name()
            .unwrap_or_else(|| format!("Monitor {}", index + 1));
        let is_primary = primary_pos.map(|p| p == pos).unwrap_or(index == 0);
        out.push((
            DisplayMonitorInfo {
                index,
                name,
                width: size.width,
                height: size.height,
                pos_x: pos.x,
                pos_y: pos.y,
                scale_factor: handle.scale_factor(),
                is_primary,
            },
            handle,
        ));
    }
    out
}

/// Computes an aspect-preserving destination viewport `(x, y, w, h)` inside a `(win_w, win_h)`
/// output surface for a source texture of aspect ratio `src_aspect` (`width / height`).
pub fn compute_letterbox_viewport(win_w: u32, win_h: u32, src_aspect: f32) -> (i32, i32, i32, i32) {
    if win_w == 0 || win_h == 0 || src_aspect <= 0.0 {
        return (0, 0, win_w as i32, win_h as i32);
    }
    let dst_aspect = win_w as f32 / win_h as f32;
    if (dst_aspect - src_aspect).abs() < 0.002 {
        (0, 0, win_w as i32, win_h as i32)
    } else if dst_aspect > src_aspect {
        // Output is wider than source -> pillarbox (black bars left & right).
        let vp_h = win_h as i32;
        let vp_w = ((win_h as f32 * src_aspect).round() as i32).clamp(1, win_w as i32);
        let vp_x = (win_w as i32 - vp_w) / 2;
        (vp_x, 0, vp_w, vp_h)
    } else {
        // Output is taller than source -> letterbox (black bars top & bottom).
        let vp_w = win_w as i32;
        let vp_h = ((win_w as f32 / src_aspect).round() as i32).clamp(1, win_h as i32);
        let vp_y = (win_h as i32 - vp_h) / 2;
        (0, vp_y, vp_w, vp_h)
    }
}

const PROJ_VERT_SRC: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    vec2 pos = vec2(
        (gl_VertexID == 1) ? 3.0 : -1.0,
        (gl_VertexID == 2) ? 3.0 : -1.0
    );
    v_uv = 0.5 * (pos + 1.0);
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#;

const PROJ_FRAG_SRC: &str = r#"#version 330 core
in vec2 v_uv;
out vec4 f_color;
uniform sampler2D u_master;
void main() {
    f_color = vec4(texture(u_master, v_uv).rgb, 1.0);
}
"#;

/// Dedicated borderless fullscreen (or windowed preview) Projector Output window.
pub struct ProjectorWindow {
    window: Window,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    gl: glow::Context,
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    u_master: Option<glow::NativeUniformLocation>,
    target_label: String,
    renderer_name: String,
    destroyed: bool,
}

impl ProjectorWindow {
    /// Opens a Projector Output window sharing OpenGL textures with `main_context`,
    /// enables `SwapInterval::Wait(1)` on the Projector surface, and logs its GPU renderer.
    ///
    /// Note: Leaves the Projector context current; caller should re-bind `main_context` afterward.
    pub fn open(
        event_loop: &ActiveEventLoop,
        gl_display: &Display,
        main_context: &PossiblyCurrentContext,
        target: ProjectorTarget,
        logger: &mut GlErrorLogger,
    ) -> Result<Self, String> {
        let monitors = enumerate_monitors(event_loop);

        let (window_attrs, target_label, hide_cursor) = match target {
            ProjectorTarget::Monitor(idx) => {
                let (info, handle) = monitors
                    .into_iter()
                    .nth(idx)
                    .ok_or_else(|| format!("Monitor index {idx} is no longer connected"))?;
                let label = info.short_label();
                logger.log_info(&format!(
                    "[PROJ] Opening borderless fullscreen on `{}` (`{}`): pos=({}, {}) size={}x{} scale={:.2}",
                    label, info.name, info.pos_x, info.pos_y, info.width, info.height, info.scale_factor
                ));
                let attrs = Window::default_attributes()
                    .with_title(format!("MilkDrop VJ — Projector Output ({label})"))
                    .with_decorations(false)
                    .with_fullscreen(Some(Fullscreen::Borderless(Some(handle))));
                (attrs, label, true)
            }
            ProjectorTarget::WindowedPreview => {
                let label = "Windowed Preview (1280x720)".to_string();
                logger.log_info("[PROJ] Opening windowed Projector Output preview (1280x720)");
                let attrs = Window::default_attributes()
                    .with_title("MilkDrop VJ — Projector Output (Windowed Preview)")
                    .with_inner_size(LogicalSize::new(1280.0, 720.0));
                (attrs, label, false)
            }
        };

        let window = event_loop
            .create_window(window_attrs)
            .map_err(|e| format!("Failed to create Projector window: {e}"))?;
        if hide_cursor {
            window.set_cursor_visible(false);
        }

        let raw_handle = window
            .window_handle()
            .map_err(|e| format!("Failed to get Projector raw window handle: {e}"))?
            .as_raw();

        let template = ConfigTemplateBuilder::new()
            .compatible_with_native_window(raw_handle)
            .with_alpha_size(8)
            .with_depth_size(24)
            .build();
        let gl_config = unsafe {
            gl_display
                .find_configs(template)
                .map_err(|e| format!("Failed to find GL config for Projector window: {e}"))?
                .reduce(|best, cand| {
                    if cand.num_samples() < best.num_samples() {
                        cand
                    } else {
                        best
                    }
                })
                .ok_or_else(|| "No OpenGL config compatible with Projector window".to_string())?
        };

        let ctx_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .with_sharing(main_context)
            .build(Some(raw_handle));
        let not_current = unsafe {
            gl_display
                .create_context(&gl_config, &ctx_attrs)
                .map_err(|e| format!("Failed to create shared GL context for Projector: {e}"))?
        };

        let surface_attrs = window
            .build_surface_attributes(Default::default())
            .map_err(|e| format!("Failed to build Projector surface attributes: {e}"))?;
        let surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .map_err(|e| format!("Failed to create Projector window surface: {e}"))?
        };
        let context = not_current
            .make_current(&surface)
            .map_err(|e| format!("Failed to make Projector context current: {e}"))?;

        // Single-VSync rule: audience Projector owns VSync `Wait(1)` for tear-free output!
        let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()));

        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|symbol: &CStr| {
                gl_display.get_proc_address(symbol) as *const _
            })
        };

        let renderer_name = unsafe { gl.get_parameter_string(glow::RENDERER) };
        let phys = window.inner_size();
        logger.log_info(&format!(
            "[PROJ] Projector GL_RENDERER: {renderer_name} | surface={}x{} px | VSync=Wait(1)",
            phys.width, phys.height
        ));

        let (program, vao, u_master) = unsafe {
            let vert = compile_shader(&gl, glow::VERTEX_SHADER, PROJ_VERT_SRC)?;
            let frag = compile_shader(&gl, glow::FRAGMENT_SHADER, PROJ_FRAG_SRC)?;
            let prog = gl
                .create_program()
                .map_err(|e| format!("Failed to create Projector shader program: {e}"))?;
            gl.attach_shader(prog, vert);
            gl.attach_shader(prog, frag);
            gl.link_program(prog);
            let linked = gl.get_program_link_status(prog);
            let log = gl.get_program_info_log(prog);
            gl.detach_shader(prog, vert);
            gl.detach_shader(prog, frag);
            gl.delete_shader(vert);
            gl.delete_shader(frag);
            if !linked {
                gl.delete_program(prog);
                return Err(format!("Projector shader link failed: {log}"));
            }
            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("Failed to create Projector VAO: {e}"))?;
            let u_master = gl.get_uniform_location(prog, "u_master");
            (prog, vao, u_master)
        };

        logger.check_frame(&gl, 0, "projector-init");

        Ok(Self {
            window,
            surface,
            context,
            gl,
            program,
            vao,
            u_master,
            target_label,
            renderer_name,
            destroyed: false,
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn target_label(&self) -> &str {
        &self.target_label
    }

    pub fn renderer_name(&self) -> &str {
        &self.renderer_name
    }

    /// Resizes the Projector GL surface when the OS reports a window size change.
    pub fn resize(&self, width: u32, height: u32) {
        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            if self.context.make_current(&self.surface).is_ok() {
                self.surface.resize(&self.context, w, h);
            }
        }
    }

    /// Draws `master_texture` fullscreen with clean aspect-ratio letterboxing, waiting on
    /// `master_fence` (GPU-only `glWaitSync`) before sampling, then swaps buffers (VSync `Wait(1)`).
    pub fn render_and_swap(
        &self,
        master_texture: glow::NativeTexture,
        master_fence: Option<glow::NativeFence>,
        src_aspect: f32,
        frame_index: u64,
        logger: &mut GlErrorLogger,
    ) -> Result<(), String> {
        self.context
            .make_current(&self.surface)
            .map_err(|e| format!("Projector make_current failed: {e}"))?;

        if let Some(fence) = master_fence {
            unsafe {
                self.gl.wait_sync(fence, 0, glow::TIMEOUT_IGNORED);
            }
        }

        let size = self.window.inner_size();
        let (vp_x, vp_y, vp_w, vp_h) = compute_letterbox_viewport(size.width, size.height, src_aspect);

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.disable(glow::BLEND);

            // Clear entire display to pure black so letterbox/pillarbox bars are seamless.
            self.gl.viewport(0, 0, size.width as i32, size.height as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            self.gl.viewport(vp_x, vp_y, vp_w, vp_h);
            self.gl.use_program(Some(self.program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(master_texture));
            self.gl.uniform_1_i32(self.u_master.as_ref(), 0);

            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);

            self.gl.bind_vertex_array(None);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.use_program(None);
        }

        logger.check_frame(&self.gl, frame_index, "projector-blit");

        self.surface
            .swap_buffers(&self.context)
            .map_err(|e| format!("Projector swap_buffers failed: {e}"))?;
        Ok(())
    }

    /// Destroys GPU objects owned by the Projector context while it is current.
    pub fn destroy(&mut self) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        if self.context.make_current(&self.surface).is_ok() {
            unsafe {
                self.gl.delete_vertex_array(self.vao);
                self.gl.delete_program(self.program);
            }
        }
    }
}

impl Drop for ProjectorWindow {
    fn drop(&mut self) {
        self.destroy();
    }
}

unsafe fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    src: &str,
) -> Result<glow::NativeShader, String> {
    let shader = gl
        .create_shader(shader_type)
        .map_err(|e| format!("Failed to create Projector shader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("Projector shader compile failed: {log}"));
    }
    Ok(shader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_preserves_16_by_9_aspect_on_all_monitor_shapes() {
        let aspect_16_9 = 16.0 / 9.0;
        // Exact 16:9 -> full viewport.
        assert_eq!(
            compute_letterbox_viewport(1920, 1080, aspect_16_9),
            (0, 0, 1920, 1080)
        );
        // 4:3 projector (1024x768) -> letterbox top & bottom bars.
        assert_eq!(
            compute_letterbox_viewport(1024, 768, aspect_16_9),
            (0, 96, 1024, 576)
        );
        // Ultrawide 21:9 (2560x1080) -> pillarbox left & right bars.
        assert_eq!(
            compute_letterbox_viewport(2560, 1080, aspect_16_9),
            (320, 0, 1920, 1080)
        );
    }
}
