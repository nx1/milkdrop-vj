//! Deterministic test-audio generator for Spike Stages 2 and 3 (no WASAPI yet).
//!
//! Combines three signals into stereo interleaved `f32` PCM (`[-1.0, 1.0]`):
//! 1. A smooth **sine sweep** gliding from 200 Hz to 2,000 Hz every 4 seconds.
//! 2. A **low-frequency bass pulse** (65 Hz damped kick every 0.5 s = 120 BPM)
//!    to strongly trigger MilkDrop's bass/beat detection.
//! 3. Subtle deterministic **white noise** for high-frequency sparkle.

use std::f32::consts::TAU;

/// Generates stereo PCM audio chunks without requiring any audio hardware.
pub struct TestAudioGenerator {
    sample_rate: f32,
    sample_index: u64,
    sweep_phase: f32,
    pulse_phase: f32,
    rng_state: u32,
}

impl TestAudioGenerator {
    /// Creates a generator running at `sample_rate` Hz (for example `48_000.0`).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            sample_index: 0,
            sweep_phase: 0.0,
            pulse_phase: 0.0,
            rng_state: 0x1234_5678,
        }
    }

    /// Fills `out_interleaved_lr` (`[L0, R0, L1, R1, ...]`) with either the
    /// generated test signal (`active = true`) or pure silence (`active = false`).
    pub fn fill_stereo(&mut self, out_interleaved_lr: &mut [f32], active: bool) {
        if !active {
            out_interleaved_lr.fill(0.0);
            return;
        }

        let frames = out_interleaved_lr.len() / 2;
        for i in 0..frames {
            let t = self.sample_index as f32 / self.sample_rate;
            self.sample_index += 1;

            // 1. Sine sweep: cycles smoothly from 200 Hz to 2,000 Hz every 4 seconds.
            let sweep_pos = (t % 4.0) / 4.0;
            let sweep_freq = 200.0 + 1800.0 * sweep_pos;
            self.sweep_phase = (self.sweep_phase + TAU * sweep_freq / self.sample_rate) % TAU;
            let sweep = 0.25 * self.sweep_phase.sin();

            // 2. Low-frequency bass pulse at 120 BPM (one 65 Hz kick every 0.5 seconds).
            let beat_time = t % 0.5;
            let envelope = (-beat_time * 16.0).exp(); // sharp attack, fast ~60ms decay
            self.pulse_phase = (self.pulse_phase + TAU * 65.0 / self.sample_rate) % TAU;
            let pulse = 0.65 * envelope * self.pulse_phase.sin();

            // 3. Subtle white noise using a fast deterministic xorshift32 PRNG.
            let noise = 0.08 * self.next_noise();

            // Give Left and Right a slight stereo difference on the sweep for wider visuals.
            let left = (pulse + sweep + noise).clamp(-1.0, 1.0);
            let right = (pulse + 0.25 * (self.sweep_phase + 0.6).sin() + noise).clamp(-1.0, 1.0);

            out_interleaved_lr[2 * i] = left;
            out_interleaved_lr[2 * i + 1] = right;
        }
    }

    /// Fast deterministic pseudo-random float in `-1.0..=1.0`.
    fn next_noise(&mut self) -> f32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng_state = x;
        // Convert u32 to [-1.0, 1.0]
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}
