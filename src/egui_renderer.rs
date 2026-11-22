//! Zero-Reallocation OpenGL 3.3 Core Renderer for `egui 0.37`.
//!
//! Why we wrote a focused 200-line `egui` OpenGL 3.3 Core renderer instead of `egui_glow::Painter`:
//! - `egui_glow::Painter::paint_mesh()` calls `glBufferData(..., GL_STREAM_DRAW)` twice per
//!   clipped mesh (~25 reallocations per frame = ~1,500/sec). On NVIDIA Windows OpenGL driver
//!   `596.58` with `wglShareLists` shared contexts (Pattern 1 multi-deck), NVIDIA leaked every
//!   reallocated buffer (`+3.2 MiB` every 5 seconds = `+2.3 GiB/hour`, isolated by bisection).
//! - This renderer allocates one reusable VBO and one reusable EBO, growing them only when a
//!   frame exceeds capacity, and uploads each frame's batched meshes once with `glBufferSubData`
//!   — zero per-frame `glBufferData` reallocations and flat memory over hours of live use.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::sync::Arc;

use egui::epaint::{ImageDelta, Primitive, Vertex};
use egui::{ClippedPrimitive, TextureFilter, TextureId, TextureOptions, TextureWrapMode, TexturesDelta};
use glow::HasContext;

const EGUI_VERT_SRC: &str = r#"#version 330 core
uniform vec2 u_screen_size;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_tc;
layout(location = 2) in vec4 a_srgba;
out vec4 v_rgba_in_gamma;
out vec2 v_tc;
void main() {
    gl_Position = vec4(
        2.0 * a_pos.x / u_screen_size.x - 1.0,
        1.0 - 2.0 * a_pos.y / u_screen_size.y,
        0.0,
        1.0
    );
    v_rgba_in_gamma = a_srgba / 255.0;
    v_tc = a_tc;
}
"#;

const EGUI_FRAG_SRC: &str = r#"#version 330 core
uniform sampler2D u_sampler;
in vec4 v_rgba_in_gamma;
in vec2 v_tc;
out vec4 f_color;
void main() {
    vec4 texture_in_gamma = texture(u_sampler, v_tc);
    f_color = v_rgba_in_gamma * texture_in_gamma;
}
"#;

/// One batched draw call within the frame's single VBO/EBO upload.
struct DrawCmd {
    clip_rect: egui::Rect,
    texture_id: TextureId,
    index_offset_bytes: i32,
    index_count: i32,
}

/// Reusable OpenGL 3.3 Core renderer for `egui 0.37` meshes and textures.
pub struct EguiGlRenderer {
    gl: Arc<glow::Context>,
    program: glow::NativeProgram,
    u_screen_size: Option<glow::NativeUniformLocation>,
    u_sampler: Option<glow::NativeUniformLocation>,
    vao: glow::NativeVertexArray,
    vbo: glow::NativeBuffer,
    ebo: glow::NativeBuffer,
    vbo_capacity: usize,
    ebo_capacity: usize,
    /// CPU-side staging buffers reused every frame (zero heap churn after warm-up).
    cpu_vertices: Vec<u8>,
    cpu_indices: Vec<u32>,
    draw_cmds: Vec<DrawCmd>,
    /// Managed `egui` textures (font atlas, icons) — owned and deleted by this renderer.
    managed_textures: HashMap<TextureId, glow::NativeTexture>,
    /// External shared OpenGL textures (Deck A, Deck B, Master) — NOT deleted by this renderer.
    native_textures: HashMap<TextureId, glow::NativeTexture>,
    next_native_id: u64,
    max_texture_side: usize,
}

