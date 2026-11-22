//! Lock-free Single-Producer Single-Consumer (SPSC) Ring Buffer for stereo audio.
//!
//! Plain-language guide to the Rust concurrency concepts used here:
//! - **Thread:** An independent stream of execution running alongside the main
//!   window loop. Our WASAPI capture runs on its own background thread so waiting
//!   for Windows audio events never stalls 60 FPS OpenGL rendering.
//! - **Ring buffer:** A fixed-size circular array with a `write_pos` (moved only
//!   by the audio capture thread) and a `read_pos` (moved only by the main render
//!   thread). Because memory is allocated once up front, live audio never triggers
//!   heap allocations or locks.
//! - **Atomics (`AtomicUsize`, `AtomicU64`):** Special numbers that two threads
//!   can read and update safely without a mutex lock. `Ordering::Release` on writes
//!   and `Ordering::Acquire` on reads guarantee that when the render thread sees a
//!   new `write_pos`, the audio samples written before it are already visible in RAM.
//! - **`Send` marker trait:** A compile-time badge in Rust meaning "this value is
//!   safe to move to another thread." `RingProducer` and `RingConsumer` are `Send`
//!   so we can move `RingProducer` into `std::thread::spawn(...)`, while OpenGL
//!   decks are NOT `Send` and stay locked to the main thread.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// One timestamped stereo audio frame stored in the ring buffer.
#[derive(Clone, Copy, Default)]
pub struct TimedStereoFrame {
    pub left: f32,
    pub right: f32,
    /// Microseconds since `RingInner::epoch` when this sample was captured.
    pub captured_at_us: u64,
}

struct RingInner {
    /// Power-of-two slot count so wrapping uses a fast bitmask (`pos & mask`).
    mask: usize,
    capacity: usize,
    /// Fixed pre-allocated slots. Safety: each index is written ONLY by the single
    /// producer while outside `[read_pos, write_pos)` and read ONLY by the single
    /// consumer while inside `[read_pos, write_pos)`.
    slots: Box<[UnsafeCell<TimedStereoFrame>]>,
    write_pos: AtomicUsize,
    read_pos: AtomicUsize,
    /// Total stereo frames dropped because the buffer was full when the producer wrote.
    overrun_frames: AtomicU64,
    /// Total render-side pops that found the buffer empty (underrun / silence).
    underrun_reads: AtomicU64,
    epoch: Instant,
}

// SAFETY: `RingInner` enforces strict single-producer / single-consumer disjoint slot
// access synchronized by `Acquire`/`Release` atomics on `write_pos` and `read_pos`.
unsafe impl Sync for RingInner {}

/// Producer endpoint moved onto the background audio capture thread.
pub struct RingProducer {
    inner: Arc<RingInner>,
}

/// Consumer endpoint kept on the main OpenGL render thread.
pub struct RingConsumer {
    inner: Arc<RingInner>,
}

/// Creates a connected `(RingProducer, RingConsumer)` pair with `capacity_frames`
/// rounded up to the next power of two (minimum 1024 frames).
pub fn create_audio_ring(capacity_frames: usize) -> (RingProducer, RingConsumer) {
    let cap = capacity_frames.max(1024).next_power_of_two();
    let mut vec = Vec::with_capacity(cap);
    for _ in 0..cap {
        vec.push(UnsafeCell::new(TimedStereoFrame::default()));
    }
    let inner = Arc::new(RingInner {
        mask: cap - 1,
        capacity: cap,
        slots: vec.into_boxed_slice(),
        write_pos: AtomicUsize::new(0),
        read_pos: AtomicUsize::new(0),
        overrun_frames: AtomicU64::new(0),
        underrun_reads: AtomicU64::new(0),
        epoch: Instant::now(),
    });
    (
        RingProducer {
            inner: Arc::clone(&inner),
        },
        RingConsumer { inner },
    )
}

impl RingProducer {
    /// Returns microseconds elapsed since this ring buffer was created.
    pub fn now_us(&self) -> u64 {
        self.inner.epoch.elapsed().as_micros() as u64
    }

