//! monitor-ui — egui configuration editor and live viewer for the monitor suite.
//!
//! Usage:
//!   monitor-ui.exe <LOG_DIR>
//!
//! Panels:
//!   1. Configuration  — edit poll intervals and enabled flags for all monitors
//!   2. Watched Processes — live table rebuilt from proc_resources.N.jsonl
//!   3. System Resources  — last sample from sys_resources.N.jsonl with progress bars
//!   4. go2rtc Streams    — last stream_sample from go2rtc_streams.N.jsonl

use eframe::egui;
use egui_plot;
use process_monitor::config::Config;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let log_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Monitor Configuration")
            .with_inner_size([640.0, 960.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Monitor Configuration",
        options,
        Box::new(move |_cc| Ok(Box::new(MonitorApp::load(log_dir)))),
    )
}

// ── Data types for the viewers ────────────────────────────────────────────────

#[derive(Default)]
struct ProcessRow {
    pid:         u32,
    name:        String,
    cpu_percent: f64,
    memory_mb:   f64,
    handles:     u32,
    threads:     u32,
    /// HH:MM:SS of the last resource_sample that included this process
    /// (or spawn / exit time when no sample is available).
    last_seen:   String,
    /// false once a `process_exited` event is seen for this PID.
    alive:       bool,
}

struct NetRow {
    interface: String,
    rx:        f64,
    tx:        f64,
    errors:    u64,
    dropped:   u64,
}

struct DiskRow {
    path:          String,
    total_gb:      f64,
    free_gb:       f64,
    free_pct:      f64,
    read_mb_per_sec:  f64,
    write_mb_per_sec: f64,
}

struct CoreRow {
    id:       u32,
    used_pct: f64,
}

struct GpuRow {
    name:         String,
    util_pct:     f64,
    vram_total_mb: f64,
    vram_used_mb:  f64,
    vram_free_mb:  f64,
    temp_c:        Option<u32>,
    encoder_pct:   Option<u32>,
    decoder_pct:   Option<u32>,
    power_w:       Option<f64>,
}

struct SysSample {
    ts:                  String,
    cpu_used_pct:        f64,
    cpu_free_pct:        f64,
    memory_total_mb:     f64,
    memory_used_mb:      f64,
    memory_free_pct:     f64,
    swap_total_mb:       f64,
    swap_used_mb:        f64,
    swap_used_pct:       f64,
    cores:               Vec<CoreRow>,
    network:             Vec<NetRow>,
    disks:               Vec<DiskRow>,
    gpus:                Vec<GpuRow>,
}

struct StreamRow {
    name:            String,
    producer_active: bool,
    producer_url:    String,
    consumer_count:  u32,
    ts:              String,
}

// ── Timeline ──────────────────────────────────────────────────────────────────

const N_BUCKETS: usize = 60;

#[derive(Clone, Default)]
struct TimelineBucket {
    ts_start:     f64,   // unix seconds
    ts_end:       f64,
    proc_count:   usize,
    sys_count:    usize,
    go2rtc_count: usize,
    warn_count:   usize,
    error_count:  usize,
}

impl TimelineBucket {
    fn total(&self) -> usize { self.proc_count + self.sys_count + self.go2rtc_count }
}

#[derive(Clone)]
struct Timeline {
    buckets: Vec<TimelineBucket>,
    t_start: f64,
    t_end:   f64,
}

// ── Tab selection ─────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Tab { Runtime, Configuration }

// ── App state ─────────────────────────────────────────────────────────────────

struct MonitorApp {
    selected_tab: Tab,
    log_dir: PathBuf,
    config:  Result<Config, String>,

    // ── Configuration panel ───────────────────────────────────────────────────
    proc_enabled:        bool,
    sys_enabled:         bool,
    proc_poll_secs:      u32,
    proc_snapshot_secs:  u32,
    proc_min_tick_ms:    u32,
    sys_poll_secs:       u32,
    sys_min_tick_ms:     u32,
    dirty:               bool,
    status:              String,

    // ── Process viewer ────────────────────────────────────────────────────────
    proc_rows:         Vec<ProcessRow>,
    proc_last_refresh: Option<Instant>,
    proc_source_file:  String,

    // ── System resource viewer ────────────────────────────────────────────────
    sys_sample:        Option<SysSample>,
    sys_last_refresh:  Option<Instant>,
    sys_source_file:   String,

    // ── go2rtc config ─────────────────────────────────────────────────────────
    go2rtc_enabled:      bool,
    go2rtc_api_url:      String,
    go2rtc_poll_secs:    u32,
    go2rtc_min_tick_ms:  u32,

    // ── go2rtc stream viewer ──────────────────────────────────────────────────
    go2rtc_stream_rows:   Vec<StreamRow>,
    go2rtc_last_refresh:  Option<Instant>,
    go2rtc_source_file:   String,

    /// How often the UI re-reads the log files (seconds). 0 = manual only.
    ui_refresh_secs: u32,

    // ── Timeline ──────────────────────────────────────────────────────────────
    timeline:     Option<Timeline>,
    /// 0 = oldest bucket … N_BUCKETS-1 = live (no cutoff).
    timeline_idx: usize,

    // ── Resource plot ─────────────────────────────────────────────────────────
    /// All collected time-series. Keys: "cpu", "ram", "swap",
    /// "net:{iface}:rx", "net:{iface}:tx", "disk:{path}:rd", "disk:{path}:wr",
    /// "gpu:{name}:util"
    plot_series:   HashMap<String, Vec<[f64; 2]>>,
    /// Checkbox keys currently selected for display. Uses the "group" key
    /// (e.g. "net:eth0") which maps to one or more series keys.
    plot_selected: HashSet<String>,
}

impl MonitorApp {
    fn load(log_dir: PathBuf) -> Self {
        let mut app = match Config::load(&log_dir) {
            Ok(cfg) => {
                let proc_poll_secs     = (cfg.monitors.process_monitor.resource_poll_interval_ms / 1_000) as u32;
                let proc_snapshot_secs = (cfg.monitors.process_monitor.snapshot_interval_ms       / 1_000) as u32;
                let proc_min_tick_ms   = cfg.monitors.process_monitor.min_tick_ms as u32;
                let sys_poll_secs      = (cfg.monitors.system_monitor.poll_interval_ms            / 1_000) as u32;
                let sys_min_tick_ms    = cfg.monitors.system_monitor.min_tick_ms as u32;
                let go2rtc_poll_secs   = (cfg.monitors.go2rtc_monitor.poll_interval_ms / 1_000) as u32;
                let go2rtc_min_tick_ms = cfg.monitors.go2rtc_monitor.min_tick_ms as u32;
                let ui_refresh_secs    = cfg.ui.refresh_secs;
                Self {
                    log_dir,
                    proc_enabled: cfg.monitors.process_monitor.enabled,
                    sys_enabled:  cfg.monitors.system_monitor.enabled,
                    go2rtc_enabled:    cfg.monitors.go2rtc_monitor.enabled,
                    go2rtc_api_url:    cfg.monitors.go2rtc_monitor.api_url.clone(),
                    go2rtc_poll_secs,
                    go2rtc_min_tick_ms,
                    config: Ok(cfg),
                    proc_poll_secs,
                    proc_snapshot_secs,
                    proc_min_tick_ms,
                    sys_poll_secs,
                    sys_min_tick_ms,
                    dirty:  false,
                    status: String::new(),
                    proc_rows:         Vec::new(),
                    proc_last_refresh: None,
                    proc_source_file:  String::new(),
                    sys_sample:        None,
                    sys_last_refresh:  None,
                    sys_source_file:   String::new(),
                    go2rtc_stream_rows:  Vec::new(),
                    go2rtc_last_refresh: None,
                    go2rtc_source_file:  String::new(),
                    ui_refresh_secs,
                    selected_tab:  Tab::Runtime,
                    timeline:      None,
                    timeline_idx:  N_BUCKETS - 1,
                    plot_series:   HashMap::new(),
                    plot_selected: ["cpu".to_string(), "ram".to_string()].into_iter().collect(),
                }
            }
            Err(e) => Self {
                log_dir,
                config: Err(e.to_string()),
                proc_enabled:       true,
                sys_enabled:        true,
                go2rtc_enabled:     false,
                go2rtc_api_url:     "http://localhost:1984".into(),
                go2rtc_poll_secs:   10,
                go2rtc_min_tick_ms: 500,
                proc_poll_secs:     5,
                proc_snapshot_secs: 60,
                proc_min_tick_ms:   500,
                sys_poll_secs:      30,
                sys_min_tick_ms:    500,
                dirty:  false,
                status: String::new(),
                proc_rows:           Vec::new(),
                proc_last_refresh:   None,
                proc_source_file:    String::new(),
                sys_sample:          None,
                sys_last_refresh:    None,
                sys_source_file:     String::new(),
                go2rtc_stream_rows:  Vec::new(),
                go2rtc_last_refresh: None,
                go2rtc_source_file:  String::new(),
                ui_refresh_secs:     5,
                selected_tab:  Tab::Runtime,
                timeline:      None,
                timeline_idx:  N_BUCKETS - 1,
                plot_series:   HashMap::new(),
                plot_selected: ["cpu".to_string(), "ram".to_string()].into_iter().collect(),
            },
        };
        app.refresh_processes(None);
        app.refresh_system(None);
        app.refresh_streams(None);
        app.build_timeline();
        app
    }

