//! filebeat — tails external log files and forwards new lines into the monitor's log folder.
//!
//! # Usage
//!
//!   filebeat <LOG_DIR>             (with console window)
//!   filebeat <LOG_DIR> --no-console (run silently)
//!
//! # Behaviour
//!
//! * Reads `monitor.config.json` for a `monitors.filebeat` section.
//! * On each `poll_interval_ms`, scans every configured source for files
//!   matching its glob pattern.
//! * For each matching file, reads only bytes added since the last poll
//!   (byte offsets are persisted in `filebeat_state.json` in the log dir).
//! * Each forwarded line is wrapped in the standard NDJSON envelope and
//!   written to a per-source log file named `<source_name>.N.jsonl`.
//! * Handles log rotation: if a file shrinks below the last known offset,
//!   the offset is reset to 0 so the new file content is read from the start.
//! * Partial lines (no trailing newline) are left for the next poll.
//!
//! # Config example
//!
//! ```json
//! {
//!   "monitors": {
//!     "filebeat": {
//!       "enabled": true,
//!       "poll_interval_ms": 5000,
//!       "min_tick_ms": 500,
//!       "sources": [
//!         { "name": "myapp", "folder": "C:/apps/myapp/logs", "pattern": "*.log" }
//!       ]
//!     }
//!   }
//! }
//! ```

use anyhow::Result;
use clap::Parser;
use crossbeam_channel::{bounded, Sender};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use process_monitor::{
    cprint,
    config::{Config, FilebeatSourceConfig, LogRotationConfig},
    events::*,
    send,
    watch_config,
    writer::LogWriter,
};

// ── Path helpers ─────────────────────────────────────────────────────────────

/// Expand `%VARNAME%` tokens in `s` using the current process environment.
/// Unresolved tokens are left unchanged (e.g. `%MISSING%` stays as-is).
/// Variable name lookup is case-insensitive (matches Windows behaviour).
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest   = s;
    while let Some(start) = rest.find('%') {
        result.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('%') {
            let var_name = &rest[..end];
            rest = &rest[end + 1..];
            if var_name.is_empty() {
                // %% → literal %
                result.push('%');
            } else {
                let val = std::env::var(var_name)
                    .or_else(|_| std::env::var(var_name.to_uppercase()))
                    .unwrap_or_else(|_| format!("%{var_name}%"));
                result.push_str(&val);
            }
        } else {
            // No closing % — emit the leading % and stop expanding.
            result.push('%');
            result.push_str(rest);
            rest = "";
        }
    }
    result.push_str(rest);
    result
}

// ── Hashing ───────────────────────────────────────────────────────────────────

fn hash_str(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Scan `path` line by line and return the byte offset immediately after the
/// first complete line whose content hashes to `target_hash`, or `None`.
fn find_line_by_hash(path: &Path, target_hash: u64) -> Option<u64> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if buf.ends_with('\n') {
                    let line = buf.trim_end_matches('\n').trim_end_matches('\r');
                    if hash_str(line) == target_hash {
                        return reader.seek(SeekFrom::Current(0)).ok();
                    }
                } else {
                    break; // partial line at EOF — skip
                }
            }
            Err(_) => break,
        }
    }
    None
}

const MONITOR: &str = "filebeat";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "filebeat", about = "Tails external log files into the monitor log folder")]
struct Args {
    /// Directory that contains monitor.config.json (log files are also written here)
    log_dir: PathBuf,

    /// Detach from the console window (run silently in the background)
    #[arg(long)]
    no_console: bool,
}

// ── State types ───────────────────────────────────────────────────────────────

/// Persisted read state — survives restarts so already-forwarded lines are not duplicated.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ForwarderState {
    /// Keyed by the absolute path of the source file (as a UTF-8 string).
    files: HashMap<String, FileOffset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileOffset {
    /// Next byte to read from.
    offset: u64,
    /// Size of the file when offset was last updated — used to detect rotation (shrink).
    size_at_last_read: u64,
    /// 0-based count of lines forwarded from this file path so far.
    #[serde(default)]
    line_index: u64,
    /// Hash of the last forwarded line — used to resume after a reappearance.
    #[serde(default)]
    last_line_hash: Option<u64>,
    /// True when the file was absent on the last poll — prevents repeated warnings.
    #[serde(default)]
    missing: bool,
    /// Source name this file belongs to — used to detect which tracked files disappeared.
    #[serde(default)]
    source_name: String,
}

// ── State helpers ─────────────────────────────────────────────────────────────

fn load_state(log_dir: &Path) -> ForwarderState {
    let path = log_dir.join("filebeat_state.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(log_dir: &Path, state: &ForwarderState) {
    let path = log_dir.join("filebeat_state.json");
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&path, json);
    }
}

