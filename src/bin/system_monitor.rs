//! system-monitor — system-wide free-resource logger.
//!
//! # Purpose
//!
//! Provides a continuous view of the *headroom* available to the host so that
//! a video server (go2rtc) can be shown to have enough CPU, RAM and disk to
//! operate reliably.  It writes NDJSON into its own log file while sharing
//! `monitor.config.json` with process-monitor.
//!
//! # Usage
//!
//!   system-monitor.exe <LOG_DIR>
//!   system-monitor.exe <LOG_DIR> --no-console
//!
//! # Thread model
//!
//! ```text
//! main thread
//!   ├── config-watcher thread   (notify debouncer, reloads Arc<RwLock<Config>>)
//!   ├── writer thread           (receives serialised lines via channel, writes NDJSON)
//!   └── monitor loop            (CPU / RAM / disk sampling, runs on main thread)
//! ```
//!
//! # CPU measurement note
//!
//! `sysinfo` computes CPU usage over the interval between two consecutive
//! `refresh_cpu_usage()` calls.  The first call on startup establishes the
//! baseline (returns 0 %); every subsequent call inside the loop uses the
//! `poll_interval_ms` sleep as the measurement window — giving accurate
//! readings without any additional sleep.

use anyhow::Result;
use clap::Parser;
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Disks, System};

use process_monitor::{
    cprint,
    config::Config,
    events::*,
    send,
    watch_config,
    writer::LogWriter,
};

const MONITOR: &str = "system_monitor";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "system-monitor", about = "go2rtc system free-resource monitor")]
struct Args {
    /// Directory that contains monitor.config.json (log files are also written here)
    log_dir: PathBuf,

    /// Detach from the console window (run silently in the background)
    #[arg(long)]
    no_console: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args = Args::parse();

    if args.no_console {
        process_monitor::console::detach();
    }

    let log_dir = args.log_dir.canonicalize()
        .unwrap_or_else(|_| args.log_dir.clone());

    // ── Load config ───────────────────────────────────────────────────────────
    let cfg = Config::load(&log_dir)?;
    let sm  = cfg.monitors.system_monitor.clone();

    if !sm.enabled {
        cprint!(args.no_console, "[system-monitor] disabled in config — exiting");
        return Ok(());
    }

    let rotation = cfg.log_rotation.clone();
    let config   = Arc::new(RwLock::new(cfg));

    // ── Create log writer ─────────────────────────────────────────────────────
    let monitor_pid = std::process::id();

    let mut log_writer = LogWriter::new(
        &log_dir,
        &sm.log_file,
        rotation.max_file_size_mb,
        rotation.keep_files,
        monitor_pid,
        MONITOR,
    )?;

    // ── Write initial monitor_start ───────────────────────────────────────────
    let start = LogEntry::info(MONITOR, "monitor_start", MonitorStartData {
        pid:            monitor_pid,
        log_file:       log_writer.current_log_file_name(),
        rotation:       false,
        continued_from: None,
    });
    log_writer.write_entry(&start)?;

    cprint!(args.no_console,
        "[system-monitor] started  pid={}  log={}",
        monitor_pid, log_writer.current_log_file_name()
    );

    // ── Event channel (monitor loop → writer thread) ──────────────────────────
    let (tx, rx) = bounded::<String>(256);

    // ── Writer thread ─────────────────────────────────────────────────────────
    let writer_thread = {
        let no_console = args.no_console;
        thread::spawn(move || {
            for line in &rx {
                if let Err(e) = log_writer.write_line(&line) {
                    cprint!(no_console, "[writer] error: {e}");
                }
            }
            // Channel closed → monitor loop is done.  Write stop marker.
            let stop = LogEntry::info(MONITOR, "monitor_stop", MonitorStopData {
                pid:       monitor_pid,
                reason:    "shutdown",
                exit_code: 0,
            });
            if let Ok(line) = serde_json::to_string(&stop) {
                let _ = log_writer.write_line(&line);
            }
            cprint!(no_console, "[system-monitor] writer thread exited cleanly");
        })
    };

    // ── Config-watcher thread ─────────────────────────────────────────────────
    let _config_watcher = {
        let config     = config.clone();
        let log_dir    = log_dir.clone();
        let tx         = tx.clone();
        let no_console = args.no_console;
        thread::spawn(move || watch_config(MONITOR, config, log_dir, tx, no_console))
    };

