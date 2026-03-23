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
use process_monitor::config::Config;
use std::{
    collections::HashMap,
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
}

struct DiskRow {
    path:     String,
    total_gb: f64,
    free_gb:  f64,
    free_pct: f64,
}

struct GpuRow {
    name:         String,
    util_pct:     f64,
    vram_free_mb: f64,
    temp_c:       u32,
    encoder_pct:  Option<u32>,
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

// ── App state ─────────────────────────────────────────────────────────────────

struct MonitorApp {
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
            },
        };
        app.refresh_processes();
        app.refresh_system();
        app.refresh_streams();
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

    /// Rebuild the process table from the latest proc_resources log file.
    fn refresh_processes(&mut self) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.process_monitor.log_file.clone(),
            Err(_)  => "proc_resources.jsonl".to_string(),
        };
        let log_path = match find_latest_log(&self.log_dir, &base) {
            Some(p) => p,
            None => {
                self.proc_source_file  = "no log file found".into();
                self.proc_last_refresh = Some(Instant::now());
                return;
            }
        };
        let content = match std::fs::read_to_string(&log_path) {
            Ok(c)  => c,
            Err(e) => {
                self.proc_source_file  = format!("read error: {e}");
                self.proc_last_refresh = Some(Instant::now());
                return;
            }
        };

        let mut map: HashMap<u32, ProcessRow> = HashMap::new();
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
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
                        row.alive     = false;
                        row.last_seen = ts_time(&v);
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

        let mut rows: Vec<ProcessRow> = map.into_values().collect();
        rows.sort_by(|a, b| b.alive.cmp(&a.alive).then(a.name.cmp(&b.name)));
        self.proc_rows         = rows;
        self.proc_last_refresh = Some(Instant::now());
        self.proc_source_file  = log_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    }

    /// Read the latest system_resource_sample from the sys_resources log file.
    fn refresh_system(&mut self) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.system_monitor.log_file.clone(),
            Err(_)  => "sys_resources.jsonl".to_string(),
        };
        let log_path = match find_latest_log(&self.log_dir, &base) {
            Some(p) => p,
            None => {
                self.sys_source_file  = "no log file found".into();
                self.sys_last_refresh = Some(Instant::now());
                return;
            }
        };
        let content = match std::fs::read_to_string(&log_path) {
            Ok(c)  => c,
            Err(e) => {
                self.sys_source_file  = format!("read error: {e}");
                self.sys_last_refresh = Some(Instant::now());
                return;
            }
        };

        // Walk all lines; keep the last system_resource_sample.
        let mut last: Option<serde_json::Value> = None;
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("event").and_then(|e| e.as_str()) == Some("system_resource_sample") {
                    last = Some(v);
                }
            }
        }

        self.sys_sample = last.as_ref().map(parse_sys_sample);
        self.sys_last_refresh = Some(Instant::now());
        self.sys_source_file  = log_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    }

    /// Read the last `stream_sample` event from the go2rtc_streams log file.
    fn refresh_streams(&mut self) {
        let base = match &self.config {
            Ok(cfg) => cfg.monitors.go2rtc_monitor.log_file.clone(),
            Err(_)  => "go2rtc_streams.jsonl".to_string(),
        };
        let log_path = match find_latest_log(&self.log_dir, &base) {
            Some(p) => p,
            None => {
                self.go2rtc_source_file  = "no log file found".into();
                self.go2rtc_last_refresh = Some(Instant::now());
                return;
            }
        };
        let content = match std::fs::read_to_string(&log_path) {
            Ok(c)  => c,
            Err(e) => {
                self.go2rtc_source_file  = format!("read error: {e}");
                self.go2rtc_last_refresh = Some(Instant::now());
                return;
            }
        };

        // Keep only the last stream_sample event.
        let mut last: Option<serde_json::Value> = None;
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("event").and_then(|e| e.as_str()) == Some("stream_sample") {
                    last = Some(v);
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
        } else {
            Vec::new()
        };

        self.go2rtc_last_refresh = Some(Instant::now());
        self.go2rtc_source_file  = log_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    }
}