    fn save(&mut self) {
        let cfg = match &mut self.config {
            Ok(c)  => c,
            Err(_) => return,
        };
        cfg.monitors.process_monitor.enabled                    = self.proc_enabled;
        cfg.monitors.process_monitor.resource_poll_interval_ms = self.proc_poll_secs     as u64 * 1_000;
        cfg.monitors.process_monitor.snapshot_interval_ms      = self.proc_snapshot_secs as u64 * 1_000;
        cfg.monitors.process_monitor.min_tick_ms               = self.proc_min_tick_ms   as u64;
        cfg.monitors.system_monitor.enabled                    = self.sys_enabled;
        cfg.monitors.system_monitor.poll_interval_ms           = self.sys_poll_secs      as u64 * 1_000;
        cfg.monitors.system_monitor.min_tick_ms                = self.sys_min_tick_ms    as u64;
        cfg.monitors.go2rtc_monitor.enabled                    = self.go2rtc_enabled;
        cfg.monitors.go2rtc_monitor.api_url                    = self.go2rtc_api_url.clone();
        cfg.monitors.go2rtc_monitor.poll_interval_ms           = self.go2rtc_poll_secs   as u64 * 1_000;
        cfg.monitors.go2rtc_monitor.min_tick_ms                = self.go2rtc_min_tick_ms as u64;
        cfg.ui.refresh_secs                                    = self.ui_refresh_secs;

        let json = match serde_json::to_string_pretty(&cfg) {
            Ok(j)  => j,
            Err(e) => { self.status = format!("Serialise error: {e}"); return; }
        };
        let tmp_path    = self.log_dir.join("monitor.config.json.tmp");
        let config_path = self.log_dir.join("monitor.config.json");
        if let Err(e) = std::fs::write(&tmp_path, &json) {
            self.status = format!("Write error: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &config_path) {
            self.status = format!("Rename error: {e}");
            return;
        }
        self.dirty  = false;
        self.status = "Saved — monitors will pick up the change automatically.".into();
    }

    // ── Timeline helpers ──────────────────────────────────────────────────────

    /// True when viewing the live (newest) end of the timeline.
    fn is_live(&self) -> bool {
        match &self.timeline {
            None     => true,
            Some(tl) => self.timeline_idx >= tl.buckets.len().saturating_sub(1),
        }
    }

    /// Returns `None` (no cutoff = live) or `Some(ts)` for historical views.
    fn selected_cutoff_ts(&self) -> Option<f64> {
        if self.is_live() { return None; }
        self.timeline.as_ref()
            .and_then(|tl| tl.buckets.get(self.timeline_idx))
            .map(|b| b.ts_end)
    }

    /// Scan all log files and rebuild the 60-bucket activity heatmap.
    fn build_timeline(&mut self) {
        let proc_base = match &self.config {
            Ok(cfg) => cfg.monitors.process_monitor.log_file.clone(),
            Err(_)  => "proc_resources.jsonl".to_string(),
        };
        let sys_base = match &self.config {
            Ok(cfg) => cfg.monitors.system_monitor.log_file.clone(),
            Err(_)  => "sys_resources.jsonl".to_string(),
        };
        let go2rtc_base = match &self.config {
            Ok(cfg) => cfg.monitors.go2rtc_monitor.log_file.clone(),
            Err(_)  => "go2rtc_streams.jsonl".to_string(),
        };

        let mut timestamps: Vec<(f64, usize)> = Vec::new(); // (unix_secs, source 0/1/2)
        for (src, base) in [&proc_base, &sys_base, &go2rtc_base].iter().enumerate() {
            for path in find_all_logs(&self.log_dir, base) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for line in content.lines() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                            if let Some(ts) = parse_ts_secs(&v) {
                                timestamps.push((ts, src));
                            }
                        }
                    }
                }
            }
        }

        if timestamps.is_empty() { self.timeline = None; return; }

        let t_start = timestamps.iter().map(|(t, _)| *t).fold(f64::MAX, f64::min);
        let t_end   = timestamps.iter().map(|(t, _)| *t).fold(f64::MIN, f64::max);
        let dur     = (t_end - t_start).max(1.0);
        let bucket_dur = dur / N_BUCKETS as f64;

        let mut buckets = vec![TimelineBucket::default(); N_BUCKETS];
        for i in 0..N_BUCKETS {
            buckets[i].ts_start = t_start + i as f64 * bucket_dur;
            buckets[i].ts_end   = t_start + (i + 1) as f64 * bucket_dur;
        }
        // Second pass: fill bucket counters (we need level too, so re-scan from files)
        // Re-use the timestamps vec by also storing level: 0=info,1=warn,2=error
        drop(timestamps); // free memory before re-scan

        let mut entries: Vec<(f64, usize, u8)> = Vec::new(); // (ts, src, level)
        for (src, base) in [&proc_base, &sys_base, &go2rtc_base].iter().enumerate() {
            for path in find_all_logs(&self.log_dir, base) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for line in content.lines() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                            if let Some(ts) = parse_ts_secs(&v) {
                                let lvl = match v.get("level").and_then(|l| l.as_str()) {
                                    Some("ERROR") => 2,
                                    Some("WARN")  => 1,
                                    _             => 0,
                                };
                                entries.push((ts, src, lvl));
                            }
                        }
                    }
                }
            }
        }

        for (ts, src, lvl) in &entries {
            let idx = (((ts - t_start) / bucket_dur) as usize).min(N_BUCKETS - 1);
            match src {
                0 => buckets[idx].proc_count   += 1,
                1 => buckets[idx].sys_count    += 1,
                2 => buckets[idx].go2rtc_count += 1,
                _ => {}
            }
            match lvl {
                2 => buckets[idx].error_count += 1,
                1 => buckets[idx].warn_count  += 1,
                _ => {}
            }
        }

        let was_live = self.is_live();
        self.timeline = Some(Timeline { buckets, t_start, t_end });
        if was_live { self.timeline_idx = N_BUCKETS - 1; }
        // else keep the existing historical position

        self.build_plot_data();
    }

    /// Scan system log files and build all available time-series for the chart.
    fn build_plot_data(&mut self) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.system_monitor.log_file.clone(),
            Err(_)  => "sys_resources.jsonl".to_string(),
        };
        let mut series: HashMap<String, Vec<[f64; 2]>> = HashMap::new();
        for path in find_all_logs(&self.log_dir, &base) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                for line in content.lines() {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                    if v.get("event").and_then(|e| e.as_str()) != Some("system_resource_sample") { continue; }
                    let Some(ts) = parse_ts_secs(&v) else { continue };

                    // CPU (system-wide)
                    let cpu = v.get("cpu_used_percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    series.entry("cpu".into()).or_default().push([ts, cpu]);

                    // Per-core CPU
                    if let Some(cores) = v.get("cores").and_then(|c| c.as_array()) {
                        for core in cores {
                            let id   = core.get("id")          .and_then(|x| x.as_u64()).unwrap_or(0);
                            let used = core.get("used_percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            series.entry(format!("cpu_core:{id}")).or_default().push([ts, used]);
                        }
                    }

                    // RAM
                    let mem_total = v.get("memory_total_mb").and_then(|x| x.as_f64()).unwrap_or(1.0);
                    let mem_used  = v.get("memory_used_mb") .and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let ram_pct   = if mem_total > 0.0 { mem_used / mem_total * 100.0 } else { 0.0 };
                    series.entry("ram".into()).or_default().push([ts, ram_pct]);

                    // Swap
                    let swap_total = v.get("swap_total_mb").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let swap_used  = v.get("swap_used_mb") .and_then(|x| x.as_f64()).unwrap_or(0.0);
                    if swap_total > 0.0 {
                        let swap_pct = swap_used / swap_total * 100.0;
                        series.entry("swap".into()).or_default().push([ts, swap_pct]);
                    }

                    // Network
                    if let Some(nets) = v.get("network").and_then(|n| n.as_array()) {
                        for net in nets {
                            let iface = val_str(net, "interface");
                            if iface.is_empty() { continue; }
                            let rx = net.get("rx_mb_per_sec").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            let tx = net.get("tx_mb_per_sec").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            series.entry(format!("net:{iface}:rx")).or_default().push([ts, rx]);
                            series.entry(format!("net:{iface}:tx")).or_default().push([ts, tx]);
                        }
                    }

                    // Disks
                    if let Some(disks) = v.get("disks").and_then(|d| d.as_array()) {
                        for disk in disks {
                            let path = val_str(disk, "path");
                            if path.is_empty() { continue; }
                            let rd = disk.get("read_mb_per_sec") .and_then(|x| x.as_f64()).unwrap_or(0.0);
                            let wr = disk.get("write_mb_per_sec").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            series.entry(format!("disk:{path}:rd")).or_default().push([ts, rd]);
                            series.entry(format!("disk:{path}:wr")).or_default().push([ts, wr]);
                        }
                    }

                    // GPUs
                    if let Some(gpus) = v.get("gpus").and_then(|g| g.as_array()) {
                        for gpu in gpus {
                            let name = val_str(gpu, "name");
                            if name.is_empty() { continue; }
                            let util = gpu.get("gpu_used_percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            series.entry(format!("gpu:{name}:util")).or_default().push([ts, util]);
                            // VRAM used %
                            let vram_total = gpu.get("vram_total_mb").and_then(|x| x.as_f64()).unwrap_or(1.0);
                            let vram_used  = gpu.get("vram_used_mb") .and_then(|x| x.as_f64()).unwrap_or(0.0);
                            if vram_total > 0.0 {
                                let vram_pct = vram_used / vram_total * 100.0;
                                series.entry(format!("gpu:{name}:vram")).or_default().push([ts, vram_pct]);
                            }
                            // Optional fields (only present when hardware supports them)
                            if let Some(t) = gpu.get("temperature_c").and_then(|x| x.as_f64()) {
                                series.entry(format!("gpu:{name}:temp")).or_default().push([ts, t]);
                            }
                            if let Some(e) = gpu.get("encoder_percent").and_then(|x| x.as_f64()) {
                                series.entry(format!("gpu:{name}:encoder")).or_default().push([ts, e]);
                            }
                            if let Some(d) = gpu.get("decoder_percent").and_then(|x| x.as_f64()) {
                                series.entry(format!("gpu:{name}:decoder")).or_default().push([ts, d]);
                            }
                            if let Some(p) = gpu.get("power_w").and_then(|x| x.as_f64()) {
                                series.entry(format!("gpu:{name}:power")).or_default().push([ts, p]);
                            }
                        }
                    }
                }
            }
        }
        for pts in series.values_mut() {
            pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
        }
        self.plot_series = series;
    }

    // ── Data refresh ──────────────────────────────────────────────────────────

    /// Rebuild the process table.
    /// `until_ts = None` → read only the newest file (live).
    /// `until_ts = Some(ts)` → read all files, skip entries after ts.
    fn refresh_processes(&mut self, until_ts: Option<f64>) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.process_monitor.log_file.clone(),
            Err(_)  => "proc_resources.jsonl".to_string(),
        };
        let paths: Vec<std::path::PathBuf> = if until_ts.is_some() {
            find_all_logs(&self.log_dir, &base)
        } else {
            find_latest_log(&self.log_dir, &base).into_iter().collect()
        };
        if paths.is_empty() {
            self.proc_source_file  = "no log file found".into();
            self.proc_last_refresh = Some(Instant::now());
            return;
        }

        let mut map: HashMap<u32, ProcessRow> = HashMap::new();
        for path in &paths {
            let content = match std::fs::read_to_string(path) {
                Ok(c)  => c,
                Err(e) => { self.proc_source_file = format!("read error: {e}"); return; }
            };
            for line in content.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if let Some(cut) = until_ts {
                    if parse_ts_secs(&v).map_or(false, |ts| ts > cut) { continue; }
                }
                match v.get("event").and_then(|e| e.as_str()).unwrap_or("") {
                    "process_spawned" => {
                        let pid = val_u32(&v, "pid");
                        let name = val_str(&v, "name");
                        let last_seen = ts_time(&v);
                        map.entry(pid).or_insert_with(|| ProcessRow {
                            pid, name, last_seen, alive: true, ..Default::default()
                        });
                    }
                    "process_exited" => {
                        let pid = val_u32(&v, "pid");
                        if let Some(row) = map.get_mut(&pid) {
                            row.alive = false; row.last_seen = ts_time(&v);
                        }
                    }
                    "resource_sample" => {
                        let sample_ts = ts_time(&v);
                        if let Some(procs) = v.get("processes").and_then(|p| p.as_array()) {
                            for p in procs {
                                let pid  = val_u32(p, "pid");
                                let name = val_str(p, "name");
                                let row  = map.entry(pid).or_insert_with(|| ProcessRow {
                                    pid, name: name.clone(), alive: true, ..Default::default()
                                });
                                row.cpu_percent = p.get("cpu_percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
                                row.memory_mb   = p.get("memory_mb")  .and_then(|x| x.as_f64()).unwrap_or(0.0);
                                row.handles     = p.get("handles")    .and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                                row.threads     = p.get("threads")    .and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                                row.last_seen   = sample_ts.clone();
                                row.alive       = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut rows: Vec<ProcessRow> = map.into_values().collect();
        rows.sort_by(|a, b| b.alive.cmp(&a.alive).then(a.name.cmp(&b.name)));
        self.proc_rows         = rows;
        self.proc_last_refresh = Some(Instant::now());
        self.proc_source_file  = paths.last().and_then(|p| p.file_name())
            .unwrap_or_default().to_string_lossy().into_owned();
    }

    /// Read the system_resource_sample at or before the cutoff.
    fn refresh_system(&mut self, until_ts: Option<f64>) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.system_monitor.log_file.clone(),
            Err(_)  => "sys_resources.jsonl".to_string(),
        };
        let paths: Vec<std::path::PathBuf> = if until_ts.is_some() {
            find_all_logs(&self.log_dir, &base)
        } else {
            find_latest_log(&self.log_dir, &base).into_iter().collect()
        };
        if paths.is_empty() {
            self.sys_source_file  = "no log file found".into();
            self.sys_last_refresh = Some(Instant::now());
            return;
        }

        let mut last: Option<serde_json::Value> = None;
        for path in &paths {
            let content = match std::fs::read_to_string(path) {
                Ok(c)  => c,
                Err(e) => { self.sys_source_file = format!("read error: {e}"); return; }
            };
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(cut) = until_ts {
                        if parse_ts_secs(&v).map_or(false, |ts| ts > cut) { continue; }
                    }
                    if v.get("event").and_then(|e| e.as_str()) == Some("system_resource_sample") {
                        last = Some(v);
                    }
                }
            }
        }

        self.sys_sample       = last.as_ref().map(parse_sys_sample);
        self.sys_last_refresh = Some(Instant::now());
        self.sys_source_file  = paths.last().and_then(|p| p.file_name())
            .unwrap_or_default().to_string_lossy().into_owned();
    }

    /// Read the stream_sample at or before the cutoff.
    fn refresh_streams(&mut self, until_ts: Option<f64>) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.go2rtc_monitor.log_file.clone(),
            Err(_)  => "go2rtc_streams.jsonl".to_string(),
        };
        let paths: Vec<std::path::PathBuf> = if until_ts.is_some() {
            find_all_logs(&self.log_dir, &base)
        } else {
            find_latest_log(&self.log_dir, &base).into_iter().collect()
        };
        if paths.is_empty() {
            self.go2rtc_source_file  = "no log file found".into();
            self.go2rtc_last_refresh = Some(Instant::now());
            return;
        }

        let mut last: Option<serde_json::Value> = None;
        for path in &paths {
            let content = match std::fs::read_to_string(path) {
                Ok(c)  => c,
                Err(e) => { self.go2rtc_source_file = format!("read error: {e}"); return; }
            };
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(cut) = until_ts {
                        if parse_ts_secs(&v).map_or(false, |ts| ts > cut) { continue; }
                    }
                    if v.get("event").and_then(|e| e.as_str()) == Some("stream_sample") {
                        last = Some(v);
                    }
                }
            }
        }

        self.go2rtc_stream_rows = if let Some(ref v) = last {
            let ts = ts_time(v);
            v.get("streams").and_then(|s| s.as_array())
                .map(|arr| arr.iter().map(|s| StreamRow {
                    name:            val_str(s, "name"),
                    producer_active: s.get("producer_active").and_then(|x| x.as_bool()).unwrap_or(false),
                    producer_url:    val_str(s, "producer_url"),
                    consumer_count:  s.get("consumer_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                    ts:              ts.clone(),
                }).collect())
                .unwrap_or_default()
        } else { Vec::new() };

        self.go2rtc_last_refresh = Some(Instant::now());
        self.go2rtc_source_file  = paths.last().and_then(|p| p.file_name())
            .unwrap_or_default().to_string_lossy().into_owned();
    }
}

