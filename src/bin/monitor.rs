//! monitor — starts and supervises all monitor processes.
//!
//! # Usage
//!
//!   monitor <LOG_DIR>             (with console window)
//!   monitor <LOG_DIR> --no-console (run silently)
//!
//! # Behaviour
//!
//! * Reads `monitor.config.json` to decide which monitors are enabled.
//! * Spawns each enabled monitor from the same directory as this executable.
//!   Which monitors are available depends on the build features:
//!     - `process_monitor` feature (default, Windows only): process-monitor
//!     - always: system-monitor, go2rtc-monitor
//!     - `monitor_ui` feature (default, GUI targets only): monitor-ui
//! * Checks every 500 ms whether any child has exited. If so, waits
//!   `RESTART_DELAY` seconds and then restarts it — unless the monitor was
//!   disabled in the config in the meantime.
//! * Terminating the monitor (Ctrl-C / SIGTERM) kills all children first,
//!   then exits cleanly.
//! * Writes its own NDJSON log to `watchdog.N.jsonl` in `<LOG_DIR>`.
//!
//! # Build targets
//!
//!   Windows (full):          cargo build
//!   Linux / ARM (headless):  cargo build --no-default-features
//!   Linux / ARM (with UI):   cargo build --no-default-features --features monitor_ui
//!
//! # Config hot-reload
//!
//! The config is re-read on every supervision tick, so:
//!   - Enabling a monitor in config starts it on the next tick.
//!   - Disabling a running monitor kills it; it will not be restarted.

use anyhow::Result;
use clap::Parser;
use crossbeam_channel::bounded;
use std::{
    path::PathBuf,
    process::{Child, Command},
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
    writer::LogWriter,
};

const MONITOR:       &str      = "monitor";
const RESTART_DELAY: Duration  = Duration::from_secs(3);
const POLL_INTERVAL: Duration  = Duration::from_millis(500);

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "monitor", about = "Supervisor for the monitor suite — keeps all monitors alive")]
struct Args {
    /// Directory that contains monitor.config.json (log files are also written here)
    log_dir: PathBuf,

    /// Detach from the console window (run silently in the background)
    #[arg(long)]
    no_console: bool,
}

// ── Managed child ─────────────────────────────────────────────────────────────

struct ManagedChild {
    /// Human-readable name used in log messages.
    name:            &'static str,
    /// Executable file stem (without `.exe`), expected next to this binary.
    binary:          &'static str,
    /// When `false`, the child is never restarted after it exits (e.g. monitor-ui:
    /// closing the window is intentional and the watchdog should not reopen it).
    restart_on_exit: bool,
    /// When `false`, `--no-console` is NOT forwarded even if the watchdog uses it
    /// (e.g. monitor-ui is a GUI app and the flag would be meaningless / harmful).
    pass_no_console: bool,
    child:           Option<Child>,
    started_at:      Option<Instant>,
    /// Time of the most recent exit (drives restart timer).
    last_exit_at:    Option<Instant>,
    restart_count:   u32,
}

impl ManagedChild {
    /// A background monitor: restarted on exit, forwards `--no-console`.
    fn monitor(name: &'static str, binary: &'static str) -> Self {
        Self {
            name,
            binary,
            restart_on_exit: true,
            pass_no_console: true,
            child:           None,
            started_at:      None,
            last_exit_at:    None,
            restart_count:   0,
        }
    }

    /// A GUI tool: started once, never restarted, never gets `--no-console`.
    #[cfg_attr(not(feature = "monitor_ui"), allow(dead_code))]
    fn gui(name: &'static str, binary: &'static str) -> Self {
        Self {
            name,
            binary,
            restart_on_exit: false,
            pass_no_console: false,
            child:           None,
            started_at:      None,
            last_exit_at:    None,
            restart_count:   0,
        }
    }

    fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    fn is_running(&self) -> bool {
        self.child.is_some()
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args = Args::parse();

    if args.no_console {
        process_monitor::console::detach();
    }

    let log_dir = args.log_dir.canonicalize()
        .unwrap_or_else(|_| args.log_dir.clone());

    // Directory that contains this executable — siblings live here too.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    // ── Load config ───────────────────────────────────────────────────────────
    let cfg      = Config::load(&log_dir)?;
    let rotation = cfg.log_rotation.clone();
    // Config is re-read from disk on each supervision tick (no watcher thread
    // needed — avoids the hung-join problem that would block clean shutdown).
    let mut current_cfg    = cfg;
    let mut last_cfg_check = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now); // force a re-read on the very first tick

