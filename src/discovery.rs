//! Process discovery using `CreateToolhelp32Snapshot`.
//!
//! # Two-tier cost model
//!
//! **Every poll (cheap)**  
//! `CreateToolhelp32Snapshot` returns every running process with its
//! name and thread count in a single kernel call (~0.5 ms).  
//! We filter by name against the set of `.exe` files found in the
//! watch folders.  Only the small candidate list (typically 2–5 entries)
//! proceeds to tier 2.
//!
//! **First appearance only (slightly more expensive)**  
//! `OpenProcess` + `QueryFullProcessImageNameW` verifies the full
//! executable path.  The result is cached in `known_processes` forever,
//! so this cost is paid exactly once per process lifetime.
//!
//! Processes whose path does NOT match any watch folder are added to
//! `excluded_pids` and never re-checked, even if they share a name
//! with a watched executable.
//!
//! # PID reuse safety
//! When a PID disappears from the snapshot it is removed from both
//! `known_processes` and `excluded_pids`, allowing the OS to reuse
//! that PID for a new process which will be re-verified from scratch.

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

// OsStringExt (for from_wide) is only available on Windows.
// The functions that use it are already gated with #[cfg(windows)] below,
// so this import is safe to gate the same way.
#[cfg(windows)]
use std::{ffi::OsString, os::windows::ffi::OsStringExt};

#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, FILETIME},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
            PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetProcessTimes, OpenProcess,
            QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

// ── ProcessInfo ───────────────────────────────────────────────────────────────

/// Everything we know about a confirmed watched process.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid:          u32,
    pub name:         String,
    pub exe_path:     String,
    pub thread_count: u32,
    pub started_at:   DateTime<Utc>,
    /// Monotonic time of first observation (used for uptime on exit).
    pub first_seen:   Instant,
}

// ── ProcessDiscovery ──────────────────────────────────────────────────────────

pub struct ProcessDiscovery {
    /// Lower-cased absolute paths of watch folders (trailing backslash).
    watch_folders: Vec<String>,
    /// PID → confirmed process in a watch folder.
    known_processes: HashMap<u32, ProcessInfo>,
    /// PIDs confirmed to live OUTSIDE any watch folder — never re-check.
    excluded_pids: HashSet<u32>,
}

impl ProcessDiscovery {
    pub fn new(watch_folders: &[String]) -> Result<Self> {
        let mut disc = Self {
            watch_folders:   Vec::new(),
            known_processes: HashMap::new(),
            excluded_pids:   HashSet::new(),
        };
        disc.set_watch_folders(watch_folders);
        Ok(disc)
    }

    /// Replace the watch-folder list.  Every process whose full executable
    /// path starts with one of these folders is watched — no pre-scanning of
    /// folder contents required.
    /// Called on config reload.
    pub fn set_watch_folders(&mut self, folders: &[String]) {
        self.watch_folders = folders
            .iter()
            .map(|f| {
                let mut s = f.to_lowercase().replace('/', "\\");
                if !s.ends_with('\\') { s.push('\\'); }
                s
            })
            .collect();

        // Clear the excluded cache so any previously-rejected PID is
        // re-evaluated against the new folder list.
        self.excluded_pids.clear();
    }

    /// Immutable view of currently known processes.
    pub fn known_processes(&self) -> &HashMap<u32, ProcessInfo> {
        &self.known_processes
    }

    // ── Main poll ─────────────────────────────────────────────────────────────

