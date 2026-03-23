# Monitor Suite

Rust 2024 · Windows 10+ · five standalone `.exe` files

A lightweight monitoring suite for video-streaming servers running **go2rtc** and **ffmpeg**.
All binaries share one `monitor.config.json` and write their own NDJSON log files.

| Binary | Purpose | Log file |
|--------|---------|----------|
| `monitor-watchdog.exe` | Supervisor — starts, watches, and restarts all monitors | `watchdog.jsonl` |
| `process-monitor.exe` | Per-process CPU, RAM, handles, spawn/exit events | `proc_resources.jsonl` |
| `system-monitor.exe` | System-wide CPU, RAM, swap, disk, network, GPU | `sys_resources.jsonl` |
| `go2rtc-monitor.exe` | go2rtc stream state — producers, consumers, up/down events | `go2rtc_streams.jsonl` |
| `monitor-ui.exe` | egui desktop app — live config editor and resource viewer | — |

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
monitor-watchdog.exe
process-monitor.exe
system-monitor.exe
go2rtc-monitor.exe
monitor-ui.exe
```

No runtime dependencies — each ships as a single file.

---

## Usage

All binaries take the same first argument: the directory that contains
`monitor.config.json`.  Log files are also written there.

### Recommended: run everything through the watchdog

```powershell
# Start all enabled monitors and keep them alive
monitor-watchdog.exe C:\monitor\

# Same, but detached — no console window
monitor-watchdog.exe C:\monitor\ --no-console
```

The watchdog starts every monitor that is `"enabled": true` in the config,
**plus `monitor-ui`**, monitors them every 500 ms, and restarts any monitor that
exits unexpectedly within 3 s.  Ctrl-C (or closing the console window) kills all
children first, then exits cleanly.

If a monitor is **disabled** in the config while the watchdog is running, the
watchdog kills that child and does not restart it.  If a monitor is **enabled**
in the config while the watchdog is running, the watchdog starts it within 5 s —
no restart of the watchdog required.

### Run monitors individually (advanced)

```powershell
# With console window
process-monitor.exe C:\monitor\
system-monitor.exe  C:\monitor\
go2rtc-monitor.exe  C:\monitor\

# Detached, no window
process-monitor.exe C:\monitor\ --no-console
system-monitor.exe  C:\monitor\ --no-console
go2rtc-monitor.exe  C:\monitor\ --no-console

# Configuration UI (started automatically by the watchdog — only needed here
# when running monitors individually)
monitor-ui.exe C:\monitor\
```

### Live configuration

The UI writes changes atomically.

| Component | Picks up config changes within… |
|-----------|----------------------------------|
| `process-monitor`, `system-monitor`, `go2rtc-monitor` | ~400 ms (built-in file-watcher) |
| `monitor-watchdog` (enabled / disabled flags) | 5 s (periodic disk poll) |

No restart required for any component.

---

## monitor-watchdog

The watchdog is the single entry point for production deployments.

### Behaviour summary

| Situation | Watchdog action |
|-----------|-----------------|
| Monitor enabled in config | Start on next tick (500 ms) |
| Monitor exits unexpectedly | Log `child_exited`, restart after 3 s |
| Monitor disabled in config while running | Kill immediately, do not restart |
| Spawn fails (binary not found, etc.) | Log `child_start_failed`, retry after 3 s |
| Watchdog receives Ctrl-C | Kill all children, drain log, exit 0 |
| `monitor-ui` window closed by user | Log `child_exited` at INFO, do **not** reopen |

`monitor-ui` is always launched alongside the monitors — no config flag needed.
It is treated as a one-shot GUI tool: if the user closes the window the watchdog
leaves it closed.  All other children are restarted automatically.

Config changes are picked up every **5 s** by re-reading `monitor.config.json`
from disk — no file-watcher thread is used in the watchdog.

### Watchdog log events

| Event | Level | Description |
|-------|-------|-------------|
| `child_started` | INFO | A monitor was started for the first time |
| `child_restarted` | INFO | A monitor was restarted after an unexpected exit |
| `child_exited` | WARN | A monitor exited (includes `exit_code` and `uptime_seconds`) |
| `child_start_failed` | ERROR | Could not spawn the binary |

---

## monitor-ui

The UI has four panels.

### Configuration panel

Edit all monitors without restarting them.

| Control | Range | Effect when saved |
|---------|-------|-------------------|
| Process Monitor — enabled | checkbox | Monitor starts / skips on next launch |
| Resource poll interval | 0 – 60 s | Per-process CPU / RAM sample rate (`0` = off) |
| Snapshot interval | 0 – 600 s | Full process-tree snapshot rate (`0` = off) |
| Response interval | 50 – 5000 ms | How quickly the monitor reacts to config changes or Ctrl-C |
| System Monitor — enabled | checkbox | Monitor starts / skips on next launch |
| Poll interval | 0 – 300 s | System-wide sample rate (`0` = off) |
| Response interval | 50 – 5000 ms | How quickly the monitor reacts to config changes or Ctrl-C |
| go2rtc Monitor — enabled | checkbox | Monitor starts / skips on next launch |
| API URL | text field | go2rtc base URL, e.g. `http://localhost:1984` |
| Poll interval | 0 – 300 s | Stream API poll rate (`0` = off) |
| Response interval | 50 – 5000 ms | How quickly the monitor reacts to config changes or Ctrl-C |

