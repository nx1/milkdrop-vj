//! Windows WASAPI Loopback implementation of `AudioSource` (using `wasapi 0.27.0`).
//!
//! How WASAPI Loopback works on Windows:
//! - Normally, an output device (speakers, headphones, USB DJ Master Out) is a
//!   `Direction::Render` device.
//! - Opening a `Direction::Render` device's `IAudioClient` and initializing it with
//!   `Direction::Capture` tells `wasapi` to set `AUDCLNT_STREAMFLAGS_LOOPBACK`, which
//!   gives us a read-only copy of whatever Windows is mixing to that output device.
//! - **Silence quirk:** When nothing is playing on that output device, Windows stops
//!   delivering loopback packets. We wait with a short `20 ms` event timeout so
//!   `read_pcm_stereo()` NEVER blocks forever—when no packets arrive, it outputs
//!   zeroed silence frames and sets `silence_injected = true`.
//! - **Device unplug / invalidation:** If the device is unplugged or disabled while
//!   capturing, Windows returns an error (such as `AUDCLNT_E_DEVICE_INVALIDATED`).
//!   We catch that cleanly, stop the broken client, set `device_lost_error = Some(...)`,
//!   and keep returning silence instead of crashing.

use std::collections::VecDeque;
use std::thread;
use std::time::Duration;

use wasapi::{
    initialize_mta, AudioCaptureClient, AudioClient, DeviceEnumerator, DeviceState, Direction,
    Handle, SampleType, StreamMode, WaveFormat,
};

use crate::audio::{
    convert_raw_bytes_to_stereo_f32, AudioDeviceInfo, AudioReadResult, AudioSource,
    AudioStreamFormat, NativeSampleType,
};

/// Active WASAPI capture handles held only while `start()` is running.
struct ActiveCapture {
    audio_client: AudioClient,
    capture_client: AudioCaptureClient,
    event_handle: Option<Handle>,
    native_channels: usize,
    native_sample_type: NativeSampleType,
    native_sample_rate: u32,
    raw_bytes: VecDeque<u8>,
    stereo_queue: VecDeque<f32>,
    device_lost: Option<String>,
}

/// Captures live audio playing on any active Windows playback (render) endpoint.
pub struct WasapiLoopbackSource {
    selected_device_id: Option<String>,
    active: Option<ActiveCapture>,
}

impl WasapiLoopbackSource {
    pub fn new() -> Result<Self, String> {
        // Initialize COM on this thread (Multi-Threaded Apartment).
        // Ignore errors if COM is already initialized in another mode on this thread.
        let _ = initialize_mta();
        Ok(Self {
            selected_device_id: None,
            active: None,
        })
    }

    /// Queries the current Windows default Render (output) device ID and name.
    /// Useful for detecting when the user switches default output in Windows Settings.
    pub fn query_current_default_render_device() -> Option<(String, String)> {
        let _ = initialize_mta();
        let enumerator = DeviceEnumerator::new().ok()?;
        let dev = enumerator.get_default_device(Direction::Render).ok()?;
        let id = dev.get_id().ok()?;
        let name = dev.get_friendlyname().ok()?;
        Some((id, name))
    }
}

