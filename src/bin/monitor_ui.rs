//! monitor-ui — egui configuration editor and process viewer for the monitor suite.
//!
//! Usage:
//!   monitor-ui.exe <LOG_DIR>
//!
//! Reads `<LOG_DIR>/monitor.config.json`, lets you edit poll intervals for both
//! monitors, and writes the file back atomically.  The running monitors pick up
//! the change immediately via their config-watcher thread — no restart needed.
//!
//! The lower panel reads the process-monitor NDJSON log and shows a live table
//! of watched processes with their last-known CPU, memory, handles and threads.

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
            .with_inner_size([620.0, 700.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Monitor Configuration",
        options,
        Box::new(move |_cc| Ok(Box::new(MonitorApp::load(log_dir)))),
    )
}

// ── Process viewer row ────────────────────────────────────────────────────────

#[derive(Default)]
struct ProcessRow {
    pid:         u32,
    name:        String,
    cpu_percent: f64,
    memory_mb:   f64,
    handles:     u32,
    threads:     u32,
    /// HH:MM:SS of the last resource_sample that included this process
    /// (or spawn time if it has never been sampled yet).
    last_seen:   String,
    /// false once a `process_exited` event is seen for this PID.
    alive:       bool,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct MonitorApp {
    log_dir: PathBuf,
    config:  Result<Config, String>,

    /// Enable toggles — map to `monitors.*.enabled` in the config.
    proc_enabled: bool,
    sys_enabled:  bool,

    /// Interval values shown in the UI (in seconds, whole numbers).
    proc_poll_secs:     u32,
    proc_snapshot_secs: u32,
    sys_poll_secs:      u32,

    dirty:  bool,
    status: String,

    // ── Process viewer ────────────────────────────────────────────────────────
    proc_rows:           Vec<ProcessRow>,
    proc_last_refresh:   Option<Instant>,
    proc_source_file:    String,
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

impl MonitorApp {
    fn load(log_dir: PathBuf) -> Self {
        let mut app = match Config::load(&log_dir) {
            Ok(cfg) => {
                let proc_poll_secs     = (cfg.monitors.process_monitor.resource_poll_interval_ms / 1_000) as u32;
                let proc_snapshot_secs = (cfg.monitors.process_monitor.snapshot_interval_ms       / 1_000) as u32;
                let sys_poll_secs      = (cfg.monitors.system_monitor.poll_interval_ms            / 1_000) as u32;

                Self {
                    log_dir,
                    proc_enabled: cfg.monitors.process_monitor.enabled,
                    sys_enabled:  cfg.monitors.system_monitor.enabled,
                    config: Ok(cfg),
                    proc_poll_secs,
                    proc_snapshot_secs,
                    sys_poll_secs,
                    dirty:  false,
                    status: String::new(),
                    proc_rows:         Vec::new(),
                    proc_last_refresh: None,
                    proc_source_file:  String::new(),
                }
            }
            Err(e) => Self {
                log_dir,
                config: Err(e.to_string()),
                proc_enabled: true,
                sys_enabled:  true,
                proc_poll_secs:     5,
                proc_snapshot_secs: 60,
                sys_poll_secs:      30,
                dirty:  false,
                status: String::new(),
                proc_rows:         Vec::new(),
                proc_last_refresh: None,
                proc_source_file:  String::new(),
            },
        };
        app.refresh_processes();
        app
    }

    fn save(&mut self) {
        let cfg = match &mut self.config {
            Ok(c) => c,
            Err(_) => return,
        };

        // Push UI values back into the config (converting seconds → ms).
        cfg.monitors.process_monitor.enabled                    = self.proc_enabled;
        cfg.monitors.process_monitor.resource_poll_interval_ms = self.proc_poll_secs     as u64 * 1_000;
        cfg.monitors.process_monitor.snapshot_interval_ms      = self.proc_snapshot_secs as u64 * 1_000;
        cfg.monitors.system_monitor.enabled                    = self.sys_enabled;
        cfg.monitors.system_monitor.poll_interval_ms           = self.sys_poll_secs      as u64 * 1_000;

        // Serialise and write atomically (temp file → rename).
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

    /// Re-read the latest process-monitor log file and rebuild the process table.
    fn refresh_processes(&mut self) {
        let log_file_base = match &self.config {
            Ok(cfg) => cfg.monitors.process_monitor.log_file.clone(),
            Err(_)  => "proc_resources.jsonl".to_string(),
        };

        let log_path = match find_latest_log(&self.log_dir, &log_file_base) {
            Some(p) => p,
            None => {
                self.proc_source_file = "no log file found".into();
                self.proc_last_refresh = Some(Instant::now());
                return;
            }
        };

        let content = match std::fs::read_to_string(&log_path) {
            Ok(c)  => c,
            Err(e) => {
                self.proc_source_file = format!("read error: {e}");
                self.proc_last_refresh = Some(Instant::now());
                return;
            }
        };

        // pid → row; rebuilt fresh on every refresh so exited processes fall off
        // when they are no longer mentioned in any recent resource_sample.
        let mut map: HashMap<u32, ProcessRow> = HashMap::new();

        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };

            match v.get("event").and_then(|e| e.as_str()).unwrap_or("") {
                "process_spawned" => {
                    let pid       = val_u32(&v, "pid");
                    let name      = val_str(&v, "name");
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
                    // Each resource_sample completely overwrites CPU/mem for the
                    // listed PIDs and marks them alive.
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

        // Sort: alive first, then alphabetically by name.
        let mut rows: Vec<ProcessRow> = map.into_values().collect();
        rows.sort_by(|a, b| b.alive.cmp(&a.alive).then(a.name.cmp(&b.name)));

        self.proc_rows         = rows;
        self.proc_last_refresh = Some(Instant::now());
        self.proc_source_file  = log_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
    }
}

// ── egui render loop ──────────────────────────────────────────────────────────

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-refresh the process table every REFRESH_INTERVAL.
        let due = self.proc_last_refresh
            .map_or(true, |t| t.elapsed() >= REFRESH_INTERVAL);
        if due {
            self.refresh_processes();
        }
        ctx.request_repaint_after(REFRESH_INTERVAL);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Monitor Configuration");
            ui.label(
                egui::RichText::new(self.log_dir.display().to_string())
                    .small()
                    .color(egui::Color32::GRAY),
            );

            ui.add_space(12.0);

            // ── Error state ───────────────────────────────────────────────────
            if let Err(ref msg) = self.config {
                ui.colored_label(egui::Color32::RED, format!("Cannot load config: {msg}"));
                return;
            }

            // ── Process Monitor ───────────────────────────────────────────────
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
                        ui.add(
                            egui::Slider::new(&mut self.proc_poll_secs, 0..=60)
                                .suffix(" s")
                                .clamping(egui::SliderClamping::Always),
                        );
                        ui.label(interval_hint(self.proc_poll_secs));
                        if self.proc_poll_secs != before { self.dirty = true; self.status.clear(); }
                        ui.end_row();

                        ui.label("Snapshot interval");
                        let before = self.proc_snapshot_secs;
                        ui.add(
                            egui::Slider::new(&mut self.proc_snapshot_secs, 0..=600)
                                .suffix(" s")
                                .clamping(egui::SliderClamping::Always),
                        );
                        ui.label(interval_hint(self.proc_snapshot_secs));
                        if self.proc_snapshot_secs != before { self.dirty = true; self.status.clear(); }
                        ui.end_row();
                    });
                });
            });

            ui.add_space(10.0);

            // ── System Monitor ────────────────────────────────────────────────
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
                        ui.add(
                            egui::Slider::new(&mut self.sys_poll_secs, 0..=300)
                                .suffix(" s")
                                .clamping(egui::SliderClamping::Always),
                        );
                        ui.label(interval_hint(self.sys_poll_secs));
                        if self.sys_poll_secs != before { self.dirty = true; self.status.clear(); }
                        ui.end_row();
                    });
                });
            });

            ui.add_space(16.0);

            // ── Save button + status ──────────────────────────────────────────
            ui.horizontal(|ui| {
                let save_btn = ui.add_enabled(
                    self.dirty,
                    egui::Button::new("💾  Save"),
                );
                if save_btn.clicked() {
                    self.save();
                }

                if self.dirty {
                    ui.colored_label(egui::Color32::YELLOW, "  Unsaved changes");
                } else if !self.status.is_empty() {
                    ui.colored_label(egui::Color32::GREEN, format!("  ✓  {}", self.status));
                }
            });

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(8.0);

            // ── Process viewer ────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Watched Processes").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("⟳  Refresh").clicked() {
                        self.refresh_processes();
                    }
                    if !self.proc_source_file.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.proc_source_file)
                                .small()
                                .color(egui::Color32::GRAY),
                        );
                    }
                });
            });

            ui.add_space(4.0);

            if self.proc_rows.is_empty() {
                ui.label(
                    egui::RichText::new("No processes found — is process-monitor running?")
                        .color(egui::Color32::GRAY),
                );
            } else {
                // Column header
                egui::Grid::new("proc_header")
                    .num_columns(7)
                    .spacing([12.0, 2.0])
                    .striped(false)
                    .show(ui, |ui| {
                        for label in ["Name", "PID", "CPU %", "Mem MB", "Handles", "Threads", "Last seen"] {
                            ui.label(egui::RichText::new(label).strong().small());
                        }
                        ui.end_row();
                    });

                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        egui::Grid::new("proc_table")
                            .num_columns(7)
                            .spacing([12.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for row in &self.proc_rows {
                                    let color = if row.alive {
                                        egui::Color32::WHITE
                                    } else {
                                        egui::Color32::GRAY
                                    };
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
                                    ui.label(
                                        egui::RichText::new(&row.last_seen)
                                            .small()
                                            .color(egui::Color32::GRAY),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
            }
        });
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Find the highest-numbered rotation of `base_name` (e.g. `proc_resources.jsonl`)
/// in `log_dir`.  Files are named `<stem>.<n>.jsonl`.
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

/// Extract `HH:MM:SS` from a `ts` field like `"2026-03-23T10:00:00.000Z"`.
fn ts_time(v: &serde_json::Value) -> String {
    let ts = v.get("ts").and_then(|x| x.as_str()).unwrap_or("");
    // ts format: 2026-03-23T10:00:00.000Z  →  split on T, take time, drop millis
    ts.splitn(2, 'T')
        .nth(1)
        .unwrap_or("")
        .splitn(2, '.')
        .next()
        .unwrap_or("")
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
