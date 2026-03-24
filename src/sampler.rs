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
//! Kernel and user components are returned separately so callers can
//! distinguish I/O-heavy (high kernel%) from compute-heavy (high user%)
//! workloads without any extra API calls.
//!
//! # I/O sampling
//!
//! `GetProcessIoCounters` returns cumulative byte/operation counts.
//! The `IoSampler` computes per-second deltas identically to the CPU sampler.
//! It uses a separate, slower interval because it adds one extra syscall per
//! tracked process.

use process_monitor::events::{IoProcessSample, ProcessSample};
use std::{collections::HashMap, time::Instant};

#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, FILETIME},
    System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            GetProcessHandleCount, GetProcessIoCounters, GetProcessTimes,
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, IO_COUNTERS,
        },
    },
};

// ── CPU state per PID ─────────────────────────────────────────────────────────

struct CpuState {
    prev_kernel_100ns: u64,
    prev_user_100ns:   u64,
    prev_wall:         Instant,
}

// ── I/O state per PID ────────────────────────────────────────────────────────

struct IoState {
    prev_read_bytes:  u64,
    prev_write_bytes: u64,
    prev_read_ops:    u64,
    prev_write_ops:   u64,
    prev_wall:        Instant,
}

// ── ResourceSampler ───────────────────────────────────────────────────────────

pub struct ResourceSampler {
    cpu_states: HashMap<u32, CpuState>,
    io_states:  HashMap<u32, IoState>,
    num_cpus:   u32,
}

impl ResourceSampler {
    pub fn new() -> Self {
        Self {
            cpu_states: HashMap::new(),
            io_states:  HashMap::new(),
            num_cpus:   num_cpus::get() as u32,
        }
    }

    /// Sample CPU, memory, pagefile, handles and threads for one process.
    /// Returns `None` if the process can no longer be opened (it may have
    /// exited between discovery and sampling).
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

    /// Sample I/O counters for one process.
    /// Returns `None` if the process can no longer be opened or on the first
    /// call for a PID (no baseline yet).
    pub fn sample_io(&mut self, pid: u32, name: &str) -> Option<IoProcessSample> {
        #[cfg(windows)]
        {
            self.sample_io_windows(pid, name)
        }
        #[cfg(not(windows))]
        {
            let _ = (pid, name);
            None
        }
    }

    /// Remove cached state for a PID that has exited.
    pub fn remove(&mut self, pid: u32) {
        self.cpu_states.remove(&pid);
        self.io_states.remove(&pid);
    }

    // ── Windows implementation — resource sample ──────────────────────────────

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

            // ── CPU (total, kernel, user) ─────────────────────────────────────
            let (cpu_total, cpu_kernel, cpu_user) = self.cpu_percent(pid, handle);

            // ── Memory (working set + pagefile) ───────────────────────────────
            let mut pmc = PROCESS_MEMORY_COUNTERS::default();
            pmc.cb      = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let (memory_mb, pagefile_mb) = if GetProcessMemoryInfo(
                handle, &mut pmc, pmc.cb,
            ).is_ok() {
                (
                    pmc.WorkingSetSize  as f64 / 1_048_576.0,
                    pmc.PagefileUsage   as f64 / 1_048_576.0,
                )
            } else {
                (0.0, 0.0)
            };

            // ── Handle count ──────────────────────────────────────────────────
            let mut handles = 0u32;
            let _ = GetProcessHandleCount(handle, &mut handles);

            let _ = CloseHandle(handle);

            Some(ProcessSample {
                pid,
                name:               name.to_string(),
                cpu_percent:        round2(cpu_total),
                cpu_kernel_percent: round2(cpu_kernel),
                cpu_user_percent:   round2(cpu_user),
                memory_mb:          round2(memory_mb),
                pagefile_mb:        round2(pagefile_mb),
                handles,
                threads: thread_count,
            })
        }
    }

    // ── Windows implementation — I/O sample ───────────────────────────────────

    #[cfg(windows)]
    fn sample_io_windows(&mut self, pid: u32, name: &str) -> Option<IoProcessSample> {
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            ).ok()?;

            let mut ioc = IO_COUNTERS::default();
            if GetProcessIoCounters(handle, &mut ioc).is_err() {
                let _ = CloseHandle(handle);
                return None;
            }
            let _ = CloseHandle(handle);

            let now = Instant::now();

            let result = if let Some(prev) = self.io_states.get(&pid) {
                let secs = prev.prev_wall.elapsed().as_secs_f64().max(f64::EPSILON);

                let read_mb_s   = (ioc.ReadTransferCount .saturating_sub(prev.prev_read_bytes)  as f64 / 1_048_576.0) / secs;
                let write_mb_s  = (ioc.WriteTransferCount.saturating_sub(prev.prev_write_bytes) as f64 / 1_048_576.0) / secs;
                let read_ops_s  =  ioc.ReadOperationCount .saturating_sub(prev.prev_read_ops)   as f64 / secs;
                let write_ops_s =  ioc.WriteOperationCount.saturating_sub(prev.prev_write_ops)  as f64 / secs;

                Some(IoProcessSample {
                    pid,
                    name:                 name.to_string(),
                    io_read_mb_per_sec:   round2(read_mb_s),
                    io_write_mb_per_sec:  round2(write_mb_s),
                    io_read_ops_per_sec:  round2(read_ops_s),
                    io_write_ops_per_sec: round2(write_ops_s),
                })
            } else {
                None // first sample — no baseline yet
            };

            self.io_states.insert(pid, IoState {
                prev_read_bytes:  ioc.ReadTransferCount,
                prev_write_bytes: ioc.WriteTransferCount,
                prev_read_ops:    ioc.ReadOperationCount,
                prev_write_ops:   ioc.WriteOperationCount,
                prev_wall:        now,
            });

            result
        }
    }

    /// Calculate (total%, kernel%, user%) for `pid` using the already-open handle.
    /// Returns (0, 0, 0) on the first call for this PID.
    #[cfg(windows)]
    unsafe fn cpu_percent(
        &mut self,
        pid:    u32,
        handle: windows::Win32::Foundation::HANDLE,
    ) -> (f64, f64, f64) {
        unsafe {
            let mut creation = FILETIME::default();
            let mut exit     = FILETIME::default();
            let mut kernel   = FILETIME::default();
            let mut user     = FILETIME::default();

            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_err() {
                return (0.0, 0.0, 0.0);
            }

            let kernel_100ns = filetime_to_u64(kernel);
            let user_100ns   = filetime_to_u64(user);
            let now          = Instant::now();

            let result = if let Some(prev) = self.cpu_states.get(&pid) {
                let delta_kernel = kernel_100ns.saturating_sub(prev.prev_kernel_100ns);
                let delta_user   = user_100ns  .saturating_sub(prev.prev_user_100ns);
                let delta_wall   = prev.prev_wall.elapsed().as_nanos() as u64 / 100;

                if delta_wall > 0 {
                    let max_pct = 100.0 * self.num_cpus as f64;
                    let k = (delta_kernel as f64 / delta_wall as f64 * 100.0).clamp(0.0, max_pct);
                    let u = (delta_user   as f64 / delta_wall as f64 * 100.0).clamp(0.0, max_pct);
                    (k + u, k, u)
                } else {
                    (0.0, 0.0, 0.0)
                }
            } else {
                (0.0, 0.0, 0.0)
            };

            self.cpu_states.insert(pid, CpuState {
                prev_kernel_100ns: kernel_100ns,
                prev_user_100ns:   user_100ns,
                prev_wall:         now,
            });

            result
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
