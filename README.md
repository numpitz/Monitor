# Monitor Suite

Rust 2024 · Windows 10+ · three standalone `.exe` files

A lightweight monitoring suite for video-streaming servers running **go2rtc** and **ffmpeg**.
All three binaries share one `monitor.config.json` and write their own NDJSON log files.

| Binary | Purpose | Log file |
|--------|---------|----------|
| `process-monitor.exe` | Per-process CPU, RAM, handles, spawn/exit events | `proc_resources.jsonl` |
| `system-monitor.exe` | System-wide free CPU, RAM, swap, disk, network, GPU | `sys_resources.jsonl` |
| `monitor-ui.exe` | egui desktop app — live config editor and process viewer | — |

---

## Build

```powershell
# Debug (fast compile, for development)
cargo build

# Release (optimised, stripped — for production)
cargo build --release

# Release with NVIDIA GPU monitoring (requires NVIDIA driver)
cargo build --release --features nvidia
```

Outputs in `target\release\` (or `target\debug\`):

```
process-monitor.exe
system-monitor.exe
monitor-ui.exe
```

No runtime dependencies — each ships as a single file.

---

## Usage

All binaries take the same first argument: the directory that contains
`monitor.config.json`.  Log files are also written there.

```powershell
# Monitors — with console window
process-monitor.exe C:\monitor\
system-monitor.exe  C:\monitor\

# Monitors — detached, no window (background / supervisor mode)
process-monitor.exe C:\monitor\ --no-console
system-monitor.exe  C:\monitor\ --no-console

# Configuration UI
monitor-ui.exe C:\monitor\
```

### Live configuration

The UI writes changes atomically.  Both monitors pick them up within ~400 ms
via their built-in config-watcher — **no restart required**.

---

## monitor-ui

The UI has two panels.

### Configuration panel

Edit both monitors without restarting them.

| Control | Range | Effect when saved |
|---------|-------|-------------------|
| Process Monitor — enabled | checkbox | Monitor starts / skips on next launch |
| Resource poll interval | 0 – 60 s | Per-process CPU / RAM sample rate (`0` = off) |
| Snapshot interval | 0 – 600 s | Full process-tree snapshot rate (`0` = off) |
| System Monitor — enabled | checkbox | Monitor starts / skips on next launch |
| Poll interval | 0 – 300 s | System-wide sample rate (`0` = off) |

Setting an interval to **0** pauses that sampling block immediately — the monitor
process keeps running and responds to future config changes without a restart.

### Process viewer panel

Reads the latest `proc_resources.N.jsonl` log file and shows the current state
of all watched processes.  Refreshes automatically every **5 seconds**; a manual
**⟳ Refresh** button is available in the panel header.

| Column | Description |
|--------|-------------|
| Name | Process executable name |
| PID | OS process ID |
| CPU % | CPU usage from the last `resource_sample` |
| Mem MB | Resident memory from the last `resource_sample` |
| Handles | Open handle count |
| Threads | Thread count |
| Last seen | `HH:MM:SS` of the last sample (or spawn / exit time) |

Alive processes are shown in white.  Processes that have exited appear dimmed in
grey with `—` for metrics and the exit time in **Last seen**.

---

## Config (`monitor.config.json`)

All three binaries read the same file.

```json
{
  "log_rotation": { ... },
  "monitors": {
    "process_monitor": { ... },
    "system_monitor":  { ... }
  }
}
```

### `log_rotation`

| Key | Default | Description |
|-----|---------|-------------|
| `max_file_size_mb` | 10 | Rotate when the active log file exceeds this size |
| `keep_files` | 5 | Number of rotated files to keep |

---

### `monitors.process_monitor`

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Set `false` to skip on startup |
| `log_file` | `proc_resources.jsonl` | Base name for log files |
| `resource_poll_interval_ms` | 5000 | Per-process CPU / RAM sample frequency |
| `snapshot_interval_ms` | 60000 | Full process-tree snapshot frequency |
| `watch_folders` | required | Absolute paths — every `.exe` here is watched |
| `log.cpu_alert_threshold_percent` | null | WARN when a process exceeds this CPU % |
| `log.memory_alert_mb` | null | WARN when a process exceeds this RAM (MB) |

---

### `monitors.system_monitor`

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Set `false` to skip on startup |
| `log_file` | `sys_resources.jsonl` | Base name for log files |
| `poll_interval_ms` | 30000 | How often to sample (30 s recommended) |
| `watch_disks` | `[]` (all) | Mount points to report, e.g. `["C:\\"]` |
| `watch_network_interfaces` | `[]` (all) | Interface names to report, e.g. `["Ethernet"]` |

#### Thresholds — two levels per metric

Every metric has a **WARN** threshold (approaching a limit) and an **ERROR/alert**
threshold (limit breached).  Both are optional — omit either to disable that level.

| Key | Default | Fires when… |
|-----|---------|-------------|
| `log.cpu_warn_free_percent` | 30 | CPU headroom < 30 % |
| `log.cpu_alert_free_percent` | 10 | CPU headroom < 10 % |
| `log.cpu_core_warn_percent` | 85 | Any single core > 85 % |
| `log.cpu_core_alert_percent` | 95 | Any single core > 95 % |
| `log.memory_warn_free_mb` | 1000 | Available RAM < 1000 MB |
| `log.memory_alert_free_mb` | 500 | Available RAM < 500 MB |
| `log.swap_warn_used_percent` | 30 | Swap used > 30 % |
| `log.swap_alert_used_percent` | 70 | Swap used > 70 % |
| `log.disk_warn_free_gb` | 20 | Any watched disk free < 20 GB |
| `log.disk_alert_free_gb` | 10 | Any watched disk free < 10 GB |
| `log.network_rx_warn_mbps` | null | Any interface RX > threshold MB/s |
| `log.network_tx_warn_mbps` | null | Any interface TX > threshold MB/s |
| `log.network_error_alert` | `true` | Any interface has RX or TX errors |
| `log.gpu_warn_util_percent` | 80 | GPU utilisation > 80 % |
| `log.gpu_alert_util_percent` | 95 | GPU utilisation > 95 % |
| `log.gpu_encoder_warn_percent` | 80 | NVENC encoder > 80 % |
| `log.gpu_vram_warn_free_mb` | 500 | VRAM free < 500 MB |
| `log.gpu_vram_alert_free_mb` | 200 | VRAM free < 200 MB |
| `log.gpu_temp_warn_c` | 80 | GPU temperature > 80 °C |
| `log.gpu_temp_alert_c` | 90 | GPU temperature > 90 °C |

---

## Log file naming

Each monitor uses numbered rotation independently:

```
proc_resources.0.jsonl   ← oldest
proc_resources.1.jsonl
proc_resources.2.jsonl   ← active

