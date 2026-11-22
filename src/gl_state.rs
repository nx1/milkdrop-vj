//! OpenGL state save/restore hygiene around `libprojectM` render calls.
//!
//! Why this module exists:
//! `libprojectM` renders MilkDrop presets using its own internal shaders, VAOs,
//! blend modes, and internal framebuffers (FBOs). When `projectm_opengl_render_frame()`
//! returns, it can leave `READ_FRAMEBUFFER_BINDING`, shader programs, or blend states
//! modified. Saving and restoring GL state around every render call prevents
//! cross-contamination and ensures `glCopyTexSubImage2D` reads from the deck's
//! actual rendered output instead of an internal scratch buffer.

use glow::HasContext;

/// Snapshot of OpenGL state saved before `projectm_opengl_render_frame()` and
/// restored immediately afterward.
pub struct GlStateSnapshot {
    program: Option<glow::NativeProgram>,
    vertex_array: Option<glow::NativeVertexArray>,
    array_buffer: Option<glow::NativeBuffer>,
    draw_framebuffer: Option<glow::NativeFramebuffer>,
    read_framebuffer: Option<glow::NativeFramebuffer>,
    draw_buffer: i32,
    read_buffer: i32,
    active_texture: i32,
    texture_binding_2d: Option<glow::NativeTexture>,
    blend_enabled: bool,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_eq_rgb: i32,
    blend_eq_alpha: i32,
    viewport: [i32; 4],
    scissor_test_enabled: bool,
    scissor_box: [i32; 4],
    color_writemask: [bool; 4],
    unpack_alignment: i32,
}

impl GlStateSnapshot {
    /// Captures the current OpenGL state on the active context.
    pub fn save(gl: &glow::Context) -> Self {
        unsafe {
            let mut viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);

            let mut scissor_box = [0i32; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);

            let mut color_mask_raw = [1i32; 4];
            gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut color_mask_raw);
            let color_writemask = [
                color_mask_raw[0] != 0,
                color_mask_raw[1] != 0,
                color_mask_raw[2] != 0,
                color_mask_raw[3] != 0,
            ];

            Self {
                program: gl.get_parameter_program(glow::CURRENT_PROGRAM),
                vertex_array: gl.get_parameter_vertex_array(glow::VERTEX_ARRAY_BINDING),
                array_buffer: gl.get_parameter_buffer(glow::ARRAY_BUFFER_BINDING),
                draw_framebuffer: gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING),
                read_framebuffer: gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING),
                draw_buffer: gl.get_parameter_i32(glow::DRAW_BUFFER),
                read_buffer: gl.get_parameter_i32(glow::READ_BUFFER),
                active_texture: gl.get_parameter_i32(glow::ACTIVE_TEXTURE),
                texture_binding_2d: gl.get_parameter_texture(glow::TEXTURE_BINDING_2D),
                blend_enabled: gl.is_enabled(glow::BLEND),
                blend_src_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                blend_dst_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB),
                blend_src_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                blend_dst_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                blend_eq_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                blend_eq_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA),
                viewport,
                scissor_test_enabled: gl.is_enabled(glow::SCISSOR_TEST),
                scissor_box,
                color_writemask,
                unpack_alignment: gl.get_parameter_i32(glow::UNPACK_ALIGNMENT),
            }
        }
    }

    /// Restores the saved OpenGL state in absolute terms.
    pub fn restore(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.draw_framebuffer);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, self.read_framebuffer);
            if self.draw_framebuffer.is_none() && self.draw_buffer != 0 {
                gl.draw_buffer(self.draw_buffer as u32);
            }
            if self.read_framebuffer.is_none() && self.read_buffer != 0 {
                gl.read_buffer(self.read_buffer as u32);
            }

            gl.use_program(self.program);
            gl.bind_vertex_array(self.vertex_array);
            gl.bind_buffer(glow::ARRAY_BUFFER, self.array_buffer);

            gl.active_texture(self.active_texture as u32);
            gl.bind_texture(glow::TEXTURE_2D, self.texture_binding_2d);

            if self.blend_enabled {
                gl.enable(glow::BLEND);
            } else {
                gl.disable(glow::BLEND);
            }
            gl.blend_func_separate(
                self.blend_src_rgb as u32,
                self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32,
                self.blend_dst_alpha as u32,
            );
            gl.blend_equation_separate(self.blend_eq_rgb as u32, self.blend_eq_alpha as u32);

            gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );

            if self.scissor_test_enabled {
                gl.enable(glow::SCISSOR_TEST);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
            gl.scissor(
                self.scissor_box[0],
                self.scissor_box[1],
                self.scissor_box[2],
                self.scissor_box[3],
            );

            gl.color_mask(
                self.color_writemask[0],
                self.color_writemask[1],
                self.color_writemask[2],
                self.color_writemask[3],
            );
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, self.unpack_alignment);
        }
    }
}

/// Resets both DRAW and READ framebuffers to the default framebuffer (FBO 0 — the
/// deck's pbuffer) and aligns `GL_READ_BUFFER` with the pbuffer's default `GL_DRAW_BUFFER`.
///
/// Must be called right before `glCopyTexSubImage2D` or `glReadPixels` so we never
/// accidentally read from one of `libprojectM`'s internal scratch FBOs.
pub fn reset_read_to_default_fbo(gl: &glow::Context) {
    unsafe {
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        let draw_buf = gl.get_parameter_i32(glow::DRAW_BUFFER);
        if draw_buf != 0 {
            gl.read_buffer(draw_buf as u32);
        }
    }
}
