//! system-monitor — system-wide free-resource logger for video streaming.
//!
//! # Purpose
//!
//! Provides continuous visibility into the resources a video server (go2rtc +
//! ffmpeg) needs in order to operate without frame drops or stream errors:
//!
//! - **CPU headroom** — system-wide and per-core (ffmpeg pins individual cores)
//! - **Available RAM** — OOM kills degrade streams without warning
//! - **Swap / pagefile** — high swap usage causes stutter in real-time encoding
//! - **Network throughput & errors** — packet loss corrupts streams
//! - **Disk free space** — recording failures and temp-file exhaustion
//! - **GPU** — NVENC/NVDEC utilisation, VRAM, temperature (NVIDIA only via NVML)
//!
//! # Alerting model
//!
//! Every metric supports two configurable thresholds:
//! - `*_warn_*`  → logged at **WARN** level (approaching a limit)
//! - `*_alert_*` → logged at **ERROR** level (limit breached, action required)
//!
//! # GPU support
//!
//! Build with `--features nvidia` to enable NVIDIA GPU monitoring via NVML
//! (part of the NVIDIA driver — no extra install required on a GPU machine):
//!
//!   cargo build --release --features nvidia
//!
//! AMD and Intel GPUs are detected and listed in `system_info` at startup but
//! do not yet provide real-time utilisation metrics.

use anyhow::Result;
use clap::Parser;
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Disks, Networks, System};

use process_monitor::{
    cprint,
    config::Config,
    events::*,
    send,
    watch_config,
    writer::LogWriter,
};

// NVML is only compiled in when the `nvidia` feature is enabled.
#[cfg(feature = "nvidia")]
use nvml_wrapper::{enum_wrappers::device::TemperatureSensor, Nvml};

// PDH GPU fallback — compiled on Windows regardless of the nvidia feature flag.
#[cfg(windows)]
#[path = "../pdh_gpu.rs"]
mod pdh_gpu;

// ── Network drop counter ──────────────────────────────────────────────────────
//
// sysinfo exposes rx/tx throughput and error counts, but not packet-drop counts.
// We read MIB_IF_ROW2 directly via GetIfTable2 (iphlpapi), which contains
// InDiscards / OutDiscards — the same source sysinfo uses for interface names
// (the `Alias` wide-string field), so the names match perfectly.

mod net_drops {
    use std::collections::HashMap;

    /// Returns a map of interface alias → (in_discards, out_discards).
    #[cfg(windows)]
    pub fn get() -> HashMap<String, (u64, u64)> {
        use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
        let mut map = HashMap::new();
        unsafe {
            let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table_ptr).is_err() || table_ptr.is_null() {
                return map;
            }
            let num = (*table_ptr).NumEntries as usize;
            // The Table field is declared as [MIB_IF_ROW2; 1] but the kernel
            // allocates `NumEntries` rows contiguously — use a raw pointer walk.
            let row_ptr = (*table_ptr).Table.as_ptr();
            for i in 0..num {
                let row = &*row_ptr.add(i);
                // Alias is the friendly name ("Ethernet", "Wi-Fi", …).
                let alias_end = row.Alias.iter().position(|&c| c == 0).unwrap_or(row.Alias.len());
                if let Ok(name) = String::from_utf16(&row.Alias[..alias_end]) {
                    map.insert(name, (row.InDiscards, row.OutDiscards));
                }
            }
            FreeMibTable(table_ptr as *const _);
        }
        map
    }

    #[cfg(not(windows))]
    pub fn get() -> HashMap<String, (u64, u64)> {
        HashMap::new()
    }
}

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

// ── NVML GPU monitor (NVIDIA only) ────────────────────────────────────────────

/// Wraps optional NVML access so the rest of the code can call `.sample()`
/// unconditionally and receive an empty Vec when NVML is not available.
struct GpuMonitor {
    #[cfg(feature = "nvidia")]
    nvml: Option<Nvml>,
    #[cfg(not(feature = "nvidia"))]
    _phantom: (),
    #[cfg(windows)]
    pdh: Option<pdh_gpu::PdhGpuMonitor>,
}

