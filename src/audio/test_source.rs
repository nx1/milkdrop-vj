//! Synthetic test-audio implementation of `AudioSource`.
//!
//! Wrapping our deterministic `TestAudioGenerator` inside `AudioSource` lets the
//! rest of the program swap seamlessly between live Windows WASAPI audio and
//! generated test audio without changing a single line of Deck or UI code.

use crate::audio::{
    AudioDeviceInfo, AudioReadResult, AudioSource, AudioStreamFormat, NativeSampleType,
};
use crate::test_audio::TestAudioGenerator;

pub const TEST_SOURCE_DEVICE_ID: &str = "synthetic:test-signal";

pub struct TestAudioSource {
    sample_rate: u32,
    generator: TestAudioGenerator,
    active: bool,
    running: bool,
}

impl TestAudioSource {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            generator: TestAudioGenerator::new(sample_rate as f32),
            active: true,
            running: false,
        }
    }

    /// Toggles between generating the test signal and pure silence.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }
}

impl AudioSource for TestAudioSource {
    fn list_devices(&self) -> Result<Vec<AudioDeviceInfo>, String> {
        Ok(vec![AudioDeviceInfo {
            id: TEST_SOURCE_DEVICE_ID.to_string(),
            name: "Synthetic Test Signal (200-2000Hz Sweep + 120 BPM Bass Pulse + Noise)".to_string(),
            is_default: false,
        }])
    }

    fn select_device(&mut self, id: &str) -> Result<(), String> {
        if id == TEST_SOURCE_DEVICE_ID {
            Ok(())
        } else {
            Err(format!("Unknown synthetic audio device id `{id}`"))
        }
    }

    fn start(&mut self) -> Result<AudioStreamFormat, String> {
        self.running = true;
        Ok(AudioStreamFormat {
            device_name: "Synthetic Test Signal".to_string(),
            native_sample_rate: self.sample_rate,
            native_channels: 2,
            native_sample_type: NativeSampleType::Float32,
            output_sample_rate: self.sample_rate,
            output_channels: 2,
            capture_mode_label: "Synthetic Generator".to_string(),
        })
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn read_pcm_stereo(&mut self, out_interleaved_lr: &mut [f32]) -> Result<AudioReadResult, String> {
        if !self.running {
            return Err("TestAudioSource is not started".to_string());
        }
        self.generator.fill_stereo(out_interleaved_lr, self.active);
        let frames = out_interleaved_lr.len() / 2;
        Ok(AudioReadResult {
            frames_written: frames,
            packets_read: if self.active { 1 } else { 0 },
            silence_injected: !self.active,
            device_lost_error: None,
        })
    }
}
