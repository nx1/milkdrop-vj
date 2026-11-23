//! GPU Mixer FBO, 5 Blend Modes, Smooth Emergency Blackout, and Two Live GPU Effect Racks
//! (Milestone 2 Stage `2A-2` + Milestone 3 Stage `3C`).
//!
//! Renders Deck A's and Deck B's shared `GL_TEXTURE_2D` outputs through a single OpenGL 3.3 Core
//! fragment shader into an off-screen `master_texture` (attached to `master_fbo`).
//!
//! Two Live GPU Effect Racks applied cleanly in the same zero-allocation pass:
//! - **FX1 (Color & Geometry):**
//!   - **Hue Shift (`0.0..=1.0` -> `0..360°`):** YIQ-space hue rotation (preserves luminance!).
//!   - **Mirror / Kaleidoscope (`0.0..=1.0`):** Polar coordinate symmetry folding (2x Mirror -> 8x Kaleid).
//!   - **Invert (`0.0..=1.0`):** Instant photo-negative color inversion.
//! - **FX2 (Performance & Punch):**
//!   - **Strobe (`0.0..=1.0`):** Crisp beat-synced (or 12 Hz default) strobe flash.
//!   - **Freeze (`bool`):** Captures the live Deck A/B blend into a dedicated `freeze_texture` on the
//!     rising edge so FX1/Strobe/Blackout can still be performed live on top of the frozen frame!
//!   - **Beat Zoom-Punch (`0.0..=1.0`):** Subtle center-anchored zoom kick synchronized with `Space` /
//!     Tap Tempo Auto-Pulse so even audio-insensitive MilkDrop presets hit hard on the beat.

use std::sync::Arc;

use glow::HasContext;

/// The 5 GPU blend modes supported by the mixer.
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

/// Live parameters for the two GPU Effect Racks (`FX1` + `FX2`) passed into `GpuMixer::render()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MasterFxParams {
    /// Hue rotation (`0.0..=1.0` -> `0..360°`).
    pub hue_shift: f32,
    /// Mirror / Kaleidoscope amount (`0.0` = off, `> 0.0` = 2x..8x radial symmetry).
    pub kaleidoscope: f32,
    /// Color inversion blend (`0.0..=1.0`).
    pub invert: f32,
    /// Instant strobe flash state (`0.0` = off, `0.0..=1.0` flash intensity, or negative for black gate).
    pub strobe_flash: f32,
    /// True while Frame Freeze is active (freezes target frame while FX1/Strobe/Blackout stay live).
    pub freeze: bool,
    /// Instant center zoom-punch amount (`0.0..=1.0`, driven by Beat Pulse flash).
    pub zoom_punch: f32,
}

impl MasterFxParams {
    /// Extracts only the effects assigned to a single target (`Deck A`, `Deck B`, or `Master`).
    pub fn filter_for_target(self, fx1_active: bool, fx2_active: bool) -> Self {
        Self {
            hue_shift: if fx1_active { self.hue_shift } else { 0.0 },
            kaleidoscope: if fx1_active { self.kaleidoscope } else { 0.0 },
            invert: if fx1_active { self.invert } else { 0.0 },
            strobe_flash: if fx2_active { self.strobe_flash } else { 0.0 },
            freeze: if fx2_active { self.freeze } else { false },
            zoom_punch: if fx2_active { self.zoom_punch } else { 0.0 },
        }
    }
}

/// Traktor-style routing targets for a single GPU Effect Unit (`A`, `B`, or `Master`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FxUnitRoute {
    pub deck_a: bool,
    pub deck_b: bool,
    pub master: bool,
}

impl Default for FxUnitRoute {
    fn default() -> Self {
        Self {
            deck_a: false,
            deck_b: false,
            master: true,
        }
    }
}

/// Complete live routing state for FX Unit 1 (Color/Geo) and FX Unit 2 (Performance/Punch).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FxRoutingState {
    pub fx1: FxUnitRoute,
    pub fx2: FxUnitRoute,
}