sys_resources.0.jsonl
sys_resources.1.jsonl    ← active
```

Every file starts with a `monitor_start` entry.  Rotation entries include
`"continued_from"` so readers can follow the chain backwards.

---

## Event reference

All events share the same envelope:

```json
{
  "ts":      "2026-03-23T10:00:00.000Z",
  "monitor": "process_monitor | system_monitor",
  "event":   "<event_name>",
  "level":   "INFO | WARN | ERROR",
  "...":     "event-specific fields"
}
```

---

### Shared events (both monitors)

#### `monitor_start`
```json
{ "pid": 4821, "log_file": "proc_resources.2.jsonl", "rotation": false }
```

#### `monitor_stop`
```json
{ "pid": 4821, "reason": "shutdown", "exit_code": 0 }
```

#### `config_reloaded`
```json
{ "path": "C:\\monitor\\monitor.config.json" }
```

---

### process-monitor events

#### `process_spawned`
```json
{ "pid": 1235, "name": "ffmpeg.exe", "exe_path": "C:\\go2rtc\\bin\\ffmpeg.exe" }
```

#### `process_exited`
```json
{ "pid": 1235, "name": "ffmpeg.exe", "uptime_seconds": 1195 }
```

#### `resource_sample`
Written every `resource_poll_interval_ms`.
```json
{
  "processes": [
    { "pid": 1234, "name": "go2rtc.exe",
      "cpu_percent": 12.3, "memory_mb": 84.2, "handles": 312, "threads": 18 }
  ],
  "total_cpu_percent": 12.3,
  "total_memory_mb": 84.2
}
```

#### `process_tree_snapshot`
Written every `snapshot_interval_ms`.
```json
{
  "count": 1,
  "processes": [
    { "pid": 1234, "name": "go2rtc.exe", "exe_path": "C:\\go2rtc\\go2rtc.exe",
      "started_at": "2026-03-20T09:45:00.000Z", "threads": 18, "memory_mb": 0.0 }
  ]
}
```

#### `cpu_alert` / `memory_alert`
```json
{ "msg": "go2rtc.exe cpu=91.2% exceeds threshold 80%" }
```

---

### system-monitor events

#### `system_info`
Written once at startup — static facts about the host.
```json
{
  "cpu_brand": "Intel Core i7-12700K", "cpu_arch": "x86_64", "cpu_core_count": 12,
  "memory_total_mb": 32768, "swap_total_mb": 8192,
  "os_name": "Windows", "os_version": "11", "hostname": "SERVER-01",
  "gpus": ["NVIDIA GeForce RTX 4090"], "gpu_monitoring": "nvml"
}
```

#### `system_resource_sample`
Written every `poll_interval_ms`.
```json
{
  "cpu_used_percent": 34.5,  "cpu_free_percent": 65.5,
  "cores": [
    { "id": 0, "used_percent": 45.2, "frequency_mhz": 3600 }
  ],
  "memory_total_mb": 32768, "memory_used_mb": 9200,
  "memory_free_mb": 7184,   "memory_free_percent": 43.8,
  "swap_total_mb": 8192,    "swap_used_mb": 512,   "swap_used_percent": 6.25,
  "network": [
    { "interface": "Ethernet",
      "rx_mb_per_sec": 9.5, "tx_mb_per_sec": 2.1,
      "rx_errors": 0, "tx_errors": 0 }
  ],
  "disks": [
    { "path": "C:\\", "total_gb": 476.84, "free_gb": 210.12, "free_percent": 44.06 }
  ],
  "gpus": [
    { "index": 0, "name": "NVIDIA GeForce RTX 4090",
      "gpu_used_percent": 42.0, "vram_total_mb": 24576, "vram_used_mb": 8192,
      "vram_free_mb": 16384, "vram_free_percent": 66.67,
      "temperature_c": 68, "encoder_percent": 35, "decoder_percent": 10, "power_w": 180 }
  ]
}
```

#### Alert events

| Event | Level | Example message |
|-------|-------|-----------------|
| `cpu_headroom_alert` | WARN / ERROR | `CPU headroom 8% below threshold 10%` |
| `cpu_core_alert` | WARN / ERROR | `Core 2 used 97.1% above threshold 95%` |
| `memory_headroom_alert` | WARN / ERROR | `free RAM 420 MB below threshold 500 MB` |
| `swap_alert` | WARN / ERROR | `swap used 75% above threshold 70%` |
| `disk_headroom_alert` | WARN / ERROR | `disk C:\ free 8.1 GB below threshold 10 GB` |
| `network_rx_alert` | WARN | `Ethernet RX 95.2 MB/s above threshold 80 MB/s` |
| `network_error_alert` | ERROR | `Ethernet errors: rx=3 tx=0` |
| `gpu_util_alert` | WARN / ERROR | `GPU RTX 4090 utilisation 96% above threshold 95%` |
| `gpu_vram_alert` | WARN / ERROR | `GPU RTX 4090 VRAM free 180 MB below threshold 200 MB` |
| `gpu_temp_alert` | WARN / ERROR | `GPU RTX 4090 temperature 91°C above threshold 90°C` |
| `gpu_encoder_alert` | WARN | `GPU RTX 4090 NVENC encoder 85% above threshold 80%` |

---

## GPU monitoring (NVIDIA)

Build with `--features nvidia` to enable NVML-based GPU monitoring.

| Metric | Why it matters for streaming |
|--------|------------------------------|
| GPU utilisation % | Overall load |
| NVENC encoder % | Hardware encoder saturation (go2rtc streams) |
| NVDEC decoder % | Hardware decoder saturation (ffmpeg transcode) |
| VRAM free | Running out stops hardware encoding |
| Temperature | Thermal throttle causes frame drops |
| Power draw | Context for thermal readings |

NVML is part of the NVIDIA driver — no extra install required.
AMD and Intel GPUs are listed in `system_info` but real-time metrics require their respective vendor SDKs (not yet implemented).

---

## Grep examples

```powershell
# All WARN and ERROR entries across both monitors
Get-ChildItem C:\monitor\*.jsonl | Get-Content |
  ConvertFrom-Json | Where-Object level -ne INFO

# Free CPU headroom over the last hour (120 samples × 30 s)
Get-Content C:\monitor\sys_resources.0.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq system_resource_sample |
  Select-Object -Last 120 | Select-Object ts, cpu_free_percent

# NVENC encoder utilisation trend
Get-Content C:\monitor\sys_resources.0.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq system_resource_sample |
  ForEach-Object { $_.gpus[0].encoder_percent } |
  Measure-Object -Average -Maximum

# All processes spawned today
Get-Content C:\monitor\proc_resources.*.jsonl |
  ConvertFrom-Json | Where-Object event -eq process_spawned

# Average CPU of watched processes over last 50 samples
Get-Content C:\monitor\proc_resources.0.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq resource_sample |
  Select-Object -Last 50 |
  ForEach-Object { $_.total_cpu_percent } |
  Measure-Object -Average
```
