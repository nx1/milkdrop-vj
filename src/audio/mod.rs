//! Swappable audio input layer (`AudioSource` trait) and PCM format conversion.
//!
//! Why a trait?
//! A Rust `trait` is like a Python abstract base class. Defining one `AudioSource`
//! interface means the visualizer decks never care whether audio comes from:
//! - Windows WASAPI Loopback (`WasapiLoopbackSource`, Milestone 1)
//! - Our deterministic test signal (`TestAudioSource`, for testing without music)
//! - External Line-In / USB DJ mixer capture (Milestone 4)

pub mod beat_pulse;
pub mod capture_thread;
pub mod ring_buffer;
pub mod test_source;
pub mod visual_eq;
pub mod wasapi_loopback;

/// Describes one selectable audio endpoint (for example, laptop speakers or a USB DJ output).
#[derive(Debug, Clone)]
pub struct AudioDeviceInfo {
    /// Stable OS identifier passed to `AudioSource::select_device()`.
    pub id: String,
    /// Human-readable name shown to the user (for example `"Speakers (Realtek Audio)"`).
    pub name: String,
    /// True if Windows currently treats this device as the default playback endpoint.
    pub is_default: bool,
}

/// Sample encoding reported by the Windows audio device before conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSampleType {
    Float32,
    Int16,
    Int24,
    Int32,
}

impl std::fmt::Display for NativeSampleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeSampleType::Float32 => write!(f, "32-bit Float"),
            NativeSampleType::Int16 => write!(f, "16-bit Int"),
            NativeSampleType::Int24 => write!(f, "24-bit Int"),
            NativeSampleType::Int32 => write!(f, "32-bit Int"),
        }
    }
}

/// Describes both the device's native Windows mix format and our normalized output format.
#[derive(Debug, Clone)]
pub struct AudioStreamFormat {
    pub device_name: String,
    pub native_sample_rate: u32,
    pub native_channels: u16,
    pub native_sample_type: NativeSampleType,
    pub output_sample_rate: u32,
    pub output_channels: u16,
    pub capture_mode_label: String,
}

/// Outcome of one `AudioSource::read_pcm_stereo()` call.
#[derive(Debug, Clone)]
pub struct AudioReadResult {
    /// Number of stereo frames written (`frames * 2` floats in `out_interleaved_lr`).
    pub frames_written: usize,
    /// Number of raw audio packets received from the OS during this read.
    pub packets_read: usize,
    /// True when the OS delivered no audio (e.g. nothing playing) and we injected silence.
    pub silence_injected: bool,
    /// Set if the audio device was unplugged, disabled, or invalidated by Windows.
    pub device_lost_error: Option<String>,
}

/// Swappable audio source interface used by both the Stage A console tool and Stage B decks.
pub trait AudioSource {
    /// Lists all currently active devices supported by this backend.
    fn list_devices(&self) -> Result<Vec<AudioDeviceInfo>, String>;

    /// Selects a device by its `AudioDeviceInfo::id` (call before `start()`).
    fn select_device(&mut self, id: &str) -> Result<(), String>;

    /// Opens and starts audio capture, returning the native and output stream formats.
    fn start(&mut self) -> Result<AudioStreamFormat, String>;

    /// Stops audio capture and releases OS handles cleanly.
    fn stop(&mut self);

    /// Fills `out_interleaved_lr` (`[L0, R0, L1, R1, ...]`, `f32` in `-1.0..=1.0`)
    /// with the next available stereo audio chunk. Must NEVER block forever on silence.
    fn read_pcm_stereo(&mut self, out_interleaved_lr: &mut [f32]) -> Result<AudioReadResult, String>;
}

