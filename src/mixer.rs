//! Milestone 2 — Stage `2A-2`: GPU Mixer FBO, 5 Blend Modes, and Smooth Emergency Blackout.
//!
//! Renders Deck A's and Deck B's shared `GL_TEXTURE_2D` outputs through a single OpenGL 3.3 Core
//! fragment shader into an off-screen `master_texture` (attached to `master_fbo`).
//!
//! Why render Master into an off-screen FBO texture?
//! 1. The Control Console (`egui`) displays the exact mixed Master output live in its center column.
//! 2. In Stage `2B`, the borderless fullscreen Projector Output window samples this exact same
//!    `master_texture` — guaranteeing the audience sees pixel-for-pixel what is in Master Preview.
//!
//! Five blend modes (verified against Section 13.3 of `docs/project_research_and_design.md`):
//! - `0` Crossfade (default): `mix(A, B, t)`
//! - `1` Additive:            `clamp(A * min(1, 2*(1-t)) + B * min(1, 2*t), 0, 1)`
//! - `2` Multiply:            `mix(mix(A, B, t), A * B, 1 - abs(2*t - 1))`
//! - `3` Difference:          `mix(mix(A, B, t), abs(A - B), 1 - abs(2*t - 1))`
//! - `4` Luma Key:            `mix(mix(A, B, smoothstep(0.08, 0.35, luma(B)) * t), B, smoothstep(0.92, 1.0, t))`
//!
//! Emergency Blackout multiplies the mixed color by `(1.0 - u_blackout_alpha)`, smoothly ramped
//! over `0.4 seconds` on the main loop.

use std::sync::Arc;

use glow::HasContext;

/// The 5 GPU blend modes supported by the Milestone 2 mixer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    Crossfade = 0,
    Additive = 1,
    Multiply = 2,
    Difference = 3,
    LumaKey = 4,
}

impl BlendMode {
    pub const ALL: [BlendMode; 5] = [
        BlendMode::Crossfade,
        BlendMode::Additive,
        BlendMode::Multiply,
        BlendMode::Difference,
        BlendMode::LumaKey,
    ];

    pub fn from_index(idx: usize) -> Self {
        match idx {
            1 => Self::Additive,
            2 => Self::Multiply,
            3 => Self::Difference,
            4 => Self::LumaKey,
            _ => Self::Crossfade,
        }
    }

    pub fn as_index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Crossfade => "Crossfade",
            Self::Additive => "Additive",
            Self::Multiply => "Multiply",
            Self::Difference => "Difference",
            Self::LumaKey => "Luma Key",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Crossfade => "Linear DJ crossfade: mix(A, B, t)",
            Self::Additive => "Glowing light addition in the middle; pure A/B at ends",
            Self::Multiply => "Darker color-filter overlap in the middle; pure A/B at ends",
            Self::Difference => "High-contrast inverted color difference where decks overlap",
            Self::LumaKey => "Bright areas of Deck B key over Deck A; dark areas stay transparent",
        }
    }
}

/// Deck live state derived strictly from the crossfader position `t` (`0.0..=1.0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckAirState {
    /// Deck weight `<= 0.05` (off-air, safe to load in 1 click/press).
    Cue,
    /// Deck weight `0.05 < w < 0.95` (visibly blended in Master, protected).
    Mixed,
    /// Deck weight `>= 0.95` (dominates Master, protected).
    OnAir,
}

impl DeckAirState {
    /// Computes Deck A's and Deck B's states from `crossfader` (`0.0 = A`, `1.0 = B`).
    pub fn from_weight(weight: f32) -> Self {
        if weight <= 0.05 {
            Self::Cue
        } else if weight >= 0.95 {
            Self::OnAir
        } else {
            Self::Mixed
        }
    }

    /// True when the deck is visible to the audience (`MIXED` or `ON-AIR`) and protected by Load Safety.
    pub fn is_protected(self) -> bool {
        !matches!(self, Self::Cue)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cue => "CUE",
            Self::Mixed => "MIXED",
            Self::OnAir => "ON-AIR",
        }
    }
}

/// Duration (in seconds) for the smooth Emergency Blackout fade-to-black and fade-in.
pub const BLACKOUT_FADE_SECONDS: f32 = 0.4;

/// Advances `current` toward `target` (`0.0` or `1.0`) smoothly over `BLACKOUT_FADE_SECONDS`.
pub fn step_blackout_alpha(current: f32, target_on: bool, dt_secs: f32) -> f32 {
    let rate = 1.0 / BLACKOUT_FADE_SECONDS;
    let delta = rate * dt_secs.clamp(0.0, 0.1);
    if target_on {
        (current + delta).min(1.0)
    } else {
        (current - delta).max(0.0)
    }
}