// ── egui render loop ──────────────────────────────────────────────────────────

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-refresh all viewers when the interval is active (> 0).
        if self.ui_refresh_secs > 0 {
            let interval    = Duration::from_secs(self.ui_refresh_secs as u64);
            let proc_due    = self.proc_last_refresh   .map_or(true, |t| t.elapsed() >= interval);
            let sys_due     = self.sys_last_refresh    .map_or(true, |t| t.elapsed() >= interval);
            let streams_due = self.go2rtc_last_refresh .map_or(true, |t| t.elapsed() >= interval);
            if proc_due    { self.refresh_processes(); }
            if sys_due     { self.refresh_system(); }
            if streams_due { self.refresh_streams(); }
            ctx.request_repaint_after(interval);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Monitor Configuration");
                ui.label(
                    egui::RichText::new(self.log_dir.display().to_string())
                        .small()
                        .color(egui::Color32::GRAY),
                );
                ui.add_space(12.0);

                if let Err(ref msg) = self.config {
                    ui.colored_label(egui::Color32::RED, format!("Cannot load config: {msg}"));
                    return;
                }

                // ── Process Monitor config ─────────────────────────────────────
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let before = self.proc_enabled;
                        ui.checkbox(&mut self.proc_enabled, egui::RichText::new("Process Monitor").strong());
                        if self.proc_enabled != before { self.dirty = true; self.status.clear(); }
                        if !self.proc_enabled {
                            ui.colored_label(egui::Color32::YELLOW, "  disabled — monitor will not start");
                        }
                    });
                    ui.separator();
                    ui.add_enabled_ui(self.proc_enabled, |ui| {
                        egui::Grid::new("proc_grid")
                            .num_columns(3)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                            ui.label("Resource poll interval");
                            let before = self.proc_poll_secs;
                            ui.add(egui::Slider::new(&mut self.proc_poll_secs, 0..=60)
                                .suffix(" s").clamping(egui::SliderClamping::Always));
                            ui.label(interval_hint(self.proc_poll_secs));
                            if self.proc_poll_secs != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();

                            ui.label("Snapshot interval");
                            let before = self.proc_snapshot_secs;
                            ui.add(egui::Slider::new(&mut self.proc_snapshot_secs, 0..=600)
                                .suffix(" s").clamping(egui::SliderClamping::Always));
                            ui.label(interval_hint(self.proc_snapshot_secs));
                            if self.proc_snapshot_secs != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();

                            ui.label("Response interval");
                            let before = self.proc_min_tick_ms;
                            ui.add(egui::Slider::new(&mut self.proc_min_tick_ms, 50..=5000)
                                .suffix(" ms").clamping(egui::SliderClamping::Always));
                            ui.label(egui::RichText::new("config change / Ctrl-C reaction time").color(egui::Color32::GRAY).small());
                            if self.proc_min_tick_ms != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();
                        });
                    });
                });

                ui.add_space(10.0);

                // ── System Monitor config ──────────────────────────────────────
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let before = self.sys_enabled;
                        ui.checkbox(&mut self.sys_enabled, egui::RichText::new("System Monitor").strong());
                        if self.sys_enabled != before { self.dirty = true; self.status.clear(); }
                        if !self.sys_enabled {
                            ui.colored_label(egui::Color32::YELLOW, "  disabled — monitor will not start");
                        }
                    });
                    ui.separator();
                    ui.add_enabled_ui(self.sys_enabled, |ui| {
                        egui::Grid::new("sys_grid")
                            .num_columns(3)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                            ui.label("Poll interval");
                            let before = self.sys_poll_secs;
                            ui.add(egui::Slider::new(&mut self.sys_poll_secs, 0..=300)
                                .suffix(" s").clamping(egui::SliderClamping::Always));
                            ui.label(interval_hint(self.sys_poll_secs));
                            if self.sys_poll_secs != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();

                            ui.label("Response interval");
                            let before = self.sys_min_tick_ms;
                            ui.add(egui::Slider::new(&mut self.sys_min_tick_ms, 50..=5000)
                                .suffix(" ms").clamping(egui::SliderClamping::Always));
                            ui.label(egui::RichText::new("config change / Ctrl-C reaction time").color(egui::Color32::GRAY).small());
                            if self.sys_min_tick_ms != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();
                        });
                    });
                });

                ui.add_space(10.0);

                // ── go2rtc Monitor config ──────────────────────────────────────
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let before = self.go2rtc_enabled;
                        ui.checkbox(&mut self.go2rtc_enabled, egui::RichText::new("go2rtc Monitor").strong());
                        if self.go2rtc_enabled != before { self.dirty = true; self.status.clear(); }
                        if !self.go2rtc_enabled {
                            ui.colored_label(egui::Color32::YELLOW, "  disabled — monitor will not start");
                        }
                    });
                    ui.separator();
                    ui.add_enabled_ui(self.go2rtc_enabled, |ui| {
                        egui::Grid::new("go2rtc_grid")
                            .num_columns(3)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                            ui.label("API URL");
                            let before = self.go2rtc_api_url.clone();
                            ui.add(egui::TextEdit::singleline(&mut self.go2rtc_api_url)
                                .desired_width(260.0)
                                .hint_text("http://localhost:1984"));
                            ui.label(egui::RichText::new("base URL of go2rtc").color(egui::Color32::GRAY).small());
                            if self.go2rtc_api_url != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();

                            ui.label("Poll interval");
                            let before = self.go2rtc_poll_secs;
                            ui.add(egui::Slider::new(&mut self.go2rtc_poll_secs, 0..=300)
                                .suffix(" s").clamping(egui::SliderClamping::Always));
                            ui.label(interval_hint(self.go2rtc_poll_secs));
                            if self.go2rtc_poll_secs != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();

                            ui.label("Response interval");
                            let before = self.go2rtc_min_tick_ms;
                            ui.add(egui::Slider::new(&mut self.go2rtc_min_tick_ms, 50..=5000)
                                .suffix(" ms").clamping(egui::SliderClamping::Always));
                            ui.label(egui::RichText::new("config change / Ctrl-C reaction time").color(egui::Color32::GRAY).small());
                            if self.go2rtc_min_tick_ms != before { self.dirty = true; self.status.clear(); }
                            ui.end_row();
                        });
                    });
                });

                ui.add_space(16.0);

                // ── Save button ────────────────────────────────────────────────
                ui.horizontal(|ui| {
                    let save_btn = ui.add_enabled(self.dirty, egui::Button::new("💾  Save"));
                    if save_btn.clicked() { self.save(); }
                    if self.dirty {
                        ui.colored_label(egui::Color32::YELLOW, "  Unsaved changes");
                    } else if !self.status.is_empty() {
                        ui.colored_label(egui::Color32::GREEN, format!("  ✓  {}", self.status));
                    }
                });

                // ── UI refresh interval ────────────────────────────────────────
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label("UI auto-refresh");
                    let before = self.ui_refresh_secs;
                    ui.add(egui::Slider::new(&mut self.ui_refresh_secs, 0..=60)
                        .suffix(" s")
                        .clamping(egui::SliderClamping::Always));
                    ui.label(egui::RichText::new(interval_hint(self.ui_refresh_secs))
                        .color(egui::Color32::GRAY).small());
                    if self.ui_refresh_secs != before { self.dirty = true; self.status.clear(); }
                });

                // ── Watched Processes viewer ───────────────────────────────────
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Watched Processes").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⟳  Refresh").clicked() {
                            self.refresh_processes();
                        }
                        if !self.proc_source_file.is_empty() {
                            ui.label(egui::RichText::new(&self.proc_source_file)
                                .small().color(egui::Color32::GRAY));
                        }
                    });
                });
                ui.add_space(4.0);

                if self.proc_rows.is_empty() {
                    ui.label(egui::RichText::new("No processes found — is process-monitor running?")
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
                        .max_height(180.0)
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

                // ── System Resources viewer ────────────────────────────────────
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("System Resources").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⟳  Refresh").clicked() {
                            self.refresh_system();
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
                    // Unwrap is safe: we just checked is_none above.
                    let s = self.sys_sample.as_ref().unwrap();

                    ui.label(egui::RichText::new(format!("Last sample:  {}", s.ts))
                        .small().color(egui::Color32::GRAY));
                    ui.add_space(6.0);

                    // CPU
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("CPU ").strong().monospace());
                        ui.add(egui::ProgressBar::new(s.cpu_used_pct as f32 / 100.0)
                            .desired_width(220.0)
                            .fill(threshold_color(s.cpu_used_pct, 70.0, 90.0, Dir::Above))
                            .text(format!("{:.1}% used", s.cpu_used_pct)));
                        ui.label(egui::RichText::new(format!("{:.1}% free", s.cpu_free_pct))
                            .color(threshold_color(s.cpu_free_pct, 30.0, 10.0, Dir::Below)));
                    });

                    // RAM
                    let mem_used_frac = (s.memory_used_mb / s.memory_total_mb.max(1.0)) as f32;
                    ui.horizontal(|ui| {
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
                            .num_columns(4).spacing([16.0, 3.0]).striped(true)
                            .show(ui, |ui| {
                                for n in &s.network {
                                    ui.label(&n.interface);
                                    ui.label(format!("↓ {:.2} MB/s", n.rx));
                                    ui.label(format!("↑ {:.2} MB/s", n.tx));
                                    if n.errors > 0 {
                                        ui.label(egui::RichText::new(format!("{} errors", n.errors))
                                            .color(egui::Color32::RED));
                                    } else {
                                        ui.label(egui::RichText::new("no errors")
                                            .color(egui::Color32::GRAY));
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
                            .num_columns(4).spacing([16.0, 3.0]).striped(true)
                            .show(ui, |ui| {
                                for d in &s.disks {
                                    let used_frac = 1.0 - (d.free_pct as f32 / 100.0);
                                    ui.label(&d.path);
                                    ui.add(egui::ProgressBar::new(used_frac)
                                        .desired_width(140.0)
                                        .fill(threshold_color(d.free_gb, 20.0, 10.0, Dir::Below))
                                        .text(format!("{:.1} GB free", d.free_gb)));
                                    ui.label(format!("/ {:.1} GB", d.total_gb));
                                    ui.label(egui::RichText::new(format!("{:.1}% free", d.free_pct))
                                        .color(threshold_color(d.free_gb, 20.0, 10.0, Dir::Below)));
                                    ui.end_row();
                                }
                            });
                    }

                    // GPUs
                    if !s.gpus.is_empty() {
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("GPU").strong());
                        egui::Grid::new("gpu_grid")
                            .num_columns(5).spacing([16.0, 3.0]).striped(true)
                            .show(ui, |ui| {
                                for g in &s.gpus {
                                    ui.label(&g.name);
                                    ui.label(egui::RichText::new(format!("{:.0}% util", g.util_pct))
                                        .color(threshold_color(g.util_pct, 80.0, 95.0, Dir::Above)));
                                    ui.label(egui::RichText::new(format!("{:.0} MB VRAM free", g.vram_free_mb))
                                        .color(threshold_color(g.vram_free_mb, 500.0, 200.0, Dir::Below)));
                                    ui.label(egui::RichText::new(format!("{}°C", g.temp_c))
                                        .color(threshold_color(g.temp_c as f64, 80.0, 90.0, Dir::Above)));
                                    if let Some(enc) = g.encoder_pct {
                                        ui.label(egui::RichText::new(format!("Enc {}%", enc))
                                            .color(threshold_color(enc as f64, 80.0, 95.0, Dir::Above)));
                                    } else {
                                        ui.label("");
                                    }
                                    ui.end_row();
                                }
                            });
                    }
                }

                // ── go2rtc Streams viewer ──────────────────────────────────────
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("go2rtc Streams").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⟳  Refresh").clicked() {
                            self.refresh_streams();
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
                        .num_columns(4).spacing([12.0, 2.0])
                        .show(ui, |ui| {
                            for label in ["Name", "Status", "Consumers", "Last seen"] {
                                ui.label(egui::RichText::new(label).strong().small());
                            }
                            ui.end_row();
                        });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("stream_scroll")
                        .max_height(180.0)
                        .show(ui, |ui| {
                            egui::Grid::new("stream_table")
                                .num_columns(4).spacing([12.0, 4.0]).striped(true)
                                .show(ui, |ui| {
                                    for row in &self.go2rtc_stream_rows {
                                        let name_label = ui.label(&row.name);
                                        if !row.producer_url.is_empty() {
                                            name_label.on_hover_text(&row.producer_url);
                                        }
                                        if row.producer_active {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(80, 180, 80),
                                                "Active",
                                            );
                                        } else {
                                            ui.colored_label(egui::Color32::GRAY, "Inactive");
                                        }
                                        ui.label(row.consumer_count.to_string());
                                        ui.label(egui::RichText::new(&row.ts)
                                            .small().color(egui::Color32::GRAY));
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }); // ScrollArea
        }); // CentralPanel
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
        }).collect())
        .unwrap_or_default();

    let disks = v.get("disks").and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(|d| DiskRow {
            path:     val_str(d, "path"),
            total_gb: d.get("total_gb")     .and_then(|x| x.as_f64()).unwrap_or(0.0),
            free_gb:  d.get("free_gb")      .and_then(|x| x.as_f64()).unwrap_or(0.0),
            free_pct: d.get("free_percent") .and_then(|x| x.as_f64()).unwrap_or(0.0),
        }).collect())
        .unwrap_or_default();

    let gpus = v.get("gpus").and_then(|g| g.as_array())
        .map(|arr| arr.iter().map(|g| GpuRow {
            name:         val_str(g, "name"),
            util_pct:     g.get("gpu_used_percent").and_then(|x| x.as_f64()).unwrap_or(0.0),
            vram_free_mb: g.get("vram_free_mb")    .and_then(|x| x.as_f64()).unwrap_or(0.0),
            temp_c:       g.get("temperature_c")   .and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            encoder_pct:  g.get("encoder_percent") .and_then(|x| x.as_u64()).map(|x| x as u32),
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
