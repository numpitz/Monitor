//! Configuration loaded from `<log_dir>/monitor.config.json`.
//!
//! All fields have sane defaults so the config can be minimal.
//! The file is re-read on disk change by the config-watcher thread
//! (see lib.rs) and the Arc<RwLock<Config>> is swapped atomically.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

// ── Top-level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub log_rotation: LogRotationConfig,

    pub monitors: MonitorsConfig,
}

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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct MonitorsConfig {
    pub process_monitor: ProcessMonitorConfig,

    #[serde(default)]
    pub system_monitor: SystemMonitorConfig,
}

// ── process-monitor ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
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

    /// Absolute paths. Every .exe found here is watched by name.
    pub watch_folders: Vec<String>,

    #[serde(default)]
    pub log: ProcessMonitorLogConfig,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct SystemMonitorConfig {
    #[serde(default = "yes")]
    pub enabled: bool,

    #[serde(default = "default_sys_log_file")]
    pub log_file: String,

    /// How often to sample system-wide CPU, RAM and disk (milliseconds).
    /// Default 30 s — system resources change slowly.
    #[serde(default = "default_sys_poll_ms")]
    pub poll_interval_ms: u64,

    /// Disk mount points to measure free space on (e.g. `["C:\\"]`).
    /// An empty list means *all* mounted disks are reported.
    #[serde(default)]
    pub watch_disks: Vec<String>,

    #[serde(default)]
    pub log: SystemMonitorLogConfig,
}

impl Default for SystemMonitorConfig {
    fn default() -> Self {
        Self {
            enabled:          true,
            log_file:         default_sys_log_file(),
            poll_interval_ms: default_sys_poll_ms(),
            watch_disks:      Vec::new(),
            log:              SystemMonitorLogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemMonitorLogConfig {
    #[serde(default = "yes")] pub cpu:    bool,
    #[serde(default = "yes")] pub memory: bool,
    #[serde(default = "yes")] pub disk:   bool,

    /// Emit a `cpu_headroom_alert` when free CPU headroom falls below this %.
    #[serde(default)]
    pub cpu_alert_free_percent: Option<f64>,

    /// Emit a `memory_headroom_alert` when available RAM falls below this MB.
    #[serde(default)]
    pub memory_alert_free_mb: Option<f64>,

    /// Emit a `disk_headroom_alert` when free space on any watched disk falls
    /// below this GB.
    #[serde(default)]
    pub disk_alert_free_gb: Option<f64>,
}

impl Default for SystemMonitorLogConfig {
    fn default() -> Self {
        Self {
            cpu:                    true,
            memory:                 true,
            disk:                   true,
            cpu_alert_free_percent: None,
            memory_alert_free_mb:   None,
            disk_alert_free_gb:     None,
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
