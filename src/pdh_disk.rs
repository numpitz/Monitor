//! Disk I/O rate monitoring via Windows PDH.
//!
//! Uses `\LogicalDisk(*)\Disk Read Bytes/sec` and
//! `\LogicalDisk(*)\Disk Write Bytes/sec` — available on Windows 10+
//! without any feature flags or extra drivers.
//!
//! PDH rate counters require two collections to produce a value; the first
//! call after `init()` returns zeros (priming tick).

#![cfg(windows)]

use std::alloc;
use std::collections::HashMap;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW, PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE,
};

const PDH_MORE_DATA: u32 = 0x800007D2u32;

// ── Public API ────────────────────────────────────────────────────────────────

pub struct PdhDiskMonitor {
    query:     isize,
    cnt_read:  isize,
    cnt_write: isize,
    primed:    bool,
}

// SAFETY: PDH handles are plain isize values; never shared across threads.
unsafe impl Send for PdhDiskMonitor {}

impl PdhDiskMonitor {
    /// Initialise PDH counters for all logical drives.
    /// Returns `None` if PDH setup fails.
    pub fn init() -> Option<Self> {
        unsafe {
            let mut query: isize = 0;
            if PdhOpenQueryW(windows::core::PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }

            let mut cnt_read:  isize = 0;
            let mut cnt_write: isize = 0;

            let ok =
                add_counter(query, r"\LogicalDisk(*)\Disk Read Bytes/sec",  &mut cnt_read)  &&
                add_counter(query, r"\LogicalDisk(*)\Disk Write Bytes/sec", &mut cnt_write);

            if !ok {
                PdhCloseQuery(query);
                return None;
            }

            // Prime: seeds the baseline for rate calculation on the next call.
            PdhCollectQueryData(query);

            Some(Self { query, cnt_read, cnt_write, primed: false })
        }
    }

    /// Collect I/O rates.
    ///
    /// Returns a map of Windows mount path (e.g. `"C:\\"`) →
    /// `(read_mb_per_sec, write_mb_per_sec)`.
    ///
    /// The first call after `init()` returns zeros (PDH priming tick).
    pub fn sample(&mut self) -> HashMap<String, (f64, f64)> {
        unsafe { PdhCollectQueryData(self.query) };

        let mut out: HashMap<String, (f64, f64)> = HashMap::new();

        if self.primed {
            for (name, bytes_per_sec) in counter_items_f64(self.cnt_read) {
                if name == "_total" { continue; }
                // PDH instance: "c:" → mount path: "C:\"
                let path = pdh_to_mount(&name);
                out.entry(path).or_default().0 = bytes_per_sec / 1_048_576.0;
            }
            for (name, bytes_per_sec) in counter_items_f64(self.cnt_write) {
                if name == "_total" { continue; }
                let path = pdh_to_mount(&name);
                out.entry(path).or_default().1 = bytes_per_sec / 1_048_576.0;
            }
        }

        self.primed = true;
        out
    }
}

impl Drop for PdhDiskMonitor {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert a lower-cased PDH logical-disk instance name (e.g. `"c:"`) to the
/// mount-point path that sysinfo uses on Windows (e.g. `"C:\"`).
fn pdh_to_mount(pdh: &str) -> String {
    let mut s = pdh.to_uppercase();
    if !s.ends_with('\\') { s.push('\\'); }
    s
}

unsafe fn add_counter(query: isize, path: &str, counter: &mut isize) -> bool {
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    unsafe {
        PdhAddEnglishCounterW(
            query,
            windows::core::PCWSTR(wide.as_ptr()),
            0,
            counter,
        ) == 0
    }
}

fn counter_items_f64(counter: isize) -> Vec<(String, f64)> {
    unsafe {
        let mut buf_bytes:  u32 = 0;
        let mut item_count: u32 = 0;
        PdhGetFormattedCounterArrayW(
            counter, PDH_FMT_DOUBLE,
            &mut buf_bytes, &mut item_count, None,
        );
        if buf_bytes == 0 {
            return Vec::new();
        }

        let item_size  = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let alloc_size = (buf_bytes as usize).max(item_count as usize * item_size) + item_size;
        let layout = alloc::Layout::from_size_align(alloc_size, 8).unwrap();
        let ptr = alloc::alloc_zeroed(layout) as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if ptr.is_null() {
            return Vec::new();
        }

        let status = PdhGetFormattedCounterArrayW(
            counter, PDH_FMT_DOUBLE,
            &mut buf_bytes, &mut item_count,
            Some(ptr),
        );

        let mut result = Vec::new();
        if status == 0 || status == PDH_MORE_DATA {
            let items = std::slice::from_raw_parts(ptr, item_count as usize);
            for item in items {
                if item.FmtValue.CStatus != 0 { continue; }
                let val = item.FmtValue.Anonymous.doubleValue;
                if val < 0.0 { continue; }
                if let Ok(name) = item.szName.to_string() {
                    result.push((name.to_lowercase(), val));
                }
            }
        }

        alloc::dealloc(ptr as *mut u8, layout);
        result
    }
}
