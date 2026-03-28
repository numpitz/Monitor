//! process-monitor — entry point.
//!
//! # Usage
//!
//!   process-monitor.exe <LOG_DIR>             (with console window)
//!   process-monitor.exe <LOG_DIR> --no-console (detaches from console)
//!
//! `<LOG_DIR>` must contain `monitor.config.json`.  All log files are
//! written into that same directory.
//!
//! # Thread model
//!
//! ```text
//! main thread
//!   ├── config-watcher thread   (notify debouncer, reloads Arc<RwLock<Config>>)
//!   ├── writer thread           (receives serialised lines via channel, writes NDJSON)
//!   └── monitor loop            (discovery + sampling + snapshots, runs on main thread)
//! ```
//!
//! Shutdown is triggered by Ctrl-C (or the supervisor sending SIGINT).
//! The main loop exits cleanly, drops the event channel, and the writer
//! thread writes the `monitor_stop` entry before joining.

#[path = "../discovery.rs"]
mod discovery;
#[path = "../sampler.rs"]
mod sampler;
#[cfg(windows)]
#[path = "../net_sampler.rs"]
mod net_sampler;

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

use process_monitor::{
    cprint,
    config::Config,
    events::*,
    send,
    watch_config,
    writer::LogWriter,
};

use discovery::ProcessDiscovery;
use sampler::ResourceSampler;

