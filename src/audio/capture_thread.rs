//! Background WASAPI Loopback capture thread feeding a lock-free `RingProducer`.
//!
//! Architecture:
//! - Background thread owns `WasapiLoopbackSource` (COM MTA initialized on that thread)
//!   and continuously reads WASAPI packets, pushing timestamped stereo `f32` frames into
//!   the lock-free SPSC ring buffer.
//! - Main OpenGL render thread owns `RingConsumer` and reads audio without ever blocking
//!   or calling Windows audio APIs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::audio::ring_buffer::{create_audio_ring, PopResult, RingConsumer};
use crate::audio::wasapi_loopback::WasapiLoopbackSource;
use crate::audio::{AudioSource, AudioStreamFormat};

/// Shared counters updated atomically by the capture thread for logging/UI.
struct CaptureCounters {
    packets_received: AtomicU64,
    silence_injections: AtomicU64,
    device_lost_events: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// Manages one live WASAPI loopback capture thread and its lock-free ring buffer.
pub struct LiveAudioCapture {
    format: AudioStreamFormat,
    consumer: RingConsumer,
    stop_flag: Arc<AtomicBool>,
    counters: Arc<CaptureCounters>,
    thread_handle: Option<JoinHandle<()>>,
}

impl LiveAudioCapture {
    /// Starts capturing on a dedicated background thread for the chosen Windows output
    /// device (or the Windows default output device if `device_id` is `None`).
    pub fn start(device_id: Option<String>) -> Result<Self, String> {
        // 4,096 stereo frames (~85 ms @ 48 kHz) gives headroom for brief render spikes
        // without adding latency (we drain all arrived frames on every 16.7 ms render tick).
        let (mut producer, consumer) = create_audio_ring(4096);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(CaptureCounters {
            packets_received: AtomicU64::new(0),
            silence_injections: AtomicU64::new(0),
            device_lost_events: AtomicU64::new(0),
            last_error: Mutex::new(None),
        });

        // One-shot channel to report startup `Result<AudioStreamFormat, String>` back
        // from the capture thread so WASAPI COM handles live strictly on that thread.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<AudioStreamFormat, String>>();

        let stop_bg = Arc::clone(&stop_flag);
        let counters_bg = Arc::clone(&counters);

        let thread_handle = thread::Builder::new()
            .name("wasapi-loopback-capture".to_string())
            .spawn(move || {
                let mut src = match WasapiLoopbackSource::new() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                if let Some(ref id) = device_id {
                    if let Err(e) = src.select_device(id) {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                }
                let fmt = match src.start() {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                let sample_rate = fmt.output_sample_rate;
                let _ = init_tx.send(Ok(fmt));

                // Read ~10 ms chunks (480 stereo frames @ 48 kHz, matching WASAPI period).
                let chunk_frames = ((sample_rate as usize) / 100).max(256);
                let mut buf = vec![0.0_f32; chunk_frames * 2];
                let mut already_reported_lost = false;

                while !stop_bg.load(Ordering::Relaxed) {
                    match src.read_pcm_stereo(&mut buf) {
                        Ok(res) => {
                            if res.packets_read > 0 {
                                counters_bg
                                    .packets_received
                                    .fetch_add(res.packets_read as u64, Ordering::Relaxed);
                            }
                            if res.silence_injected {
                                counters_bg
                                    .silence_injections
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            if let Some(lost_msg) = res.device_lost_error {
                                if !already_reported_lost {
                                    already_reported_lost = true;
                                    counters_bg
                                        .device_lost_events
                                        .fetch_add(1, Ordering::Relaxed);
                                    if let Ok(mut err_slot) = counters_bg.last_error.lock() {
                                        *err_slot = Some(lost_msg);
                                    }
                                }
                            }
                            if res.frames_written > 0 {
                                let arrival_us = producer.now_us();
                                producer.push_interleaved_stereo(
                                    &buf[..res.frames_written * 2],
                                    sample_rate,
                                    arrival_us,
                                );
                            }
                        }
                        Err(e) => {
                            if let Ok(mut err_slot) = counters_bg.last_error.lock() {
                                *err_slot = Some(e);
                            }
                            break;
                        }
                    }
                }

                src.stop();
            })
            .map_err(|e| format!("Failed to spawn WASAPI capture thread: {e}"))?;

        let format = init_rx
            .recv()
            .map_err(|_| "WASAPI capture thread exited before reporting format".to_string())??;

        Ok(Self {
            format,
            consumer,
            stop_flag,
            counters,
            thread_handle: Some(thread_handle),
        })
    }

    pub fn format(&self) -> &AudioStreamFormat {
        &self.format
    }

    /// Non-blocking pop called from the main render thread once per video frame.
    pub fn pop_stereo(&mut self, out_interleaved_lr: &mut Vec<f32>, max_frames: usize) -> PopResult {
        self.consumer
            .pop_interleaved_stereo(out_interleaved_lr, max_frames)
    }

    pub fn overrun_frames(&self) -> u64 {
        self.consumer.overrun_frames()
    }

    pub fn underrun_reads(&self) -> u64 {
        self.consumer.underrun_reads()
    }

    pub fn packets_received(&self) -> u64 {
        self.counters.packets_received.load(Ordering::Relaxed)
    }

    pub fn silence_injections(&self) -> u64 {
        self.counters.silence_injections.load(Ordering::Relaxed)
    }

    pub fn device_lost_events(&self) -> u64 {
        self.counters.device_lost_events.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.counters.last_error.lock().ok().and_then(|g| g.clone())
    }
}

impl Drop for LiveAudioCapture {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}
