//! Minimal OpenGL 3.3 Core side-by-side compositor for Stage 3.
//!
//! Draws Deck A's shared `GL_TEXTURE_2D` on the left half of the window and
//! Deck B's shared `GL_TEXTURE_2D` on the right half of the window.
//! Uses a single fullscreen triangle per half (no vertex buffer needed in GL 3.3 Core,
//! only an empty VAO).

use glow::HasContext;

const VERT_SHADER_SRC: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    // Three vertices forming a giant triangle that covers the entire viewport [-1, 1].
    vec2 pos = vec2(
        (gl_VertexID == 1) ? 3.0 : -1.0,
        (gl_VertexID == 2) ? 3.0 : -1.0
    );
    v_uv = 0.5 * (pos + 1.0);
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#;

const FRAG_SHADER_SRC: &str = r#"#version 330 core
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_tex;
void main() {
    frag_color = vec4(texture(u_tex, v_uv).rgb, 1.0);
}
"#;

/// Draws two shared deck textures side by side in the main window.
pub struct SideBySideCompositor {
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    tex_uniform: Option<glow::NativeUniformLocation>,
}

impl SideBySideCompositor {
    /// Compiles the shader program and creates the empty VAO on the main GL context.
    pub fn new(gl: &glow::Context) -> Result<Self, String> {
        unsafe {
            let program = gl
                .create_program()
                .map_err(|e| format!("Failed to create shader program: {e}"))?;

            let vert = compile_shader(gl, glow::VERTEX_SHADER, VERT_SHADER_SRC)?;
            let frag = compile_shader(gl, glow::FRAGMENT_SHADER, FRAG_SHADER_SRC)?;

            gl.attach_shader(program, vert);
            gl.attach_shader(program, frag);
            gl.link_program(program);

            let linked = gl.get_program_link_status(program);
            let link_log = gl.get_program_info_log(program);
            gl.detach_shader(program, vert);
            gl.detach_shader(program, frag);
            gl.delete_shader(vert);
            gl.delete_shader(frag);

            if !linked {
                gl.delete_program(program);
                return Err(format!("Failed to link compositor shader: {link_log}"));
            }

            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("Failed to create compositor VAO: {e}"))?;
            let tex_uniform = gl.get_uniform_location(program, "u_tex");

            Ok(Self {
                program,
                vao,
                tex_uniform,
            })
        }
    }

    /// Renders `left_tex` (Deck A) in the left half and `right_tex` (Deck B) in the right half,
    /// leaving a 4-pixel dark divider in the middle.
    pub fn draw(
        &self,
        gl: &glow::Context,
        window_width: i32,
        window_height: i32,
        left_tex: glow::NativeTexture,
        right_tex: glow::NativeTexture,
    ) {
        let divider = 4;
        let half_w = ((window_width - divider) / 2).max(1);
        let right_x = half_w + divider;
        let right_w = (window_width - right_x).max(1);

        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.viewport(0, 0, window_width, window_height);
            gl.clear_color(0.05, 0.05, 0.07, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            if let Some(loc) = &self.tex_uniform {
                gl.uniform_1_i32(Some(loc), 0);
            }

            // Left half: Deck A
            gl.viewport(0, 0, half_w, window_height);
            gl.bind_texture(glow::TEXTURE_2D, Some(left_tex));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            // Right half: Deck B
            gl.viewport(right_x, 0, right_w, window_height);
            gl.bind_texture(glow::TEXTURE_2D, Some(right_tex));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
    }

    /// Frees the GPU shader program and VAO while the main OpenGL context is still current.
    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.program);
        }
    }
}

unsafe fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    src: &str,
) -> Result<glow::NativeShader, String> {
    let shader = gl
        .create_shader(shader_type)
        .map_err(|e| format!("Failed to create shader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("Shader compile failed: {log}"));
    }
    Ok(shader)
}