const MIXER_VERT_SRC: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    // Fullscreen triangle covering clip space [-1, 1] without needing a vertex buffer.
    vec2 pos = vec2(
        (gl_VertexID == 1) ? 3.0 : -1.0,
        (gl_VertexID == 2) ? 3.0 : -1.0
    );
    v_uv = 0.5 * (pos + 1.0);
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#;

const MIXER_FRAG_SRC: &str = r#"#version 330 core
in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_tex_a;
uniform sampler2D u_tex_b;
uniform float u_crossfader;
uniform int u_blend_mode;
uniform float u_blackout_alpha;

void main() {
    vec3 A = texture(u_tex_a, v_uv).rgb;
    vec3 B = texture(u_tex_b, v_uv).rgb;
    float t = clamp(u_crossfader, 0.0, 1.0);
    float mid = 1.0 - abs(2.0 * t - 1.0);

    vec3 color;
    if (u_blend_mode == 1) {
        // Additive
        color = clamp(A * min(1.0, 2.0 * (1.0 - t)) + B * min(1.0, 2.0 * t), 0.0, 1.0);
    } else if (u_blend_mode == 2) {
        // Multiply
        color = mix(mix(A, B, t), A * B, mid);
    } else if (u_blend_mode == 3) {
        // Difference
        color = mix(mix(A, B, t), abs(A - B), mid);
    } else if (u_blend_mode == 4) {
        // Luma Key (bright parts of Deck B key over Deck A; reaches pure B at t = 1.0)
        float luma_b = dot(B, vec3(0.2126, 0.7152, 0.0722));
        float key_mask = smoothstep(0.08, 0.35, luma_b) * t;
        vec3 keyed = mix(A, B, key_mask);
        color = mix(keyed, B, smoothstep(0.92, 1.0, t));
    } else {
        // Crossfade (default)
        color = mix(A, B, t);
    }

    // Emergency Blackout smooth fade-to-black.
    color *= (1.0 - clamp(u_blackout_alpha, 0.0, 1.0));
    f_color = vec4(color, 1.0);
}
"#;

/// Off-screen OpenGL 3.3 Core GPU mixer producing `master_texture`.
pub struct GpuMixer {
    gl: Arc<glow::Context>,
    width: u32,
    height: u32,
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    fbo: glow::NativeFramebuffer,
    master_texture: glow::NativeTexture,
    u_tex_a: Option<glow::NativeUniformLocation>,
    u_tex_b: Option<glow::NativeUniformLocation>,
    u_crossfader: Option<glow::NativeUniformLocation>,
    u_blend_mode: Option<glow::NativeUniformLocation>,
    u_blackout_alpha: Option<glow::NativeUniformLocation>,
}

