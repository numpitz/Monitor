//! Shared library for process-monitor and system-monitor.
//!
//! Both binaries share:
//! - Configuration format and hot-reload logic
//! - NDJSON log writer with file-rotation
//! - Structured event envelope (`LogEntry`)
//! - Console-detach helper
//!
//! Each binary defines its own `const MONITOR: &str` and passes it to every
//! `LogEntry` constructor so log lines are tagged correctly.

pub mod config;
pub mod console;
pub mod events;
pub mod writer;

use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc, time::Duration};

// ── Channel helper ────────────────────────────────────────────────────────────

/// Serialise `entry` and push it onto the writer channel.
/// Drops the entry (with a stderr warning) when the channel is full rather
/// than blocking the measurement loop.
pub fn send<T: serde::Serialize>(tx: &Sender<String>, entry: &T) {
    if let Ok(line) = serde_json::to_string(entry) {
        if tx.try_send(line).is_err() {
            eprintln!("WARNING: event channel full, entry dropped");
        }
    }
}

// ── Console helper ────────────────────────────────────────────────────────────

/// `cprint!(no_console, "format {}", args)` — writes to stdout only when the
/// console window is attached.  A no-op when `no_console` is `true`.
#[macro_export]
macro_rules! cprint {
    ($no_console:expr, $($arg:tt)*) => {
        if !$no_console {
            println!($($arg)*);
        }
    };
}

// ── Config hot-reload ─────────────────────────────────────────────────────────

/// Watches `<log_dir>/monitor.config.json` for on-disk changes and swaps the
/// shared `Arc<RwLock<Config>>` atomically on every successful reload.
///
/// Runs on a dedicated thread; blocks until the watcher channel closes (which
/// happens when the watcher object is dropped after the binary exits).
pub fn watch_config(
    monitor:    &'static str,
    config:     Arc<RwLock<config::Config>>,
    log_dir:    PathBuf,
    tx:         Sender<String>,
    no_console: bool,
) {
    use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
    use std::sync::mpsc::channel;

    let config_path = log_dir.join("monitor.config.json");

    let (file_tx, file_rx) = channel::<DebounceEventResult>();
    let mut debouncer = match new_debouncer(
        Duration::from_millis(400),
        move |res| { let _ = file_tx.send(res); },
    ) {
        Ok(d)  => d,
        Err(e) => {
            cprint!(no_console, "[config-watcher] cannot create debouncer: {e}");
            return;
        }
    };

    if let Err(e) = debouncer
        .watcher()
        .watch(&config_path, RecursiveMode::NonRecursive)
    {
        cprint!(no_console, "[config-watcher] cannot watch {}: {e}", config_path.display());
        return;
    }

    cprint!(no_console, "[config-watcher] watching {}", config_path.display());

    for result in &file_rx {
        match result {
            Ok(_events) => {
                match config::Config::load(&log_dir) {
                    Ok(new_cfg) => {
                        *config.write() = new_cfg;
                        cprint!(no_console, "[config-watcher] reloaded");
                        send(&tx, &events::LogEntry::info(
                            monitor,
                            "config_reloaded",
                            events::ConfigReloadedData {
                                path: config_path.to_string_lossy().into_owned(),
                            },
                        ));
                    }
                    Err(e) => {
                        cprint!(no_console, "[config-watcher] reload failed: {e}");
                    }
                }
            }
            Err(e) => {
                cprint!(no_console, "[config-watcher] notify error: {e:?}");
            }
        }
    }
}
