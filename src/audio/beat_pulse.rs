//! Manual Beat Pulse (`Space`) & Tap Tempo (`T`) Engine (Milestone 3 — Stage `3B`).
//!
//! How it works (human-in-the-loop visual beat shaping):
//! - **Manual Beat Pulse (`Space`):** Triggers a punchy, exponentially damped `70 Hz`
//!   sub-bass transient (`~80 ms` decay) summed cleanly onto each deck's private stereo PCM
//!   buffer right before `libprojectM` renders. Your speakers are never modified.
//! - **Tap Tempo (`T`):** Tracks recent tap intervals (automatically resetting after `2.0 s`
//!   of inactivity), computes a smooth BPM (`40..=220 BPM`), and phase-locks an optional
//!   **Auto-Pulse** beat grid so visuals can lock to your tapped tempo even during quiet ambient
//!   intros or beatless breakdowns.
//!
//! Note on preset behavior: `libprojectM` presets react to beats based on their own equations
//! (`bass / bass_att` rolling average). Rapidly mashing `Space` raises the rolling average, so
//! spacing hits musically produces the strongest visual kick reaction.

use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

/// Fundamental frequency of the synthesized visual kick pulse (`70 Hz` lands squarely inside
/// `libprojectM`'s bass FFT bins).
const PULSE_FREQ_HZ: f32 = 70.0;
/// Exponential decay time constant (`28 ms` -> crisp ~85 ms kick punch).
const PULSE_DECAY_SECS: f32 = 0.028;
/// Maximum number of recent tap intervals averaged for Tap Tempo BPM.
const MAX_TAP_HISTORY: usize = 6;
/// Gaps longer than `2.0 s` start a fresh tap-tempo sequence.
const TAP_RESET_TIMEOUT: Duration = Duration::from_secs(2);
/// Valid musical BPM clamp range.
pub const MIN_BPM: f64 = 40.0;
pub const MAX_BPM: f64 = 220.0;

/// Synthesizes manual and tap-tempo visual beat pulses into stereo PCM buffers.
#[derive(Debug)]
pub struct BeatPulseEngine {
    sample_rate: u32,
    /// Remaining pulse envelope amplitude (`0.0..=1.0`, decays exponentially each sample).
    env: f32,
    /// Continuous oscillator phase in radians (`0..TAU`).
    phase: f32,
    /// Visual UI flash intensity (`0.0..=1.0`, decays smoothly over ~180 ms for the BEAT button).
    ui_flash: f32,
    /// Recent tap timestamps for Tap Tempo BPM calculation.
    tap_times: VecDeque<Instant>,
    /// Current calculated BPM (`None` until at least 2 taps occur).
    bpm: Option<f64>,
    /// Next scheduled Auto-Pulse fire time (phase-locked to the latest tap).
    next_auto_beat: Option<Instant>,
}

impl Default for BeatPulseEngine {
    fn default() -> Self {
        Self::new(48_000)
    }
}

