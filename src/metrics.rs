//! Frame timing statistics and Windows process memory reporting for Stage 3.
//!
//! Tracks average and worst-case (maximum) frame times separately for Deck A,
//! Deck B, and total frame time, plus Windows Working Set and Private Commit
//! memory in megabytes (MB) for the 10-minute stability soak test.

use std::ffi::c_void;

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

/// Snapshot of process memory usage in megabytes (MiB).
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

    let ok = unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb)
    };
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