impl GpuMonitor {
    fn init(no_console: bool) -> Self {
        #[cfg(feature = "nvidia")]
        {
            match Nvml::init() {
                Ok(nvml) => {
                    cprint!(no_console, "[system-monitor] NVML initialised — NVIDIA GPU monitoring active");
                    return Self {
                        nvml: Some(nvml),
                        #[cfg(windows)]
                        pdh: None,
                    };
                }
                Err(e) => {
                    cprint!(no_console, "[system-monitor] NVML unavailable ({e}) — trying PDH fallback");
                }
            }
        }

        #[cfg(windows)]
        {
            let pdh = pdh_gpu::PdhGpuMonitor::init();
            let ok  = pdh.is_some();
            if ok {
                cprint!(no_console, "[system-monitor] PDH GPU monitoring active (cross-vendor)");
            } else {
                cprint!(no_console, "[system-monitor] no GPU monitoring available");
            }
            return Self {
                #[cfg(feature = "nvidia")]
                nvml: None,
                #[cfg(not(feature = "nvidia"))]
                _phantom: (),
                pdh,
            };
        }

        #[cfg(not(windows))]
        {
            let _ = no_console;
            Self {
                #[cfg(feature = "nvidia")]
                nvml: None,
                #[cfg(not(feature = "nvidia"))]
                _phantom: (),
            }
        }
    }

    /// Collect a sample for every detected GPU.
    fn sample(&mut self) -> Vec<GpuSample> {
        // NVIDIA via NVML (highest fidelity — temperature, power, etc.)
        #[cfg(feature = "nvidia")]
        if let Some(nvml) = &self.nvml {
            let count = match nvml.device_count() {
                Ok(n)  => n,
                Err(_) => return Vec::new(),
            };
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                if let Some(s) = sample_gpu(nvml, i) { out.push(s); }
            }
            if !out.is_empty() { return out; }
        }

        // Cross-vendor fallback via PDH + DXGI
        #[cfg(windows)]
        if let Some(pdh) = &mut self.pdh {
            return pdh.sample().into_iter().map(|s| {
                let vram_free_mb  = (s.vram_total_mb - s.vram_used_mb).max(0.0);
                let vram_free_pct = if s.vram_total_mb > 0.0 {
                    vram_free_mb / s.vram_total_mb * 100.0
                } else {
                    0.0
                };
                GpuSample {
                    index:             s.index,
                    name:              s.name,
                    gpu_used_percent:  round2(s.gpu_used_pct),
                    vram_total_mb:     round2(s.vram_total_mb),
                    vram_used_mb:      round2(s.vram_used_mb),
                    vram_free_mb:      round2(vram_free_mb),
                    vram_free_percent: round2(vram_free_pct),
                    temperature_c:     None,   // not available via PDH
                    encoder_percent:   Some(s.encoder_pct as u32),
                    decoder_percent:   Some(s.decoder_pct as u32),
                    power_w:           None,
                }
            }).collect();
        }

        Vec::new()
    }

    /// Return the name of every detected GPU (used in `system_info`).
    fn gpu_names(&self) -> Vec<String> {
        #[cfg(feature = "nvidia")]
        if let Some(nvml) = &self.nvml {
            let count = nvml.device_count().unwrap_or(0);
            let names: Vec<String> = (0..count)
                .filter_map(|i| nvml.device_by_index(i).ok())
                .filter_map(|d| d.name().ok())
                .collect();
            if !names.is_empty() { return names; }
        }
        #[cfg(windows)]
        if let Some(pdh) = &self.pdh {
            return pdh.adapter_names();
        }
        Vec::new()
    }

    /// "nvml" when NVML is active, "pdh" for cross-vendor fallback, "none" otherwise.
    fn backend(&self) -> &'static str {
        #[cfg(feature = "nvidia")]
        if self.nvml.is_some() { return "nvml"; }
        #[cfg(windows)]
        if self.pdh.is_some()  { return "pdh";  }
        "none"
    }
}

