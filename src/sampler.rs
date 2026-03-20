//! Per-process resource sampling.
//!
//! # CPU% measurement
//!
//! Windows `GetProcessTimes` returns cumulative kernel+user CPU time as
//! a `FILETIME` (100 ns ticks).  We store the previous reading and the
//! wall-clock instant, then on the next sample compute:
//!
//! ```text
//! cpu% = (Δcpu_ticks / Δwall_ticks) × 100
//! ```
//!
//! This gives the percentage of *one logical CPU core* used by the
//! process (same as Task Manager's "Details" column).
//!
//! # Rust + GC concern
//!
//! This module is written in Rust, which has no garbage collector.
//! The measurement loop runs at a perfectly uniform interval with no
//! hidden CPU spikes from a runtime.  This was the primary reason for
//! choosing Rust over Go or C# for this particular monitor.

use crate::events::ProcessSample;
use std::{collections::HashMap, time::Instant};

#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, FILETIME},
    System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            GetProcessHandleCount, GetProcessTimes,
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

// ── CPU state per PID ─────────────────────────────────────────────────────────

struct CpuState {
    prev_cpu_100ns: u64,
    prev_wall:      Instant,
}

// ── ResourceSampler ───────────────────────────────────────────────────────────

pub struct ResourceSampler {
    cpu_states: HashMap<u32, CpuState>,
    num_cpus:   u32,
}

impl ResourceSampler {
    pub fn new() -> Self {
        Self {
            cpu_states: HashMap::new(),
            num_cpus:   num_cpus::get() as u32,
        }
    }

    /// Sample one process.  Returns `None` if the process can no longer
    /// be opened (it may have exited between discovery and sampling).
    pub fn sample(&mut self, pid: u32, name: &str, thread_count: u32) -> Option<ProcessSample> {
        #[cfg(windows)]
        {
            self.sample_windows(pid, name, thread_count)
        }
        #[cfg(not(windows))]
        {
            let _ = (pid, name, thread_count);
            None
        }
    }

    /// Remove cached CPU state for a PID that has exited.
    /// Must be called whenever `ProcessDiscovery` reports a process exit.
    pub fn remove(&mut self, pid: u32) {
        self.cpu_states.remove(&pid);
    }

    // ── Windows implementation ────────────────────────────────────────────────

    #[cfg(windows)]
    fn sample_windows(
        &mut self,
        pid:          u32,
        name:         &str,
        thread_count: u32,
    ) -> Option<ProcessSample> {
        use std::mem::size_of;

        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            ).ok()?;

            // ── CPU ───────────────────────────────────────────────────────────
            let cpu_percent = self.cpu_percent(pid, handle);

            // ── Memory (working set) ──────────────────────────────────────────
            let mut pmc  = PROCESS_MEMORY_COUNTERS::default();
            pmc.cb       = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let memory_mb = if GetProcessMemoryInfo(
                handle,
                &mut pmc,
                pmc.cb,
            ).is_ok() {
                pmc.WorkingSetSize as f64 / 1_048_576.0
            } else {
                0.0
            };

            // ── Handle count ──────────────────────────────────────────────────
            let mut handles = 0u32;
            let _ = GetProcessHandleCount(handle, &mut handles);

            let _ = CloseHandle(handle);

            Some(ProcessSample {
                pid,
                name: name.to_string(),
                cpu_percent: round2(cpu_percent),
                memory_mb:   round2(memory_mb),
                handles,
                threads: thread_count,
            })
        }
    }

    /// Calculate CPU% for `pid` using the already-open `handle`.
    /// Returns 0.0 on the first call for this PID (no previous state).
    #[cfg(windows)]
    unsafe fn cpu_percent(
        &mut self,
        pid:    u32,
        handle: windows::Win32::Foundation::HANDLE,
    ) -> f64 {
        // Rust 2024: unsafe operations inside an unsafe fn still need
        // an explicit unsafe {} block.
        unsafe {
            let mut creation = FILETIME::default();
            let mut exit     = FILETIME::default();
            let mut kernel   = FILETIME::default();
            let mut user     = FILETIME::default();

            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_err() {
                return 0.0;
            }

            let cpu_100ns = filetime_to_u64(kernel) + filetime_to_u64(user);
            let now       = Instant::now();

            let percent = if let Some(prev) = self.cpu_states.get(&pid) {
                let delta_cpu  = cpu_100ns.saturating_sub(prev.prev_cpu_100ns);
                // Convert elapsed duration to 100-ns units
                let delta_wall = prev.prev_wall.elapsed().as_nanos() as u64 / 100;

                if delta_wall > 0 {
                    // Result is % of one logical core;
                    // divide by num_cpus to get % of total system CPU if preferred.
                    (delta_cpu as f64 / delta_wall as f64 * 100.0)
                        .min(100.0_f64 * self.num_cpus as f64)
                        .max(0.0)
                } else {
                    0.0
                }
            } else {
                0.0 // First sample — no baseline yet
            };

            self.cpu_states.insert(pid, CpuState { prev_cpu_100ns: cpu_100ns, prev_wall: now });
            percent
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Round a float to 2 decimal places for clean JSON output.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
