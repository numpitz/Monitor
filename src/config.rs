//! Configuration loaded from `<log_dir>/monitor.config.json`.
//!
//! All fields have sane defaults so the config can be minimal.
//! The file is re-read on disk change by the config-watcher thread
//! (see lib.rs) and the Arc<RwLock<Config>> is swapped atomically.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Top-level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub log_rotation: LogRotationConfig,

    #[serde(default)]
    pub ui: UiConfig,

    pub monitors: MonitorsConfig,
}

// ── UI settings ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiConfig {
    /// How often monitor-ui re-reads log files (seconds). `0` = manual only.
    #[serde(default = "default_ui_refresh_secs")]
    pub refresh_secs: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { refresh_secs: default_ui_refresh_secs() }
    }
}

fn default_ui_refresh_secs() -> u32 { 5 }

impl Config {
    pub fn load(log_dir: &Path) -> Result<Self> {
        let path = log_dir.join("monitor.config.json");
        let raw  = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("cannot parse {}", path.display()))
    }
}

// ── Log rotation ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogRotationConfig {
    #[serde(default = "default_max_mb")]
    pub max_file_size_mb: u64,

    #[serde(default = "default_keep")]
    pub keep_files: u32,
}

impl Default for LogRotationConfig {
    fn default() -> Self {
        Self { max_file_size_mb: default_max_mb(), keep_files: default_keep() }
    }
}

fn default_max_mb() -> u64 { 10 }
fn default_keep()   -> u32 { 5  }

// ── Monitors section ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitorsConfig {
    pub process_monitor: ProcessMonitorConfig,

    #[serde(default)]
    pub system_monitor: SystemMonitorConfig,

    #[serde(default)]
    pub go2rtc_monitor: Go2rtcMonitorConfig,
}

// ── process-monitor ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessMonitorConfig {
    #[serde(default = "yes")]
    pub enabled: bool,

    #[serde(default = "default_proc_log_file")]
    pub log_file: String,

    /// How often to sample CPU / memory / handles (milliseconds).
    #[serde(default = "default_resource_poll_ms")]
    pub resource_poll_interval_ms: u64,

    /// How often to write a full process-tree snapshot (milliseconds).
    #[serde(default = "default_snapshot_ms")]
    pub snapshot_interval_ms: u64,

    /// Granularity of the sleep loop (milliseconds).
    /// Controls how quickly the monitor reacts to interval changes or Ctrl-C.
    /// Smaller = more responsive; larger = less CPU overhead. Default 500 ms.
    #[serde(default = "default_min_tick_ms")]
    pub min_tick_ms: u64,

    /// Absolute paths. Every .exe found here is watched by name.
    pub watch_folders: Vec<String>,

    #[serde(default)]
    pub log: ProcessMonitorLogConfig,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessMonitorLogConfig {
    #[serde(default = "yes")] pub cpu_percent:   bool,
    #[serde(default = "yes")] pub memory_mb:     bool,
    #[serde(default = "yes")] pub handle_count:  bool,
    #[serde(default = "yes")] pub thread_count:  bool,
    #[serde(default = "yes")] pub process_spawn: bool,
    #[serde(default = "yes")] pub process_exit:  bool,
    #[serde(default = "yes")] pub snapshot:      bool,

    /// Emit a cpu_alert entry when a process exceeds this threshold.
    #[serde(default)]
    pub cpu_alert_threshold_percent: Option<f64>,

    /// Emit a memory_alert entry when a process exceeds this threshold (MB).
    #[serde(default)]
    pub memory_alert_mb: Option<f64>,
}

impl Default for ProcessMonitorLogConfig {
    fn default() -> Self {
        Self {
            cpu_percent:                 true,
            memory_mb:                   true,
            handle_count:                true,
            thread_count:                true,
            process_spawn:               true,
            process_exit:                true,
            snapshot:                    true,
            cpu_alert_threshold_percent: None,
            memory_alert_mb:             None,
        }
    }
}

// ── system-monitor ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMonitorConfig {
    #[serde(default = "yes")]
    pub enabled: bool,

    #[serde(default = "default_sys_log_file")]
    pub log_file: String,

    /// How often to sample system-wide resources (milliseconds).
    /// Default 30 s — system resources change slowly and streaming servers
    /// need stable averages, not noisy second-by-second snapshots.
    #[serde(default = "default_sys_poll_ms")]
    pub poll_interval_ms: u64,

    /// Granularity of the sleep loop — see ProcessMonitorConfig for details.
    #[serde(default = "default_min_tick_ms")]
    pub min_tick_ms: u64,

    /// Disk mount points to measure free space on (e.g. `["C:\\"]`).
    /// An empty list means *all* mounted disks are reported.
    #[serde(default)]
    pub watch_disks: Vec<String>,

    /// Network interface names to include (e.g. `["Ethernet", "Wi-Fi"]`).
    /// An empty list means *all* non-loopback interfaces are reported.
    #[serde(default)]
    pub watch_network_interfaces: Vec<String>,

    #[serde(default)]
    pub log: SystemMonitorLogConfig,
}