impl AudioSource for WasapiLoopbackSource {
    fn list_devices(&self) -> Result<Vec<AudioDeviceInfo>, String> {
        let _ = initialize_mta();
        let enumerator = DeviceEnumerator::new()
            .map_err(|e| format!("Failed to create WASAPI DeviceEnumerator: {e}"))?;

        let default_id = enumerator
            .get_default_device(Direction::Render)
            .ok()
            .and_then(|d| d.get_id().ok());

        let collection = enumerator
            .get_device_collection(Direction::Render)
            .map_err(|e| format!("Failed to enumerate Windows output devices: {e}"))?;

        let mut devices = Vec::new();
        for item in &collection {
            let dev = match item {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Only list currently active (plugged-in, enabled) playback endpoints.
            let state = match dev.get_state() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !matches!(state, DeviceState::Active) {
                continue;
            }
            let id = dev.get_id().unwrap_or_default();
            let name = dev
                .get_friendlyname()
                .unwrap_or_else(|_| "<Unnamed Windows Audio Device>".to_string());
            let is_default = default_id.as_deref() == Some(id.as_str());
            devices.push(AudioDeviceInfo {
                id,
                name,
                is_default,
            });
        }

        // Put the Windows default output device first for convenience.
        devices.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
        Ok(devices)
    }

    fn select_device(&mut self, id: &str) -> Result<(), String> {
        self.selected_device_id = Some(id.to_string());
        Ok(())
    }

    fn start(&mut self) -> Result<AudioStreamFormat, String> {
        self.stop();
        let _ = initialize_mta();

        let enumerator = DeviceEnumerator::new()
            .map_err(|e| format!("Failed to create WASAPI DeviceEnumerator: {e}"))?;

        let device = match &self.selected_device_id {
            Some(id) => enumerator
                .get_device(id)
                .map_err(|e| format!("Failed to open selected audio device `{id}`: {e}"))?,
            None => enumerator
                .get_default_device(Direction::Render)
                .map_err(|e| format!("Failed to open default Windows output device: {e}"))?,
        };

        let device_name = device
            .get_friendlyname()
            .unwrap_or_else(|_| "<Unknown Output Device>".to_string());

        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| format!("Failed to get IAudioClient for `{device_name}`: {e}"))?;

        // Read the device's exact native Windows mix format (sample rate, channels, bit depth).
        let native_fmt = audio_client
            .get_mixformat()
            .map_err(|e| format!("Failed to query native mix format for `{device_name}`: {e}"))?;

        let native_sample_rate = native_fmt.get_samplespersec();
        let native_channels = native_fmt.get_nchannels();
        let native_sample_type = classify_wave_format(&native_fmt)?;

        let (def_period_hns, _min_period_hns) = audio_client
            .get_device_period()
            .map_err(|e| format!("Failed to query WASAPI device period: {e}"))?;

        // Try event-driven shared loopback first (lowest latency, clean 20 ms silence timeout).
        // If a driver refuses event-driven loopback, fall back to PollingShared cleanly.
        let event_mode = StreamMode::EventsShared {
            autoconvert: false,
            buffer_duration_hns: def_period_hns * 2,
        };

        let (event_handle, capture_mode_label) = match audio_client.initialize_client(
            &native_fmt,
            Direction::Capture,
            event_mode,
            None,
        ) {
            Ok(()) => {
                let h = audio_client
                    .set_get_eventhandle()
                    .map_err(|e| format!("Failed to create WASAPI event handle: {e}"))?;
                (Some(h), "WASAPI Loopback (EventsShared, native format)".to_string())
            }
            Err(ev_err) => {
                // Re-acquire a fresh IAudioClient for PollingShared fallback.
                audio_client = device.get_iaudioclient().map_err(|e| {
                    format!(
                        "EventsShared failed ({ev_err}) and re-opening IAudioClient failed: {e}"
                    )
                })?;
                let poll_mode = StreamMode::PollingShared {
                    autoconvert: false,
                    buffer_duration_hns: def_period_hns * 2,
                };
                audio_client
                    .initialize_client(&native_fmt, Direction::Capture, poll_mode, None)
                    .map_err(|poll_err| {
                        format!(
                            "Both EventsShared ({ev_err}) and PollingShared ({poll_err}) failed on `{device_name}`"
                        )
                    })?;
                (None, "WASAPI Loopback (PollingShared fallback, native format)".to_string())
            }
        };

        let capture_client = audio_client
            .get_audiocaptureclient()
            .map_err(|e| format!("Failed to get IAudioCaptureClient: {e}"))?;

        audio_client
            .start_stream()
            .map_err(|e| format!("Failed to start WASAPI loopback stream: {e}"))?;

        self.active = Some(ActiveCapture {
            audio_client,
            capture_client,
            event_handle,
            native_channels: native_channels as usize,
            native_sample_type,
            native_sample_rate,
            raw_bytes: VecDeque::with_capacity(65_536),
            stereo_queue: VecDeque::with_capacity(16_384),
            device_lost: None,
        });

        Ok(AudioStreamFormat {
            device_name,
            native_sample_rate,
            native_channels,
            native_sample_type,
            output_sample_rate: native_sample_rate,
            output_channels: 2,
            capture_mode_label,
        })
    }

    fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active.audio_client.stop_stream();
        }
    }

    fn read_pcm_stereo(&mut self, out_interleaved_lr: &mut [f32]) -> Result<AudioReadResult, String> {
        let frames_requested = out_interleaved_lr.len() / 2;
        if frames_requested == 0 {
            return Ok(AudioReadResult {
                frames_written: 0,
                packets_read: 0,
                silence_injected: false,
                device_lost_error: None,
            });
        }

        let Some(active) = self.active.as_mut() else {
            out_interleaved_lr.fill(0.0);
            return Err("WasapiLoopbackSource is not started".to_string());
        };

        // If the device was already lost earlier, keep returning clean silence without crashing.
        if let Some(err) = active.device_lost.clone() {
            thread::sleep(Duration::from_millis(16));
            out_interleaved_lr.fill(0.0);
            return Ok(AudioReadResult {
                frames_written: frames_requested,
                packets_read: 0,
                silence_injected: true,
                device_lost_error: Some(err),
            });
        }

        let mut packets_read = 0usize;

        // Wait up to 20 ms for a WASAPI packet if we do not already have enough stereo samples.
        if active.stereo_queue.len() < frames_requested * 2 {
            if let Some(ref handle) = active.event_handle {
                // `wait_for_event(20)` returns Err on timeout when Windows sends no loopback
                // packets (silence). Timeout is NORMAL during silence — never treat it as fatal!
                let _ = handle.wait_for_event(20);
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Drain all available WASAPI packets without blocking.
        loop {
            match active.capture_client.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_frames_avail)) => {
                    match active
                        .capture_client
                        .read_from_device_to_deque(&mut active.raw_bytes)
                    {
                        Ok(_info) => {
                            packets_read += 1;
                            if !active.raw_bytes.is_empty() {
                                let contiguous: Vec<u8> = active.raw_bytes.drain(..).collect();
                                let mut converted = Vec::new();
                                convert_raw_bytes_to_stereo_f32(
                                    &contiguous,
                                    active.native_channels,
                                    active.native_sample_type,
                                    &mut converted,
                                );
                                active.stereo_queue.extend(converted);
                            }
                        }
                        Err(e) => {
                            let msg = format!("WASAPI capture read failed (device lost/changed?): {e}");
                            active.device_lost = Some(msg.clone());
                            out_interleaved_lr.fill(0.0);
                            return Ok(AudioReadResult {
                                frames_written: frames_requested,
                                packets_read,
                                silence_injected: true,
                                device_lost_error: Some(msg),
                            });
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("WASAPI get_next_packet_size failed (device lost/changed?): {e}");
                    active.device_lost = Some(msg.clone());
                    out_interleaved_lr.fill(0.0);
                    return Ok(AudioReadResult {
                        frames_written: frames_requested,
                        packets_read,
                        silence_injected: true,
                        device_lost_error: Some(msg),
                    });
                }
            }
        }

        // Cap buffered audio to ~100 ms so latency can never drift upward if the caller falls behind.
        let max_buffered_floats = (active.native_sample_rate as usize / 10) * 2;
        if active.stereo_queue.len() > max_buffered_floats {
            let excess = active.stereo_queue.len() - max_buffered_floats;
            active.stereo_queue.drain(..excess);
        }

        let needed_floats = frames_requested * 2;
        if active.stereo_queue.len() >= needed_floats {
            for sample in out_interleaved_lr.iter_mut().take(needed_floats) {
                *sample = active.stereo_queue.pop_front().unwrap_or(0.0);
            }
            Ok(AudioReadResult {
                frames_written: frames_requested,
                packets_read,
                silence_injected: false,
                device_lost_error: None,
            })
        } else if !active.stereo_queue.is_empty() {
            // Partial packet available: output what arrived and zero-pad the rest.
            let avail_frames = active.stereo_queue.len() / 2;
            let avail_floats = avail_frames * 2;
            for sample in out_interleaved_lr.iter_mut().take(avail_floats) {
                *sample = active.stereo_queue.pop_front().unwrap_or(0.0);
            }
            out_interleaved_lr[avail_floats..needed_floats].fill(0.0);
            Ok(AudioReadResult {
                frames_written: frames_requested,
                packets_read,
                silence_injected: false,
                device_lost_error: None,
            })
        } else {
            // True silence: Windows delivered zero loopback packets during the 20 ms wait.
            // Inject zeroed silence frames so the caller never blocks and visuals stay alive.
            out_interleaved_lr.fill(0.0);
            Ok(AudioReadResult {
                frames_written: frames_requested,
                packets_read,
                silence_injected: true,
                device_lost_error: None,
            })
        }
    }
}

impl Drop for WasapiLoopbackSource {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Inspects a Windows `WaveFormat` to identify its sample encoding (`Float32`, `Int16`, `Int24`, `Int32`).
fn classify_wave_format(fmt: &WaveFormat) -> Result<NativeSampleType, String> {
    let bits = fmt.get_bitspersample();
    let subformat = fmt
        .get_subformat()
        .map_err(|e| format!("Unsupported WASAPI wave subformat: {e}"))?;

    match (subformat, bits) {
        (SampleType::Float, 32) => Ok(NativeSampleType::Float32),
        (SampleType::Int, 16) => Ok(NativeSampleType::Int16),
        (SampleType::Int, 24) => Ok(NativeSampleType::Int24),
        (SampleType::Int, 32) => Ok(NativeSampleType::Int32),
        (other_type, other_bits) => Err(format!(
            "Unsupported native WASAPI sample format: {other_type:?}, {other_bits}-bit"
        )),
    }
}