impl EguiGlRenderer {
    /// Creates shaders, VAO, and pre-sized reusable VBO/EBO on the currently active GL context.
    pub fn new(gl: Arc<glow::Context>) -> Result<Self, String> {
        let initial_vbo_cap = 512 * 1024; // 512 KiB (~26,000 vertices)
        let initial_ebo_cap = 256 * 1024; // 256 KiB (~65,000 indices)

        unsafe {
            let vert = compile_shader(&gl, glow::VERTEX_SHADER, EGUI_VERT_SRC)?;
            let frag = compile_shader(&gl, glow::FRAGMENT_SHADER, EGUI_FRAG_SRC)?;
            let program = gl
                .create_program()
                .map_err(|e| format!("Failed to create egui shader program: {e}"))?;
            gl.attach_shader(program, vert);
            gl.attach_shader(program, frag);
            gl.link_program(program);
            let linked = gl.get_program_link_status(program);
            let log = gl.get_program_info_log(program);
            gl.detach_shader(program, vert);
            gl.detach_shader(program, frag);
            gl.delete_shader(vert);
            gl.delete_shader(frag);
            if !linked {
                gl.delete_program(program);
                return Err(format!("Failed to link egui shader program: {log}"));
            }

            let u_screen_size = gl.get_uniform_location(program, "u_screen_size");
            let u_sampler = gl.get_uniform_location(program, "u_sampler");

            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("Failed to create egui VAO: {e}"))?;
            let vbo = gl
                .create_buffer()
                .map_err(|e| format!("Failed to create egui VBO: {e}"))?;
            let ebo = gl
                .create_buffer()
                .map_err(|e| format!("Failed to create egui EBO: {e}"))?;

            gl.bind_vertex_array(Some(vao));

            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_size(
                glow::ARRAY_BUFFER,
                initial_vbo_cap as i32,
                glow::DYNAMIC_DRAW,
            );

            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
            gl.buffer_data_size(
                glow::ELEMENT_ARRAY_BUFFER,
                initial_ebo_cap as i32,
                glow::DYNAMIC_DRAW,
            );

            let stride = size_of::<Vertex>() as i32;
            // location 0: a_pos (vec2 f32)
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(
                0,
                2,
                glow::FLOAT,
                false,
                stride,
                offset_of!(Vertex, pos) as i32,
            );
            // location 1: a_tc (vec2 f32)
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(
                1,
                2,
                glow::FLOAT,
                false,
                stride,
                offset_of!(Vertex, uv) as i32,
            );
            // location 2: a_srgba (4 x u8, unnormalized 0..=255 -> divided by 255 in shader)
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(
                2,
                4,
                glow::UNSIGNED_BYTE,
                false,
                stride,
                offset_of!(Vertex, color) as i32,
            );

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);

            let max_tex = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(2048) as usize;

