//! monitor-ui — egui configuration editor for process-monitor and system-monitor.
//!
//! Usage:
//!   monitor-ui.exe <LOG_DIR>
//!
//! Reads `<LOG_DIR>/monitor.config.json`, lets you edit poll intervals for both
//! monitors, and writes the file back atomically.  The running monitors pick up
//! the change immediately via their config-watcher thread — no restart needed.

use eframe::egui;
use process_monitor::config::Config;
use std::path::PathBuf;

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let log_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Monitor Configuration")
            .with_inner_size([520.0, 420.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Monitor Configuration",
        options,
        Box::new(move |_cc| Ok(Box::new(MonitorApp::load(log_dir)))),
    )
}

// ── App state ─────────────────────────────────────────────────────────────────

struct MonitorApp {
    log_dir:  PathBuf,
    config:   Result<Config, String>,
    /// Enable toggles — map to `monitors.*.enabled` in the config.
    proc_enabled: bool,
    sys_enabled:  bool,
    /// Interval values shown in the UI (in seconds, whole numbers).
    proc_poll_secs:     u32,
    proc_snapshot_secs: u32,
    sys_poll_secs:      u32,
    dirty:   bool,
    status:  String,
}

impl MonitorApp {
    fn load(log_dir: PathBuf) -> Self {
        match Config::load(&log_dir) {
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
            },
        }
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
}

// ── egui render loop ──────────────────────────────────────────────────────────

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        });
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