impl Default for SystemMonitorConfig {
    fn default() -> Self {
        Self {
            enabled:                  true,
            log_file:                 default_sys_log_file(),
            poll_interval_ms:         default_sys_poll_ms(),
            min_tick_ms:              default_min_tick_ms(),
            watch_disks:              Vec::new(),
            watch_network_interfaces: Vec::new(),
            log:                      SystemMonitorLogConfig::default(),
        }
    }
}

/// Fine-grained control over what system-monitor logs and when it alerts.
///
/// Two-tier alerting:
/// - `*_warn_*`  → logged at **WARN** level (approaching a limit)
/// - `*_alert_*` → logged at **ERROR** level (limit breached, action needed)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMonitorLogConfig {
    // ── Toggle groups ─────────────────────────────────────────────────────────
    #[serde(default = "yes")] pub cpu:         bool,
    /// Log individual core usage and frequency (useful for ffmpeg bottleneck detection).
    #[serde(default = "yes")] pub cpu_per_core: bool,
    #[serde(default = "yes")] pub memory:      bool,
    /// Log swap / pagefile usage.  High swap = stream stutter.
    #[serde(default = "yes")] pub swap:        bool,
    #[serde(default = "yes")] pub disk:        bool,
    /// Log per-interface network throughput and errors.
    #[serde(default = "yes")] pub network:     bool,

    // ── CPU thresholds (system-wide free headroom) ────────────────────────────
    /// WARN when free CPU headroom falls below this %.
    #[serde(default)] pub cpu_warn_free_percent:  Option<f64>,
    /// ERROR when free CPU headroom falls below this %.
    #[serde(default)] pub cpu_alert_free_percent: Option<f64>,

    // ── CPU per-core thresholds ───────────────────────────────────────────────
    /// WARN when any single core exceeds this % (ffmpeg single-core bottleneck).
    #[serde(default)] pub cpu_core_warn_percent:  Option<f64>,
    /// ERROR when any single core exceeds this %.
    #[serde(default)] pub cpu_core_alert_percent: Option<f64>,

    // ── Memory thresholds (available RAM) ─────────────────────────────────────
    /// WARN when available RAM falls below this MB.
    #[serde(default)] pub memory_warn_free_mb:  Option<f64>,
    /// ERROR when available RAM falls below this MB.
    #[serde(default)] pub memory_alert_free_mb: Option<f64>,

    // ── Swap / pagefile thresholds ────────────────────────────────────────────
    /// WARN when swap used % exceeds this value.
    #[serde(default)] pub swap_warn_used_percent:  Option<f64>,
    /// ERROR when swap used % exceeds this value.
    #[serde(default)] pub swap_alert_used_percent: Option<f64>,

    // ── Disk thresholds ───────────────────────────────────────────────────────
    /// WARN when free space on any watched disk falls below this GB.
    #[serde(default)] pub disk_warn_free_gb:  Option<f64>,
    /// ERROR when free space on any watched disk falls below this GB.
    #[serde(default)] pub disk_alert_free_gb: Option<f64>,

    // ── Network thresholds ────────────────────────────────────────────────────
    /// Emit a WARN when receive throughput exceeds this value (MB/s) on any interface.
    #[serde(default)] pub network_rx_warn_mbps: Option<f64>,
    /// Emit a WARN when transmit throughput exceeds this value (MB/s) on any interface.
    #[serde(default)] pub network_tx_warn_mbps: Option<f64>,
    /// Emit an ERROR alert when any interface has receive or transmit errors.
    #[serde(default = "yes")] pub network_error_alert: bool,
    /// Emit a WARN alert when any interface has received or transmitted dropped packets.
    #[serde(default = "yes")] pub network_drop_alert: bool,

    // ── GPU thresholds (NVIDIA NVML only) ─────────────────────────────────────
    /// Log GPU metrics (requires `nvidia` feature flag at build time).
    #[serde(default = "yes")] pub gpu: bool,
    /// WARN when GPU overall utilisation exceeds this %.
    #[serde(default)] pub gpu_warn_util_percent:  Option<f64>,
    /// ERROR when GPU overall utilisation exceeds this %.
    #[serde(default)] pub gpu_alert_util_percent: Option<f64>,
    /// WARN when NVENC encoder utilisation exceeds this %.
    #[serde(default)] pub gpu_encoder_warn_percent: Option<f64>,
    /// WARN when available VRAM drops below this MB.
    #[serde(default)] pub gpu_vram_warn_free_mb:  Option<f64>,
    /// ERROR when available VRAM drops below this MB.
    #[serde(default)] pub gpu_vram_alert_free_mb: Option<f64>,
    /// WARN when GPU temperature exceeds this °C.
    #[serde(default)] pub gpu_temp_warn_c:  Option<f64>,
    /// ERROR when GPU temperature exceeds this °C.
    #[serde(default)] pub gpu_temp_alert_c: Option<f64>,
}