    // ── Shutdown flag (set by Ctrl-C handler) ─────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("failed to install Ctrl-C handler");
    }

    // ── Initialise sysinfo ────────────────────────────────────────────────────
    // First refresh_cpu_usage() call establishes the measurement baseline.
    // CPU % on the next call will cover the poll_interval sleep as its window.
    let mut sys = System::new();
    sys.refresh_cpu_usage();

    cprint!(args.no_console,
        "[system-monitor] poll interval {}ms", sm.poll_interval_ms
    );

    // ── Main monitoring loop ──────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        // Read current config once per iteration — picks up any hot-reload changes,
        // including interval changes written by a future UI application.
        let sm_cfg      = config.read().monitors.system_monitor.clone();
        let log_cfg     = sm_cfg.log.clone();
        let watch_disks = sm_cfg.watch_disks.clone();
        let poll_interval = Duration::from_millis(sm_cfg.poll_interval_ms);

        // ── CPU ───────────────────────────────────────────────────────────────
        // refresh_cpu_usage uses the elapsed time since the previous call as
        // its measurement window — here that window is the poll_interval sleep.
        sys.refresh_cpu_usage();
        let cpu_used = sys.global_cpu_usage() as f64;
        let cpu_free = (100.0 - cpu_used).max(0.0);

        // ── Memory ────────────────────────────────────────────────────────────
        sys.refresh_memory();
        let mem_total_mb = sys.total_memory()     as f64 / 1_048_576.0;
        let mem_used_mb  = sys.used_memory()      as f64 / 1_048_576.0;
        let mem_free_mb  = sys.available_memory() as f64 / 1_048_576.0;
        let mem_free_pct = if mem_total_mb > 0.0 {
            (mem_free_mb / mem_total_mb * 100.0).min(100.0)
        } else {
            0.0
        };

        // ── Disks ─────────────────────────────────────────────────────────────
        // Build a fresh disk snapshot each poll so we always see current values.
        let disks = Disks::new_with_refreshed_list();

        let watched: Vec<_> = disks.list().iter().filter(|d| {
            if watch_disks.is_empty() {
                return true; // report all disks when no filter is configured
            }
            let mp = d.mount_point().to_string_lossy().to_lowercase();
            watch_disks.iter().any(|w| mp.starts_with(&w.to_lowercase()))
        }).collect();

        let disk_samples: Vec<DiskSample> = watched.iter().map(|d| {
            let total_gb = d.total_space()     as f64 / 1_073_741_824.0;
            let free_gb  = d.available_space() as f64 / 1_073_741_824.0;
            let free_pct = if total_gb > 0.0 {
                (free_gb / total_gb * 100.0).min(100.0)
            } else {
                0.0
            };
            DiskSample {
                path:         d.mount_point().to_string_lossy().into_owned(),
                total_gb:     round2(total_gb),
                free_gb:      round2(free_gb),
                free_percent: round2(free_pct),
            }
        }).collect();

        // ── Write sample ──────────────────────────────────────────────────────
        send(&tx, &LogEntry::info(MONITOR, "system_resource_sample", SystemResourceSampleData {
            cpu_used_percent:    round2(cpu_used),
            cpu_free_percent:    round2(cpu_free),
            memory_total_mb:     round2(mem_total_mb),
            memory_used_mb:      round2(mem_used_mb),
            memory_free_mb:      round2(mem_free_mb),
            memory_free_percent: round2(mem_free_pct),
            disks:               disk_samples,
        }));

        // ── Threshold alerts ──────────────────────────────────────────────────
        if let Some(th) = log_cfg.cpu_alert_free_percent {
            if cpu_free < th {
                send(&tx, &LogEntry::warn(MONITOR, "cpu_headroom_alert", WarningData {
                    msg:    format!("CPU headroom {cpu_free:.1}% below threshold {th:.0}%"),
                    detail: None,
                }));
            }
        }

        if let Some(th) = log_cfg.memory_alert_free_mb {
            if mem_free_mb < th {
                send(&tx, &LogEntry::warn(MONITOR, "memory_headroom_alert", WarningData {
                    msg:    format!("free RAM {mem_free_mb:.0} MB below threshold {th:.0} MB"),
                    detail: None,
                }));
            }
        }

        if let Some(th) = log_cfg.disk_alert_free_gb {
            for d in &watched {
                let free_gb = d.available_space() as f64 / 1_073_741_824.0;
                if free_gb < th {
                    send(&tx, &LogEntry::warn(MONITOR, "disk_headroom_alert", WarningData {
                        msg: format!(
                            "disk {} free {free_gb:.1} GB below threshold {th:.0} GB",
                            d.mount_point().display()
                        ),
                        detail: None,
                    }));
                }
            }
        }

        // ── Sleep for the remainder of the poll interval ──────────────────────
        let elapsed = tick.elapsed();
        if elapsed < poll_interval {
            thread::sleep(poll_interval - elapsed);
        }
    }

    cprint!(args.no_console, "[system-monitor] shutting down…");

    drop(tx);
    let _ = writer_thread.join();

    Ok(())
}

/// Round a float to 2 decimal places for clean JSON output.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