impl BeatPulseEngine {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(8_000),
            env: 0.0,
            phase: 0.0,
            ui_flash: 0.0,
            tap_times: VecDeque::with_capacity(MAX_TAP_HISTORY + 1),
            bpm: None,
            next_auto_beat: None,
        }
    }

    /// Fires a single visual beat pulse immediately (called on `Space`, UI `BEAT`, or Auto-Pulse).
    pub fn trigger_pulse(&mut self, strength: f32) {
        let clamped = strength.clamp(0.05, 1.0);
        self.env = self.env.max(clamped);
        // Reset phase to 0 so every hit starts cleanly with zero DC offset click.
        self.phase = 0.0;
        self.ui_flash = 1.0;
    }

    /// Records a Tap Tempo tap at `now`, updating BPM and re-aligning the Auto-Pulse phase.
    pub fn record_tap_at(&mut self, now: Instant) -> Option<f64> {
        if let Some(&last) = self.tap_times.back() {
            if now.duration_since(last) > TAP_RESET_TIMEOUT {
                self.tap_times.clear();
            }
        }
        self.tap_times.push_back(now);
        while self.tap_times.len() > MAX_TAP_HISTORY {
            self.tap_times.pop_front();
        }

        if self.tap_times.len() >= 2 {
            let first = *self.tap_times.front().unwrap();
            let last = *self.tap_times.back().unwrap();
            let spans = (self.tap_times.len() - 1) as f64;
            let total_secs = last.duration_since(first).as_secs_f64();
            if total_secs > 0.05 {
                let avg_period = total_secs / spans;
                let bpm = (60.0 / avg_period).clamp(MIN_BPM, MAX_BPM);
                self.bpm = Some(bpm);
                let period = Duration::from_secs_f64(60.0 / bpm);
                self.next_auto_beat = Some(now + period);
            }
        }
        self.bpm
    }

    /// Convenience wrapper using `Instant::now()`.
    pub fn record_tap(&mut self) -> Option<f64> {
        self.record_tap_at(Instant::now())
    }

    /// Clears the tapped BPM and stops Auto-Pulse scheduling.
    pub fn clear_tempo(&mut self) {
        self.tap_times.clear();
        self.bpm = None;
        self.next_auto_beat = None;
    }

    pub fn bpm(&self) -> Option<f64> {
        self.bpm
    }

    /// Returns current UI flash glow (`0.0..=1.0`) for highlighting the `BEAT` button.
    pub fn ui_flash(&self) -> f32 {
        self.ui_flash
    }

    /// Advances the Auto-Pulse beat clock (if `auto_enabled` and BPM is set) and decays UI flash.
    pub fn update_clock(&mut self, now: Instant, dt_secs: f32, auto_enabled: bool, strength: f32) {
        self.ui_flash = (self.ui_flash - dt_secs.clamp(0.0, 0.1) * 5.5).max(0.0);

        if !auto_enabled {
            return;
        }
        let Some(bpm) = self.bpm else {
            return;
        };
        let period = Duration::from_secs_f64(60.0 / bpm);
        match self.next_auto_beat {
            Some(mut due) if now >= due => {
                self.trigger_pulse(strength);
                // Advance cleanly past `now` even if a single frame hiccupped.
                while due <= now {
                    due += period;
                }
                self.next_auto_beat = Some(due);
            }
            None => {
                self.next_auto_beat = Some(now + period);
            }
            _ => {}
        }
    }

    /// Synthesizes the active damped `70 Hz` pulse and sums it cleanly into BOTH `deck_a_lr` and
    /// `deck_b_lr` (in-place, zero heap allocations). Safe to call even when `env == 0.0`.
    pub fn mix_into_decks(
        &mut self,
        deck_a_lr: &mut [f32],
        deck_b_lr: &mut [f32],
        sample_rate: u32,
    ) {
        let sr = sample_rate.max(8_000);
        self.sample_rate = sr;
        if self.env < 1e-4 {
            self.env = 0.0;
            return;
        }

        let frames = (deck_a_lr.len().min(deck_b_lr.len())) / 2;
        if frames == 0 {
            return;
        }

        let sr_f = sr as f32;
        let phase_step = TAU * PULSE_FREQ_HZ / sr_f;
        let decay_per_sample = (-1.0 / (PULSE_DECAY_SECS * sr_f)).exp();

        for i in 0..frames {
            let sample = self.env * self.phase.sin();
            self.phase += phase_step;
            if self.phase >= TAU {
                self.phase -= TAU;
            }
            self.env *= decay_per_sample;

            let idx = i * 2;
            deck_a_lr[idx] = (deck_a_lr[idx] + sample).clamp(-1.0, 1.0);
            deck_a_lr[idx + 1] = (deck_a_lr[idx + 1] + sample).clamp(-1.0, 1.0);
            deck_b_lr[idx] = (deck_b_lr[idx] + sample).clamp(-1.0, 1.0);
            deck_b_lr[idx + 1] = (deck_b_lr[idx + 1] + sample).clamp(-1.0, 1.0);
        }

        if self.env < 1e-4 {
            self.env = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_tempo_computes_accurate_bpm_and_resets_after_pause() {
        let mut engine = BeatPulseEngine::new(48_000);
        let t0 = Instant::now();
        // 120 BPM = 500 ms per beat.
        assert!(engine.record_tap_at(t0).is_none());
        let bpm1 = engine
            .record_tap_at(t0 + Duration::from_millis(500))
            .expect("bpm after 2 taps");
        assert!((bpm1 - 120.0).abs() < 0.1, "Expected 120 BPM, got {bpm1}");

        let bpm2 = engine
            .record_tap_at(t0 + Duration::from_millis(1000))
            .expect("bpm after 3 taps");
        assert!((bpm2 - 120.0).abs() < 0.1);

        // After > 2s pause, new taps at 150 BPM (400 ms) should cleanly reset history.
        let t_new = t0 + Duration::from_millis(4000);
        let _ = engine.record_tap_at(t_new);
        let bpm_new = engine
            .record_tap_at(t_new + Duration::from_millis(400))
            .expect("new bpm after reset");
        assert!(
            (bpm_new - 150.0).abs() < 0.2,
            "Expected 150 BPM after pause reset, got {bpm_new}"
        );
    }

    #[test]
    fn beat_pulse_injects_damped_energy_into_silence_and_decays() {
        let mut engine = BeatPulseEngine::new(48_000);
        engine.trigger_pulse(0.8);

        let mut a = vec![0.0_f32; 800 * 2];
        let mut b = vec![0.0_f32; 800 * 2];
        engine.mix_into_decks(&mut a, &mut b, 48_000);

        let peak_a = a.iter().fold(0.0_f32, |acc, &s| acc.max(s.abs()));
        assert!(peak_a > 0.5, "Pulse should inject strong initial bass hit (peak={peak_a})");
        assert_eq!(a, b);

        // After ~300 ms (18 ticks of 800 frames), envelope should decay completely to 0.
        for _ in 0..18 {
            a.fill(0.0);
            b.fill(0.0);
            engine.mix_into_decks(&mut a, &mut b, 48_000);
        }
        let tail_peak = a.iter().fold(0.0_f32, |acc, &s| acc.max(s.abs()));
        assert!(tail_peak < 1e-3, "Pulse should decay cleanly to silence (tail={tail_peak})");
    }

    #[test]
    fn auto_pulse_fires_on_tapped_beat_interval() {
        let mut engine = BeatPulseEngine::new(48_000);
        let t0 = Instant::now();
        engine.record_tap_at(t0);
        engine.record_tap_at(t0 + Duration::from_millis(500)); // 120 BPM -> next due at t0 + 1000 ms

        engine.update_clock(t0 + Duration::from_millis(900), 0.016, true, 0.75);
        assert_eq!(engine.ui_flash(), 0.0, "Should not fire before beat is due");

        engine.update_clock(t0 + Duration::from_millis(1005), 0.016, true, 0.75);
        assert!(engine.ui_flash() > 0.9, "Auto-Pulse should fire once beat period elapses");
    }
}