/// Sample one NVIDIA GPU by index.  Returns None if any mandatory query fails.
#[cfg(feature = "nvidia")]
fn sample_gpu(nvml: &Nvml, index: u32) -> Option<GpuSample> {
    let device = nvml.device_by_index(index).ok()?;
    let name   = device.name().ok()?;

    let util      = device.utilization_rates().ok()?;
    let mem       = device.memory_info().ok()?;
    let temp      = device.temperature(TemperatureSensor::Gpu).ok()?;

    let vram_total_mb = mem.total as f64 / 1_048_576.0;
    let vram_used_mb  = mem.used  as f64 / 1_048_576.0;
    let vram_free_mb  = mem.free  as f64 / 1_048_576.0;

    // Encoder / decoder utilisation — may not be available on all GPU models.
    let encoder_percent = device.encoder_utilization().ok().map(|e| e.utilization);
    let decoder_percent = device.decoder_utilization().ok().map(|d| d.utilization);

    // Power in milliwatts → watts.
    let power_w = device.power_usage().ok().map(|mw| mw / 1_000);

    Some(GpuSample {
        index,
        name,
        gpu_used_percent:  round2(util.gpu as f64),
        vram_total_mb:     round2(vram_total_mb),
        vram_used_mb:      round2(vram_used_mb),
        vram_free_mb:      round2(vram_free_mb),
        vram_free_percent: round2(pct(vram_free_mb, vram_total_mb)),
        temperature_c:     Some(temp),
        encoder_percent,
        decoder_percent,
        power_w,
    })
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

    // ── Shutdown flag ─────────────────────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("failed to install Ctrl-C handler");
    }

    // ── Initialise subsystems ─────────────────────────────────────────────────
    let mut sys = System::new();
    sys.refresh_cpu_all(); // establishes CPU measurement baseline

    let mut networks = Networks::new_with_refreshed_list();

    let mut gpu_monitor = GpuMonitor::init(args.no_console);

    // ── Write system_info ─────────────────────────────────────────────────────
    {
        sys.refresh_memory();
        let core_count    = sys.cpus().len();
        let cpu_brand     = sys.cpus().first().map(|c| c.brand().to_string()).unwrap_or_default();
        let cpu_arch      = System::cpu_arch().unwrap_or_default();
        let mem_total_mb  = sys.total_memory() as f64 / 1_048_576.0;
        let swap_total_mb = sys.total_swap()   as f64 / 1_048_576.0;
        let os_name       = System::name()       .unwrap_or_default();
        let os_version    = System::os_version() .unwrap_or_default();
        let hostname      = System::host_name()  .unwrap_or_default();
        let gpu_names     = gpu_monitor.gpu_names();
        let gpu_backend   = gpu_monitor.backend();

        cprint!(args.no_console,
            "[system-monitor] {os_name} | {core_count} cores | {mem_total_mb:.0} MB RAM | GPU backend: {gpu_backend}"
        );

        send(&tx, &LogEntry::info(MONITOR, "system_info", SystemInfoData {
            cpu_brand,
            cpu_arch,
            cpu_core_count:  core_count,
            memory_total_mb: round2(mem_total_mb),
            swap_total_mb:   round2(swap_total_mb),
            os_name,
            os_version,
            hostname,
            gpus:           gpu_names,
            gpu_monitoring: gpu_backend.to_string(),
        }));
    }

    cprint!(args.no_console, "[system-monitor] poll interval {}ms", sm.poll_interval_ms);

    // ── Main monitoring loop ──────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        // Read current config once per iteration (picks up hot-reload changes).
        let sm_cfg       = config.read().monitors.system_monitor.clone();
        let log_cfg      = sm_cfg.log.clone();
        let watch_disks  = sm_cfg.watch_disks.clone();
        let watch_ifaces = sm_cfg.watch_network_interfaces.clone();
        let poll_interval = Duration::from_millis(sm_cfg.poll_interval_ms);

        // Always refresh CPU to keep the measurement window accurate even when
        // sampling is paused (so the next sample after re-enabling is correct).
        sys.refresh_cpu_all();

        // Only collect, write, and alert when the interval is active (> 0).
        if sm_cfg.poll_interval_ms > 0 {
            let elapsed_secs = tick.elapsed().as_secs_f64().max(0.001);

            // ── CPU ───────────────────────────────────────────────────────────
            let cpu_used = sys.global_cpu_usage() as f64;
            let cpu_free = (100.0 - cpu_used).max(0.0);

            let cores: Vec<CoreSample> = if log_cfg.cpu_per_core {
                sys.cpus().iter().enumerate().map(|(id, cpu)| CoreSample {
                    id,
                    used_percent:  round2(cpu.cpu_usage() as f64),
                    frequency_mhz: cpu.frequency(),
                }).collect()
            } else {
                Vec::new()
            };

            // ── Memory ────────────────────────────────────────────────────────
            sys.refresh_memory();
            let mem_total_mb = sys.total_memory()     as f64 / 1_048_576.0;
            let mem_used_mb  = sys.used_memory()      as f64 / 1_048_576.0;
            let mem_free_mb  = sys.available_memory() as f64 / 1_048_576.0;
            let mem_free_pct = pct(mem_free_mb, mem_total_mb);

            // ── Swap / pagefile ───────────────────────────────────────────────
            let swap_total_mb = sys.total_swap() as f64 / 1_048_576.0;
            let swap_used_mb  = sys.used_swap()  as f64 / 1_048_576.0;
            let swap_used_pct = pct(swap_used_mb, swap_total_mb);

            // ── Network ───────────────────────────────────────────────────────
            networks.refresh();
            let drop_map = network_drop_counts();

            let net_samples: Vec<NetworkSample> = if log_cfg.network {
                networks.iter()
                    .filter(|(name, _)| iface_included(name, &watch_ifaces))
                    .map(|(name, data)| {
                        let (rx_dropped, tx_dropped) = drop_map.get(name.as_str())
                            .copied()
                            .unwrap_or((0, 0));
                        NetworkSample {
                            interface:     name.clone(),
                            rx_mb_per_sec: round2(data.received()    as f64 / 1_048_576.0 / elapsed_secs),
                            tx_mb_per_sec: round2(data.transmitted() as f64 / 1_048_576.0 / elapsed_secs),
                            rx_errors:     data.errors_on_received(),
                            tx_errors:     data.errors_on_transmitted(),
                            rx_dropped,
                            tx_dropped,
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };

            // ── Disks ─────────────────────────────────────────────────────────
            let disks = Disks::new_with_refreshed_list();
            let disk_samples: Vec<DiskSample> = if log_cfg.disk {
                disks.list().iter()
                    .filter(|d| disk_included(&d.mount_point().to_string_lossy(), &watch_disks))
                    .map(|d| {
                        let total_gb = d.total_space()     as f64 / 1_073_741_824.0;
                        let free_gb  = d.available_space() as f64 / 1_073_741_824.0;
                        DiskSample {
                            path:         d.mount_point().to_string_lossy().into_owned(),
                            total_gb:     round2(total_gb),
                            free_gb:      round2(free_gb),
                            free_percent: round2(pct(free_gb, total_gb)),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };

            // ── GPU (NVIDIA via NVML) ─────────────────────────────────────────
            let gpu_samples: Vec<GpuSample> = if log_cfg.gpu {
                gpu_monitor.sample()
            } else {
                Vec::new()
            };

            // ── Write sample ──────────────────────────────────────────────────
            send(&tx, &LogEntry::info(MONITOR, "system_resource_sample", SystemResourceSampleData {
                cpu_used_percent:    round2(cpu_used),
                cpu_free_percent:    round2(cpu_free),
                cores:               cores.clone(),
                memory_total_mb:     round2(mem_total_mb),
                memory_used_mb:      round2(mem_used_mb),
                memory_free_mb:      round2(mem_free_mb),
                memory_free_percent: round2(mem_free_pct),
                swap_total_mb:       round2(swap_total_mb),
                swap_used_mb:        round2(swap_used_mb),
                swap_used_percent:   round2(swap_used_pct),
                network:             net_samples.clone(),
                disks:               disk_samples.clone(),
                gpus:                gpu_samples.clone(),
            }));

            // ── Threshold alerts ──────────────────────────────────────────────

            check_warn_alert(&tx, MONITOR, "cpu_headroom_alert",
                cpu_free,
                log_cfg.cpu_warn_free_percent, log_cfg.cpu_alert_free_percent,
                |v, th| format!("CPU headroom {v:.1}% below threshold {th:.0}%"),
                ThresholdDir::Below);

            if log_cfg.cpu_per_core {
                for core in &cores {
                    check_warn_alert(&tx, MONITOR, "cpu_core_alert",
                        core.used_percent,
                        log_cfg.cpu_core_warn_percent, log_cfg.cpu_core_alert_percent,
                        |v, th| format!("Core {} used {v:.1}% above threshold {th:.0}%", core.id),
                        ThresholdDir::Above);
                }
            }

            check_warn_alert(&tx, MONITOR, "memory_headroom_alert",
                mem_free_mb,
                log_cfg.memory_warn_free_mb, log_cfg.memory_alert_free_mb,
                |v, th| format!("free RAM {v:.0} MB below threshold {th:.0} MB"),
                ThresholdDir::Below);

            check_warn_alert(&tx, MONITOR, "swap_alert",
                swap_used_pct,
                log_cfg.swap_warn_used_percent, log_cfg.swap_alert_used_percent,
                |v, th| format!("swap used {v:.1}% above threshold {th:.0}%"),
                ThresholdDir::Above);

            for disk in &disk_samples {
                check_warn_alert(&tx, MONITOR, "disk_headroom_alert",
                    disk.free_gb,
                    log_cfg.disk_warn_free_gb, log_cfg.disk_alert_free_gb,
                    |v, th| format!("disk {} free {v:.1} GB below threshold {th:.0} GB", disk.path),
                    ThresholdDir::Below);
            }

            if let Some(th) = log_cfg.network_rx_warn_mbps {
                for n in &net_samples {
                    if n.rx_mb_per_sec > th {
                        send(&tx, &LogEntry::warn(MONITOR, "network_rx_alert", WarningData {
                            msg: format!("{} RX {:.2} MB/s above threshold {th:.0} MB/s", n.interface, n.rx_mb_per_sec),
                            detail: None,
                        }));
                    }
                }
            }
            if let Some(th) = log_cfg.network_tx_warn_mbps {
                for n in &net_samples {
                    if n.tx_mb_per_sec > th {
                        send(&tx, &LogEntry::warn(MONITOR, "network_tx_alert", WarningData {
                            msg: format!("{} TX {:.2} MB/s above threshold {th:.0} MB/s", n.interface, n.tx_mb_per_sec),
                            detail: None,
                        }));
                    }
                }
            }
            if log_cfg.network_error_alert {
                for n in &net_samples {
                    if n.rx_errors > 0 || n.tx_errors > 0 {
                        send(&tx, &LogEntry::error(MONITOR, "network_error_alert", WarningData {
                            msg: format!("{} errors: rx={} tx={}", n.interface, n.rx_errors, n.tx_errors),
                            detail: None,
                        }));
                    }
                }
            }
            if log_cfg.network_drop_alert {
                for n in &net_samples {
                    if n.rx_dropped > 0 || n.tx_dropped > 0 {
                        send(&tx, &LogEntry::warn(MONITOR, "network_drop_alert", WarningData {
                            msg: format!("{} dropped: rx={} tx={}", n.interface, n.rx_dropped, n.tx_dropped),
                            detail: None,
                        }));
                    }
                }
            }

            // ── GPU alerts ────────────────────────────────────────────────────
            for gpu in &gpu_samples {
                check_warn_alert(&tx, MONITOR, "gpu_util_alert",
                    gpu.gpu_used_percent,
                    log_cfg.gpu_warn_util_percent, log_cfg.gpu_alert_util_percent,
                    |v, th| format!("GPU {} utilisation {v:.1}% above threshold {th:.0}%", gpu.name),
                    ThresholdDir::Above);

                check_warn_alert(&tx, MONITOR, "gpu_vram_alert",
                    gpu.vram_free_mb,
                    log_cfg.gpu_vram_warn_free_mb, log_cfg.gpu_vram_alert_free_mb,
                    |v, th| format!("GPU {} VRAM free {v:.0} MB below threshold {th:.0} MB", gpu.name),
                    ThresholdDir::Below);

                if let Some(temp_c) = gpu.temperature_c {
                    check_warn_alert(&tx, MONITOR, "gpu_temp_alert",
                        temp_c as f64,
                        log_cfg.gpu_temp_warn_c, log_cfg.gpu_temp_alert_c,
                        |v, th| format!("GPU {} temperature {v:.0}°C above threshold {th:.0}°C", gpu.name),
                        ThresholdDir::Above);
                }

                if let Some(enc) = gpu.encoder_percent {
                    if let Some(th) = log_cfg.gpu_encoder_warn_percent {
                        if enc as f64 > th {
                            send(&tx, &LogEntry::warn(MONITOR, "gpu_encoder_alert", WarningData {
                                msg: format!("GPU {} NVENC encoder {enc}% above threshold {th:.0}%", gpu.name),
                                detail: None,
                            }));
                        }
                    }
                }
            }
        }

        // ── Sleep — chunked so config changes and Ctrl-C take effect quickly ──
        let sleep_for = if poll_interval.is_zero() {
            Duration::from_secs(1)
        } else {
            poll_interval
        };
        loop {
            let elapsed = tick.elapsed();
            if elapsed >= sleep_for { break; }
            if !running.load(Ordering::SeqCst) { break; }
            let chunk = (sleep_for - elapsed).min(Duration::from_millis(sm_cfg.min_tick_ms.max(50)));
            thread::sleep(chunk);
            let new_cfg = config.read().monitors.system_monitor.clone();
            if Duration::from_millis(new_cfg.poll_interval_ms) != poll_interval
                || new_cfg.min_tick_ms != sm_cfg.min_tick_ms
            { break; }
        }
    }

    cprint!(args.no_console, "[system-monitor] shutting down…");

    drop(tx);
    let _ = writer_thread.join();

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns interface alias → (in_discards, out_discards).
fn network_drop_counts() -> HashMap<String, (u64, u64)> {
    net_drops::get()
}

fn round2(v: f64) -> f64 { (v * 100.0).round() / 100.0 }

fn pct(part: f64, total: f64) -> f64 {
    if total > 0.0 { (part / total * 100.0).clamp(0.0, 100.0) } else { 0.0 }
}

fn disk_included(mount: &str, watch_disks: &[String]) -> bool {
    if watch_disks.is_empty() { return true; }
    let m = mount.to_lowercase();
    watch_disks.iter().any(|w| m.starts_with(&w.to_lowercase()))
}

fn iface_included(name: &str, watch_ifaces: &[String]) -> bool {
    if name.to_lowercase().contains("loopback") { return false; }
    if watch_ifaces.is_empty() { return true; }
    watch_ifaces.iter().any(|w| w.eq_ignore_ascii_case(name))
}

enum ThresholdDir { Below, Above }

fn check_warn_alert(
    tx:       &crossbeam_channel::Sender<String>,
    monitor:  &'static str,
    event:    &'static str,
    value:    f64,
    warn_th:  Option<f64>,
    alert_th: Option<f64>,
    msg_fn:   impl Fn(f64, f64) -> String,
    dir:      ThresholdDir,
) {
    let crossed = |th: f64| match dir {
        ThresholdDir::Below => value < th,
        ThresholdDir::Above => value > th,
    };
    if let Some(th) = alert_th {
        if crossed(th) {
            send(tx, &LogEntry::error(monitor, event, WarningData { msg: msg_fn(value, th), detail: None }));
            return;
        }
    }
    if let Some(th) = warn_th {
        if crossed(th) {
            send(tx, &LogEntry::warn(monitor, event, WarningData { msg: msg_fn(value, th), detail: None }));
        }
    }
}