            Ok(Self {
                gl,
                program,
                u_screen_size,
                u_sampler,
                vao,
                vbo,
                ebo,
                vbo_capacity: initial_vbo_cap,
                ebo_capacity: initial_ebo_cap,
                cpu_vertices: Vec::with_capacity(initial_vbo_cap),
                cpu_indices: Vec::with_capacity(initial_ebo_cap / 4),
                draw_cmds: Vec::with_capacity(64),
                managed_textures: HashMap::new(),
                native_textures: HashMap::new(),
                next_native_id: 1,
                max_texture_side: max_tex,
            })
        }
    }

    pub fn max_texture_side(&self) -> usize {
        self.max_texture_side
    }

    /// Registers an externally owned OpenGL texture (for example Deck A's shared texture)
    /// so `egui::Image` can display it without taking ownership or deleting it on teardown.
    pub fn register_native_texture(&mut self, tex: glow::NativeTexture) -> TextureId {
        let id = TextureId::User(self.next_native_id);
        self.next_native_id += 1;
        self.native_textures.insert(id, tex);
        id
    }

    /// Updates an already-registered native texture handle (used when resolution toggles in 2A-5).
    #[allow(dead_code)]
    pub fn update_native_texture(&mut self, id: TextureId, tex: glow::NativeTexture) {
        self.native_textures.insert(id, tex);
    }

    /// Applies texture uploads, batches all `egui` primitives into one `glBufferSubData`,
    /// draws them with per-primitive scissor clipping, and frees retired managed textures.
    pub fn paint(
        &mut self,
        screen_size_px: [u32; 2],
        pixels_per_point: f32,
        clipped_primitives: &[ClippedPrimitive],
        textures_delta: &TexturesDelta,
    ) {
        // 1. Upload any new or updated managed textures (font atlas, etc.).
        for (id, deltas) in &textures_delta.set {
            for delta in deltas {
                self.set_texture(*id, delta);
            }
        }

        // 2. Batch all meshes for this frame into reusable CPU staging buffers.
        self.cpu_vertices.clear();
        self.cpu_indices.clear();
        self.draw_cmds.clear();

        let mut base_vertex: u32 = 0;
        for ClippedPrimitive {
            clip_rect,
            primitive,
        } in clipped_primitives
        {
            let Primitive::Mesh(mesh) = primitive else {
                continue;
            };
            if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                continue;
            }
            let vert_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    mesh.vertices.as_ptr() as *const u8,
                    mesh.vertices.len() * size_of::<Vertex>(),
                )
            };
            let index_offset_bytes = (self.cpu_indices.len() * size_of::<u32>()) as i32;
            self.cpu_vertices.extend_from_slice(vert_bytes);
            self.cpu_indices
                .extend(mesh.indices.iter().map(|&idx| idx + base_vertex));
            base_vertex += mesh.vertices.len() as u32;

            self.draw_cmds.push(DrawCmd {
                clip_rect: *clip_rect,
                texture_id: mesh.texture_id,
                index_offset_bytes,
                index_count: mesh.indices.len() as i32,
            });
        }

        // 3. Upload once with `glBufferSubData` (grow VBO/EBO only if capacity exceeded) and draw.
        if !self.draw_cmds.is_empty() && screen_size_px[0] > 0 && screen_size_px[1] > 0 {
            let vbo_needed = self.cpu_vertices.len();
            let ebo_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    self.cpu_indices.as_ptr() as *const u8,
                    self.cpu_indices.len() * size_of::<u32>(),
                )
            };
            let ebo_needed = ebo_bytes.len();

            unsafe {
                self.gl.enable(glow::SCISSOR_TEST);
                self.gl.disable(glow::CULL_FACE);
                self.gl.disable(glow::DEPTH_TEST);
                self.gl.disable(glow::FRAMEBUFFER_SRGB);
                self.gl.color_mask(true, true, true, true);
                self.gl.enable(glow::BLEND);
                self.gl
                    .blend_equation_separate(glow::FUNC_ADD, glow::FUNC_ADD);
                self.gl.blend_func_separate(
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::ONE_MINUS_DST_ALPHA,
                    glow::ONE,
                );

                self.gl.viewport(
                    0,
                    0,
                    screen_size_px[0] as i32,
                    screen_size_px[1] as i32,
                );
                self.gl.use_program(Some(self.program));
                self.gl.uniform_2_f32(
                    self.u_screen_size.as_ref(),
                    screen_size_px[0] as f32 / pixels_per_point,
                    screen_size_px[1] as f32 / pixels_per_point,
                );
                self.gl.uniform_1_i32(self.u_sampler.as_ref(), 0);
                self.gl.active_texture(glow::TEXTURE0);

                self.gl.bind_vertex_array(Some(self.vao));

                // Upload vertices via glBufferSubData (no per-frame reallocation!).
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                if vbo_needed > self.vbo_capacity {
                    let new_cap = vbo_needed.next_power_of_two().max(self.vbo_capacity * 2);
                    self.gl.buffer_data_size(
                        glow::ARRAY_BUFFER,
                        new_cap as i32,
                        glow::DYNAMIC_DRAW,
                    );
                    self.vbo_capacity = new_cap;
                }
                self.gl
                    .buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, &self.cpu_vertices);

                // Upload indices via glBufferSubData (no per-frame reallocation!).
                self.gl
                    .bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(self.ebo));
                if ebo_needed > self.ebo_capacity {
                    let new_cap = ebo_needed.next_power_of_two().max(self.ebo_capacity * 2);
                    self.gl.buffer_data_size(
                        glow::ELEMENT_ARRAY_BUFFER,
                        new_cap as i32,
                        glow::DYNAMIC_DRAW,
                    );
                    self.ebo_capacity = new_cap;
                }
                self.gl
                    .buffer_sub_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, 0, ebo_bytes);

                let width_px = screen_size_px[0] as i32;
                let height_px = screen_size_px[1] as i32;

                for cmd in &self.draw_cmds {
                    let Some(tex) = self.resolve_texture(cmd.texture_id) else {
                        continue;
                    };

                    let min_x = (pixels_per_point * cmd.clip_rect.min.x).round() as i32;
                    let min_y = (pixels_per_point * cmd.clip_rect.min.y).round() as i32;
                    let max_x = (pixels_per_point * cmd.clip_rect.max.x).round() as i32;
                    let max_y = (pixels_per_point * cmd.clip_rect.max.y).round() as i32;

                    let x0 = min_x.clamp(0, width_px);
                    let y0 = min_y.clamp(0, height_px);
                    let x1 = max_x.clamp(x0, width_px);
                    let y1 = max_y.clamp(y0, height_px);
                    let w = x1 - x0;
                    let h = y1 - y0;
                    if w <= 0 || h <= 0 {
                        continue;
                    }

                    self.gl.scissor(x0, height_px - y1, w, h);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_elements(
                        glow::TRIANGLES,
                        cmd.index_count,
                        glow::UNSIGNED_INT,
                        cmd.index_offset_bytes,
                    );
                }

                self.gl.bind_texture(glow::TEXTURE_2D, None);
                self.gl.bind_vertex_array(None);
                self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
                self.gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
                self.gl.use_program(None);
                self.gl.disable(glow::SCISSOR_TEST);
            }
        }

        // 4. Free any retired managed textures.
        for id in &textures_delta.free {
            if let Some(tex) = self.managed_textures.remove(id) {
                unsafe {
                    self.gl.delete_texture(tex);
                }
            }
        }
    }

    fn resolve_texture(&self, id: TextureId) -> Option<glow::NativeTexture> {
        self.native_textures
            .get(&id)
            .copied()
            .or_else(|| self.managed_textures.get(&id).copied())
    }

    fn set_texture(&mut self, tex_id: TextureId, delta: &ImageDelta) {
        let gl_tex = *self.managed_textures.entry(tex_id).or_insert_with(|| unsafe {
            self.gl
                .create_texture()
                .expect("Failed to create egui managed texture")
        });

        let egui::ImageData::Color(image) = &delta.image;
        let w = image.width() as i32;
        let h = image.height() as i32;
        let data: &[u8] = unsafe {
            std::slice::from_raw_parts(image.pixels.as_ptr() as *const u8, image.pixels.len() * 4)
        };

        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(gl_tex));
            apply_texture_options(&self.gl, delta.options);
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);

            if let Some([x, y]) = delta.pos {
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    x as i32,
                    y as i32,
                    w,
                    h,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(data)),
                );
            } else {
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w,
                    h,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(data)),
                );
            }
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Destroys all OpenGL resources owned by this renderer (leaves native deck textures untouched).
    pub fn destroy(&mut self) {
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_buffer(self.ebo);
            for (_, tex) in self.managed_textures.drain() {
                self.gl.delete_texture(tex);
            }
        }
        self.native_textures.clear();
    }
}

unsafe fn apply_texture_options(gl: &glow::Context, options: TextureOptions) {
    let mag = match options.magnification {
        TextureFilter::Nearest => glow::NEAREST,
        TextureFilter::Linear => glow::LINEAR,
    } as i32;
    let min = match options.minification {
        TextureFilter::Nearest => glow::NEAREST,
        TextureFilter::Linear => glow::LINEAR,
    } as i32;
    let wrap = match options.wrap_mode {
        TextureWrapMode::ClampToEdge => glow::CLAMP_TO_EDGE,
        TextureWrapMode::Repeat => glow::REPEAT,
        TextureWrapMode::MirroredRepeat => glow::MIRRORED_REPEAT,
    } as i32;
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, mag);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, min);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, wrap);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, wrap);
}

unsafe fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    src: &str,
) -> Result<glow::NativeShader, String> {
    let shader = gl
        .create_shader(shader_type)
        .map_err(|e| format!("Failed to create egui shader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("egui shader compilation failed: {log}"));
    }
    Ok(shader)
}