/// Computes the instantaneous strobe flash value (`+1.0` bright flash / `-0.75` dark gate)
/// synced to `bpm` (16th-note pulse) or `12 Hz` when no Tap Tempo BPM is set.
pub fn compute_strobe_flash(strobe_amount: f32, elapsed_secs: f64, bpm: Option<f64>) -> f32 {
    let amt = strobe_amount.clamp(0.0, 1.0);
    if amt < 0.05 {
        return 0.0;
    }
    // 4 flashes per beat (16th notes) when BPM is tapped, else 12 Hz classic club strobe.
    let hz = bpm
        .map(|b| (b / 60.0 * 4.0).clamp(4.0, 18.0))
        .unwrap_or(12.0);
    let phase = (elapsed_secs * hz).fract();
    if phase < 0.35 {
        amt
    } else {
        -0.85 * amt
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
uniform sampler2D u_tex_freeze;
uniform float u_crossfader;
uniform int u_blend_mode;
uniform float u_blackout_alpha;

// Stage 3C Two GPU Effect Racks uniforms
uniform float u_hue_shift;     // 0.0..1.0 -> 0..2*PI radians
uniform float u_kaleidoscope;  // 0.0 = off; >0.0 = 2..8 radial symmetry segments
uniform float u_invert;        // 0.0..1.0
uniform float u_strobe_flash;  // >0 white flash, <0 black gate, 0 off
uniform int u_freeze_active;   // 1 = sample frozen frame instead of live Deck A/B blend
uniform float u_zoom_punch;    // 0.0..1.0 subtle center zoom on beat hit

const float PI = 3.14159265359;
const float TAU = 6.28318530718;

// Luminance-preserving YIQ hue rotation (smooth 360-degree wrap with zero RGB banding).
vec3 rotate_hue(vec3 c, float turns) {
    float angle = turns * TAU;
    float cosA = cos(angle);
    float sinA = sin(angle);
    vec3 k = vec3(0.57735026919); // 1 / sqrt(3)
    return clamp(
        c * cosA + cross(k, c) * sinA + k * dot(k, c) * (1.0 - cosA),
        0.0,
        1.0
    );
}

// Mirror / Kaleidoscope UV folding around screen center.
vec2 apply_kaleidoscope_uv(vec2 uv, float amount) {
    if (amount <= 0.01) {
        return uv;
    }
    vec2 p = uv - 0.5;
    if (amount < 0.28) {
        // Clean horizontal mirror mode at low slider values.
        p.x = abs(p.x);
        return clamp(p + 0.5, 0.0, 1.0);
    }
    // 4, 6, or 8 symmetric kaleidoscope wedges.
    float segs = (amount < 0.55) ? 4.0 : ((amount < 0.82) ? 6.0 : 8.0);
    float r = length(p);
    float a = atan(p.y, p.x);
    float sector = TAU / segs;
    a = mod(a, sector);
    a = abs(a - sector * 0.5);
    vec2 folded = vec2(cos(a), sin(a)) * r + 0.5;
    return clamp(folded, 0.0, 1.0);
}

vec3 blend_decks(vec2 uv) {
    vec3 A = texture(u_tex_a, uv).rgb;
    vec3 B = texture(u_tex_b, uv).rgb;
    float t = clamp(u_crossfader, 0.0, 1.0);
    float mid = 1.0 - abs(2.0 * t - 1.0);

    if (u_blend_mode == 1) {
        return clamp(A * min(1.0, 2.0 * (1.0 - t)) + B * min(1.0, 2.0 * t), 0.0, 1.0);
    } else if (u_blend_mode == 2) {
        return mix(mix(A, B, t), A * B, mid);
    } else if (u_blend_mode == 3) {
        return mix(mix(A, B, t), abs(A - B), mid);
    } else if (u_blend_mode == 4) {
        float luma_b = dot(B, vec3(0.2126, 0.7152, 0.0722));
        float key_mask = smoothstep(0.08, 0.35, luma_b) * t;
        vec3 keyed = mix(A, B, key_mask);
        return mix(keyed, B, smoothstep(0.92, 1.0, t));
    }
    return mix(A, B, t);
}

void main() {
    // 1. Apply Beat Zoom-Punch + FX1 Kaleidoscope/Mirror UV transform.
    vec2 uv = v_uv;
    if (u_zoom_punch > 0.001) {
        float zoom = 1.0 - clamp(u_zoom_punch, 0.0, 1.0) * 0.08;
        uv = (uv - 0.5) * zoom + 0.5;
    }
    uv = apply_kaleidoscope_uv(uv, u_kaleidoscope);

    // 2. Sample either the frozen snapshot (FX2 Freeze) or live Deck A/B blend.
    vec3 color = (u_freeze_active == 1)
        ? texture(u_tex_freeze, uv).rgb
        : blend_decks(uv);

    // 3. FX1 Color transforms: Hue Shift + Color Invert.
    if (u_hue_shift > 0.002 && u_hue_shift < 0.998) {
        color = rotate_hue(color, u_hue_shift);
    }
    if (u_invert > 0.001) {
        color = mix(color, 1.0 - color, clamp(u_invert, 0.0, 1.0));
    }

    // 4. FX2 Strobe flash / dark gate.
    if (u_strobe_flash > 0.001) {
        color = mix(color, vec3(1.0), clamp(u_strobe_flash * 0.85, 0.0, 0.92));
    } else if (u_strobe_flash < -0.001) {
        color *= clamp(1.0 + u_strobe_flash, 0.0, 1.0);
    }

    // 5. Emergency Blackout always wins cleanly as the final stage.
    color *= (1.0 - clamp(u_blackout_alpha, 0.0, 1.0));
    f_color = vec4(color, 1.0);
}
"#;

/// Reusable off-screen RGBA8 texture + FBO pair owned by `GpuMixer` (zero per-frame allocs).
#[derive(Clone, Copy)]
struct FboTarget {
    fbo: glow::NativeFramebuffer,
    tex: glow::NativeTexture,
}

impl FboTarget {
    unsafe fn new(gl: &glow::Context, width: u32, height: u32, label: &str) -> Result<Self, String> {
        let fbo = gl
            .create_framebuffer()
            .map_err(|e| format!("Failed to create {label} FBO: {e}"))?;
        let tex = match create_rgba8_texture(gl, width, height) {
            Ok(t) => t,
            Err(e) => {
                gl.delete_framebuffer(fbo);
                return Err(e);
            }
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_texture(tex);
            gl.delete_framebuffer(fbo);
            return Err(format!("{label} FBO incomplete (status=0x{status:04X})"));
        }
        Ok(Self { fbo, tex })
    }

    unsafe fn resize(
        &self,
        gl: &glow::Context,
        width: u32,
        height: u32,
        label: &str,
    ) -> Result<(), String> {
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
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

        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(self.tex),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        if status != glow::FRAMEBUFFER_COMPLETE {
            return Err(format!(
                "{label} FBO incomplete after resize to {width}x{height} (status=0x{status:04X})"
            ));
        }
        Ok(())
    }

    unsafe fn destroy(&self, gl: &glow::Context) {
        gl.delete_texture(self.tex);
        gl.delete_framebuffer(self.fbo);
    }
}

/// Off-screen OpenGL 3.3 Core GPU mixer producing per-deck post-FX textures (`deck_a_texture`,
/// `deck_b_texture`) and `master_texture` with Traktor-style FX1/FX2 routing + per-target Freeze FBOs.
pub struct GpuMixer {
    gl: Arc<glow::Context>,
    width: u32,
    height: u32,
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    deck_a_target: FboTarget,
    deck_b_target: FboTarget,
    master_target: FboTarget,
    freeze_a_target: FboTarget,
    freeze_b_target: FboTarget,
    freeze_master_target: FboTarget,
    was_frozen_a: bool,
    was_frozen_b: bool,
    was_frozen_master: bool,
    u_tex_a: Option<glow::NativeUniformLocation>,
    u_tex_b: Option<glow::NativeUniformLocation>,
    u_tex_freeze: Option<glow::NativeUniformLocation>,
    u_crossfader: Option<glow::NativeUniformLocation>,
    u_blend_mode: Option<glow::NativeUniformLocation>,
    u_blackout_alpha: Option<glow::NativeUniformLocation>,
    u_hue_shift: Option<glow::NativeUniformLocation>,
    u_kaleidoscope: Option<glow::NativeUniformLocation>,
    u_invert: Option<glow::NativeUniformLocation>,
    u_strobe_flash: Option<glow::NativeUniformLocation>,
    u_freeze_active: Option<glow::NativeUniformLocation>,
    u_zoom_punch: Option<glow::NativeUniformLocation>,
}

impl GpuMixer {
    /// Compiles the mixer + FX shader and allocates per-deck, Master, and Freeze FBOs
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

            let mut created: Vec<FboTarget> = Vec::with_capacity(6);
            let make = |created: &mut Vec<FboTarget>, label: &str| -> Result<FboTarget, String> {
                match FboTarget::new(&gl, width, height, label) {
                    Ok(t) => {
                        created.push(t);
                        Ok(t)
                    }
                    Err(e) => {
                        for prev in created.drain(..) {
                            prev.destroy(&gl);
                        }
                        gl.delete_vertex_array(vao);
                        gl.delete_program(program);
                        Err(e)
                    }
                }
            };

            let deck_a_target = make(&mut created, "Deck A FX")?;
            let deck_b_target = make(&mut created, "Deck B FX")?;
            let master_target = make(&mut created, "Master Mixer")?;
            let freeze_a_target = make(&mut created, "Deck A Freeze")?;
            let freeze_b_target = make(&mut created, "Deck B Freeze")?;
            let freeze_master_target = make(&mut created, "Master Freeze")?;

            let u_tex_a = gl.get_uniform_location(program, "u_tex_a");
            let u_tex_b = gl.get_uniform_location(program, "u_tex_b");
            let u_tex_freeze = gl.get_uniform_location(program, "u_tex_freeze");
            let u_crossfader = gl.get_uniform_location(program, "u_crossfader");
            let u_blend_mode = gl.get_uniform_location(program, "u_blend_mode");
            let u_blackout_alpha = gl.get_uniform_location(program, "u_blackout_alpha");
            let u_hue_shift = gl.get_uniform_location(program, "u_hue_shift");
            let u_kaleidoscope = gl.get_uniform_location(program, "u_kaleidoscope");
            let u_invert = gl.get_uniform_location(program, "u_invert");
            let u_strobe_flash = gl.get_uniform_location(program, "u_strobe_flash");
            let u_freeze_active = gl.get_uniform_location(program, "u_freeze_active");
            let u_zoom_punch = gl.get_uniform_location(program, "u_zoom_punch");

            Ok(Self {
                gl,
                width,
                height,
                program,
                vao,
                deck_a_target,
                deck_b_target,
                master_target,
                freeze_a_target,
                freeze_b_target,
                freeze_master_target,
                was_frozen_a: false,
                was_frozen_b: false,
                was_frozen_master: false,
                u_tex_a,
                u_tex_b,
                u_tex_freeze,
                u_crossfader,
                u_blend_mode,
                u_blackout_alpha,
                u_hue_shift,
                u_kaleidoscope,
                u_invert,
                u_strobe_flash,
                u_freeze_active,
                u_zoom_punch,
            })
        }
    }

    /// Returns the post-FX Deck A texture (so the Deck A preview shows cue FX live before blending).
    pub fn deck_a_texture(&self) -> glow::NativeTexture {
        self.deck_a_target.tex
    }

    /// Returns the post-FX Deck B texture (so the Deck B preview shows cue FX live before blending).
    pub fn deck_b_texture(&self) -> glow::NativeTexture {
        self.deck_b_target.tex
    }

    /// Returns the OpenGL texture handle containing the latest mixed Master Output frame.
    pub fn master_texture(&self) -> glow::NativeTexture {
        self.master_target.tex
    }

    /// Resizes all mixer, deck-FX, and freeze textures in-place when toggling deck resolution (`2A-5`),
    /// preserving all GL texture handles so `egui` texture IDs stay valid without reallocation.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        self.width = width;
        self.height = height;
        self.was_frozen_a = false;
        self.was_frozen_b = false;
        self.was_frozen_master = false;
        unsafe {
            for (target, label) in [
                (self.deck_a_target, "Deck A FX"),
                (self.deck_b_target, "Deck B FX"),
                (self.master_target, "Master Mixer"),
                (self.freeze_a_target, "Deck A Freeze"),
                (self.freeze_b_target, "Deck B Freeze"),
                (self.freeze_master_target, "Master Freeze"),
            ] {
                target.resize(&self.gl, width, height, label)?;
            }
        }
        Ok(())
    }

    /// Applies Traktor-style per-deck FX1/FX2 (`deck_a_target`, `deck_b_target`), then blends them
    /// into `master_texture` with Master FX1/FX2 and smooth Emergency Blackout (`0.0..=1.0`).
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        raw_tex_a: glow::NativeTexture,
        raw_tex_b: glow::NativeTexture,
        crossfader: f32,
        blend_mode: BlendMode,
        blackout_alpha: f32,
        fx: MasterFxParams,
        routing: FxRoutingState,
    ) {
        unsafe {
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.disable(glow::BLEND);
            self.gl
                .viewport(0, 0, self.width as i32, self.height as i32);

            // --- Pass 1a: Deck A pre-fader FX (visible in Deck A preview even while in CUE!) ---
            let fx_a = fx.filter_for_target(routing.fx1.deck_a, routing.fx2.deck_a);
            if fx_a.freeze && !self.was_frozen_a {
                self.draw_pass(
                    self.freeze_a_target.fbo,
                    raw_tex_a,
                    raw_tex_a,
                    self.freeze_a_target.tex,
                    0.0,
                    BlendMode::Crossfade,
                    0.0,
                    MasterFxParams::default(),
                    false,
                );
            }
            self.was_frozen_a = fx_a.freeze;
            self.draw_pass(
                self.deck_a_target.fbo,
                raw_tex_a,
                raw_tex_a,
                self.freeze_a_target.tex,
                0.0,
                BlendMode::Crossfade,
                0.0,
                fx_a,
                fx_a.freeze,
            );

            // --- Pass 1b: Deck B pre-fader FX (visible in Deck B preview even while in CUE!) ---
            let fx_b = fx.filter_for_target(routing.fx1.deck_b, routing.fx2.deck_b);
            if fx_b.freeze && !self.was_frozen_b {
                self.draw_pass(
                    self.freeze_b_target.fbo,
                    raw_tex_b,
                    raw_tex_b,
                    self.freeze_b_target.tex,
                    0.0,
                    BlendMode::Crossfade,
                    0.0,
                    MasterFxParams::default(),
                    false,
                );
            }
            self.was_frozen_b = fx_b.freeze;
            self.draw_pass(
                self.deck_b_target.fbo,
                raw_tex_b,
                raw_tex_b,
                self.freeze_b_target.tex,
                0.0,
                BlendMode::Crossfade,
                0.0,
                fx_b,
                fx_b.freeze,
            );

            // --- Pass 2: Master Blend + Master FX1/FX2 + Emergency Blackout ---
            let fx_m = fx.filter_for_target(routing.fx1.master, routing.fx2.master);
            if fx_m.freeze && !self.was_frozen_master {
                self.draw_pass(
                    self.freeze_master_target.fbo,
                    self.deck_a_target.tex,
                    self.deck_b_target.tex,
                    self.freeze_master_target.tex,
                    crossfader,
                    blend_mode,
                    0.0,
                    MasterFxParams::default(),
                    false,
                );
            }
            self.was_frozen_master = fx_m.freeze;
            self.draw_pass(
                self.master_target.fbo,
                self.deck_a_target.tex,
                self.deck_b_target.tex,
                self.freeze_master_target.tex,
                crossfader,
                blend_mode,
                blackout_alpha,
                fx_m,
                fx_m.freeze,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_pass(
        &self,
        target_fbo: glow::NativeFramebuffer,
        tex_a: glow::NativeTexture,
        tex_b: glow::NativeTexture,
        freeze_tex: glow::NativeTexture,
        crossfader: f32,
        blend_mode: BlendMode,
        blackout_alpha: f32,
        fx: MasterFxParams,
        use_freeze_tex: bool,
    ) {
        self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target_fbo));
        self.gl.use_program(Some(self.program));

        self.gl.active_texture(glow::TEXTURE0);
        self.gl.bind_texture(glow::TEXTURE_2D, Some(tex_a));
        self.gl.uniform_1_i32(self.u_tex_a.as_ref(), 0);

        self.gl.active_texture(glow::TEXTURE1);
        self.gl.bind_texture(glow::TEXTURE_2D, Some(tex_b));
        self.gl.uniform_1_i32(self.u_tex_b.as_ref(), 1);

        self.gl.active_texture(glow::TEXTURE2);
        self.gl.bind_texture(glow::TEXTURE_2D, Some(freeze_tex));
        self.gl.uniform_1_i32(self.u_tex_freeze.as_ref(), 2);

        self.gl
            .uniform_1_f32(self.u_crossfader.as_ref(), crossfader.clamp(0.0, 1.0));
        self.gl
            .uniform_1_i32(self.u_blend_mode.as_ref(), blend_mode.as_index() as i32);
        self.gl.uniform_1_f32(
            self.u_blackout_alpha.as_ref(),
            blackout_alpha.clamp(0.0, 1.0),
        );

        self.gl
            .uniform_1_f32(self.u_hue_shift.as_ref(), fx.hue_shift.clamp(0.0, 1.0));
        self.gl.uniform_1_f32(
            self.u_kaleidoscope.as_ref(),
            fx.kaleidoscope.clamp(0.0, 1.0),
        );
        self.gl
            .uniform_1_f32(self.u_invert.as_ref(), fx.invert.clamp(0.0, 1.0));
        self.gl.uniform_1_f32(
            self.u_strobe_flash.as_ref(),
            fx.strobe_flash.clamp(-1.0, 1.0),
        );
        self.gl.uniform_1_i32(
            self.u_freeze_active.as_ref(),
            if use_freeze_tex { 1 } else { 0 },
        );
        self.gl
            .uniform_1_f32(self.u_zoom_punch.as_ref(), fx.zoom_punch.clamp(0.0, 1.0));

        self.gl.bind_vertex_array(Some(self.vao));
        self.gl.draw_arrays(glow::TRIANGLES, 0, 3);

        // Cleanly unbind all state so downstream egui rendering starts from FBO 0.
        self.gl.bind_vertex_array(None);
        self.gl.active_texture(glow::TEXTURE2);
        self.gl.bind_texture(glow::TEXTURE_2D, None);
        self.gl.active_texture(glow::TEXTURE1);
        self.gl.bind_texture(glow::TEXTURE_2D, None);
        self.gl.active_texture(glow::TEXTURE0);
        self.gl.bind_texture(glow::TEXTURE_2D, None);
        self.gl.use_program(None);
        self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }

    /// Destroys all OpenGL objects owned by this mixer while the main context is current.
    pub fn destroy(&mut self) {
        unsafe {
            self.freeze_master_target.destroy(&self.gl);
            self.freeze_b_target.destroy(&self.gl);
            self.freeze_a_target.destroy(&self.gl);
            self.master_target.destroy(&self.gl);
            self.deck_b_target.destroy(&self.gl);
            self.deck_a_target.destroy(&self.gl);
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

    #[test]
    fn strobe_flash_respects_bpm_and_turns_off_cleanly() {
        assert_eq!(compute_strobe_flash(0.0, 1.0, Some(120.0)), 0.0);
        // At 120 BPM (8 Hz = 125 ms cycle): t = 0.01s is bright flash (+1.0), t = 0.08s is dark gate (-0.85).
        assert!(compute_strobe_flash(1.0, 0.01, Some(120.0)) > 0.9);
        assert!(compute_strobe_flash(1.0, 0.08, Some(120.0)) < -0.5);
    }

    #[test]
    fn fx_routing_filter_isolates_units_per_target() {
        let fx = MasterFxParams {
            hue_shift: 0.4,
            kaleidoscope: 0.67,
            invert: 1.0,
            strobe_flash: 0.85,
            freeze: true,
            zoom_punch: 0.9,
        };
        // FX1 on Deck A only, FX2 on Deck B only, Master dry.
        let deck_a = fx.filter_for_target(true, false);
        assert_eq!(deck_a.hue_shift, 0.4);
        assert_eq!(deck_a.kaleidoscope, 0.67);
        assert_eq!(deck_a.invert, 1.0);
        assert_eq!(deck_a.strobe_flash, 0.0);
        assert!(!deck_a.freeze);
        assert_eq!(deck_a.zoom_punch, 0.0);

        let deck_b = fx.filter_for_target(false, true);
        assert_eq!(deck_b.hue_shift, 0.0);
        assert_eq!(deck_b.kaleidoscope, 0.0);
        assert_eq!(deck_b.invert, 0.0);
        assert_eq!(deck_b.strobe_flash, 0.85);
        assert!(deck_b.freeze);
        assert_eq!(deck_b.zoom_punch, 0.9);

        let master = fx.filter_for_target(false, false);
        assert_eq!(master.hue_shift, 0.0);
        assert!(!master.freeze);
    }
}