    // ── Create log writer ─────────────────────────────────────────────────────
    let monitor_pid = std::process::id();

    let mut log_writer = LogWriter::new(
        &log_dir,
        "watchdog.jsonl",
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
        "[monitor] started  pid={}  log={}",
        monitor_pid, log_writer.current_log_file_name()
    );

    // ── Event channel ─────────────────────────────────────────────────────────
    let (tx, rx) = bounded::<String>(512);

    // ── Writer thread ─────────────────────────────────────────────────────────
    let writer_thread = {
        let no_console = args.no_console;
        thread::spawn(move || {
            for line in &rx {
                if let Err(e) = log_writer.write_line(&line) {
                    cprint!(no_console, "[monitor writer] error: {e}");
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
            cprint!(no_console, "[monitor] writer thread exited cleanly");
        })
    };

    // ── Shutdown flag ─────────────────────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("failed to install Ctrl-C handler");
    }

    // ── Managed children (order determines startup order) ─────────────────────
    // Each child is only included when its feature is active at compile time.
    let mut children = vec![
        #[cfg(feature = "process_monitor")]
        ManagedChild::monitor("process-monitor", "process-monitor"),
        ManagedChild::monitor("system-monitor",  "system-monitor"),
        ManagedChild::monitor("go2rtc-monitor",  "go2rtc-monitor"),
        ManagedChild::monitor("filebeat",         "filebeat"),
        #[cfg(feature = "monitor_ui")]
        ManagedChild::gui    ("monitor-ui",       "monitor-ui"),
    ];

    // ── Main supervision loop ─────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        // Re-read config from disk every 5 s — picks up any changes the user
        // makes while the watchdog is running without needing a watcher thread.
        if last_cfg_check.elapsed() >= Duration::from_secs(5) {
            if let Ok(new_cfg) = Config::load(&log_dir) {
                current_cfg = new_cfg;
            }
            last_cfg_check = Instant::now();
        }

        // Build an enabled flag per child, in the same order as `children`.
        // cfg attributes must mirror those used in the children vec above.
        let monitors_cfg = &current_cfg.monitors;
        let enabled = [
            #[cfg(feature = "process_monitor")]
            monitors_cfg.process_monitor.enabled,
            monitors_cfg.system_monitor.enabled,
            monitors_cfg.go2rtc_monitor.enabled,
            monitors_cfg.filebeat.enabled,
            #[cfg(feature = "monitor_ui")]
            true, // monitor-ui: always start when built
        ];

        for (mc, &en) in children.iter_mut().zip(enabled.iter()) {

            // ── Disabled: kill if currently running ───────────────────────────
            if !en {
                if mc.is_running() {
                    cprint!(args.no_console,
                        "[monitor] {} disabled in config — stopping", mc.name);
                    kill_child(mc, args.no_console);
                }
                continue;
            }

            // ── Check whether the child has exited ────────────────────────────
            let exit_status = if let Some(child) = &mut mc.child {
                match child.try_wait() {
                    Ok(Some(status)) => Some(status),
                    Ok(None)         => None,   // still running
                    Err(e) => {
                        cprint!(args.no_console,
                            "[monitor] try_wait error for {}: {e}", mc.name);
                        None
                    }
                }
            } else {
                None
            };

            if let Some(status) = exit_status {
                let uptime    = mc.started_at.map_or(0, |t| t.elapsed().as_secs());
                let exit_code = status.code();
                mc.child      = None;

                if mc.restart_on_exit {
                    mc.last_exit_at = Some(Instant::now());
                    cprint!(args.no_console,
                        "[monitor] {} exited  code={:?}  uptime={}s  — will restart in {}s",
                        mc.name, exit_code, uptime, RESTART_DELAY.as_secs());
                    send(&tx, &LogEntry::warn(MONITOR, "child_exited", ChildExitedData {
                        name:           mc.name.to_string(),
                        exit_code,
                        uptime_seconds: uptime,
                    }));
                } else {
                    cprint!(args.no_console,
                        "[monitor] {} closed  code={:?}  uptime={}s  (not restarting)",
                        mc.name, exit_code, uptime);
                    send(&tx, &LogEntry::info(MONITOR, "child_exited", ChildExitedData {
                        name:           mc.name.to_string(),
                        exit_code,
                        uptime_seconds: uptime,
                    }));
                }
            }

            // ── Start or restart if not running and delay has passed ───────────
            if !mc.is_running() {
                // Never restart a GUI tool once the user has closed it.
                if !mc.restart_on_exit && mc.restart_count > 0 {
                    continue;
                }

                let delay_passed = mc.last_exit_at
                    .map_or(true, |t| t.elapsed() >= RESTART_DELAY);

                if delay_passed {
                    let effective_no_console = args.no_console && mc.pass_no_console;
                    match spawn_child(&exe_dir, mc.binary, &log_dir, effective_no_console) {
                        Ok(child) => {
                            let pid = child.id();
                            mc.child      = Some(child);
                            mc.started_at = Some(Instant::now());

                            if mc.restart_count == 0 {
                                cprint!(args.no_console,
                                    "[monitor] started {} pid={}", mc.name, pid);
                                send(&tx, &LogEntry::info(MONITOR, "child_started", ChildStartedData {
                                    name:          mc.name.to_string(),
                                    pid,
                                    restart_count: 0,
                                }));
                            } else {
                                mc.restart_count += 1;
                                cprint!(args.no_console,
                                    "[monitor] restarted {} pid={}  attempt #{}",
                                    mc.name, pid, mc.restart_count);
                                send(&tx, &LogEntry::info(MONITOR, "child_restarted", ChildStartedData {
                                    name:          mc.name.to_string(),
                                    pid,
                                    restart_count: mc.restart_count,
                                }));
                            }
                        }
                        Err(e) => {
                            cprint!(args.no_console,
                                "[monitor] failed to start {}: {e}", mc.name);
                            send(&tx, &LogEntry::error(MONITOR, "child_start_failed", WarningData {
                                msg:    format!("failed to start {}", mc.name),
                                detail: Some(e.to_string()),
                            }));
                            // Back off: treat as if we just exited to wait another RESTART_DELAY.
                            mc.last_exit_at = Some(Instant::now());
                        }
                    }
                }
            }
        }

        // Sleep the remainder of the poll interval.
        let elapsed = tick.elapsed();
        if elapsed < POLL_INTERVAL {
            thread::sleep(POLL_INTERVAL - elapsed);
        }
    }