const MONITOR: &str = "process_monitor";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "process-monitor", about = "go2rtc process & resource monitor")]
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

    // Detach console first — before any println! would create a window.
    if args.no_console {
        process_monitor::console::detach();
    }

    let log_dir = args.log_dir.canonicalize()
        .unwrap_or_else(|_| args.log_dir.clone());

    // ── Load config ───────────────────────────────────────────────────────────
    let cfg = Config::load(&log_dir)?;
    let pm  = cfg.monitors.process_monitor.clone();

    if !pm.enabled {
        cprint!(args.no_console, "[process-monitor] disabled in config — exiting");
        return Ok(());
    }

    let rotation = cfg.log_rotation.clone();
    let config   = Arc::new(RwLock::new(cfg));

    // ── Create log writer ─────────────────────────────────────────────────────
    let monitor_pid = std::process::id();

    let mut log_writer = LogWriter::new(
        &log_dir,
        &pm.log_file,
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
        "[process-monitor] started  pid={}  log={}",
        monitor_pid, log_writer.current_log_file_name()
    );

    // ── Event channel (monitor loop → writer thread) ──────────────────────────
    // Bounded: provides back-pressure if the writer is slow.
    let (tx, rx) = bounded::<String>(512);

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
            cprint!(no_console, "[process-monitor] writer thread exited cleanly");
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

    // ── Build discovery & sampler ─────────────────────────────────────────────
    let mut discovery = ProcessDiscovery::new(&pm.watch_folders)?;
    let mut sampler   = ResourceSampler::new();

    let mut last_snapshot = Instant::now()
        .checked_sub(Duration::from_millis(pm.snapshot_interval_ms)) // trigger snapshot on first poll
        .unwrap_or_else(Instant::now);

    let mut last_io_sample = Instant::now()
        .checked_sub(Duration::from_millis(pm.io_poll_interval_ms)) // trigger I/O sample on first poll
        .unwrap_or_else(Instant::now);

    let mut last_port_sample = Instant::now()
        .checked_sub(Duration::from_millis(pm.port_poll_interval_ms)) // trigger port sample on first poll
        .unwrap_or_else(Instant::now);

    cprint!(args.no_console, "[process-monitor] watching {} folder(s):", pm.watch_folders.len());
    for f in &pm.watch_folders {
        cprint!(args.no_console, "  {f}");
    }

    // ── Main monitoring loop ──────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        // Read current config once per iteration — picks up any hot-reload changes,
        // including interval changes written by a future UI application.
        let pm_cfg            = config.read().monitors.process_monitor.clone();
        let log_cfg           = pm_cfg.log.clone();
        let poll_interval     = Duration::from_millis(pm_cfg.resource_poll_interval_ms);
        let snapshot_interval = Duration::from_millis(pm_cfg.snapshot_interval_ms);
        let io_interval       = Duration::from_millis(pm_cfg.io_poll_interval_ms);
        let port_interval     = Duration::from_millis(pm_cfg.port_poll_interval_ms);

        // ── 1. Discover spawns / exits ────────────────────────────────────────
        match discovery.poll() {
            Err(e) => {
                send(&tx, &LogEntry::error(MONITOR, "warning", WarningData {
                    msg:    "process discovery failed".into(),
                    detail: Some(e.to_string()),
                }));
            }
            Ok((spawned, exited)) => {
                if log_cfg.process_spawn {
                    for p in &spawned {
                        cprint!(args.no_console,
                            "[+] {} pid={}", p.name, p.pid);
                        send(&tx, &LogEntry::info(MONITOR, "process_spawned", ProcessSpawnedData {
                            pid:      p.pid,
                            name:     p.name.clone(),
                            exe_path: p.exe_path.clone(),
                        }));
                    }
                }
                if log_cfg.process_exit {
                    for p in &exited {
                        let uptime = p.first_seen.elapsed().as_secs();
                        cprint!(args.no_console,
                            "[-] {} pid={}  uptime={}s", p.name, p.pid, uptime);
                        send(&tx, &LogEntry::warn(MONITOR, "process_exited", ProcessExitedData {
                            pid:            p.pid,
                            name:           p.name.clone(),
                            uptime_seconds: uptime,
                        }));
                        sampler.remove(p.pid);
                    }
                }
            }
        }

        // ── 2. Sample resources (skipped when resource_poll_interval_ms == 0) ───
        if pm_cfg.resource_poll_interval_ms > 0 {
            let known = discovery.known_processes();
            if !known.is_empty() {
                let mut samples      = Vec::with_capacity(known.len());
                let mut total_cpu    = 0.0_f64;
                let mut total_mem_mb = 0.0_f64;

                for info in known.values() {
                    if let Some(s) = sampler.sample(info.pid, &info.name, info.thread_count) {
                        if let Some(th) = log_cfg.cpu_alert_threshold_percent {
                            if s.cpu_percent > th {
                                send(&tx, &LogEntry::warn(MONITOR, "cpu_alert", WarningData {
                                    msg: format!(
                                        "{} cpu={:.1}% exceeds threshold {:.0}%",
                                        s.name, s.cpu_percent, th
                                    ),
                                    detail: None,
                                }));
                            }
                        }
                        if let Some(th) = log_cfg.memory_alert_mb {
                            if s.memory_mb > th {
                                send(&tx, &LogEntry::warn(MONITOR, "memory_alert", WarningData {
                                    msg: format!(
                                        "{} mem={:.1} MB exceeds threshold {:.0} MB",
                                        s.name, s.memory_mb, th
                                    ),
                                    detail: None,
                                }));
                            }
                        }
                        total_cpu    += s.cpu_percent;
                        total_mem_mb += s.memory_mb;
                        samples.push(s);
                    }
                }

                send(&tx, &LogEntry::info(MONITOR, "resource_sample", ResourceSampleData {
                    processes:         samples,
                    total_cpu_percent: (total_cpu   * 100.0).round() / 100.0,
                    total_memory_mb:   (total_mem_mb * 100.0).round() / 100.0,
                }));
            }
        }

        // ── 3. I/O sample (separate, slower interval) ────────────────────────
        if log_cfg.io_counters
            && pm_cfg.io_poll_interval_ms > 0
            && last_io_sample.elapsed() >= io_interval
        {
            let known = discovery.known_processes();
            if !known.is_empty() {
                let mut io_samples = Vec::with_capacity(known.len());
                for info in known.values() {
                    if let Some(s) = sampler.sample_io(info.pid, &info.name) {
                        io_samples.push(s);
                    }
                }
                if !io_samples.is_empty() {
                    send(&tx, &LogEntry::info(MONITOR, "io_sample", IoSampleData {
                        processes: io_samples,
                    }));
                }
            }
            last_io_sample = Instant::now();
        }

        // ── 3.5. Port sample (listening TCP/UDP ports per process) ───────────
        #[cfg(windows)]
        if log_cfg.port_counters
            && pm_cfg.port_poll_interval_ms > 0
            && last_port_sample.elapsed() >= port_interval
        {
            let known = discovery.known_processes();
            if !known.is_empty() {
                let pids: Vec<u32> = known.keys().copied().collect();
                let entries = net_sampler::sample_listening(&pids);
                let processes: Vec<ProcessPortInfo> = entries.into_iter()
                    .filter_map(|e| {
                        let info = known.get(&e.pid)?;
                        Some(ProcessPortInfo {
                            pid:        e.pid,
                            name:       info.name.clone(),
                            tcp_listen: e.tcp_listen,
                            udp_listen: e.udp_listen,
                        })
                    })
                    .collect();
                if !processes.is_empty() {
                    send(&tx, &LogEntry::info(MONITOR, "port_sample", PortSampleData { processes }));
                }
            }
            last_port_sample = Instant::now();
        }

        // ── 4. Snapshot (skipped when snapshot_interval_ms == 0) ─────────────
        if log_cfg.snapshot
            && pm_cfg.snapshot_interval_ms > 0
            && last_snapshot.elapsed() >= snapshot_interval
        {
            let entries: Vec<ProcessSnapshotEntry> = discovery
                .known_processes()
                .values()
                .map(|p| ProcessSnapshotEntry {
                    pid:        p.pid,
                    name:       p.name.clone(),
                    exe_path:   p.exe_path.clone(),
                    started_at: p.started_at,
                    threads:    p.thread_count,
                    memory_mb:  0.0,
                })
                .collect();

            let count = entries.len();
            send(&tx, &LogEntry::info(MONITOR, "process_tree_snapshot", TreeSnapshotData {
                count,
                processes: entries,
            }));
            last_snapshot = Instant::now();
        }

        // ── 5. Sleep — chunked so config changes and Ctrl-C take effect quickly ──
        //
        // Instead of one blocking sleep for the full interval, we sleep in 500 ms
        // slices.  After each slice we check:
        //   a) whether the running flag was cleared (Ctrl-C)
        //   b) whether the configured interval changed
        // If either is true we break out immediately so the next loop iteration
        // picks up the new settings without waiting the full old interval.
        let sleep_for = if poll_interval.is_zero() {
            Duration::from_secs(1)
        } else {
            poll_interval
        };
        loop {
            let elapsed = tick.elapsed();
            if elapsed >= sleep_for { break; }
            if !running.load(Ordering::SeqCst) { break; }
            let chunk = (sleep_for - elapsed).min(Duration::from_millis(pm_cfg.min_tick_ms.max(50)));
            thread::sleep(chunk);
            // Break early if the operator changed the interval in the config.
            let new_cfg = config.read().monitors.process_monitor.clone();
            if Duration::from_millis(new_cfg.resource_poll_interval_ms) != poll_interval
                || new_cfg.min_tick_ms != pm_cfg.min_tick_ms
            { break; }
        }
    }

    cprint!(args.no_console, "[process-monitor] shutting down…");

    // Drop tx → writer thread drains the channel and writes monitor_stop.
    drop(tx);
    let _ = writer_thread.join();

    Ok(())
}