// ── Reappearance handling ─────────────────────────────────────────────────────

/// Called when a previously-missing file shows up in the glob results again.
/// Decides at which byte offset to resume reading and emits the appropriate event.
fn handle_reappearance(
    path:        &Path,
    fo:          &mut FileOffset,
    file_name:   &str,
    source_name: &str,
    tx:          &Sender<String>,
    no_console:  bool,
) {
    fo.missing = false;

    let current_size = match std::fs::metadata(path) {
        Ok(m)  => m.len(),
        Err(_) => return,   // can't stat; read_new_lines will handle the error
    };

    // ── Case 1: file is empty ─────────────────────────────────────────────────
    if current_size == 0 {
        cprint!(no_console,
            "[filebeat] file reappeared empty: {} line_index={} (source: {})",
            file_name, fo.line_index, source_name);
        send(tx, &LogEntry::warn(MONITOR, "file_appeared_empty", FilebeatFileAppearedData {
            source_name:       source_name.to_string(),
            source_file:       file_name.to_string(),
            last_line_index:   fo.line_index,
            resumed_at_offset: 0,
        }));
        fo.offset            = 0;
        fo.size_at_last_read = 0;
        fo.line_index        = 0;
        fo.last_line_hash    = None;
        return;
    }

    // ── Case 2: file is at exactly the same position ──────────────────────────
    if current_size == fo.offset {
        cprint!(no_console,
            "[filebeat] file reappeared at same state: {} line_index={} (source: {})",
            file_name, fo.line_index, source_name);
        send(tx, &LogEntry::info(MONITOR, "file_appeared_same_state", FilebeatFileAppearedData {
            source_name:       source_name.to_string(),
            source_file:       file_name.to_string(),
            last_line_index:   fo.line_index,
            resumed_at_offset: fo.offset,
        }));
        return; // offset unchanged — reading resumes normally
    }

    // ── Case 3: file has content but differs — try to find last forwarded line ─
    if let Some(hash) = fo.last_line_hash {
        if let Some(resume_at) = find_line_by_hash(path, hash) {
            cprint!(no_console,
                "[filebeat] file reappeared, resuming after last known line: \
                 {} offset={} line_index={} (source: {})",
                file_name, resume_at, fo.line_index, source_name);
            send(tx, &LogEntry::info(MONITOR, "file_appeared_resumed", FilebeatFileAppearedData {
                source_name:       source_name.to_string(),
                source_file:       file_name.to_string(),
                last_line_index:   fo.line_index,
                resumed_at_offset: resume_at,
            }));
            fo.offset            = resume_at;
            fo.size_at_last_read = resume_at;
            return;
        }
    }

    // ── Case 4: cannot determine position — start from the beginning ──────────
    cprint!(no_console,
        "[filebeat] file reappeared, no match for last line — reading from start: \
         {} (source: {})",
        file_name, source_name);
    send(tx, &LogEntry::warn(MONITOR, "file_appeared_no_match", FilebeatFileAppearedData {
        source_name:       source_name.to_string(),
        source_file:       file_name.to_string(),
        last_line_index:   fo.line_index,
        resumed_at_offset: 0,
    }));
    fo.offset            = 0;
    fo.size_at_last_read = 0;
    fo.line_index        = 0;
    fo.last_line_hash    = None;
}

// ── Core polling ──────────────────────────────────────────────────────────────

/// Return value from `read_new_lines` describing what happened.
struct ReadResult {
    lines_forwarded:            u64,
    rotated:                    bool,
    /// Byte offset at the moment rotation was detected (before reset). 0 if not rotated.
    previous_offset:            u64,
    /// Line index at the moment rotation was detected (before reset). 0 if not rotated.
    line_index_before_rotation: u64,
}