    // ── Shutdown: kill all children, then exit ────────────────────────────────
    cprint!(args.no_console, "[monitor] shutting down — stopping children…");

    for mc in &mut children {
        if mc.is_running() {
            cprint!(args.no_console, "[monitor] stopping {} pid={}", mc.name,
                mc.pid().unwrap_or(0));
            kill_child(mc, args.no_console);
        }
    }

    drop(tx);
    let _ = writer_thread.join();

    cprint!(args.no_console, "[monitor] done");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Spawn a child monitor process. Returns the `Child` handle on success.
fn spawn_child(
    exe_dir:    &PathBuf,
    binary:     &str,
    log_dir:    &PathBuf,
    no_console: bool,
) -> Result<Child> {
    let exe_name = if cfg!(windows) {
        format!("{binary}.exe")
    } else {
        binary.to_string()
    };

    let exe_path = exe_dir.join(&exe_name);

    let mut cmd = Command::new(&exe_path);
    cmd.arg(log_dir);
    if no_console {
        cmd.arg("--no-console");
    }

    Ok(cmd.spawn()?)
}

/// Kill a managed child and clear its state.
fn kill_child(mc: &mut ManagedChild, no_console: bool) {
    if let Some(mut child) = mc.child.take() {
        let _ = child.kill();
        let _ = child.wait(); // reap to avoid zombies
    }
    mc.started_at   = None;
    mc.last_exit_at = None;
    cprint!(no_console, "[monitor] {} stopped", mc.name);
}
