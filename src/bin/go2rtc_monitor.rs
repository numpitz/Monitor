//! go2rtc-monitor — stream state logger for go2rtc instances.
//!
//! Polls `GET <api_url>/api/streams` on a configurable interval and logs:
//! - `stream_up`       when a stream's producer becomes active
//! - `stream_down`     when a stream's producer goes offline / disappears
//! - `consumer_change` when the viewer count for a stream changes
//! - `stream_sample`   a full snapshot of all streams on every poll
//!
//! All other monitor behaviours (hot-reload, rotation, Ctrl-C) are identical
//! to process-monitor and system-monitor.

use anyhow::Result;
use clap::Parser;
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use process_monitor::{
    cprint,
    config::{Config, Go2rtcMonitorLogConfig},
    events::*,
    send,
    watch_config,
    writer::LogWriter,
};

const MONITOR: &str = "go2rtc_monitor";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "go2rtc-monitor", about = "go2rtc stream state monitor")]
struct Args {
    /// Directory that contains monitor.config.json (log files are also written here)
    log_dir: PathBuf,

    /// Detach from the console window (run silently in the background)
    #[arg(long)]
    no_console: bool,
}

// ── Stream state ──────────────────────────────────────────────────────────────

struct StreamState {
    producer_active: bool,
    producer_url:    Option<String>,
    consumer_count:  usize,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args = Args::parse();

    if args.no_console {
        process_monitor::console::detach();
    }

    let log_dir = args.log_dir.canonicalize()
        .unwrap_or_else(|_| args.log_dir.clone());

    let cfg = Config::load(&log_dir)?;
    let gm  = cfg.monitors.go2rtc_monitor.clone();

    if !gm.enabled {
        cprint!(args.no_console, "[go2rtc-monitor] disabled in config — exiting");
        return Ok(());
    }

    let rotation = cfg.log_rotation.clone();
    let config   = Arc::new(RwLock::new(cfg));
    let monitor_pid = std::process::id();

    let mut log_writer = LogWriter::new(
        &log_dir,
        &gm.log_file,
        rotation.max_file_size_mb,
        rotation.keep_files,
        monitor_pid,
        MONITOR,
    )?;

    let start = LogEntry::info(MONITOR, "monitor_start", MonitorStartData {
        pid:            monitor_pid,
        log_file:       log_writer.current_log_file_name(),
        rotation:       false,
        continued_from: None,
    });
    log_writer.write_entry(&start)?;

    cprint!(args.no_console,
        "[go2rtc-monitor] started  pid={}  log={}",
        monitor_pid, log_writer.current_log_file_name()
    );

    // ── Event channel ─────────────────────────────────────────────────────────
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
            let stop = LogEntry::info(MONITOR, "monitor_stop", MonitorStopData {
                pid:       monitor_pid,
                reason:    "shutdown",
                exit_code: 0,
            });
            if let Ok(line) = serde_json::to_string(&stop) {
                let _ = log_writer.write_line(&line);
            }
            cprint!(no_console, "[go2rtc-monitor] writer thread exited cleanly");
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