fn poll_sources(
    sources:    &[FilebeatSourceConfig],
    state:      &mut ForwarderState,
    writers:    &mut HashMap<String, LogWriter>,
    log_dir:    &Path,
    rotation:   &LogRotationConfig,
    tx:         &Sender<String>,
    no_console: bool,
) {
    let monitor_pid = std::process::id();

    for source in sources {
        // Build the full glob pattern: folder/pattern
        let folder = expand_env_vars(source.folder.trim_end_matches(['/', '\\']).trim())
            .trim_end_matches(['/', '\\'])
            .to_string();
        let glob_pattern = format!("{}/{}", folder, source.pattern);

        // Eagerly collect matching paths so we can cross-check for disappearances afterwards.
        let matched_paths: Vec<PathBuf> = match glob::glob(&glob_pattern) {
            Ok(p)  => p.flatten().collect(),
            Err(e) => {
                let msg = format!("invalid glob '{}': {e}", glob_pattern);
                cprint!(no_console, "[filebeat] {msg}");
                send(tx, &LogEntry::error(MONITOR, "source_error", WarningData {
                    msg,
                    detail: Some(source.name.clone()),
                }));
                continue;
            }
        };

        // Ensure a LogWriter exists for this source name.
        if !writers.contains_key(&source.name) {
            let log_file = format!("{}.jsonl", source.name);
            match LogWriter::new(
                log_dir,
                &log_file,
                rotation.max_file_size_mb,
                rotation.keep_files,
                monitor_pid,
                MONITOR,
            ) {
                Ok(w) => { writers.insert(source.name.clone(), w); }
                Err(e) => {
                    let msg = format!("cannot create writer for '{}': {e}", source.name);
                    cprint!(no_console, "[filebeat] {msg}");
                    send(tx, &LogEntry::error(MONITOR, "source_error", WarningData {
                        msg,
                        detail: Some(source.name.clone()),
                    }));
                    continue;
                }
            }
        }
        let writer = writers.get_mut(&source.name).unwrap();

        // Collect canonical path keys seen this poll for disappearance detection below.
        let mut current_keys: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for entry in matched_paths {
            // Canonicalise to a stable key regardless of how the path was constructed.
            let path = entry.canonicalize().unwrap_or(entry);
            let path_key = path.to_string_lossy().into_owned();

            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            current_keys.insert(path_key.clone());

            let file_offset = state.files
                .entry(path_key)
                .or_insert(FileOffset {
                    offset:            0,
                    size_at_last_read: 0,
                    line_index:        0,
                    last_line_hash:    None,
                    missing:           false,
                    source_name:       source.name.clone(),
                });

            // Keep source_name up-to-date in case of renames.
            file_offset.source_name = source.name.clone();

            // File reappeared after being absent — decide where to resume.
            if file_offset.missing {
                handle_reappearance(&path, file_offset, &file_name, &source.name, tx, no_console);
            }

            let result = read_new_lines(&path, file_offset, &source.name, writer, no_console);

            if result.rotated {
                cprint!(no_console,
                    "[filebeat] rotation detected: {} at line {} (source: {})",
                    file_name, result.line_index_before_rotation, source.name);
                send(tx, &LogEntry::warn(MONITOR, "file_rotated", FilebeatRotationData {
                    source_name:     source.name.clone(),
                    source_file:     file_name.clone(),
                    previous_offset: result.previous_offset,
                    last_line_index: result.line_index_before_rotation,
                }));
            }
            if result.lines_forwarded > 0 {
                cprint!(no_console,
                    "[filebeat] forwarded {} line(s) from {} (source: {})",
                    result.lines_forwarded, file_name, source.name);
                send(tx, &LogEntry::info(MONITOR, "lines_forwarded", FilebeatForwardedData {
                    source_name:     source.name.clone(),
                    source_file:     file_name,
                    lines_forwarded: result.lines_forwarded,
                }));
            }
        }

        // Detect files that were tracked for this source but are no longer present.
        for (path_key, fo) in state.files.iter_mut() {
            if fo.source_name != source.name { continue; }
            if current_keys.contains(path_key) { continue; }
            if fo.missing { continue; } // already warned

            fo.missing = true;
            let file_name = std::path::Path::new(path_key)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_key.clone());

            cprint!(no_console, "[filebeat] file missing: {} (source: {})",
                file_name, source.name);
            send(tx, &LogEntry::warn(MONITOR, "file_missing", WarningData {
                msg:    format!("source file missing: {file_name}"),
                detail: Some(source.name.clone()),
            }));
        }
    }
}