// ── egui render loop ──────────────────────────────────────────────────────────

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-refresh — only when viewing live data.
        if self.ui_refresh_secs > 0 {
            let interval    = Duration::from_secs(self.ui_refresh_secs as u64);
            ctx.request_repaint_after(interval);
            if self.is_live() {
                let proc_due    = self.proc_last_refresh   .map_or(true, |t| t.elapsed() >= interval);
                let sys_due     = self.sys_last_refresh    .map_or(true, |t| t.elapsed() >= interval);
                let streams_due = self.go2rtc_last_refresh .map_or(true, |t| t.elapsed() >= interval);
                if proc_due    { self.refresh_processes(None); }
                if sys_due     { self.refresh_system(None); }
                if streams_due { self.refresh_streams(None); }
                if proc_due || sys_due || streams_due { self.build_timeline(); }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ── Header ────────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.heading("Monitor");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(self.log_dir.display().to_string())
                        .small().color(egui::Color32::GRAY));
                });
            });
            ui.add_space(4.0);

            // ── Tab bar ───────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.selected_tab, Tab::Runtime,       "Runtime");
                ui.selectable_value(&mut self.selected_tab, Tab::Configuration, "Configuration");
            });
            ui.separator();
            ui.add_space(4.0);

            // ── Tab content ───────────────────────────────────────────────────
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.selected_tab {

                    // ══════════════════════════════════════════════════════════
                    Tab::Runtime => {
                        // ── Timeline heatmap ───────────────────────────────────
                        let tl_clone = self.timeline.clone();
                        let mut new_timeline_idx: Option<usize> = None;
                        let mut do_jump_live    = false;
                        let mut do_refresh_all  = false;

                        if let Some(ref tl) = tl_clone {
                            let n        = tl.buckets.len();
                            let avail_w  = ui.available_width();
                            let cell_h   = 36.0_f32;
                            let max_tot  = tl.buckets.iter().map(|b| b.total()).max().unwrap_or(1).max(1);
                            let bucket_w = avail_w / n as f32;

                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(avail_w, cell_h),
                                egui::Sense::click_and_drag(),
                            );
                            let painter = ui.painter_at(rect);
                            painter.rect_filled(rect, 3.0, egui::Color32::from_gray(28));

                            for (i, bucket) in tl.buckets.iter().enumerate() {
                                let x0   = rect.left() + i as f32 * bucket_w + 1.0;
                                let cell = egui::Rect::from_min_size(
                                    egui::pos2(x0, rect.top() + 3.0),
                                    egui::vec2((bucket_w - 2.0).max(1.0), cell_h - 6.0),
                                );
                                painter.rect_filled(cell, 2.0, heatmap_color(bucket.total(), max_tot, bucket.warn_count, bucket.error_count));
                                if i == self.timeline_idx {
                                    painter.rect_stroke(cell, 2.0,
                                        egui::Stroke::new(2.0, egui::Color32::WHITE));
                                }
                            }
                            // Cursor line
                            let cx = rect.left() + (self.timeline_idx as f32 + 0.5) * bucket_w;
                            painter.line_segment(
                                [egui::pos2(cx, rect.top()), egui::pos2(cx, rect.bottom())],
                                egui::Stroke::new(1.5, egui::Color32::from_rgba_premultiplied(255,255,255,180)),
                            );

                            // Extract interaction data before consuming response
                            let clicked       = response.clicked();
                            let dragged       = response.dragged();
                            let interact_pos  = response.interact_pointer_pos();
                            let hover_pos     = response.hover_pos();

                            let hover_text: Option<String> = hover_pos.map(|pos| {
                                let frac = ((pos.x - rect.left()) / avail_w).clamp(0.0, 1.0);
                                let idx  = ((frac * n as f32) as usize).min(n - 1);
                                let b    = &tl.buckets[idx];
                                let mut lines = vec![
                                    format!("{} – {}", fmt_bucket_time(b.ts_start), fmt_bucket_time(b.ts_end)),
                                ];
                                if b.proc_count   > 0 { lines.push(format!("Process monitor: {} entries",  b.proc_count));   }
                                if b.sys_count    > 0 { lines.push(format!("System monitor:  {} entries",  b.sys_count));    }
                                if b.go2rtc_count > 0 { lines.push(format!("go2rtc monitor:  {} entries",  b.go2rtc_count)); }
                                if b.error_count  > 0 { lines.push(format!("⚠ Errors:  {}", b.error_count)); }
                                if b.warn_count   > 0 { lines.push(format!("⚠ Warnings: {}", b.warn_count));  }
                                if b.total()      == 0 { lines.push("No log entries".to_string()); }
                                lines.join("\n")
                            });
                            if let Some(text) = hover_text { response.on_hover_text(text); }

                            if clicked || dragged {
                                if let Some(pos) = interact_pos {
                                    let frac = ((pos.x - rect.left()) / avail_w).clamp(0.0, 1.0);
                                    let idx  = ((frac * n as f32) as usize).min(n - 1);
                                    if idx != self.timeline_idx { new_timeline_idx = Some(idx); }
                                }
                            }

                            // Status bar
                            let is_live_now = self.timeline_idx >= n - 1;
                            let cutoff_label = if !is_live_now {
                                tl.buckets.get(self.timeline_idx)
                                    .map(|b| fmt_bucket_datetime(b.ts_end))
                                    .unwrap_or_default()
                            } else { String::new() };
                            let t_start_label = fmt_bucket_time(tl.t_start);
                            let t_end_label   = fmt_bucket_time(tl.t_end);

                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(t_start_label).small().color(egui::Color32::GRAY));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(egui::RichText::new(t_end_label).small().color(egui::Color32::GRAY));
                                });
                            });
                            ui.horizontal(|ui| {
                                if is_live_now {
                                    ui.colored_label(egui::Color32::from_rgb(80, 200, 80), "● LIVE");
                                } else {
                                    ui.label(format!("Viewing up to: {cutoff_label}"));
                                    if ui.small_button("Jump to live").clicked() { do_jump_live = true; }
                                }
                                ui.separator();
                                if ui.small_button("⟳  Refresh").clicked() { do_refresh_all = true; }
                                ui.separator();
                                ui.label("Auto-refresh:");
                                let before = self.ui_refresh_secs;
                                ui.add(egui::Slider::new(&mut self.ui_refresh_secs, 0..=60)
                                    .suffix(" s").clamping(egui::SliderClamping::Always));
                                if self.ui_refresh_secs != before { self.dirty = true; self.status.clear(); }
                            });
                        } else {
                            // No timeline yet: plain controls
                            ui.horizontal(|ui| {
                                if ui.small_button("⟳  Refresh all").clicked() { do_refresh_all = true; }
                                ui.separator();
                                ui.label("Auto-refresh:");
                                let before = self.ui_refresh_secs;
                                ui.add(egui::Slider::new(&mut self.ui_refresh_secs, 0..=60)
                                    .suffix(" s").clamping(egui::SliderClamping::Always));
                                if self.ui_refresh_secs != before { self.dirty = true; self.status.clear(); }
                            });
                        }

                        // Apply timeline interactions
                        if let Some(idx) = new_timeline_idx {
                            self.timeline_idx = idx;
                            let cut = self.selected_cutoff_ts();
                            self.refresh_processes(cut);
                            self.refresh_system(cut);
                            self.refresh_streams(cut);
                        }
                        if do_jump_live {
                            self.timeline_idx = N_BUCKETS - 1;
                            self.refresh_processes(None);
                            self.refresh_system(None);
                            self.refresh_streams(None);
                            self.build_timeline();
                        }
                        if do_refresh_all {
                            let cut = self.selected_cutoff_ts();
                            self.refresh_processes(cut);
                            self.refresh_system(cut);
                            self.refresh_streams(cut);
                            if self.is_live() { self.build_timeline(); }
                        }

                        // ── Resource plot ──────────────────────────────────────
                        if !self.plot_series.is_empty() && !self.plot_selected.is_empty() {
                            let cutoff = self.selected_cutoff_ts();
                            let palette = [
                                egui::Color32::from_rgb( 80, 200,  80),  // green
                                egui::Color32::from_rgb(100, 150, 230),  // blue
                                egui::Color32::from_rgb(230, 160,  60),  // orange
                                egui::Color32::from_rgb( 80, 210, 210),  // teal
                                egui::Color32::from_rgb(200,  80, 200),  // purple
                                egui::Color32::from_rgb(230, 230,  80),  // yellow
                                egui::Color32::from_rgb(230, 100,  80),  // red-orange
                                egui::Color32::from_rgb(230,  80, 160),  // pink
                            ];
                            // Resolve selected group-keys → (series_key, label, color)
                            let mut lines: Vec<(String, String, egui::Color32)> = Vec::new();
                            let mut ci = 0usize;
                            let mut sel_sorted: Vec<&String> = self.plot_selected.iter().collect();
                            sel_sorted.sort();
                            for sel in sel_sorted {
                                let c0 = palette[ci % palette.len()];
                                match sel.as_str() {
                                    "cpu"  => { lines.push(("cpu".into(),  "CPU %".into(),  c0)); ci += 1; }
                                    "ram"  => { lines.push(("ram".into(),  "RAM %".into(),  c0)); ci += 1; }
                                    "swap" => { lines.push(("swap".into(), "Swap %".into(), c0)); ci += 1; }
                                    s if s.starts_with("net:") => {
                                        let iface = &s["net:".len()..];
                                        lines.push((format!("net:{iface}:rx"), format!("{iface} RX MB/s"), c0));
                                        ci += 1;
                                        let c1 = palette[ci % palette.len()];
                                        lines.push((format!("net:{iface}:tx"), format!("{iface} TX MB/s"), c1));
                                        ci += 1;
                                    }
                                    s if s.starts_with("disk:") => {
                                        let p = &s["disk:".len()..];
                                        let short = p.split(['/', '\\']).filter(|s| !s.is_empty()).last().unwrap_or(p);
                                        lines.push((format!("disk:{p}:rd"), format!("{short} RD MB/s"), c0));
                                        ci += 1;
                                        let c1 = palette[ci % palette.len()];
                                        lines.push((format!("disk:{p}:wr"), format!("{short} WR MB/s"), c1));
                                        ci += 1;
                                    }
                                    s if s.starts_with("cpu_core:") => {
                                        let id = &s["cpu_core:".len()..];
                                        lines.push((s.to_string(), format!("Core {id} %"), c0));
                                        ci += 1;
                                    }
                                    s if s.starts_with("gpu:") => {
                                        // Direct key if it ends with a known metric suffix,
                                        // otherwise treat as group key → :util
                                        const GPU_SUFFIXES: &[(&str, &str)] = &[
                                            (":util",    "util %"),
                                            (":vram",    "VRAM %"),
                                            (":temp",    "°C"),
                                            (":encoder", "Enc %"),
                                            (":decoder", "Dec %"),
                                            (":power",   "W"),
                                        ];
                                        if let Some(&(sfx, unit)) = GPU_SUFFIXES.iter()
                                            .find(|(sfx, _)| s.ends_with(sfx))
                                        {
                                            let name = &s["gpu:".len()..s.len() - sfx.len()];
                                            let short: String = name.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                                            lines.push((s.to_string(), format!("{short} {unit}"), c0));
                                            ci += 1;
                                        } else {
                                            // Bare "gpu:{name}" → default to util %
                                            let name = &s["gpu:".len()..];
                                            let short: String = name.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                                            lines.push((format!("gpu:{name}:util"), format!("{short} util %"), c0));
                                            ci += 1;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            let has_data = lines.iter().any(|(k, _, _)|
                                self.plot_series.get(k).map_or(false, |v| !v.is_empty()));
                            if has_data {
                                egui_plot::Plot::new("resource_plot")
                                    .height(140.0)
                                    .include_y(0.0)
                                    .x_axis_formatter(|mark, _range| fmt_bucket_time(mark.value))
                                    .legend(egui_plot::Legend::default().position(egui_plot::Corner::LeftTop))
                                    .show(ui, |plot_ui| {
                                        for (series_key, label, color) in &lines {
                                            if let Some(all_pts) = self.plot_series.get(series_key) {
                                                let pts: Vec<[f64; 2]> = all_pts.iter()
                                                    .filter(|p| cutoff.map_or(true, |c| p[0] <= c))
                                                    .copied().collect();
                                                if !pts.is_empty() {
                                                    plot_ui.line(
                                                        egui_plot::Line::new(egui_plot::PlotPoints::new(pts))
                                                            .color(*color)
                                                            .name(label.as_str()),
                                                    );
                                                }
                                            }
                                        }
                                    });
                            }
                        }

                        ui.add_space(8.0);
                        ui.separator();

                        // ── Watched Processes ──────────────────────────────────
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Watched Processes").strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("⟳").clicked() {
                                    let cut = self.selected_cutoff_ts();
                                    self.refresh_processes(cut);
                                }
                                if !self.proc_source_file.is_empty() {
                                    ui.label(egui::RichText::new(&self.proc_source_file)
                                        .small().color(egui::Color32::GRAY));
                                }
                            });
                        });
                        ui.add_space(4.0);

                        if self.proc_rows.is_empty() {
                            ui.label(egui::RichText::new("No processes — is process-monitor running?")
                                .color(egui::Color32::GRAY));
                        } else {
                            egui::Grid::new("proc_header")
                                .num_columns(7).spacing([12.0, 2.0])
                                .show(ui, |ui| {
                                    for label in ["Name", "PID", "CPU %", "Mem MB", "Handles", "Threads", "Last seen"] {
                                        ui.label(egui::RichText::new(label).strong().small());
                                    }
                                    ui.end_row();
                                });
                            ui.separator();
                            egui::ScrollArea::vertical()
                                .id_salt("proc_scroll")
                                .max_height(200.0)
                                .show(ui, |ui| {
                                    egui::Grid::new("proc_table")
                                        .num_columns(7).spacing([12.0, 4.0]).striped(true)
                                        .show(ui, |ui| {
                                            for row in &self.proc_rows {
                                                let color = if row.alive { egui::Color32::WHITE } else { egui::Color32::GRAY };
                                                ui.label(egui::RichText::new(&row.name).color(color));
                                                ui.label(egui::RichText::new(row.pid.to_string()).color(color));
                                                if row.alive {
                                                    ui.label(format!("{:.1}", row.cpu_percent));
                                                    ui.label(format!("{:.1}", row.memory_mb));
                                                    ui.label(row.handles.to_string());
                                                    ui.label(row.threads.to_string());
                                                } else {
                                                    for _ in 0..4 {
                                                        ui.label(egui::RichText::new("—").color(egui::Color32::GRAY));
                                                    }
                                                }
                                                ui.label(egui::RichText::new(&row.last_seen)
                                                    .small().color(egui::Color32::GRAY));
                                                ui.end_row();
                                            }
                                        });
                                });
                        }

                        // ── System Resources ───────────────────────────────────
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("System Resources").strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("⟳").clicked() {
                                    let cut = self.selected_cutoff_ts();
                                    self.refresh_system(cut);
                                }
                                if !self.sys_source_file.is_empty() {
                                    ui.label(egui::RichText::new(&self.sys_source_file)
                                        .small().color(egui::Color32::GRAY));
                                }
                            });
                        });
                        ui.add_space(4.0);

                        if self.sys_sample.is_none() {
                            ui.label(egui::RichText::new("No data — is system-monitor running?")
                                .color(egui::Color32::GRAY));
                        } else {
                            let s = self.sys_sample.as_ref().unwrap();

                            ui.label(egui::RichText::new(format!("Last sample: {}", s.ts))
                                .small().color(egui::Color32::GRAY));
                            ui.add_space(6.0);

                            // CPU
                            ui.horizontal(|ui| {
                                let mut sel = self.plot_selected.contains("cpu");
                                if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                    if sel { self.plot_selected.insert("cpu".into()); }
                                    else   { self.plot_selected.remove("cpu"); }
                                }
                                ui.label(egui::RichText::new("CPU ").strong().monospace());
                                ui.add(egui::ProgressBar::new(s.cpu_used_pct as f32 / 100.0)
                                    .desired_width(220.0)
                                    .fill(threshold_color(s.cpu_used_pct, 70.0, 90.0, Dir::Above))
                                    .text(format!("{:.1}% used", s.cpu_used_pct)));
                                ui.label(egui::RichText::new(format!("{:.1}% free", s.cpu_free_pct))
                                    .color(threshold_color(s.cpu_free_pct, 30.0, 10.0, Dir::Below)));
                            });

                            // Per-core CPU — 2-per-row, same style as CPU/RAM/Swap
                            if !s.cores.is_empty() {
                                ui.add_space(2.0);
                                egui::Grid::new("core_grid")
                                    .num_columns(2).spacing([12.0, 2.0])
                                    .show(ui, |ui| {
                                        for (i, core) in s.cores.iter().enumerate() {
                                            let key = format!("cpu_core:{}", core.id);
                                            let mut sel = self.plot_selected.contains(&key);
                                            ui.horizontal(|ui| {
                                                if ui.checkbox(&mut sel, "")
                                                    .on_hover_text("Show in chart").changed() {
                                                    if sel { self.plot_selected.insert(key.clone()); }
                                                    else   { self.plot_selected.remove(&key); }
                                                }
                                                ui.label(egui::RichText::new(format!("C{} ", core.id))
                                                    .monospace().small().color(egui::Color32::GRAY));
                                                ui.add(egui::ProgressBar::new(core.used_pct as f32 / 100.0)
                                                    .desired_width(130.0)
                                                    .fill(threshold_color(core.used_pct, 70.0, 90.0, Dir::Above))
                                                    .text(format!("{:.0}%", core.used_pct)));
                                            });
                                            if i % 2 == 1 { ui.end_row(); }
                                        }
                                        if s.cores.len() % 2 == 1 { ui.end_row(); }
                                    });
                            }

                            // RAM
                            let mem_used_frac = (s.memory_used_mb / s.memory_total_mb.max(1.0)) as f32;
                            ui.horizontal(|ui| {
                                let mut sel = self.plot_selected.contains("ram");
                                if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                    if sel { self.plot_selected.insert("ram".into()); }
                                    else   { self.plot_selected.remove("ram"); }
                                }
                                ui.label(egui::RichText::new("RAM ").strong().monospace());
                                ui.add(egui::ProgressBar::new(mem_used_frac)
                                    .desired_width(220.0)
                                    .fill(threshold_color(s.memory_free_pct, 30.0, 15.0, Dir::Below))
                                    .text(format!("{:.0} / {:.0} MB", s.memory_used_mb, s.memory_total_mb)));
                                ui.label(egui::RichText::new(format!("{:.1}% free", s.memory_free_pct))
                                    .color(threshold_color(s.memory_free_pct, 30.0, 15.0, Dir::Below)));
                            });

                            // Swap
                            if s.swap_total_mb > 0.0 {
                                let swap_frac = (s.swap_used_mb / s.swap_total_mb.max(1.0)) as f32;
                                ui.horizontal(|ui| {
                                    let mut sel = self.plot_selected.contains("swap");
                                    if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                        if sel { self.plot_selected.insert("swap".into()); }
                                        else   { self.plot_selected.remove("swap"); }
                                    }
                                    ui.label(egui::RichText::new("Swap").strong().monospace());
                                    ui.add(egui::ProgressBar::new(swap_frac)
                                        .desired_width(220.0)
                                        .fill(threshold_color(s.swap_used_pct, 30.0, 70.0, Dir::Above))
                                        .text(format!("{:.0} / {:.0} MB", s.swap_used_mb, s.swap_total_mb)));
                                    ui.label(egui::RichText::new(format!("{:.1}% used", s.swap_used_pct))
                                        .color(threshold_color(s.swap_used_pct, 30.0, 70.0, Dir::Above)));
                                });
                            }

                            // Network
                            if !s.network.is_empty() {
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new("Network").strong());
                                egui::Grid::new("net_grid")
                                    .num_columns(6).spacing([16.0, 3.0]).striped(true)
                                    .show(ui, |ui| {
                                        for n in &s.network {
                                            let key = format!("net:{}", n.interface);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(&n.interface);
                                            ui.label(format!("DN  {:.2} MB/s", n.rx));
                                            ui.label(format!("UP  {:.2} MB/s", n.tx));
                                            if n.errors > 0 {
                                                ui.label(egui::RichText::new(format!("{} errors", n.errors))
                                                    .color(egui::Color32::RED));
                                            } else {
                                                ui.label(egui::RichText::new("no errors").color(egui::Color32::GRAY));
                                            }
                                            if n.dropped > 0 {
                                                ui.label(egui::RichText::new(format!("{} dropped", n.dropped))
                                                    .color(egui::Color32::YELLOW));
                                            } else {
                                                ui.label(egui::RichText::new("no drops").color(egui::Color32::GRAY));
                                            }
                                            ui.end_row();
                                        }
                                    });
                            }

                            // Disks
                            if !s.disks.is_empty() {
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new("Disks").strong());
                                egui::Grid::new("disk_grid")
                                    .num_columns(7).spacing([16.0, 3.0]).striped(true)
                                    .show(ui, |ui| {
                                        for d in &s.disks {
                                            let key = format!("disk:{}", d.path);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            let used_frac = 1.0 - (d.free_pct as f32 / 100.0);
                                            ui.label(&d.path);
                                            ui.add(egui::ProgressBar::new(used_frac)
                                                .desired_width(140.0)
                                                .fill(threshold_color(d.free_gb, 20.0, 10.0, Dir::Below))
                                                .text(format!("{:.1} GB free", d.free_gb)));
                                            ui.label(format!("/ {:.1} GB", d.total_gb));
                                            ui.label(egui::RichText::new(format!("{:.1}% free", d.free_pct))
                                                .color(threshold_color(d.free_gb, 20.0, 10.0, Dir::Below)));
                                            ui.label(format!("RD {:.2} MB/s", d.read_mb_per_sec));
                                            ui.label(format!("WR {:.2} MB/s", d.write_mb_per_sec));
                                            ui.end_row();
                                        }
                                    });
                            }

                            // GPUs
                            if !s.gpus.is_empty() {
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new("GPU").strong());
                                egui::Grid::new("gpu_grid")
                                    .num_columns(6).spacing([16.0, 3.0]).striped(true)
                                    .show(ui, |ui| {
                                        for g in &s.gpus {
                                            let key = format!("gpu:{}:util", g.name);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("util % in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(&g.name);
                                            ui.label(egui::RichText::new(format!("{:.0}% util", g.util_pct))
                                                .color(threshold_color(g.util_pct, 80.0, 95.0, Dir::Above)));
                                            ui.label(egui::RichText::new(format!("{:.0} MB VRAM free", g.vram_free_mb))
                                                .color(threshold_color(g.vram_free_mb, 500.0, 200.0, Dir::Below)));
                                            if let Some(t) = g.temp_c {
                                                ui.label(egui::RichText::new(format!("{}°C", t))
                                                    .color(threshold_color(t as f64, 80.0, 90.0, Dir::Above)));
                                            } else {
                                                ui.label(egui::RichText::new("—").color(egui::Color32::GRAY));
                                            }
                                            if let Some(enc) = g.encoder_pct {
                                                ui.label(egui::RichText::new(format!("Enc {}%", enc))
                                                    .color(threshold_color(enc as f64, 80.0, 95.0, Dir::Above)));
                                            } else {
                                                ui.label("");
                                            }
                                            ui.end_row();
                                        }
                                    });
                                // Extended GPU metrics — indented rows, same style as other metrics
                                for g in &s.gpus {
                                    // VRAM
                                    if g.vram_total_mb > 0.0 {
                                        let vram_used_frac = (g.vram_used_mb / g.vram_total_mb.max(1.0)) as f32;
                                        let vram_free_pct  = g.vram_free_mb / g.vram_total_mb.max(1.0) * 100.0;
                                        ui.horizontal(|ui| {
                                            ui.add_space(20.0);
                                            let key = format!("gpu:{}:vram", g.name);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(egui::RichText::new("VRAM").strong().monospace());
                                            ui.add(egui::ProgressBar::new(vram_used_frac)
                                                .desired_width(180.0)
                                                .fill(threshold_color(vram_free_pct, 25.0, 10.0, Dir::Below))
                                                .text(format!("{:.0}/{:.0} MB", g.vram_used_mb, g.vram_total_mb)));
                                            ui.label(egui::RichText::new(format!("{:.1}% free", vram_free_pct))
                                                .color(threshold_color(vram_free_pct, 25.0, 10.0, Dir::Below)));
                                        });
                                    }
                                    // Temperature
                                    if let Some(t) = g.temp_c {
                                        ui.horizontal(|ui| {
                                            ui.add_space(20.0);
                                            let key = format!("gpu:{}:temp", g.name);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(egui::RichText::new("Temp").strong().monospace());
                                            ui.label(egui::RichText::new(format!("{t} °C"))
                                                .color(threshold_color(t as f64, 80.0, 90.0, Dir::Above)));
                                        });
                                    }
                                    // Encoder
                                    if let Some(enc) = g.encoder_pct {
                                        ui.horizontal(|ui| {
                                            ui.add_space(20.0);
                                            let key = format!("gpu:{}:encoder", g.name);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(egui::RichText::new("Enc ").strong().monospace());
                                            ui.add(egui::ProgressBar::new(enc as f32 / 100.0)
                                                .desired_width(140.0)
                                                .fill(threshold_color(enc as f64, 80.0, 95.0, Dir::Above))
                                                .text(format!("{enc}%")));
                                        });
                                    }
                                    // Decoder
                                    if let Some(dec) = g.decoder_pct {
                                        ui.horizontal(|ui| {
                                            ui.add_space(20.0);
                                            let key = format!("gpu:{}:decoder", g.name);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(egui::RichText::new("Dec ").strong().monospace());
                                            ui.add(egui::ProgressBar::new(dec as f32 / 100.0)
                                                .desired_width(140.0)
                                                .fill(threshold_color(dec as f64, 80.0, 95.0, Dir::Above))
                                                .text(format!("{dec}%")));
                                        });
                                    }
                                    // Power
                                    if let Some(pwr) = g.power_w {
                                        ui.horizontal(|ui| {
                                            ui.add_space(20.0);
                                            let key = format!("gpu:{}:power", g.name);
                                            let mut sel = self.plot_selected.contains(&key);
                                            if ui.checkbox(&mut sel, "").on_hover_text("Show in chart").changed() {
                                                if sel { self.plot_selected.insert(key.clone()); }
                                                else   { self.plot_selected.remove(&key); }
                                            }
                                            ui.label(egui::RichText::new("Pwr ").strong().monospace());
                                            ui.label(format!("{pwr:.1} W"));
                                        });
                                    }
                                }
                            }
                        }

                        // ── go2rtc Streams ─────────────────────────────────────
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("go2rtc Streams").strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("⟳").clicked() {
                                    let cut = self.selected_cutoff_ts();
                                    self.refresh_streams(cut);
                                }
                                if !self.go2rtc_source_file.is_empty() {
                                    ui.label(egui::RichText::new(&self.go2rtc_source_file)
                                        .small().color(egui::Color32::GRAY));
                                }
                            });
                        });
                        ui.add_space(4.0);

                        if self.go2rtc_stream_rows.is_empty() {
                            ui.label(egui::RichText::new("No stream data — is go2rtc-monitor running?")
                                .color(egui::Color32::GRAY));
                        } else {
                            egui::Grid::new("stream_header")
                                .num_columns(5).spacing([12.0, 2.0])
                                .show(ui, |ui| {
                                    for label in ["Name", "Status", "Producer URL", "Consumers", "Last seen"] {
                                        ui.label(egui::RichText::new(label).strong().small());
                                    }
                                    ui.end_row();
                                });
                            ui.separator();
                            egui::ScrollArea::vertical()
                                .id_salt("stream_scroll")
                                .max_height(200.0)
                                .show(ui, |ui| {
                                    egui::Grid::new("stream_table")
                                        .num_columns(5).spacing([12.0, 4.0]).striped(true)
                                        .show(ui, |ui| {
                                            for row in &self.go2rtc_stream_rows {
                                                ui.label(&row.name);
                                                if row.producer_active {
                                                    ui.colored_label(egui::Color32::from_rgb(80, 180, 80), "Active");
                                                } else {
                                                    ui.colored_label(egui::Color32::GRAY, "Inactive");
                                                }
                                                ui.horizontal(|ui| {
                                                    if row.producer_url.is_empty() {
                                                        ui.label(egui::RichText::new("—")
                                                            .color(egui::Color32::GRAY).small());
                                                    } else {
                                                        let short: String = row.producer_url
                                                            .chars().take(40)
                                                            .chain(if row.producer_url.chars().count() > 40 { Some('…') } else { None })
                                                            .collect();
                                                        ui.label(egui::RichText::new(short).small().monospace())
                                                            .on_hover_text(&row.producer_url);
                                                        if ui.small_button("⧉").on_hover_text("Copy URL").clicked() {
                                                            ui.output_mut(|o| o.copied_text = row.producer_url.clone());
                                                        }
                                                    }
                                                });
                                                ui.label(row.consumer_count.to_string());
                                                ui.label(egui::RichText::new(&row.ts)
                                                    .small().color(egui::Color32::GRAY));
                                                ui.end_row();
                                            }
                                        });
                                });
                        }
                    } // Tab::Runtime

                    // ══════════════════════════════════════════════════════════
                    Tab::Configuration => {
                        if let Err(ref msg) = self.config {
                            ui.colored_label(egui::Color32::RED, format!("Cannot load config: {msg}"));
                            return;
                        }

                        // ── Process Monitor ────────────────────────────────────
                        ui.group(|ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let before = self.proc_enabled;
                                ui.checkbox(&mut self.proc_enabled, egui::RichText::new("Process Monitor").strong());
                                if self.proc_enabled != before { self.dirty = true; self.status.clear(); }
                                if !self.proc_enabled {
                                    ui.colored_label(egui::Color32::YELLOW, "  disabled");
                                }
                            });
                            ui.separator();
                            ui.add_enabled_ui(self.proc_enabled, |ui| {
                                egui::Grid::new("proc_grid").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                                    ui.label("Resource poll interval");
                                    let before = self.proc_poll_secs;
                                    ui.add(egui::Slider::new(&mut self.proc_poll_secs, 0..=60).suffix(" s").clamping(egui::SliderClamping::Always));
                                    ui.label(interval_hint(self.proc_poll_secs));
                                    if self.proc_poll_secs != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();

                                    ui.label("Snapshot interval");
                                    let before = self.proc_snapshot_secs;
                                    ui.add(egui::Slider::new(&mut self.proc_snapshot_secs, 0..=600).suffix(" s").clamping(egui::SliderClamping::Always));
                                    ui.label(interval_hint(self.proc_snapshot_secs));
                                    if self.proc_snapshot_secs != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();

                                    ui.label("Response interval");
                                    let before = self.proc_min_tick_ms;
                                    ui.add(egui::Slider::new(&mut self.proc_min_tick_ms, 50..=5000).suffix(" ms").clamping(egui::SliderClamping::Always));
                                    ui.label(egui::RichText::new("Ctrl-C / config reaction time").color(egui::Color32::GRAY).small());
                                    if self.proc_min_tick_ms != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();
                                });
                            });
                        });

                        ui.add_space(8.0);

                        // ── System Monitor ─────────────────────────────────────
                        ui.group(|ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let before = self.sys_enabled;
                                ui.checkbox(&mut self.sys_enabled, egui::RichText::new("System Monitor").strong());
                                if self.sys_enabled != before { self.dirty = true; self.status.clear(); }
                                if !self.sys_enabled {
                                    ui.colored_label(egui::Color32::YELLOW, "  disabled");
                                }
                            });
                            ui.separator();
                            ui.add_enabled_ui(self.sys_enabled, |ui| {
                                egui::Grid::new("sys_grid").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                                    ui.label("Poll interval");
                                    let before = self.sys_poll_secs;
                                    ui.add(egui::Slider::new(&mut self.sys_poll_secs, 0..=300).suffix(" s").clamping(egui::SliderClamping::Always));
                                    ui.label(interval_hint(self.sys_poll_secs));
                                    if self.sys_poll_secs != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();

                                    ui.label("Response interval");
                                    let before = self.sys_min_tick_ms;
                                    ui.add(egui::Slider::new(&mut self.sys_min_tick_ms, 50..=5000).suffix(" ms").clamping(egui::SliderClamping::Always));
                                    ui.label(egui::RichText::new("Ctrl-C / config reaction time").color(egui::Color32::GRAY).small());
                                    if self.sys_min_tick_ms != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();
                                });
                            });
                        });

                        ui.add_space(8.0);

                        // ── go2rtc Monitor ─────────────────────────────────────
                        ui.group(|ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let before = self.go2rtc_enabled;
                                ui.checkbox(&mut self.go2rtc_enabled, egui::RichText::new("go2rtc Monitor").strong());
                                if self.go2rtc_enabled != before { self.dirty = true; self.status.clear(); }
                                if !self.go2rtc_enabled {
                                    ui.colored_label(egui::Color32::YELLOW, "  disabled");
                                }
                            });
                            ui.separator();
                            ui.add_enabled_ui(self.go2rtc_enabled, |ui| {
                                egui::Grid::new("go2rtc_grid").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                                    ui.label("API URL");
                                    let before = self.go2rtc_api_url.clone();
                                    ui.add(egui::TextEdit::singleline(&mut self.go2rtc_api_url)
                                        .desired_width(260.0).hint_text("http://localhost:1984"));
                                    ui.label(egui::RichText::new("base URL of go2rtc").color(egui::Color32::GRAY).small());
                                    if self.go2rtc_api_url != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();

                                    ui.label("Poll interval");
                                    let before = self.go2rtc_poll_secs;
                                    ui.add(egui::Slider::new(&mut self.go2rtc_poll_secs, 0..=300).suffix(" s").clamping(egui::SliderClamping::Always));
                                    ui.label(interval_hint(self.go2rtc_poll_secs));
                                    if self.go2rtc_poll_secs != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();

                                    ui.label("Response interval");
                                    let before = self.go2rtc_min_tick_ms;
                                    ui.add(egui::Slider::new(&mut self.go2rtc_min_tick_ms, 50..=5000).suffix(" ms").clamping(egui::SliderClamping::Always));
                                    ui.label(egui::RichText::new("Ctrl-C / config reaction time").color(egui::Color32::GRAY).small());
                                    if self.go2rtc_min_tick_ms != before { self.dirty = true; self.status.clear(); }
                                    ui.end_row();
                                });
                            });
                        });

                        ui.add_space(8.0);

                        // ── UI refresh ─────────────────────────────────────────
                        ui.horizontal(|ui| {
                            ui.label("UI auto-refresh");
                            let before = self.ui_refresh_secs;
                            ui.add(egui::Slider::new(&mut self.ui_refresh_secs, 0..=60)
                                .suffix(" s").clamping(egui::SliderClamping::Always));
                            ui.label(egui::RichText::new(interval_hint(self.ui_refresh_secs))
                                .color(egui::Color32::GRAY).small());
                            if self.ui_refresh_secs != before { self.dirty = true; self.status.clear(); }
                        });

                        ui.add_space(12.0);

                        // ── Save button ────────────────────────────────────────
                        ui.horizontal(|ui| {
                            let save_btn = ui.add_enabled(self.dirty, egui::Button::new("💾  Save"));
                            if save_btn.clicked() { self.save(); }
                            if self.dirty {
                                ui.colored_label(egui::Color32::YELLOW, "  Unsaved changes");
                            } else if !self.status.is_empty() {
                                ui.colored_label(egui::Color32::GREEN, format!("  ✓  {}", self.status));
                            }
                        });
                    } // Tab::Configuration

                } // match
            }); // ScrollArea
        }); // CentralPanel
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return ALL rotation files for `base_name`, sorted oldest-first (lowest N first).
fn find_all_logs(log_dir: &Path, base_name: &str) -> Vec<PathBuf> {
    let stem = base_name.trim_end_matches(".jsonl");
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(log_dir) else { return Vec::new() };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();
        if let Some(rest) = name.strip_prefix(&format!("{stem}.")) {
            if let Some(n_str) = rest.strip_suffix(".jsonl") {
                if let Ok(n) = n_str.parse::<u32>() {
                    found.push((n, entry.path()));
                }
            }
        }
    }
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, p)| p).collect()
}