impl Default for SystemMonitorLogConfig {
    fn default() -> Self {
        Self {
            cpu:          true,
            cpu_per_core: true,
            memory:       true,
            swap:         true,
            disk:         true,
            network:      true,

            cpu_warn_free_percent:  Some(30.0),
            cpu_alert_free_percent: Some(10.0),

            cpu_core_warn_percent:  Some(85.0),
            cpu_core_alert_percent: Some(95.0),

            memory_warn_free_mb:  Some(1000.0),
            memory_alert_free_mb: Some(500.0),

            swap_warn_used_percent:  Some(30.0),
            swap_alert_used_percent: Some(70.0),

            disk_warn_free_gb:  Some(20.0),
            disk_alert_free_gb: Some(10.0),

            network_rx_warn_mbps:  None,
            network_tx_warn_mbps:  None,
            network_error_alert:   true,
            network_drop_alert:    true,

            gpu:                      true,
            gpu_warn_util_percent:    Some(80.0),
            gpu_alert_util_percent:   Some(95.0),
            gpu_encoder_warn_percent: Some(80.0),
            gpu_vram_warn_free_mb:    Some(500.0),
            gpu_vram_alert_free_mb:   Some(200.0),
            gpu_temp_warn_c:          Some(80.0),
            gpu_temp_alert_c:         Some(90.0),
        }
    }
}

// ── Defaults ──────────────────────────────────────────────────────────────────

fn yes()                      -> bool   { true }
fn default_proc_log_file()    -> String { "proc_resources.jsonl".into() }
fn default_resource_poll_ms() -> u64    { 5_000 }
fn default_snapshot_ms()      -> u64    { 60_000 }
fn default_sys_log_file()     -> String { "sys_resources.jsonl".into() }
fn default_sys_poll_ms()      -> u64    { 30_000 }

// ── go2rtc-monitor ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Go2rtcMonitorConfig {
    /// Disabled by default — go2rtc may not be present on every system.
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_go2rtc_log_file")]
    pub log_file: String,

    /// Base URL of the go2rtc instance, e.g. `http://localhost:1984`.
    #[serde(default = "default_go2rtc_api_url")]
    pub api_url: String,

    /// How often to poll the go2rtc streams API (milliseconds). `0` = off.
    #[serde(default = "default_go2rtc_poll_ms")]
    pub poll_interval_ms: u64,

    /// Granularity of the sleep loop — see ProcessMonitorConfig for details.
    #[serde(default = "default_min_tick_ms")]
    pub min_tick_ms: u64,

    #[serde(default)]
    pub log: Go2rtcMonitorLogConfig,
}

impl Default for Go2rtcMonitorConfig {
    fn default() -> Self {
        Self {
            enabled:          false,
            log_file:         default_go2rtc_log_file(),
            api_url:          default_go2rtc_api_url(),
            poll_interval_ms: default_go2rtc_poll_ms(),
            min_tick_ms:      default_min_tick_ms(),
            log:              Go2rtcMonitorLogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Go2rtcMonitorLogConfig {
    /// Log `stream_up` / `stream_down` when a stream's producer state changes.
    #[serde(default = "yes")] pub stream_changes:   bool,
    /// Log `consumer_change` when viewer count changes for a stream.
    #[serde(default = "yes")] pub consumer_changes: bool,
    /// Log a full `stream_sample` on every poll.
    #[serde(default = "yes")] pub stream_sample:    bool,
}

impl Default for Go2rtcMonitorLogConfig {
    fn default() -> Self {
        Self {
            stream_changes:   true,
            consumer_changes: true,
            stream_sample:    true,
        }
    }
}

fn default_go2rtc_log_file() -> String { "go2rtc_streams.jsonl".into() }
fn default_go2rtc_api_url()  -> String { "http://localhost:1984".into() }
fn default_go2rtc_poll_ms()  -> u64    { 10_000 }
fn default_min_tick_ms()     -> u64    { 500 }
