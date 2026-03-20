//! All structured event types written to the NDJSON log.
//!
//! Every log line is a `LogEntry<T>` serialised with serde_json.
//! Using `#[serde(flatten)]` the `data` fields appear at the top level
//! of each JSON object, keeping lines compact and easy to grep.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

pub const MONITOR_NAME: &str = "process_monitor";

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
//
// Produces JSON like:
//   {"ts":"...","monitor":"process_monitor","event":"resource_sample",
//    "level":"INFO","processes":[...]}
//
// The `data` struct is flattened so its fields appear at the root level.

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
    pub fn info(event: &'a str, data: T) -> Self {
        Self { ts: Utc::now(), monitor: MONITOR_NAME, event, level: Level::Info, data }
    }
    pub fn warn(event: &'a str, data: T) -> Self {
        Self { ts: Utc::now(), monitor: MONITOR_NAME, event, level: Level::Warn, data }
    }
    pub fn error(event: &'a str, data: T) -> Self {
        Self { ts: Utc::now(), monitor: MONITOR_NAME, event, level: Level::Error, data }
    }
}

// ── Per-event data structs ────────────────────────────────────────────────────

/// Written as the very first entry in every log file (including after rotation).
#[derive(Serialize)]
pub struct MonitorStartData {
    pub pid:      u32,
    pub log_file: String,
    /// true when this entry was caused by log rotation (not initial startup)
    pub rotation: bool,
    /// name of the previous log file (only present when rotation == true)
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
    pub pid:             u32,
    pub name:            String,
    pub uptime_seconds:  u64,
}

/// One row in a resource_sample event.
#[derive(Serialize)]
pub struct ProcessSample {
    pub pid:         u32,
    pub name:        String,
    pub cpu_percent: f64,   // % of one logical CPU core
    pub memory_mb:   f64,   // working set in MB
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