    // ── Shutdown flag ─────────────────────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("failed to install Ctrl-C handler");
    }

    // HTTP agent — 5 s timeout so a slow/unreachable go2rtc never blocks the loop.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();

    let mut known: HashMap<String, StreamState> = HashMap::new();

    cprint!(args.no_console,
        "[go2rtc-monitor] polling {} every {}ms",
        gm.api_url, gm.poll_interval_ms
    );

    // ── Main poll loop ────────────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let tick = std::time::Instant::now();

        let gm_cfg        = config.read().monitors.go2rtc_monitor.clone();
        let log_cfg       = gm_cfg.log.clone();
        let poll_interval = Duration::from_millis(gm_cfg.poll_interval_ms);

        if gm_cfg.poll_interval_ms > 0 {
            let url = format!("{}/api/streams", gm_cfg.api_url.trim_end_matches('/'));

            match agent.get(&url).call() {
                Err(e) => {
                    send(&tx, &LogEntry::warn(MONITOR, "api_error", WarningData {
                        msg:    format!("Cannot reach go2rtc API: {url}"),
                        detail: Some(e.to_string()),
                    }));
                }
                Ok(response) => {
                    match response.into_string() {
                        Err(e) => {
                            send(&tx, &LogEntry::warn(MONITOR, "api_error", WarningData {
                                msg:    "Failed to read go2rtc API response".into(),
                                detail: Some(e.to_string()),
                            }));
                        }
                        Ok(body) => {
                            match serde_json::from_str::<serde_json::Value>(&body) {
                                Err(e) => {
                                    send(&tx, &LogEntry::warn(MONITOR, "api_error", WarningData {
                                        msg:    "Failed to parse go2rtc API response".into(),
                                        detail: Some(e.to_string()),
                                    }));
                                }
                                Ok(json) => {
                                    poll_streams(&json, &mut known, &tx, &log_cfg);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Sleep — fall back to 1 s when poll interval is off (0).
        let sleep_for = if poll_interval.is_zero() {
            Duration::from_secs(1)
        } else {
            poll_interval
        };
        let elapsed = tick.elapsed();
        if elapsed < sleep_for {
            thread::sleep(sleep_for - elapsed);
        }
    }

    cprint!(args.no_console, "[go2rtc-monitor] shutting down…");
    drop(tx);
    let _ = writer_thread.join();
    Ok(())
}

// ── Stream polling ────────────────────────────────────────────────────────────

fn poll_streams(
    json:    &serde_json::Value,
    known:   &mut HashMap<String, StreamState>,
    tx:      &crossbeam_channel::Sender<String>,
    log_cfg: &Go2rtcMonitorLogConfig,
) {
    let Some(obj) = json.as_object() else { return };

    let mut sample: Vec<StreamInfo> = Vec::with_capacity(obj.len());
    let mut seen:   HashSet<String> = HashSet::new();

    for (name, data) in obj {
        seen.insert(name.clone());

        let producers = data.get("producers")
            .and_then(|p| p.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);

        let consumer_count = data.get("consumers")
            .and_then(|c| c.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        // A producer is active when its `state` is "active" or when the field is
        // absent (some go2rtc versions omit it while actively streaming).
        let producer_active = !producers.is_empty() && producers.iter().any(|p| {
            matches!(
                p.get("state").and_then(|s| s.as_str()),
                Some("active") | None
            )
        });

        let producer_url = producers.first()
            .and_then(|p| p.get("url"))
            .and_then(|u| u.as_str())
            .map(String::from);

        // Detect changes versus previously-known state.
        match known.get(name) {
            None => {
                if producer_active && log_cfg.stream_changes {
                    send(tx, &LogEntry::info(MONITOR, "stream_up", StreamStateChangeData {
                        name:         name.clone(),
                        producer_url: producer_url.clone(),
                    }));
                }
            }
            Some(prev) => {
                if !prev.producer_active && producer_active && log_cfg.stream_changes {
                    send(tx, &LogEntry::info(MONITOR, "stream_up", StreamStateChangeData {
                        name:         name.clone(),
                        producer_url: producer_url.clone(),
                    }));
                } else if prev.producer_active && !producer_active && log_cfg.stream_changes {
                    send(tx, &LogEntry::warn(MONITOR, "stream_down", StreamStateChangeData {
                        name:         name.clone(),
                        producer_url: prev.producer_url.clone(), // last-known active URL
                    }));
                }
                if prev.consumer_count != consumer_count && log_cfg.consumer_changes {
                    send(tx, &LogEntry::info(MONITOR, "consumer_change", ConsumerChangeData {
                        name:           name.clone(),
                        consumer_count,
                        previous_count: prev.consumer_count,
                    }));
                }
            }
        }

        known.insert(name.clone(), StreamState {
            producer_active,
            producer_url: producer_url.clone(),
            consumer_count,
        });

        sample.push(StreamInfo {
            name:            name.clone(),
            producer_active,
            producer_url,
            consumer_count,
        });
    }

    // Streams that disappeared from the API response are considered down.
    let gone: Vec<String> = known.keys()
        .filter(|n| !seen.contains(*n))
        .cloned()
        .collect();
    for name in gone {
        if log_cfg.stream_changes {
            send(tx, &LogEntry::warn(MONITOR, "stream_down", StreamStateChangeData {
                name:         name.clone(),
                producer_url: None,
            }));
        }
        known.remove(&name);
    }

    if log_cfg.stream_sample {
        let active_count = sample.iter().filter(|s| s.producer_active).count();
        let total_count  = sample.len();
        sample.sort_by(|a, b| a.name.cmp(&b.name));
        send(tx, &LogEntry::info(MONITOR, "stream_sample", StreamSampleData {
            streams: sample,
            total_count,
            active_count,
        }));
    }
}