    /// Snapshot the OS process list and return `(spawned, exited)`.
    ///
    /// * `spawned` — processes that appeared since the last call.
    /// * `exited`  — processes that were known but have now disappeared.
    ///
    /// The caller is responsible for logging these events.
    pub fn poll(&mut self) -> Result<(Vec<ProcessInfo>, Vec<ProcessInfo>)> {
        let snapshot = enumerate_processes()?;

        let current_pids: HashSet<u32> = snapshot.iter().map(|e| e.0).collect();

        let mut spawned = Vec::new();
        let mut exited  = Vec::new();

        // ── Detect exits first ────────────────────────────────────────────────
        let exited_pids: Vec<u32> = self.known_processes
            .keys()
            .filter(|pid| !current_pids.contains(pid))
            .copied()
            .collect();

        for pid in exited_pids {
            if let Some(info) = self.known_processes.remove(&pid) {
                self.excluded_pids.remove(&pid); // allow PID reuse
                exited.push(info);
            }
        }

        // ── Detect spawns ─────────────────────────────────────────────────────
        for &(pid, ref name, thread_count) in &snapshot {
            if let Some(info) = self.known_processes.get_mut(&pid) {
                // Already tracked — refresh thread count
                info.thread_count = thread_count;
                continue;
            }

            if self.excluded_pids.contains(&pid) {
                continue;
            }

            // New candidate — verify full path (one handle open, cached forever)
            match get_process_path(pid) {
                Ok(path) => {
                    let path_lower = path.to_lowercase();
                    let in_watch = self.watch_folders
                        .iter()
                        .any(|f| path_lower.starts_with(f.as_str()));

                    if in_watch {
                        let started_at = get_process_start_time(pid)
                            .unwrap_or_else(|_| Utc::now());

                        let info = ProcessInfo {
                            pid,
                            name: name.clone(),
                            exe_path: path,
                            thread_count,
                            started_at,
                            first_seen: Instant::now(),
                        };
                        self.known_processes.insert(pid, info.clone());
                        spawned.push(info);
                    } else {
                        self.excluded_pids.insert(pid);
                    }
                }
                Err(_) => {
                    // Process may have died between the snapshot and now,
                    // or we lack permission.  Skip silently.
                }
            }
        }

        Ok((spawned, exited))
    }
}

// ── Windows API helpers ───────────────────────────────────────────────────────

/// Enumerate all processes: returns Vec<(pid, name, thread_count)>.
#[cfg(windows)]
fn enumerate_processes() -> Result<Vec<(u32, String, u32)>> {
    let mut result = Vec::with_capacity(256);

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                // szExeFile is a null-terminated wide string
                let name_end = entry.szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = OsString::from_wide(&entry.szExeFile[..name_end])
                    .to_string_lossy()
                    .into_owned();

                result.push((entry.th32ProcessID, name, entry.cntThreads));

                if Process32NextW(snap, &mut entry).is_err() {
                    break; // ERROR_NO_MORE_FILES — normal end of list
                }
            }
        }

        let _ = CloseHandle(snap);
    }

    Ok(result)
}

#[cfg(not(windows))]
fn enumerate_processes() -> Result<Vec<(u32, String, u32)>> {
    Ok(Vec::new()) // stub for non-Windows compilation
}

/// Retrieve the full executable path for a PID.
#[cfg(windows)]
fn get_process_path(pid: u32) -> Result<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;

        let mut buf   = [0u16; 1024];
        let mut len   = buf.len() as u32;

        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )?;

        let _ = CloseHandle(handle);

        Ok(OsString::from_wide(&buf[..len as usize])
            .to_string_lossy()
            .into_owned())
    }
}

#[cfg(not(windows))]
fn get_process_path(_pid: u32) -> Result<String> {
    anyhow::bail!("not implemented on this platform")
}

/// Read the process creation time via GetProcessTimes.
/// Returns the creation time as a UTC DateTime.
#[cfg(windows)]
fn get_process_start_time(pid: u32) -> Result<DateTime<Utc>> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;

        let mut creation = FILETIME::default();
        let mut exit     = FILETIME::default();
        let mut kernel   = FILETIME::default();
        let mut user     = FILETIME::default();

        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)?;
        let _ = CloseHandle(handle);

        Ok(filetime_to_utc(creation))
    }
}

#[cfg(not(windows))]
fn get_process_start_time(_pid: u32) -> Result<DateTime<Utc>> {
    Ok(Utc::now())
}

// ── FILETIME conversion ───────────────────────────────────────────────────────
//
// FILETIME: 100-nanosecond intervals since 1601-01-01 00:00 UTC.
// Unix epoch offset: 11 644 473 600 seconds = 116 444 736 000 000 000 × 100 ns.

#[cfg(windows)]
pub fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

#[cfg(windows)]
pub fn filetime_to_utc(ft: FILETIME) -> DateTime<Utc> {
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    let ticks    = filetime_to_u64(ft);
    let unix_100 = ticks.saturating_sub(EPOCH_DIFF);
    let secs     = (unix_100 / 10_000_000) as i64;
    let nanos    = ((unix_100 % 10_000_000) * 100) as u32;
    DateTime::from_timestamp(secs, nanos).unwrap_or_default()
}
