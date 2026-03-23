//! All structured event types written to the NDJSON log.
//!
//! Every log line is a `LogEntry<T>` serialised with serde_json.
//! Using `#[serde(flatten)]` the `data` fields appear at the top level
//! of each JSON object, keeping lines compact and easy to grep.
//!
//! `LogEntry::info / warn / error` take a `monitor` name so the same
//! envelope works for both the process-monitor and system-monitor binaries.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

fn serialize_utc_z<S>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_str(&dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

// ── Log level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Level {
    Info,
    Warn,
    Error,
}

// ── Wrapper that every log entry uses ────────────────────────────────────────

#[derive(Serialize)]
pub struct LogEntry<'a, T: Serialize> {
    #[serde(serialize_with = "serialize_utc_z")]
    pub ts:      DateTime<Utc>,
    pub monitor: &'static str,
    pub event:   &'a str,
    pub level:   Level,
    #[serde(flatten)]
    pub data:    T,
}

impl<'a, T: Serialize> LogEntry<'a, T> {
    pub fn info(monitor: &'static str, event: &'a str, data: T) -> Self {
        Self { ts: Utc::now(), monitor, event, level: Level::Info, data }
    }
    pub fn warn(monitor: &'static str, event: &'a str, data: T) -> Self {
        Self { ts: Utc::now(), monitor, event, level: Level::Warn, data }
    }
    pub fn error(monitor: &'static str, event: &'a str, data: T) -> Self {
        Self { ts: Utc::now(), monitor, event, level: Level::Error, data }
    }
}

// ── Shared event data (used by both monitors) ─────────────────────────────────

/// Written as the very first entry in every log file (including after rotation).
#[derive(Serialize)]
pub struct MonitorStartData {
    pub pid:      u32,
    pub log_file: String,
    pub rotation: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued_from: Option<String>,
}

/// Written once on clean shutdown, before process exit.
#[derive(Serialize)]
pub struct MonitorStopData {
    pub pid:       u32,
    pub reason:    &'static str,
    pub exit_code: i32,
}

/// Config file was reloaded from disk.
#[derive(Serialize)]
pub struct ConfigReloadedData {
    pub path: String,
}

/// Non-fatal issue worth recording.
#[derive(Serialize)]
pub struct WarningData {
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ── process-monitor event data ────────────────────────────────────────────────

/// A new process appeared inside a watch folder.
#[derive(Serialize)]
pub struct ProcessSpawnedData {
    pub pid:      u32,
    pub name:     String,
    pub exe_path: String,
}

/// A previously-known process disappeared from the process list.
#[derive(Serialize)]
pub struct ProcessExitedData {
    pub pid:            u32,
    pub name:           String,
    pub uptime_seconds: u64,
}

/// One row in a resource_sample event.
#[derive(Serialize)]
pub struct ProcessSample {
    pub pid:         u32,
    pub name:        String,
    pub cpu_percent: f64,
    pub memory_mb:   f64,
    pub handles:     u32,
    pub threads:     u32,
}

/// Written every `resource_poll_interval_ms`.
#[derive(Serialize)]
pub struct ResourceSampleData {
    pub processes:         Vec<ProcessSample>,
    pub total_cpu_percent: f64,
    pub total_memory_mb:   f64,
}

/// One row in a process_tree_snapshot event.
#[derive(Serialize)]
pub struct ProcessSnapshotEntry {
    pub pid:        u32,
    pub name:       String,
    pub exe_path:   String,
    #[serde(serialize_with = "serialize_utc_z")]
    pub started_at: DateTime<Utc>,
    pub threads:    u32,
    pub memory_mb:  f64,
}

/// Written every `snapshot_interval_ms`.
#[derive(Serialize)]
pub struct TreeSnapshotData {
    pub count:     usize,
    pub processes: Vec<ProcessSnapshotEntry>,
}

// ── system-monitor event data ─────────────────────────────────────────────────

/// Written once at startup — gives static context to every subsequent sample.
#[derive(Serialize)]
pub struct SystemInfoData {
    /// e.g. "Intel(R) Core(TM) i7-12700K CPU @ 3.60GHz"
    pub cpu_brand:       String,
    pub cpu_arch:        String,
    pub cpu_core_count:  usize,
    pub memory_total_mb: f64,
    pub swap_total_mb:   f64,
    pub os_name:         String,
    pub os_version:      String,
    pub hostname:        String,
    /// Detected GPU devices (name only — real-time metrics are in `system_resource_sample`).
    pub gpus:            Vec<String>,
    /// "nvml" if NVIDIA GPU monitoring is active, "none" otherwise.
    pub gpu_monitoring:  String,
}

/// Usage of one logical CPU core.
#[derive(Serialize, Clone)]
pub struct CoreSample {
    /// Zero-based core index.
    pub id:            usize,
    pub used_percent:  f64,
    /// Current clock speed in MHz (0 if unavailable).
    pub frequency_mhz: u64,
}

/// One network interface entry inside a `system_resource_sample` event.
#[derive(Serialize, Clone)]
pub struct NetworkSample {
    pub interface:        String,
    /// Megabytes received since the previous poll.
    pub rx_mb_per_sec:    f64,
    /// Megabytes transmitted since the previous poll.
    pub tx_mb_per_sec:    f64,
    /// Cumulative receive errors on this interface.
    pub rx_errors:        u64,
    /// Cumulative transmit errors on this interface.
    pub tx_errors:        u64,
}

/// One drive entry inside a `system_resource_sample` event.
#[derive(Serialize, Clone)]
pub struct DiskSample {
    pub path:         String,
    pub total_gb:     f64,
    pub free_gb:      f64,
    /// free_gb / total_gb × 100
    pub free_percent: f64,
}

/// One GPU entry inside a `system_resource_sample` event.
///
/// Populated only when the `nvidia` feature is enabled and an NVIDIA driver
/// is present.  AMD / Intel GPUs show up in `system_info` but do not yet
/// provide real-time utilisation metrics.
#[derive(Serialize, Clone)]
pub struct GpuSample {
    /// GPU index (0-based, matches nvidia-smi order).
    pub index:             u32,
    /// Full device name, e.g. "NVIDIA GeForce RTX 4090".
    pub name:              String,
    /// Overall GPU engine utilisation (0–100 %).
    pub gpu_used_percent:  f64,
    /// VRAM total in MB.
    pub vram_total_mb:     f64,
    /// VRAM currently in use in MB.
    pub vram_used_mb:      f64,
    /// VRAM available in MB.
    pub vram_free_mb:      f64,
    /// vram_free_mb / vram_total_mb × 100.
    pub vram_free_percent: f64,
    /// GPU core temperature in °C.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c:     Option<u32>,
    /// NVENC hardware encoder utilisation (0–100 %).  None if unsupported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_percent:   Option<u32>,
    /// NVDEC hardware decoder utilisation (0–100 %).  None if unsupported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder_percent:   Option<u32>,
    /// Current power draw in watts.  None if unsupported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_w:           Option<u32>,
}

/// Written every `poll_interval_ms` by system-monitor.
///
/// All thresholds are evaluated here; separate `*_alert` events are emitted
/// alongside this entry when a threshold is crossed.
#[derive(Serialize)]
pub struct SystemResourceSampleData {
    // ── CPU ──────────────────────────────────────────────────────────────────
    /// Percentage of all logical CPU cores currently in use (0–100).
    pub cpu_used_percent: f64,
    /// Headroom left for new workloads: 100 − used.
    pub cpu_free_percent: f64,
    /// Per-core breakdown (useful for detecting single-core bottlenecks).
    pub cores:            Vec<CoreSample>,

