//! Cross-vendor GPU monitoring via Windows PDH + DXGI.
//!
//! Works with any GPU that has a WDDM driver (NVIDIA without NVML, AMD, Intel).
//! Used as a fallback when NVML is unavailable.
//!
//! Metrics provided:
//!   - GPU 3D engine utilisation %
//!   - Video encode / decode engine %
//!   - VRAM used and total MB (via DXGI)
//!
//! NOT available without a vendor SDK:
//!   - Temperature
//!   - Power draw

#![cfg(windows)]

use std::alloc;
use windows::{
    core::Interface,
    Win32::{
        Graphics::Dxgi::{
            CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1,
            DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
            DXGI_QUERY_VIDEO_MEMORY_INFO,
        },
        System::Performance::{
            PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
            PdhGetFormattedCounterArrayW, PdhOpenQueryW,
            PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
        },
    },
};

// PDH_MORE_DATA (0x800007D2): buffer too small but partial data was written — still usable.
const PDH_MORE_DATA: u32 = 0x800007D2u32;

// ── Public types ──────────────────────────────────────────────────────────────

/// Per-adapter GPU metrics from PDH + DXGI.
pub struct GpuPdhSample {
    pub index:         u32,
    pub name:          String,
    pub gpu_used_pct:  f64,
    pub encoder_pct:   f64,
    pub decoder_pct:   f64,
    pub vram_used_mb:  f64,
    pub vram_total_mb: f64,
}

// ── Internal state ────────────────────────────────────────────────────────────

struct AdapterInfo {
    name:             String,
    adapter3:         IDXGIAdapter3,
    vram_physical_mb: f64,  // DedicatedVideoMemory from DXGI_ADAPTER_DESC1
}

pub struct PdhGpuMonitor {
    /// PDH query handle (plain isize in windows 0.58)
    query:      isize,
    cnt_3d:     isize,
    cnt_encode: isize,
    cnt_decode: isize,
    adapters:   Vec<AdapterInfo>,
    primed:     bool,
}

// SAFETY: PDH handles are just isize values; we never share them across threads.
unsafe impl Send for PdhGpuMonitor {}

impl PdhGpuMonitor {
    /// Try to initialise PDH counters and enumerate DXGI adapters.
    /// Returns `None` if no hardware adapters are found or PDH fails.
    pub fn init() -> Option<Self> {
        let adapters = enumerate_adapters();
        if adapters.is_empty() {
            return None;
        }

        unsafe {
            let mut query: isize = 0;
            // szdatasource = None → real-time data source
            if PdhOpenQueryW(windows::core::PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }

            let mut cnt_3d:     isize = 0;
            let mut cnt_encode: isize = 0;
            let mut cnt_decode: isize = 0;

            let ok =
                add_counter(query, r"\GPU Engine(*engtype_3D*)\Utilization Percentage",          &mut cnt_3d)     &&
                add_counter(query, r"\GPU Engine(*engtype_VideoEncode*)\Utilization Percentage", &mut cnt_encode) &&
                add_counter(query, r"\GPU Engine(*engtype_VideoDecode*)\Utilization Percentage", &mut cnt_decode);

            if !ok {
                PdhCloseQuery(query);
                return None;
            }

            // Prime: PDH rate counters need two collections; first one seeds the baseline.
            PdhCollectQueryData(query);

            Some(Self { query, cnt_3d, cnt_encode, cnt_decode, adapters, primed: false })
        }
    }

    /// Collect a snapshot.  Returns one `GpuPdhSample` per hardware adapter.
    /// The first call after `init()` returns zero utilisation values (priming tick).
    pub fn sample(&mut self) -> Vec<GpuPdhSample> {
        unsafe { PdhCollectQueryData(self.query) };

        let n = self.adapters.len();
        let mut util_3d  = vec![0.0f64; n];
        let mut util_enc = vec![0.0f64; n];
        let mut util_dec = vec![0.0f64; n];

        if self.primed {
            sum_by_phys(self.cnt_3d,     &mut util_3d);
            sum_by_phys(self.cnt_encode, &mut util_enc);
            sum_by_phys(self.cnt_decode, &mut util_dec);
        }
        self.primed = true;

        self.adapters.iter().enumerate().map(|(i, a)| {
            let (vram_used_mb, vram_budget_mb) = dxgi_vram(&a.adapter3);
            // Physical VRAM from desc is more accurate; fall back to DXGI budget.
            let vram_total_mb = if a.vram_physical_mb > 0.0 {
                a.vram_physical_mb
            } else {
                vram_budget_mb
            };

            GpuPdhSample {
                index:         i as u32,
                name:          a.name.clone(),
                gpu_used_pct:  util_3d[i].min(100.0),
                encoder_pct:   util_enc[i].min(100.0),
                decoder_pct:   util_dec[i].min(100.0),
                vram_used_mb,
                vram_total_mb,
            }
        }).collect()
    }