    /// Pushes interleaved stereo `f32` samples (`[L0, R0, L1, R1, ...]`) captured at
    /// `sample_rate` Hz whose newest sample arrived at `packet_arrival_us`.
    ///
    /// Each frame receives an interpolated capture timestamp so the render thread
    /// can measure the exact age of the oldest sample reaching `libprojectM`.
    /// Returns `(pushed_frames, dropped_overrun_frames)`.
    pub fn push_interleaved_stereo(
        &mut self,
        interleaved_lr: &[f32],
        sample_rate: u32,
        packet_arrival_us: u64,
    ) -> (usize, usize) {
        let frames = interleaved_lr.len() / 2;
        if frames == 0 {
            return (0, 0);
        }

        let sr = sample_rate.max(1) as u64;
        let mut write = self.inner.write_pos.load(Ordering::Relaxed);
        let read = self.inner.read_pos.load(Ordering::Acquire);
        let used = write.wrapping_sub(read);
        let free = self.inner.capacity.saturating_sub(used);

        let to_push = frames.min(free);
        let dropped = frames - to_push;

        // If some frames must be dropped due to overrun, keep the NEWEST frames
        // so visual latency stays tight rather than playing stale audio.
        let start_idx = dropped;
        for idx in 0..to_push {
            let frame_i = start_idx + idx;
            let frames_from_end = (frames - 1 - frame_i) as u64;
            let offset_us = frames_from_end.saturating_mul(1_000_000) / sr;
            let captured_at_us = packet_arrival_us.saturating_sub(offset_us);

            let slot_idx = write & self.inner.mask;
            let frame = TimedStereoFrame {
                left: interleaved_lr[2 * frame_i],
                right: interleaved_lr[2 * frame_i + 1],
                captured_at_us,
            };
            unsafe {
                *self.inner.slots[slot_idx].get() = frame;
            }
            write = write.wrapping_add(1);
        }

        if to_push > 0 {
            self.inner.write_pos.store(write, Ordering::Release);
        }
        if dropped > 0 {
            self.inner
                .overrun_frames
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        (to_push, dropped)
    }
}

/// Result returned when the main render thread pops audio from `RingConsumer`.
pub struct PopResult {
    /// Number of stereo frames copied into `out_interleaved_lr`.
    pub frames_read: usize,
    /// Exact age (in milliseconds) of the oldest sample popped during this call,
    /// or `None` if the ring buffer was empty (underrun).
    pub oldest_sample_age_ms: Option<f64>,
}

impl RingConsumer {
    /// Pops up to `max_frames` stereo frames into `out_interleaved_lr` without blocking.
    /// If the buffer is empty, increments `underrun_reads` and returns `frames_read = 0`.
    pub fn pop_interleaved_stereo(
        &mut self,
        out_interleaved_lr: &mut Vec<f32>,
        max_frames: usize,
    ) -> PopResult {
        out_interleaved_lr.clear();
        let mut read = self.inner.read_pos.load(Ordering::Relaxed);
        let write = self.inner.write_pos.load(Ordering::Acquire);
        let available = write.wrapping_sub(read);

        if available == 0 {
            self.inner.underrun_reads.fetch_add(1, Ordering::Relaxed);
            return PopResult {
                frames_read: 0,
                oldest_sample_age_ms: None,
            };
        }

        let to_read = available.min(max_frames);
        out_interleaved_lr.reserve(to_read * 2);

        let now_us = self.inner.epoch.elapsed().as_micros() as u64;
        let mut oldest_us: Option<u64> = None;

        for _ in 0..to_read {
            let slot_idx = read & self.inner.mask;
            let frame = unsafe { *self.inner.slots[slot_idx].get() };
            if oldest_us.is_none() {
                oldest_us = Some(frame.captured_at_us);
            }
            out_interleaved_lr.push(frame.left);
            out_interleaved_lr.push(frame.right);
            read = read.wrapping_add(1);
        }

        self.inner.read_pos.store(read, Ordering::Release);

        let oldest_sample_age_ms = oldest_us.map(|ts| now_us.saturating_sub(ts) as f64 / 1000.0);
        PopResult {
            frames_read: to_read,
            oldest_sample_age_ms,
        }
    }

    /// Total stereo frames dropped on the producer side due to buffer full (overrun).
    pub fn overrun_frames(&self) -> u64 {
        self.inner.overrun_frames.load(Ordering::Relaxed)
    }

    /// Total empty reads on the consumer side (underrun / silence).
    pub fn underrun_reads(&self) -> u64 {
        self.inner.underrun_reads.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn ring_buffer_preserves_order_and_tracks_overruns_and_underruns() {
        let (mut prod, mut cons) = create_audio_ring(1024);

        // Underrun when empty
        let mut out = Vec::new();
        let empty = cons.pop_interleaved_stereo(&mut out, 64);
        assert_eq!(empty.frames_read, 0);
        assert!(empty.oldest_sample_age_ms.is_none());
        assert_eq!(cons.underrun_reads(), 1);

        // Push 1024 + 10 frames -> 10 dropped (overrun)
        let mut input = Vec::with_capacity(1034 * 2);
        for i in 0..1034 {
            input.push(i as f32);
            input.push(-(i as f32));
        }
        let now = prod.now_us();
        let (pushed, dropped) = prod.push_interleaved_stereo(&input, 48_000, now);
        assert_eq!(pushed, 1024);
        assert_eq!(dropped, 10);
        assert_eq!(cons.overrun_frames(), 10);

        // Newest 1024 frames were kept (starting at frame index 10)
        let pop = cons.pop_interleaved_stereo(&mut out, 1024);
        assert_eq!(pop.frames_read, 1024);
        assert_eq!(out[0], 10.0);
        assert_eq!(out[1], -10.0);
        assert_eq!(out[2046], 1033.0);
        assert_eq!(out[2047], -1033.0);
    }

    #[test]
    fn ring_producer_is_send_across_threads() {
        let (mut prod, mut cons) = create_audio_ring(1024);
        let handle = thread::spawn(move || {
            let now = prod.now_us();
            prod.push_interleaved_stereo(&[0.25, -0.25, 0.5, -0.5], 48_000, now)
        });
        let (pushed, dropped) = handle.join().expect("producer thread panicked");
        assert_eq!(pushed, 2);
        assert_eq!(dropped, 0);

        let mut out = Vec::new();
        let pop = cons.pop_interleaved_stereo(&mut out, 16);
        assert_eq!(pop.frames_read, 2);
        assert_eq!(out, vec![0.25, -0.25, 0.5, -0.5]);
    }
}