/// Converts raw interleaved bytes from any supported native Windows format (`Float32`,
/// `Int16`, `Int24`, `Int32`) and any channel count (`1`, `2`, `4`, `6`, `8`, ...) into
/// stereo interleaved `f32` samples (`[L, R, L, R, ...]`) clamped to `-1.0..=1.0`.
///
/// Channel mapping rules:
/// - **1 channel (mono):** Duplicated to both Left and Right (`L = M, R = M`).
/// - **2 channels (stereo):** Copied directly (`L = ch0, R = ch1`).
/// - **>2 channels (e.g. 5.1 / 7.1 surround):** Standard Windows channel order puts
///   Front-Left (`0`), Front-Right (`1`), Center (`2`), LFE (`3`), Rear/Side pairs (`4+`).
///   We mix Front-Left/Right + Center (`0.707`) + even/odd surround pairs (`0.5`),
///   normalized cleanly so loud multi-channel audio does not clip.
pub fn convert_raw_bytes_to_stereo_f32(
    raw_bytes: &[u8],
    channels: usize,
    sample_type: NativeSampleType,
    out_stereo: &mut Vec<f32>,
) {
    if channels == 0 {
        return;
    }

    let bytes_per_sample = match sample_type {
        NativeSampleType::Int16 => 2,
        NativeSampleType::Int24 => 3,
        NativeSampleType::Float32 | NativeSampleType::Int32 => 4,
    };
    let frame_bytes = bytes_per_sample * channels;
    if frame_bytes == 0 || raw_bytes.len() < frame_bytes {
        return;
    }

    let frame_count = raw_bytes.len() / frame_bytes;
    out_stereo.reserve(frame_count * 2);

    let mut frame_samples = [0.0_f32; 16];
    let ch_clamped = channels.min(frame_samples.len());

    for frame in raw_bytes.chunks_exact(frame_bytes) {
        for ch in 0..ch_clamped {
            let offset = ch * bytes_per_sample;
            let sample = match sample_type {
                NativeSampleType::Float32 => f32::from_le_bytes([
                    frame[offset],
                    frame[offset + 1],
                    frame[offset + 2],
                    frame[offset + 3],
                ]),
                NativeSampleType::Int16 => {
                    let s = i16::from_le_bytes([frame[offset], frame[offset + 1]]);
                    s as f32 / 32768.0
                }
                NativeSampleType::Int24 => {
                    // Sign-extend 24-bit little-endian integer into i32.
                    let raw = (frame[offset] as i32)
                        | ((frame[offset + 1] as i32) << 8)
                        | ((frame[offset + 2] as i8 as i32) << 16);
                    raw as f32 / 8_388_608.0
                }
                NativeSampleType::Int32 => {
                    let s = i32::from_le_bytes([
                        frame[offset],
                        frame[offset + 1],
                        frame[offset + 2],
                        frame[offset + 3],
                    ]);
                    s as f32 / 2_147_483_648.0
                }
            };
            frame_samples[ch] = if sample.is_finite() { sample } else { 0.0 };
        }

        let (left, right) = downmix_frame_to_stereo(&frame_samples[..ch_clamped]);
        out_stereo.push(left.clamp(-1.0, 1.0));
        out_stereo.push(right.clamp(-1.0, 1.0));
    }
}

/// Downmixes one audio frame of 1..N channels into `(left, right)`.
fn downmix_frame_to_stereo(ch: &[f32]) -> (f32, f32) {
    match ch.len() {
        0 => (0.0, 0.0),
        1 => (ch[0], ch[0]),
        2 => (ch[0], ch[1]),
        _ => {
            // Front Left (0) and Front Right (1)
            let mut l = ch[0];
            let mut r = ch[1];
            let mut weight = 1.0_f32;

            // Center channel (2) feeds both Left and Right at -3 dB (~0.707)
            if ch.len() >= 3 {
                l += ch[2] * 0.707;
                r += ch[2] * 0.707;
                weight += 0.707;
            }
            // Skip LFE (3) for balanced full-range visual energy, or add gently;
            // mix remaining surround pairs (4=BL, 5=BR, 6=SL, 7=SR, ...) at 0.5.
            let mut idx = 4;
            while idx + 1 < ch.len() {
                l += ch[idx] * 0.5;
                r += ch[idx + 1] * 0.5;
                weight += 0.5;
                idx += 2;
            }
            if idx < ch.len() {
                l += ch[idx] * 0.5;
                r += ch[idx] * 0.5;
                weight += 0.5;
            }

            (l / weight, r / weight)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_mono_i16_to_stereo_f32() {
        let raw = 16384_i16.to_le_bytes(); // +0.5
        let mut out = Vec::new();
        convert_raw_bytes_to_stereo_f32(&raw, 1, NativeSampleType::Int16, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 1e-4);
        assert!((out[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn downmixes_6ch_surround_f32_to_stereo_without_clipping() {
        // 5.1 frame: FL=1.0, FR=0.0, C=1.0, LFE=0.0, BL=1.0, BR=0.0
        let channels = [1.0_f32, 0.0, 1.0, 0.0, 1.0, 0.0];
        let mut raw = Vec::new();
        for s in channels {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = Vec::new();
        convert_raw_bytes_to_stereo_f32(&raw, 6, NativeSampleType::Float32, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0] > out[1], "Left channel should be stronger than Right");
        assert!(out[0] <= 1.0 && out[1] >= 0.0);
    }
}
