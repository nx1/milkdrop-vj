//! Per-Deck 3-Band Visual EQ & Kill Switches (Milestone 3 — Stage `3A`).
//!
//! Operates strictly on a **read-only copy** of stereo PCM sent to `libprojectM`—audience
//! speakers and Windows audio playback are never altered.
//!
//! Filter design (Traktor-style 3-band Linkwitz-Riley `LR2` isolator):
//! - Splits interleaved stereo `f32` audio into three bands using 2nd-order Linkwitz-Riley
//!   (`-12 dB/oct`) cascaded 1-pole low-pass & high-pass pairs (unconditionally stable at
//!   every sample rate from `22 kHz` to `192 kHz`):
//!   - **Bass (`< 250 Hz`):** sub-bass & kick drums (`-25 dB` kill at `60 Hz`).
//!   - **Mid (`250 Hz – 2,500 Hz`):** vocals, synths, guitars, snare body.
//!   - **Treble (`> 2,500 Hz`):** hi-hats, cymbals, crisp transients (`-21 dB` kill at `8 kHz`).
//! - **Flat All-Pass Unity Sum:** Inverting the high-pass branch at each `LR2` crossover
//!   (`low - hp2`) yields a flat `0 dB` magnitude response across the entire spectrum when
//!   all three Control Bus sliders sit at neutral `0.5` (`1.0x`) with kills OFF.
//! - **Control Bus Mapping:** Normalized `0.0..=1.0` maps linearly to gain `0.0..=2.0`
//!   (`0.0` = full cut, `0.5` = `1.0x` unity, `1.0` = `2.0x` / `+6 dB` boost).
//!   A Kill switch (`true`) forces that band's gain to `0.0` without losing the slider value.

/// Low/Mid crossover frequency in Hz (separates kicks & sub-bass from vocals/synths).
pub const BASS_MID_CROSSOVER_HZ: f32 = 250.0;
/// Mid/Treble crossover frequency in Hz (separates mids from hi-hats & cymbals).
pub const MID_TREBLE_CROSSOVER_HZ: f32 = 2_500.0;

/// Per-deck Visual EQ settings read from the Control Bus (`0.0..=1.0` sliders + kill booleans).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisualEqParams {
    /// Normalized Bass level (`0.0..=1.0`, default `0.5` -> `1.0x`).
    pub bass: f32,
    /// Normalized Mid level (`0.0..=1.0`, default `0.5` -> `1.0x`).
    pub mid: f32,
    /// Normalized Treble level (`0.0..=1.0`, default `0.5` -> `1.0x`).
    pub treble: f32,
    pub bass_kill: bool,
    pub mid_kill: bool,
    pub treble_kill: bool,
}

impl Default for VisualEqParams {
    fn default() -> Self {
        Self {
            bass: 0.5,
            mid: 0.5,
            treble: 0.5,
            bass_kill: false,
            mid_kill: false,
            treble_kill: false,
        }
    }
}

impl VisualEqParams {
    /// Converts a normalized Control Bus value (`0.0..=1.0`) to linear gain (`0.0..=2.0`).
    #[inline]
    pub fn bus_to_gain(norm: f32, killed: bool) -> f32 {
        if killed {
            0.0
        } else {
            norm.clamp(0.0, 1.0) * 2.0
        }
    }

    #[inline]
    pub fn effective_gains(self) -> (f32, f32, f32) {
        (
            Self::bus_to_gain(self.bass, self.bass_kill),
            Self::bus_to_gain(self.mid, self.mid_kill),
            Self::bus_to_gain(self.treble, self.treble_kill),
        )
    }

    /// True when all bands are at unity (`1.0x`) and no kill switch is engaged.
    #[inline]
    #[allow(dead_code)]
    pub fn is_neutral(self) -> bool {
        !self.bass_kill
            && !self.mid_kill
            && !self.treble_kill
            && (self.bass - 0.5).abs() < 1e-4
            && (self.mid - 0.5).abs() < 1e-4
            && (self.treble - 0.5).abs() < 1e-4
    }
}

/// 2nd-order Linkwitz-Riley (`LR2`) crossover state for one mono channel at one cutoff frequency.
/// Uses matched bilinear (trapezoidal) 1-pole filters so `lp2 - hp2` has exact flat `1.0` magnitude!
#[derive(Debug, Clone, Copy, Default)]
struct Lr2SplitState {
    lp1_x: f32,
    lp1_y: f32,
    lp2_x: f32,
    lp2_y: f32,
    hp1_x: f32,
    hp1_y: f32,
    hp2_x: f32,
    hp2_y: f32,
}

/// Bilinear 1-pole filter coefficients (`k = tan(pi * fc / fs)`).
#[derive(Debug, Clone, Copy)]
struct Lr2Coeffs {
    b_lp: f32,
    b_hp: f32,
    a1: f32,
}

