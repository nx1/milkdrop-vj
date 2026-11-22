//! Frame timing statistics, FPS buckets, Windows process memory, and background
//! `nvidia-smi` GPU telemetry for Stage 3 diagnostics and the 10-minute soak test.

use std::ffi::c_void;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Running average and maximum duration (in milliseconds) for one timed section.
#[derive(Default, Clone, Copy)]
pub struct TimingStats {
    samples: u64,
    sum_ms: f64,
    max_ms: f64,
}

impl TimingStats {
    pub fn record_ms(&mut self, ms: f64) {
        self.samples += 1;
        self.sum_ms += ms;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
    }

    pub fn avg_ms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.sum_ms / self.samples as f64
        }
    }

    pub fn max_ms(&self) -> f64 {
        self.max_ms
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }
}

/// Tracks 1-second FPS samples inside a window (e.g. 30s) so we can report
/// exact `min`, `avg`, and `max` FPS cleanly.
#[derive(Default, Clone)]
pub struct FpsBucketTracker {
    samples: Vec<f64>,
}

impl FpsBucketTracker {
    pub fn push(&mut self, fps: f64) {
        self.samples.push(fps);
    }

    pub fn summary_and_clear(&mut self) -> Option<(f64, f64, f64)> {
        if self.samples.is_empty() {
            return None;
        }
        let mut min_v = f64::INFINITY;
        let mut max_v = f64::NEG_INFINITY;
        let mut sum = 0.0;
        for &v in &self.samples {
            if v < min_v {
                min_v = v;
            }
            if v > max_v {
                max_v = v;
            }
            sum += v;
        }
        let avg_v = sum / self.samples.len() as f64;
        self.samples.clear();
        Some((min_v, avg_v, max_v))
    }
}

/// Snapshot of Windows process memory usage in megabytes (MiB).
#[derive(Debug, Clone, Copy)]
pub struct MemorySnapshotMb {
    pub working_set_mb: f64,
    pub private_commit_mb: f64,
}

#[cfg(target_os = "windows")]
pub fn current_process_memory_mb() -> Option<MemorySnapshotMb> {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct ProcessMemoryCountersEx {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
        PrivateUsage: usize,
    }

    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            ppsmem_counters: *mut ProcessMemoryCountersEx,
            cb: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCountersEx {
        cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
        PrivateUsage: 0,
    };

    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return None;
    }

    const MIB: f64 = 1024.0 * 1024.0;
    Some(MemorySnapshotMb {
        working_set_mb: counters.WorkingSetSize as f64 / MIB,
        private_commit_mb: counters.PrivateUsage as f64 / MIB,
    })
}

#[cfg(not(target_os = "windows"))]
pub fn current_process_memory_mb() -> Option<MemorySnapshotMb> {
    None
}

/// Latest GPU telemetry sampled from `nvidia-smi`.
#[derive(Debug, Clone, Copy)]
pub struct GpuTelemetry {
    pub vram_used_mib: u32,
    pub vram_total_mib: u32,
    pub temp_c: u32,
    pub graphics_clock_mhz: u32,
}

/// Polls `nvidia-smi` on a lightweight background thread so spawning an external
/// process NEVER stalls the 60 FPS OpenGL render thread.
pub struct GpuTelemetryMonitor {
    latest: Arc<Mutex<Option<GpuTelemetry>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl GpuTelemetryMonitor {
    /// Starts polling `nvidia-smi` immediately and then every `interval`.
    pub fn start(interval: Duration) -> Self {
        let latest = Arc::new(Mutex::new(query_nvidia_smi()));
        let stop = Arc::new(AtomicBool::new(false));

        let latest_bg = Arc::clone(&latest);
        let stop_bg = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let step = Duration::from_millis(250);
            let mut waited = Duration::ZERO;
            while !stop_bg.load(Ordering::Relaxed) {
                thread::sleep(step);
                waited += step;
                if waited >= interval {
                    waited = Duration::ZERO;
                    if let Some(sample) = query_nvidia_smi() {
                        if let Ok(mut guard) = latest_bg.lock() {
                            *guard = Some(sample);
                        }
                    }
                }
            }
        });

        Self {
            latest,
            stop,
            handle: Some(handle),
        }
    }

    /// Returns the most recent `nvidia-smi` reading without blocking the render thread.
    pub fn latest(&self) -> Option<GpuTelemetry> {
        self.latest.lock().ok().and_then(|g| *g)
    }
}

impl Drop for GpuTelemetryMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Runs `nvidia-smi --query-gpu=memory.used,memory.total,temperature.gpu,clocks.current.graphics`.
pub fn query_nvidia_smi() -> Option<GpuTelemetry> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.used,memory.total,temperature.gpu,clocks.current.graphics",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first_line = text.lines().next()?.trim();
    let parts: Vec<&str> = first_line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 4 {
        return None;
    }
    Some(GpuTelemetry {
        vram_used_mib: parts[0].parse().ok()?,
        vram_total_mib: parts[1].parse().ok()?,
        temp_c: parts[2].parse().ok()?,
        graphics_clock_mhz: parts[3].parse().ok()?,
    })
}
