//! Append-only NDJSON log writer with file-number rotation.
//!
//! # File naming
//!
//! The base name comes from the config (e.g. `proc_resources.jsonl`).
//! Files are numbered from 0 upward; the *highest* number is always
//! the currently active file:
//!
//!   proc_resources.0.jsonl  ← oldest
//!   proc_resources.1.jsonl
//!   proc_resources.2.jsonl  ← active (currently being written)
//!
//! On rotation the writer:
//!   1. Flushes and closes the current BufWriter.
//!   2. Increments the counter and opens `base.N+1.jsonl` for append.
//!   3. Writes a `monitor_start` marker (with `continued_from`) so any
//!      reader can follow the chain backwards.
//!   4. Deletes files older than `keep_files`.
//!
//! On startup the writer:
//!   1. Scans the log dir for the highest existing file number.
//!   2. Opens that file for append (creates it if missing).
//!   3. Writes one recovery newline in case the previous run crashed
//!      mid-write and left a partial JSON line.
//!
//! There are NO renames and NO locks — each monitor owns exactly one
//! writer and writes from one thread only.

use anyhow::Result;
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub struct LogWriter {
    log_dir:          PathBuf,
    base_name:        String,   // "proc_resources" (without .jsonl)
    current_file:     BufWriter<File>,
    current_file_num: u32,
    current_size:     u64,      // bytes written to current file
    max_size_bytes:   u64,
    keep_files:       u32,
    monitor_pid:      u32,
    monitor_name:     &'static str, // written into every rotation marker
}

impl LogWriter {
    /// Create (or re-open) the writer.
    ///
    /// * `log_file_name` – the config value, e.g. `"proc_resources.jsonl"`.
    /// * `monitor_pid`   – written into every rotation marker.
    /// * `monitor_name`  – e.g. `"process_monitor"`, written into every rotation marker.
    pub fn new(
        log_dir:       &Path,
        log_file_name: &str,
        max_size_mb:   u64,
        keep_files:    u32,
        monitor_pid:   u32,
        monitor_name:  &'static str,
    ) -> Result<Self> {
        let base_name = log_file_name
            .strip_suffix(".jsonl")
            .unwrap_or(log_file_name)
            .to_string();

        // Find the highest numbered file that already exists.
        let current_file_num = Self::find_highest_num(log_dir, &base_name);
        let file_path = Self::file_path(log_dir, &base_name, current_file_num);

        // Repair: if the file exists and does not end with '\n', append one.
        // This terminates any partial line left by a crash.
        if file_path.exists() {
            Self::repair_if_needed(&file_path)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;

        let current_size = file.metadata()?.len();

        Ok(Self {
            log_dir: log_dir.to_path_buf(),
            base_name,
            current_file: BufWriter::new(file),
            current_file_num,
            current_size,
            max_size_bytes: max_size_mb * 1024 * 1024,
            keep_files,
            monitor_pid,
            monitor_name,
        })
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Serialise `entry` to a single JSON line and append it.
    /// Rotates automatically when the file exceeds `max_size_bytes`.
    pub fn write_entry<T: serde::Serialize>(&mut self, entry: &T) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        self.write_line(&line)
    }

    /// Write a pre-serialised JSON string as a single NDJSON line.
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        let byte_len = line.len() as u64 + 1; // +1 for '\n'

        self.current_file.write_all(line.as_bytes())?;
        self.current_file.write_all(b"\n")?;
        self.current_file.flush()?;

        self.current_size += byte_len;

        if self.current_size >= self.max_size_bytes {
            self.rotate()?;
        }

        Ok(())
    }

    /// The file name (not full path) of the log file currently being written.
    pub fn current_log_file_name(&self) -> String {
        Self::file_name(&self.base_name, self.current_file_num)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn file_name(base: &str, num: u32) -> String {
        format!("{}.{}.jsonl", base, num)
    }

    fn file_path(log_dir: &Path, base: &str, num: u32) -> PathBuf {
        log_dir.join(Self::file_name(base, num))
    }

    /// Return the highest `N` for which `<base>.<N>.jsonl` exists, or 0.
    fn find_highest_num(log_dir: &Path, base: &str) -> u32 {
        let prefix = format!("{}.", base);
        let suffix = ".jsonl";
        let mut max = 0u32;

        if let Ok(entries) = std::fs::read_dir(log_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();

                if let Some(inner) = name
                    .strip_prefix(&prefix)
                    .and_then(|s| s.strip_suffix(suffix))
                {
                    if let Ok(n) = inner.parse::<u32>() {
                        if n > max { max = n; }
                    }
                }
            }
        }
        max
    }

    /// Append a single '\n' if the last byte of the file is not '\n'.
    fn repair_if_needed(path: &Path) -> Result<()> {
        let mut f = File::options().read(true).write(true).open(path)?;
        let len = f.metadata()?.len();
        if len == 0 { return Ok(()); }

        f.seek(SeekFrom::End(-1))?;
        let mut last = [0u8];
        f.read_exact(&mut last)?;

        if last[0] != b'\n' {
            f.seek(SeekFrom::End(0))?;
            f.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Close the current file, open the next one, write the rotation marker,
    /// and delete files that exceed `keep_files`.
    fn rotate(&mut self) -> Result<()> {
        let prev_name = self.current_log_file_name();

        // Flush the current BufWriter (file is closed when it's dropped below)
        self.current_file.flush()?;

        // Open the next file
        self.current_file_num += 1;
        let new_path = Self::file_path(
            &self.log_dir, &self.base_name, self.current_file_num,
        );
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&new_path)?;

        // Drop the old BufWriter (and its File handle) before opening the new one
        self.current_file = BufWriter::new(file);
        self.current_size = 0;

        // Write the rotation marker into the new file.
        // Uses serde_json::json! to avoid a circular dependency on events.rs.
        let marker = serde_json::json!({
            "ts":             chrono::Utc::now()
                                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "monitor":        self.monitor_name,
            "event":          "monitor_start",
            "level":          "INFO",
            "pid":            self.monitor_pid,
            "log_file":       self.current_log_file_name(),
            "rotation":       true,
            "continued_from": prev_name,
        });
        let marker_str = marker.to_string();
        self.current_file.write_all(marker_str.as_bytes())?;
        self.current_file.write_all(b"\n")?;
        self.current_file.flush()?;
        self.current_size += (marker_str.len() + 1) as u64;

        // Delete oldest files beyond keep_files
        self.delete_old_files();

        Ok(())
    }

    fn delete_old_files(&self) {
        if self.current_file_num < self.keep_files {
            return;
        }
        // Delete every file numbered <= current - keep_files
        let oldest_to_keep = self.current_file_num - self.keep_files;
        for n in 0..oldest_to_keep {
            let path = Self::file_path(&self.log_dir, &self.base_name, n);
            let _ = std::fs::remove_file(path); // ignore errors (already deleted etc.)
        }
    }
}