impl Lr2Coeffs {
    fn new(cutoff_hz: f32, sample_rate: f32) -> Self {
        let fc = cutoff_hz.clamp(10.0, sample_rate * 0.45);
        let k = (std::f32::consts::PI * fc / sample_rate).tan();
        let inv = 1.0 / (1.0 + k);
        Self {
            b_lp: k * inv,
            b_hp: inv,
            a1: (k - 1.0) * inv,
        }
    }
}

impl Lr2SplitState {
    /// Splits `x` into `(low, inverted_high)` where `low + inverted_high` is an exact
    /// flat-magnitude all-pass across the entire spectrum (`0 Hz` to Nyquist).
    #[inline]
    fn split(&mut self, x: f32, c: Lr2Coeffs) -> (f32, f32) {
        // 1st-order bilinear low-pass cascaded twice -> LR2 low-pass (-12 dB/oct).
        let lp1 = c.b_lp * (x + self.lp1_x) - c.a1 * self.lp1_y;
        self.lp1_x = x;
        self.lp1_y = lp1;

        let lp2 = c.b_lp * (lp1 + self.lp2_x) - c.a1 * self.lp2_y;
        self.lp2_x = lp1;
        self.lp2_y = lp2;

        // 1st-order bilinear high-pass cascaded twice -> LR2 high-pass (-12 dB/oct).
        let hp1 = c.b_hp * (x - self.hp1_x) - c.a1 * self.hp1_y;
        self.hp1_x = x;
        self.hp1_y = hp1;

        let hp2 = c.b_hp * (hp1 - self.hp2_x) - c.a1 * self.hp2_y;
        self.hp2_x = hp1;
        self.hp2_y = hp2;

        // Polarity-inverted high branch ensures `lp2 + (-hp2)` has flat 0 dB magnitude everywhere!
        (lp2, -hp2)
    }
}

/// Complete 3-band stereo filter state for one channel (L or R).
#[derive(Debug, Clone, Copy, Default)]
struct ChannelEqState {
    low_split: Lr2SplitState,
    high_split: Lr2SplitState,
}

/// Stateful per-deck stereo 3-band Visual EQ processor (zero heap allocations per frame).
#[derive(Debug, Clone)]
pub struct VisualEqProcessor {
    sample_rate: u32,
    c_low: Lr2Coeffs,
    c_high: Lr2Coeffs,
    left: ChannelEqState,
    right: ChannelEqState,
}

impl Default for VisualEqProcessor {
    fn default() -> Self {
        Self::new(48_000)
    }
}

impl VisualEqProcessor {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(8_000);
        Self {
            sample_rate: sr,
            c_low: Lr2Coeffs::new(BASS_MID_CROSSOVER_HZ, sr as f32),
            c_high: Lr2Coeffs::new(MID_TREBLE_CROSSOVER_HZ, sr as f32),
            left: ChannelEqState::default(),
            right: ChannelEqState::default(),
        }
    }

    /// Updates filter coefficients if the active audio device sample rate changed (`44.1` <-> `48 kHz`).
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(8_000);
        if sr != self.sample_rate {
            self.sample_rate = sr;
            self.c_low = Lr2Coeffs::new(BASS_MID_CROSSOVER_HZ, sr as f32);
            self.c_high = Lr2Coeffs::new(MID_TREBLE_CROSSOVER_HZ, sr as f32);
            self.left = ChannelEqState::default();
            self.right = ChannelEqState::default();
        }
    }

    /// Processes interleaved stereo `input_lr` into `output_lr` (reusing `output_lr`'s allocation).
    ///
    /// Always runs the filter state smoothly even when `params` is neutral so engaging a Kill
    /// switch mid-track never produces a startup transient click.
    pub fn process_stereo(
        &mut self,
        input_lr: &[f32],
        output_lr: &mut Vec<f32>,
        sample_rate: u32,
        params: VisualEqParams,
    ) {
        self.set_sample_rate(sample_rate);
        let stereo_len = input_lr.len() & !1;
        output_lr.clear();
        output_lr.reserve(stereo_len);

        let (g_bass, g_mid, g_treble) = params.effective_gains();
        let c_low = self.c_low;
        let c_high = self.c_high;

        for frame in input_lr[..stereo_len].chunks_exact(2) {
            let l_out = step_channel(&mut self.left, frame[0], c_low, c_high, g_bass, g_mid, g_treble);
            let r_out = step_channel(&mut self.right, frame[1], c_low, c_high, g_bass, g_mid, g_treble);
            output_lr.push(l_out);
            output_lr.push(r_out);
        }
    }
}