/// Parse the `ts` field of a log entry into unix seconds.
fn parse_ts_secs(v: &serde_json::Value) -> Option<f64> {
    let s = v.get("ts")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
}

/// Format unix seconds as `HH:MM` (UTC).
fn fmt_bucket_time(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Format unix seconds as `YYYY-MM-DD HH:MM:SS` (UTC).
fn fmt_bucket_datetime(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// GitHub-style four-level green heatmap colour. Grey = empty.
fn heatmap_color(count: usize, max_count: usize, warn: usize, error: usize) -> egui::Color32 {
    if count == 0 || max_count == 0 {
        return egui::Color32::from_gray(45);
    }
    if error > 0 { return egui::Color32::from_rgb(192,  57,  43); }  // red   #c0392b
    if warn  > 0 { return egui::Color32::from_rgb(212, 172,  13); }  // amber #d4ac0d
    let ratio = count as f32 / max_count as f32;
    if      ratio < 0.25 { egui::Color32::from_rgb(155, 233, 168) }  // #9be9a8
    else if ratio < 0.50 { egui::Color32::from_rgb( 64, 196,  99) }  // #40c463
    else if ratio < 0.75 { egui::Color32::from_rgb( 48, 161,  78) }  // #30a14e
    else                 { egui::Color32::from_rgb( 33, 110,  57) }  // #216e39
}

/// Find the highest-numbered rotation of `base_name` in `log_dir`.
/// Files are named `<stem>.<n>.jsonl`.
fn find_latest_log(log_dir: &Path, base_name: &str) -> Option<PathBuf> {
    let stem = base_name.trim_end_matches(".jsonl");
    let mut best: Option<(u32, PathBuf)> = None;
    let entries = std::fs::read_dir(log_dir).ok()?;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();
        if let Some(rest) = name.strip_prefix(&format!("{stem}.")) {
            if let Some(num_str) = rest.strip_suffix(".jsonl") {
                if let Ok(n) = num_str.parse::<u32>() {
                    if best.as_ref().map_or(true, |(bn, _)| n > *bn) {
                        best = Some((n, entry.path()));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn parse_sys_sample(v: &serde_json::Value) -> SysSample {
    let network = v.get("network").and_then(|n| n.as_array())
        .map(|arr| arr.iter().map(|n| NetRow {
            interface: val_str(n, "interface"),
            rx:        n.get("rx_mb_per_sec").and_then(|x| x.as_f64()).unwrap_or(0.0),
            tx:        n.get("tx_mb_per_sec").and_then(|x| x.as_f64()).unwrap_or(0.0),
            errors:    n.get("rx_errors").and_then(|x| x.as_u64()).unwrap_or(0)
                     + n.get("tx_errors").and_then(|x| x.as_u64()).unwrap_or(0),
            dropped:   n.get("rx_dropped").and_then(|x| x.as_u64()).unwrap_or(0)
                     + n.get("tx_dropped").and_then(|x| x.as_u64()).unwrap_or(0),
        }).collect())
        .unwrap_or_default();

    let disks = v.get("disks").and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(|d| DiskRow {
            path:             val_str(d, "path"),
            total_gb:         d.get("total_gb")        .and_then(|x| x.as_f64()).unwrap_or(0.0),
            free_gb:          d.get("free_gb")         .and_then(|x| x.as_f64()).unwrap_or(0.0),
            free_pct:         d.get("free_percent")    .and_then(|x| x.as_f64()).unwrap_or(0.0),
            read_mb_per_sec:  d.get("read_mb_per_sec") .and_then(|x| x.as_f64()).unwrap_or(0.0),
            write_mb_per_sec: d.get("write_mb_per_sec").and_then(|x| x.as_f64()).unwrap_or(0.0),
        }).collect())
        .unwrap_or_default();

    let cores = v.get("cores").and_then(|c| c.as_array())
        .map(|arr| arr.iter().map(|c| CoreRow {
            id:       c.get("id")          .and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            used_pct: c.get("used_percent").and_then(|x| x.as_f64()).unwrap_or(0.0),
        }).collect())
        .unwrap_or_default();

    let gpus = v.get("gpus").and_then(|g| g.as_array())
        .map(|arr| arr.iter().map(|g| GpuRow {
            name:          val_str(g, "name"),
            util_pct:      g.get("gpu_used_percent").and_then(|x| x.as_f64()).unwrap_or(0.0),
            vram_total_mb: g.get("vram_total_mb")  .and_then(|x| x.as_f64()).unwrap_or(0.0),
            vram_used_mb:  g.get("vram_used_mb")   .and_then(|x| x.as_f64()).unwrap_or(0.0),
            vram_free_mb:  g.get("vram_free_mb")   .and_then(|x| x.as_f64()).unwrap_or(0.0),
            temp_c:        g.get("temperature_c")  .and_then(|x| x.as_u64()).map(|x| x as u32),
            encoder_pct:   g.get("encoder_percent").and_then(|x| x.as_u64()).map(|x| x as u32),
            decoder_pct:   g.get("decoder_percent").and_then(|x| x.as_u64()).map(|x| x as u32),
            power_w:       g.get("power_w")        .and_then(|x| x.as_f64()),
        }).collect())
        .unwrap_or_default();

    SysSample {
        ts:              ts_time(v),
        cpu_used_pct:    v.get("cpu_used_percent")   .and_then(|x| x.as_f64()).unwrap_or(0.0),
        cpu_free_pct:    v.get("cpu_free_percent")   .and_then(|x| x.as_f64()).unwrap_or(0.0),
        memory_total_mb: v.get("memory_total_mb")    .and_then(|x| x.as_f64()).unwrap_or(0.0),
        memory_used_mb:  v.get("memory_used_mb")     .and_then(|x| x.as_f64()).unwrap_or(0.0),
        memory_free_pct: v.get("memory_free_percent").and_then(|x| x.as_f64()).unwrap_or(0.0),
        swap_total_mb:   v.get("swap_total_mb")      .and_then(|x| x.as_f64()).unwrap_or(0.0),
        swap_used_mb:    v.get("swap_used_mb")       .and_then(|x| x.as_f64()).unwrap_or(0.0),
        swap_used_pct:   v.get("swap_used_percent")  .and_then(|x| x.as_f64()).unwrap_or(0.0),
        cores,
        network,
        disks,
        gpus,
    }
}

/// Direction for threshold comparison.
enum Dir { Above, Below }

/// Green / yellow / red based on warn and alert thresholds.
fn threshold_color(value: f64, warn: f64, alert: f64, dir: Dir) -> egui::Color32 {
    let (warn_hit, alert_hit) = match dir {
        Dir::Above => (value >= warn,  value >= alert),
        Dir::Below => (value <= warn,  value <= alert),
    };
    if alert_hit      { egui::Color32::from_rgb(210, 60,  60)  }
    else if warn_hit  { egui::Color32::from_rgb(210, 160, 40)  }
    else              { egui::Color32::from_rgb(80,  180, 80)  }
}

/// Extract `HH:MM:SS` from a `ts` field like `"2026-03-23T10:00:00.000Z"`.
fn ts_time(v: &serde_json::Value) -> String {
    let ts = v.get("ts").and_then(|x| x.as_str()).unwrap_or("");
    ts.splitn(2, 'T').nth(1).unwrap_or("")
      .splitn(2, '.').next().unwrap_or("")
      .to_string()
}

fn val_u32(v: &serde_json::Value, key: &str) -> u32 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0) as u32
}

fn val_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Human-readable hint shown next to the slider value.
fn interval_hint(secs: u32) -> &'static str {
    match secs {
        0        => "off",
        1..=4    => "very frequent",
        5..=14   => "frequent",
        15..=44  => "normal",
        45..=119 => "relaxed",
        _        => "infrequent",
    }
}