Setting a poll interval to **0** pauses that sampling block immediately — the monitor
process keeps running and responds to future config changes without a restart.

The **response interval** controls how finely the internal sleep loop is sliced.
A smaller value means interval changes and Ctrl-C take effect faster; a larger
value reduces CPU overhead from wakeups.  The default 500 ms is a good balance
for most deployments.  The minimum enforced value is 50 ms.

The **UI auto-refresh** slider (0 – 60 s) controls how often the viewer panels
re-read the log files.  Set to `0` for manual-only refresh via the ⟳ buttons.

### Process viewer panel

Reads the latest `proc_resources.N.jsonl` log file and shows the current state
of all watched processes.

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

### go2rtc Streams panel

Reads the latest `go2rtc_streams.N.jsonl` and shows the last `stream_sample`.

| Column | Description |
|--------|-------------|
| Name | Stream name as configured in go2rtc |
| Status | **Active** (green) or **Inactive** (grey) |
| Consumers | Number of current viewers |
| Last seen | `HH:MM:SS` of the last `stream_sample` that included this stream |

Hovering over a stream name shows the producer URL (e.g. the RTSP source address).

### System Resources panel

Reads the latest `sys_resources.N.jsonl` and shows the last `system_resource_sample`:

- **CPU** — used % and free % with a colour-coded progress bar
- **RAM** — used / total MB and free % with a progress bar
- **Swap** — used / total MB with a progress bar
- **Network** — per-interface RX / TX MB/s, error count, and dropped packet count
- **Disks** — per-mount free GB, free % with a progress bar, and real-time read / write MB/s
- **GPU** — utilisation %, VRAM free MB, temperature °C, NVENC encoder % per card
  — **only visible when built with `--features nvidia`** (see [GPU monitoring](#gpu-monitoring-nvidia));
  the section is hidden entirely when the log contains no GPU data

Progress bars are green / yellow / red based on the default alert thresholds.

---

## Config (`monitor.config.json`)

All binaries read the same file.

```json
{
  "log_rotation": { ... },
  "ui": { ... },
  "monitors": {
    "process_monitor": { ... },
    "system_monitor":  { ... },
    "go2rtc_monitor":  { ... }
  }
}
```

### `log_rotation`

| Key | Default | Description |
|-----|---------|-------------|
| `max_file_size_mb` | 10 | Rotate when the active log file exceeds this size |
| `keep_files` | 5 | Number of rotated files to keep |

---

### `ui`

Settings persisted by `monitor-ui`.

| Key | Default | Description |
|-----|---------|-------------|
| `refresh_secs` | 5 | How often the UI re-reads log files (`0` = manual ⟳ only) |

---

### `monitors.process_monitor`

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Set `false` to skip on startup |
| `log_file` | `proc_resources.jsonl` | Base name for log files |
| `resource_poll_interval_ms` | 5000 | Per-process CPU / RAM sample frequency (`0` = off) |
| `snapshot_interval_ms` | 60000 | Full process-tree snapshot frequency (`0` = off) |
| `min_tick_ms` | 500 | Sleep granularity — how quickly the monitor reacts to interval changes or Ctrl-C (min 50) |
| `watch_folders` | required | Absolute paths — **every process whose executable path starts with one of these folders** is watched, regardless of executable name |
| `log.cpu_alert_threshold_percent` | null | WARN when a process exceeds this CPU % |
| `log.memory_alert_mb` | null | WARN when a process exceeds this RAM (MB) |

#### Process discovery

The monitor uses a **path-based** filter: any running process whose full executable
path begins with one of the `watch_folders` paths is automatically tracked — no
`.exe` file list needs to be maintained.

| Scenario | Behaviour |
|----------|-----------|
| New process starts from a watched folder | Detected on the next poll tick, logged as `process_spawned` |
| Process exits | Detected on the next poll tick, logged as `process_exited` |
| New `.exe` dropped into a watched folder and started | Picked up immediately — no config change or restart required |
| Process with same name running from a different folder | Ignored — the full path must match |
| `watch_folders` changed in config | Previous exclusion cache is cleared; all running processes are re-evaluated within `min_tick_ms` |

Each new PID is verified once via `QueryFullProcessImageNameW`.  Processes outside
the watched folders are cached in an exclusion list and never re-checked, keeping
CPU overhead near zero even on busy systems.

---

### `monitors.system_monitor`

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Set `false` to skip on startup |
| `log_file` | `sys_resources.jsonl` | Base name for log files |
| `poll_interval_ms` | 30000 | How often to sample (`0` = off; 30 s recommended) |
| `min_tick_ms` | 500 | Sleep granularity — see `process_monitor.min_tick_ms` |
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
| `log.network_drop_alert` | `true` | Any interface has discarded (dropped) packets |
| `log.gpu_warn_util_percent` | 80 | GPU utilisation > 80 % |
| `log.gpu_alert_util_percent` | 95 | GPU utilisation > 95 % |
| `log.gpu_encoder_warn_percent` | 80 | NVENC encoder > 80 % |
| `log.gpu_vram_warn_free_mb` | 500 | VRAM free < 500 MB |
| `log.gpu_vram_alert_free_mb` | 200 | VRAM free < 200 MB |
| `log.gpu_temp_warn_c` | 80 | GPU temperature > 80 °C |
| `log.gpu_temp_alert_c` | 90 | GPU temperature > 90 °C |

---

### `monitors.go2rtc_monitor`

Disabled by default — omitting the section entirely is equivalent to `"enabled": false`.

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | Set `true` to activate |
| `log_file` | `go2rtc_streams.jsonl` | Base name for log files |
| `api_url` | `http://localhost:1984` | go2rtc base URL |
| `poll_interval_ms` | 10000 | How often to poll the streams API (`0` = off) |
| `min_tick_ms` | 500 | Sleep granularity — see `process_monitor.min_tick_ms` |
| `log.stream_changes` | `true` | Log `stream_up` / `stream_down` on producer state changes |
| `log.consumer_changes` | `true` | Log `consumer_change` when viewer count changes |
| `log.stream_sample` | `true` | Log a full `stream_sample` on every poll |

Minimal example to enable go2rtc monitoring on a default install:

```json
"go2rtc_monitor": {
  "enabled": true
}
```

Full example with custom URL and logging options:

```json
"go2rtc_monitor": {
  "enabled": true,
  "api_url": "http://192.168.1.10:1984",
  "poll_interval_ms": 5000,
  "log": {
    "stream_changes":   true,
    "consumer_changes": true,
    "stream_sample":    false
  }
}
```

---

## Log file naming

Each monitor uses numbered rotation independently:

```
watchdog.0.jsonl           ← oldest
watchdog.1.jsonl           ← active

proc_resources.0.jsonl     ← oldest
proc_resources.1.jsonl
proc_resources.2.jsonl     ← active

sys_resources.0.jsonl
sys_resources.1.jsonl      ← active

go2rtc_streams.0.jsonl     ← active
```

Every file starts with a `monitor_start` entry.  Rotation entries include
`"continued_from"` so readers can follow the chain backwards.

---

## Event reference

All events share the same envelope:

```json
{
  "ts":      "2026-03-23T10:00:00.000Z",
  "monitor": "monitor_watchdog | process_monitor | system_monitor | go2rtc_monitor",
  "event":   "<event_name>",
  "level":   "INFO | WARN | ERROR",
  "...":     "event-specific fields"
}
```

---

### Shared events (all monitors)

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

### monitor-watchdog events

#### `child_started`
Emitted when a monitor is launched for the first time.
```json
{ "name": "process-monitor", "pid": 5120, "restart_count": 0 }
```

#### `child_restarted`
Emitted when a monitor is restarted after an unexpected exit.
```json
{ "name": "process-monitor", "pid": 5244, "restart_count": 1 }
```

#### `child_exited`
Emitted at WARN level when a monitor exits unexpectedly.
```json
{ "name": "process-monitor", "exit_code": 1, "uptime_seconds": 3724 }
```

#### `child_start_failed`
Emitted at ERROR level when the binary cannot be spawned (e.g. file not found).
```json
{ "msg": "failed to start process-monitor", "detail": "No such file or directory (os error 2)" }
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
      "rx_errors": 0, "tx_errors": 0,
      "rx_dropped": 0, "tx_dropped": 0 }
  ],
  "disks": [
    { "path": "C:\\", "total_gb": 476.84, "free_gb": 210.12, "free_percent": 44.06,
      "read_mb_per_sec": 12.5, "write_mb_per_sec": 3.2 }
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
| `network_drop_alert` | WARN | `Ethernet dropped: rx=12 tx=0` |
| `gpu_util_alert` | WARN / ERROR | `GPU RTX 4090 utilisation 96% above threshold 95%` |
| `gpu_vram_alert` | WARN / ERROR | `GPU RTX 4090 VRAM free 180 MB below threshold 200 MB` |
| `gpu_temp_alert` | WARN / ERROR | `GPU RTX 4090 temperature 91°C above threshold 90°C` |
| `gpu_encoder_alert` | WARN | `GPU RTX 4090 NVENC encoder 85% above threshold 80%` |

---

### go2rtc-monitor events

#### `stream_up`
Emitted when a stream's producer becomes active (or when a new active stream is first seen).
```json
{ "name": "front_yard", "producer_url": "rtsp://admin:pass@192.168.1.100/stream1" }
```

#### `stream_down`
Emitted at WARN level when a producer goes offline or a stream disappears from the API.
```json
{ "name": "front_yard" }
```

#### `consumer_change`
Emitted when the number of viewers for a stream changes.
```json
{ "name": "front_yard", "consumer_count": 2, "previous_count": 1 }
```

#### `stream_sample`
Written every `poll_interval_ms` — full snapshot of all streams.
```json
{
  "streams": [
    { "name": "front_yard", "producer_active": true,
      "producer_url": "rtsp://admin:pass@192.168.1.100/stream1", "consumer_count": 2 },
    { "name": "back_door",  "producer_active": false, "consumer_count": 0 }
  ],
  "total_count": 2,
  "active_count": 1
}
```

#### `api_error`
Emitted at WARN level when go2rtc is unreachable or returns an unparseable response.
The monitor keeps polling — it never exits due to API errors.
```json
{ "msg": "Cannot reach go2rtc API: http://localhost:1984/api/streams",
  "detail": "Connection refused (os error 111)" }
```

---

## GPU monitoring

Two backends are supported.  The monitor selects the best available one at startup
and logs which backend is active in the `system_info` event (`gpu_monitoring` field).

### Backend 1 — NVML (NVIDIA only, highest fidelity)

Build with `--features nvidia`:

```powershell
cargo build --release --features nvidia
```

NVML is part of the NVIDIA driver — no extra install required.

| Metric | Available |
|--------|-----------|
| GPU utilisation % | ✓ |
| Video encode % (NVENC) | ✓ |
| Video decode % (NVDEC) | ✓ |
| VRAM used / free / total | ✓ |
| Temperature | ✓ |
| Power draw | ✓ |

### Backend 2 — PDH + DXGI (cross-vendor, always compiled)

Automatically used when NVML is unavailable — no feature flag needed.
Works with **NVIDIA** (without driver extras), **AMD**, and **Intel** GPUs,
as long as a WDDM driver is installed (standard on Windows 10+).

| Metric | Available |
|--------|-----------|
| GPU 3D engine utilisation % | ✓ (Windows PDH) |
| Video encode % | ✓ (Windows PDH) |
| Video decode % | ✓ (Windows PDH) |
| VRAM used / total | ✓ (DXGI) |
| Temperature | ✗ (vendor SDK required) |
| Power draw | ✗ (vendor SDK required) |

In the UI and log files, temperature shows `—` / is omitted when the PDH backend is active.

### Startup log

Check the `system_info` event at startup to confirm which backend is running:

```powershell
Get-Content C:\monitor\sys_resources.*.jsonl |
  ConvertFrom-Json | Where-Object event -eq system_info |
  Select-Object -Last 1 | Select-Object gpu_monitoring, gpus
```

| `gpu_monitoring` value | Meaning |
|------------------------|---------|
| `"nvml"` | NVIDIA NVML — full metrics |
| `"pdh"` | Windows PDH + DXGI — cross-vendor metrics |
| `"none"` | No GPU detected or driver missing |

---

## Grep examples

```powershell
# Watchdog restart history — how often did each monitor need a restart?
Get-Content C:\monitor\watchdog.*.jsonl |
  ConvertFrom-Json | Where-Object event -eq child_restarted |
  Select-Object ts, name, restart_count

# All WARN and ERROR entries across all monitors
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

# Stream up/down history
Get-Content C:\monitor\go2rtc_streams.0.jsonl |
  ConvertFrom-Json |
  Where-Object { $_.event -in @("stream_up", "stream_down") } |
  Select-Object ts, event, name

# Current consumer counts across all streams
Get-Content C:\monitor\go2rtc_streams.0.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq stream_sample |
  Select-Object -Last 1 |
  ForEach-Object { $_.streams } |
  Select-Object name, producer_active, consumer_count

# All API errors (go2rtc unreachable)
Get-Content C:\monitor\go2rtc_streams.*.jsonl |
  ConvertFrom-Json | Where-Object event -eq api_error

# Network drop alerts (packet loss on any interface)
Get-Content C:\monitor\sys_resources.*.jsonl |
  ConvertFrom-Json | Where-Object event -eq network_drop_alert |
  Select-Object ts, msg
```