    /// Names of all detected hardware adapters in enumeration order.
    pub fn adapter_names(&self) -> Vec<String> {
        self.adapters.iter().map(|a| a.name.clone()).collect()
    }
}

impl Drop for PdhGpuMonitor {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

// ── DXGI helpers ──────────────────────────────────────────────────────────────

fn enumerate_adapters() -> Vec<AdapterInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut i = 0u32;
        loop {
            let Ok(a1) = factory.EnumAdapters1(i) else { break };
            i += 1;
            let Ok(desc) = a1.GetDesc1() else { continue };
            // Skip software adapters (e.g. Microsoft Basic Render Driver).
            if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 { continue }
            let Ok(a3) = a1.cast::<IDXGIAdapter3>() else { continue };

            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
            let name = String::from_utf16_lossy(&desc.Description[..len]).to_string();
            let vram_physical_mb = desc.DedicatedVideoMemory as f64 / 1_048_576.0;

            out.push(AdapterInfo { name, adapter3: a3, vram_physical_mb });
        }
    }
    out
}

/// Returns (used_mb, budget_mb) for the adapter's local (dedicated) memory segment.
fn dxgi_vram(adapter: &IDXGIAdapter3) -> (f64, f64) {
    unsafe {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        if adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).is_ok() {
            (
                info.CurrentUsage as f64 / 1_048_576.0,
                info.Budget       as f64 / 1_048_576.0,
            )
        } else {
            (0.0, 0.0)
        }
    }
}

// ── PDH helpers ───────────────────────────────────────────────────────────────

/// Add a wildcard English counter to an open PDH query.
unsafe fn add_counter(
    query:   isize,
    path:    &str,
    counter: &mut isize,
) -> bool {
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

/// Parse `_phys_N` from a PDH GPU counter instance name.
/// Instance names look like: `pid_2356_luid_0x..._phys_0_eng_3_engtype_3D`
fn phys_index(name: &str) -> Option<usize> {
    let start = name.find("_phys_")? + 6;
    let rest  = &name[start..];
    let end   = rest.find('_').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Sum utilisation values per physical adapter index into `out`.
fn sum_by_phys(counter: isize, out: &mut [f64]) {
    for (name, val) in counter_items_f64(counter) {
        if let Some(idx) = phys_index(&name) {
            if idx < out.len() {
                out[idx] += val;
            }
        }
    }
}

/// Read all instances of a PDH double counter.
/// Returns `(instance_name_lowercase, value)` pairs.
fn counter_items_f64(counter: isize) -> Vec<(String, f64)> {
    unsafe {
        // --- size query ---
        let mut buf_bytes:  u32 = 0;
        let mut item_count: u32 = 0;
        PdhGetFormattedCounterArrayW(
            counter, PDH_FMT_DOUBLE,
            &mut buf_bytes, &mut item_count, None,
        );
        if buf_bytes == 0 {
            return Vec::new();
        }

        // --- allocate buffer ---
        let item_size  = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let alloc_size = (buf_bytes as usize).max(item_count as usize * item_size) + item_size;
        let layout = alloc::Layout::from_size_align(alloc_size, 8).unwrap();
        let ptr = alloc::alloc_zeroed(layout) as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if ptr.is_null() {
            return Vec::new();
        }

        // --- fill buffer ---
        let status = PdhGetFormattedCounterArrayW(
            counter, PDH_FMT_DOUBLE,
            &mut buf_bytes, &mut item_count,
            Some(ptr),
        );

        let mut result = Vec::new();
        if status == 0 || status == PDH_MORE_DATA {
            let items = std::slice::from_raw_parts(ptr, item_count as usize);
            for item in items {
                if item.FmtValue.CStatus != 0 { continue }
                let val = item.FmtValue.Anonymous.doubleValue;
                if val < 0.0 { continue }
                // szName points into the same allocation; convert before freeing.
                if let Ok(name) = item.szName.to_string() {
                    result.push((name.to_lowercase(), val));
                }
            }
        }

        alloc::dealloc(ptr as *mut u8, layout);
        result
    }
}
