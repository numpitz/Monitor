//! Build script — copies `config\monitor.config.json` into the `logs\`
//! working directory so the binaries and their config are ready to use
//! straight after `cargo build`.
//!
//! Two destinations are kept in sync on every build:
//!
//!   target\{profile}\logs\monitor.config.json  — run from the build dir
//!   logs\monitor.config.json                   — run from the project root
//!
//! Both directories are the LOG_DIR you pass to the binaries, e.g.:
//!   target\release\monitor-watchdog.exe  target\release\logs\
//!   target\release\monitor-watchdog.exe  logs\

use std::path::PathBuf;

fn main() {
    // Re-run only when the template config changes.
    println!("cargo:rerun-if-changed=config/monitor.config.json");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let profile  = std::env::var("PROFILE").unwrap(); // "debug" or "release"

    let src = manifest.join("config").join("monitor.config.json");
    if !src.exists() {
        return;
    }

    // ── target/{profile}/logs/ ────────────────────────────────────────────────
    let build_logs = manifest.join("target").join(&profile).join("logs");
    copy_config(&src, &build_logs);

    // ── logs/ at the project root ─────────────────────────────────────────────
    let root_logs = manifest.join("logs");
    copy_config(&src, &root_logs);
}

/// Creates `dir` if necessary and copies `monitor.config.json` into it.
/// Skips the copy (but still creates the dir) when the destination already
/// exists and is newer than the source — avoids overwriting a locally
/// customised config on every build.
fn copy_config(src: &std::path::Path, dir: &std::path::Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        println!("cargo:warning=build.rs: cannot create {}: {e}", dir.display());
        return;
    }

    let dest = dir.join("monitor.config.json");

    // Only copy when the template is newer than what's already there.
    let should_copy = dest.metadata()
        .and_then(|dm| Ok((src.metadata()?.modified()?, dm.modified()?)))
        .map(|(src_time, dest_time)| src_time > dest_time)
        .unwrap_or(true); // copy if destination doesn't exist yet

    if should_copy {
        if let Err(e) = std::fs::copy(src, &dest) {
            println!("cargo:warning=build.rs: cannot copy config to {}: {e}", dest.display());
        }
    }
}