impl GpuMixer {
    /// Compiles the mixer shader and allocates the off-screen `master_texture` + FBO
    /// on the currently active main OpenGL context.
    pub fn new(gl: Arc<glow::Context>, width: u32, height: u32) -> Result<Self, String> {
        unsafe {
            let vert = compile_shader(&gl, glow::VERTEX_SHADER, MIXER_VERT_SRC)?;
            let frag = compile_shader(&gl, glow::FRAGMENT_SHADER, MIXER_FRAG_SRC)?;
            let program = gl
                .create_program()
                .map_err(|e| format!("Failed to create GPU mixer program: {e}"))?;
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
                return Err(format!("Failed to link GPU mixer shader: {log}"));
            }

            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("Failed to create GPU mixer VAO: {e}"))?;
            let fbo = gl
                .create_framebuffer()
                .map_err(|e| format!("Failed to create GPU mixer FBO: {e}"))?;
            let master_texture = create_rgba8_texture(&gl, width, height)?;

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(master_texture),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_texture(master_texture);
                gl.delete_framebuffer(fbo);
                gl.delete_vertex_array(vao);
                gl.delete_program(program);
                return Err(format!("GPU mixer FBO incomplete (status=0x{status:04X})"));
            }

            let u_tex_a = gl.get_uniform_location(program, "u_tex_a");
            let u_tex_b = gl.get_uniform_location(program, "u_tex_b");
            let u_crossfader = gl.get_uniform_location(program, "u_crossfader");
            let u_blend_mode = gl.get_uniform_location(program, "u_blend_mode");
            let u_blackout_alpha = gl.get_uniform_location(program, "u_blackout_alpha");

            Ok(Self {
                gl,
                width,
                height,
                program,
                vao,
                fbo,
                master_texture,
                u_tex_a,
                u_tex_b,
                u_crossfader,
                u_blend_mode,
                u_blackout_alpha,
            })
        }
    }

    /// Returns the OpenGL texture handle containing the latest mixed Master Output frame.
    pub fn master_texture(&self) -> glow::NativeTexture {
        self.master_texture
    }

    /// Resizes `master_texture` when the user toggles deck resolution in Stage `2A-5`.
    #[allow(dead_code)]
    pub fn resize(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        unsafe {
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.master_texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Blends `tex_a` and `tex_b` into `master_texture` using the chosen blend mode,
    /// crossfader position (`0.0..=1.0`), and smooth blackout fade (`0.0..=1.0`).
    pub fn render(
        &self,
        tex_a: glow::NativeTexture,
        tex_b: glow::NativeTexture,
        crossfader: f32,
        blend_mode: BlendMode,
        blackout_alpha: f32,
    ) {
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.disable(glow::BLEND);
            self.gl
                .viewport(0, 0, self.width as i32, self.height as i32);

            self.gl.use_program(Some(self.program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex_a));
            self.gl.uniform_1_i32(self.u_tex_a.as_ref(), 0);

            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex_b));
            self.gl.uniform_1_i32(self.u_tex_b.as_ref(), 1);

            self.gl
                .uniform_1_f32(self.u_crossfader.as_ref(), crossfader.clamp(0.0, 1.0));
            self.gl
                .uniform_1_i32(self.u_blend_mode.as_ref(), blend_mode.as_index() as i32);
            self.gl.uniform_1_f32(
                self.u_blackout_alpha.as_ref(),
                blackout_alpha.clamp(0.0, 1.0),
            );

            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);

            // Cleanly unbind all state so downstream egui rendering starts from FBO 0.
            self.gl.bind_vertex_array(None);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.use_program(None);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// Destroys all OpenGL objects owned by this mixer while the main context is current.
    pub fn destroy(&mut self) {
        unsafe {
            self.gl.delete_texture(self.master_texture);
            self.gl.delete_framebuffer(self.fbo);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_program(self.program);
        }
    }
}

unsafe fn create_rgba8_texture(
    gl: &glow::Context,
    width: u32,
    height: u32,
) -> Result<glow::NativeTexture, String> {
    let tex = gl
        .create_texture()
        .map_err(|e| format!("Failed to create GPU mixer texture: {e}"))?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        width as i32,
        height as i32,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(None),
    );
    gl.bind_texture(glow::TEXTURE_2D, None);
    Ok(tex)
}

unsafe fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    src: &str,
) -> Result<glow::NativeShader, String> {
    let shader = gl
        .create_shader(shader_type)
        .map_err(|e| format!("Failed to create mixer shader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("GPU mixer shader compile failed: {log}"));
    }
    Ok(shader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_state_thresholds_match_spec() {
        assert_eq!(DeckAirState::from_weight(0.0), DeckAirState::Cue);
        assert_eq!(DeckAirState::from_weight(0.05), DeckAirState::Cue);
        assert!(!DeckAirState::Cue.is_protected());

        assert_eq!(DeckAirState::from_weight(0.06), DeckAirState::Mixed);
        assert_eq!(DeckAirState::from_weight(0.50), DeckAirState::Mixed);
        assert_eq!(DeckAirState::from_weight(0.94), DeckAirState::Mixed);
        assert!(DeckAirState::Mixed.is_protected());

        assert_eq!(DeckAirState::from_weight(0.95), DeckAirState::OnAir);
        assert_eq!(DeckAirState::from_weight(1.0), DeckAirState::OnAir);
        assert!(DeckAirState::OnAir.is_protected());
    }

    #[test]
    fn blackout_fade_reaches_full_in_point_four_seconds() {
        let mut a = 0.0_f32;
        for _ in 0..24 {
            a = step_blackout_alpha(a, true, 1.0 / 60.0);
        }
        assert!((a - 1.0).abs() < 1e-4, "Expected blackout ~1.0 after 0.4s, got {a}");

        for _ in 0..24 {
            a = step_blackout_alpha(a, false, 1.0 / 60.0);
        }
        assert!(a.abs() < 1e-4, "Expected blackout ~0.0 after 0.4s restore, got {a}");
    }
}