    // ── Memory ───────────────────────────────────────────────────────────────
    pub memory_total_mb:     f64,
    pub memory_used_mb:      f64,
    /// Available memory (free + reclaimable on Linux; "Available" on Windows).
    pub memory_free_mb:      f64,
    pub memory_free_percent: f64,

    // ── Swap / pagefile ───────────────────────────────────────────────────────
    pub swap_total_mb:    f64,
    pub swap_used_mb:     f64,
    pub swap_used_percent: f64,

    // ── Network ───────────────────────────────────────────────────────────────
    pub network: Vec<NetworkSample>,

    // ── Disk ──────────────────────────────────────────────────────────────────
    pub disks: Vec<DiskSample>,

    // ── GPU ───────────────────────────────────────────────────────────────────
    /// One entry per detected NVIDIA GPU.  Empty when NVML is unavailable.
    pub gpus: Vec<GpuSample>,
}

// ── monitor-watchdog event data ───────────────────────────────────────────────

/// A child monitor process was started or restarted.
#[derive(Serialize)]
pub struct ChildStartedData {
    pub name:          String,
    pub pid:           u32,
    /// 0 on first start; increments on every restart.
    pub restart_count: u32,
}

/// A child monitor process exited unexpectedly.
#[derive(Serialize)]
pub struct ChildExitedData {
    pub name:           String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code:      Option<i32>,
    pub uptime_seconds: u64,
}

// ── go2rtc-monitor event data ─────────────────────────────────────────────────

/// One stream entry inside a `stream_sample` event.
#[derive(Serialize)]
pub struct StreamInfo {
    pub name:            String,
    /// true when at least one producer is in an active state.
    pub producer_active: bool,
    /// URL of the first producer, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_url:    Option<String>,
    /// Number of current consumers (viewers).
    pub consumer_count:  usize,
}

/// Written every `poll_interval_ms` by go2rtc-monitor.
#[derive(Serialize)]
pub struct StreamSampleData {
    pub streams:      Vec<StreamInfo>,
    pub total_count:  usize,
    pub active_count: usize,
}

/// A stream's producer became active (`stream_up`) or went offline (`stream_down`).
#[derive(Serialize)]
pub struct StreamStateChangeData {
    pub name:         String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_url: Option<String>,
}

/// The consumer count for a stream changed.
#[derive(Serialize)]
pub struct ConsumerChangeData {
    pub name:           String,
    pub consumer_count: usize,
    pub previous_count: usize,
}