fn read_new_lines(
    path:        &Path,
    fo:          &mut FileOffset,
    source_name: &str,
    writer:      &mut LogWriter,
    no_console:  bool,
) -> ReadResult {
    let mut result = ReadResult {
        lines_forwarded:            0,
        rotated:                    false,
        previous_offset:            0,
        line_index_before_rotation: 0,
    };

    let mut file = match File::open(path) {
        Ok(f)  => f,
        Err(_) => return result,   // file disappeared between glob scan and open
    };

    let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Rotation detection: file shrank → it was truncated or replaced.
    if current_size < fo.offset {
        result.rotated                   = true;
        result.previous_offset           = fo.offset;
        result.line_index_before_rotation = fo.line_index;
        fo.offset            = 0;
        fo.size_at_last_read = 0;
        fo.line_index        = 0;
        fo.last_line_hash    = None;
    }

    // Nothing new to read.
    if current_size == fo.offset {
        return result;
    }

    if fo.offset > 0 {
        if file.seek(SeekFrom::Start(fo.offset)).is_err() {
            return result;
        }
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut reader = BufReader::new(file);
    let mut buf    = String::new();

    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,   // EOF
            Ok(_) => {
                if !buf.ends_with('\n') {
                    // Partial line — the writer hasn't flushed it yet.  Stop here
                    // and pick it up on the next poll when it's complete.
                    break;
                }
                let line = buf.trim_end_matches('\n').trim_end_matches('\r').to_string();
                fo.last_line_hash = Some(hash_str(&line));
                let entry = LogEntry::info(MONITOR, "log_line", LogLineData {
                    source_name: source_name.to_string(),
                    source_file: file_name.clone(),
                    line,
                });
                if let Err(e) = writer.write_entry(&entry) {
                    cprint!(no_console, "[filebeat] write error for '{}': {e}", source_name);
                    break;
                }
                fo.line_index          += 1;
                result.lines_forwarded += 1;
            }
            Err(_) => break,
        }
    }

    // Update the persisted offset to the BufReader's logical position.
    if let Ok(pos) = reader.seek(SeekFrom::Current(0)) {
        fo.offset            = pos;
        fo.size_at_last_read = pos;
    }

    result
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
    let fb  = cfg.monitors.filebeat.clone();

    if !fb.enabled {
        cprint!(args.no_console, "[filebeat] disabled in config — exiting");
        return Ok(());
    }

    let rotation    = cfg.log_rotation.clone();
    let config      = Arc::new(RwLock::new(cfg));
    let monitor_pid = std::process::id();

    // ── Diagnostic log writer (monitor_start / monitor_stop / errors) ─────────
    let (tx, rx) = bounded::<String>(256);

    let mut diag_writer = LogWriter::new(
        &log_dir,
        "filebeat.jsonl",
        rotation.max_file_size_mb,
        rotation.keep_files,
        monitor_pid,
        MONITOR,
    )?;

    let start = LogEntry::info(MONITOR, "monitor_start", MonitorStartData {
        pid:            monitor_pid,
        log_file:       diag_writer.current_log_file_name(),
        rotation:       false,
        continued_from: None,
    });
    diag_writer.write_entry(&start)?;

    cprint!(args.no_console,
        "[filebeat] started  pid={}  log={}",
        monitor_pid, diag_writer.current_log_file_name()
    );

    // ── Writer thread for diagnostic log ──────────────────────────────────────
    let writer_thread = {
        let no_console = args.no_console;
        thread::spawn(move || {
            for line in &rx {
                if let Err(e) = diag_writer.write_line(&line) {
                    cprint!(no_console, "[filebeat writer] error: {e}");
                }
            }
            let stop = LogEntry::info(MONITOR, "monitor_stop", MonitorStopData {
                pid:       monitor_pid,
                reason:    "shutdown",
                exit_code: 0,
            });
            if let Ok(line) = serde_json::to_string(&stop) {
                let _ = diag_writer.write_line(&line);
            }
            cprint!(no_console, "[filebeat] writer thread exited cleanly");
        })
    };

    // ── Config watcher thread ─────────────────────────────────────────────────
    {
        let config_clone = config.clone();
        let log_dir_clone = log_dir.clone();
        let tx_clone = tx.clone();
        let no_console = args.no_console;
        thread::spawn(move || {
            watch_config(MONITOR, config_clone, log_dir_clone, tx_clone, no_console);
        });
    }

    // ── Shutdown flag ─────────────────────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("failed to install Ctrl-C handler");
    }

    // ── Per-source log writers (created lazily in poll_sources) ───────────────
    let mut source_writers: HashMap<String, LogWriter> = HashMap::new();

    // ── Load persisted read-offset state ─────────────────────────────────────
    let mut state = load_state(&log_dir);

    // ── Main poll loop ────────────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        let fb_cfg  = config.read().monitors.filebeat.clone();
        let rot_cfg = config.read().log_rotation.clone();

        poll_sources(
            &fb_cfg.sources,
            &mut state,
            &mut source_writers,
            &log_dir,
            &rot_cfg,
            &tx,
            args.no_console,
        );

        save_state(&log_dir, &state);

        // Chunked sleep — wakes on min_tick_ms boundaries so Ctrl-C and config
        // changes are noticed quickly even when poll_interval_ms is long.
        let poll_dur  = Duration::from_millis(fb_cfg.poll_interval_ms.max(100));
        let min_chunk = Duration::from_millis(fb_cfg.min_tick_ms.max(50));
        loop {
            let elapsed = tick.elapsed();
            if elapsed >= poll_dur { break; }
            if !running.load(Ordering::SeqCst) { break; }

            let remaining = poll_dur - elapsed;
            thread::sleep(remaining.min(min_chunk));

            // React to a poll_interval_ms change in the config immediately.
            if config.read().monitors.filebeat.poll_interval_ms != fb_cfg.poll_interval_ms {
                break;
            }
        }
    }

    cprint!(args.no_console, "[filebeat] shutting down");
    drop(tx);
    let _ = writer_thread.join();
    Ok(())
}