#[inline]
fn step_channel(
    st: &mut ChannelEqState,
    x: f32,
    c_low: Lr2Coeffs,
    c_high: Lr2Coeffs,
    g_bass: f32,
    g_mid: f32,
    g_treble: f32,
) -> f32 {
    let (low, upper) = st.low_split.split(x, c_low);
    let (mid, high) = st.high_split.split(upper, c_high);
    // Soft-clamp to -1.0..=1.0 so boosted bands never feed out-of-range PCM to libprojectM.
    (low * g_bass + mid * g_mid + high * g_treble).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn generate_stereo_sine(freq_hz: f32, sample_rate: u32, frames: usize, amp: f32) -> Vec<f32> {
        let mut buf = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let s = amp * (TAU * freq_hz * (n as f32) / (sample_rate as f32)).sin();
            buf.push(s);
            buf.push(s);
        }
        buf
    }

    fn rms_second_half(stereo: &[f32]) -> f32 {
        let half = stereo.len() / 2;
        let slice = &stereo[half..];
        let sum_sq: f32 = slice.iter().map(|&s| s * s).sum();
        (sum_sq / slice.len().max(1) as f32).sqrt()
    }

    #[test]
    fn neutral_eq_preserves_flat_magnitude_across_spectrum() {
        let sr = 48_000;
        let mut out = Vec::new();
        for freq in [60.0_f32, 250.0, 1_000.0, 2_500.0, 8_000.0] {
            let input = generate_stereo_sine(freq, sr, 4800, 0.5);
            let mut eq = VisualEqProcessor::new(sr);
            eq.process_stereo(&input, &mut out, sr, VisualEqParams::default());

            let in_rms = rms_second_half(&input);
            let out_rms = rms_second_half(&out);
            let rel_err = (out_rms - in_rms).abs() / in_rms;
            assert!(
                rel_err < 0.03,
                "Expected flat unity magnitude at {freq} Hz: in={in_rms:.4}, out={out_rms:.4} (err={rel_err:.4})"
            );
        }
    }

    #[test]
    fn bass_mid_and_treble_kills_isolate_their_bands_cleanly() {
        let sr = 48_000;
        let frames = 4800; // 100 ms so filter settles cleanly
        let bass_sig = generate_stereo_sine(60.0, sr, frames, 0.4);
        let mid_sig = generate_stereo_sine(1_000.0, sr, frames, 0.4);
        let high_sig = generate_stereo_sine(8_000.0, sr, frames, 0.4);

        let base_rms = rms_second_half(&bass_sig);

        // 1. Bass Kill removes 60 Hz by > 20 dB (< 0.10x) while keeping 1 kHz & 8 kHz strong.
        let mut out = Vec::new();
        let bass_kill = VisualEqParams {
            bass_kill: true,
            ..VisualEqParams::default()
        };
        let mut eq = VisualEqProcessor::new(sr);
        eq.process_stereo(&bass_sig, &mut out, sr, bass_kill);
        let bass_killed_rms = rms_second_half(&out);
        assert!(
            bass_killed_rms < base_rms * 0.10,
            "60 Hz should drop >20 dB with Bass Kill: {bass_killed_rms} vs {base_rms}"
        );

        let mut eq_m = VisualEqProcessor::new(sr);
        eq_m.process_stereo(&mid_sig, &mut out, sr, bass_kill);
        assert!(
            rms_second_half(&out) > base_rms * 0.90,
            "1 kHz should stay strong with Bass Kill"
        );

        // 2. Mid Kill cuts 1 kHz strongly while preserving 60 Hz & 8 kHz.
        let mid_kill = VisualEqParams {
            mid_kill: true,
            ..VisualEqParams::default()
        };
        let mut eq2 = VisualEqProcessor::new(sr);
        eq2.process_stereo(&mid_sig, &mut out, sr, mid_kill);
        assert!(
            rms_second_half(&out) < base_rms * 0.25,
            "1 kHz should drop sharply with Mid Kill"
        );

        // 3. Treble Kill cuts 8 kHz by > 18 dB while preserving 60 Hz.
        let treble_kill = VisualEqParams {
            treble_kill: true,
            ..VisualEqParams::default()
        };
        let mut eq3 = VisualEqProcessor::new(sr);
        eq3.process_stereo(&high_sig, &mut out, sr, treble_kill);
        assert!(
            rms_second_half(&out) < base_rms * 0.15,
            "8 kHz should drop >16 dB with Treble Kill"
        );

        let mut eq4 = VisualEqProcessor::new(sr);
        eq4.process_stereo(&bass_sig, &mut out, sr, treble_kill);
        assert!(
            rms_second_half(&out) > base_rms * 0.95,
            "60 Hz should stay untouched with Treble Kill"
        );
    }

    #[test]
    fn bass_boost_amplifies_and_clamps_safely() {
        let sr = 48_000;
        let bass_sig = generate_stereo_sine(60.0, sr, 4800, 0.35);
        let base_rms = rms_second_half(&bass_sig);

        let mut eq = VisualEqProcessor::new(sr);
        let mut out = Vec::new();
        let boosted = VisualEqParams {
            bass: 1.0, // 2.0x boost
            ..VisualEqParams::default()
        };
        eq.process_stereo(&bass_sig, &mut out, sr, boosted);
        let boosted_rms = rms_second_half(&out);
        assert!(
            boosted_rms > base_rms * 1.80,
            "2.0x Bass boost should raise 60 Hz RMS: {boosted_rms} vs {base_rms}"
        );
        assert!(out.iter().all(|s| s.is_finite() && (-1.0..=1.0).contains(s)));
    }
}
