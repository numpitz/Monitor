# Monitor suite

Rust 2024 · Windows 10+ · two standalone `.exe` files

Two companion monitors that share one `monitor.config.json` and write their
own NDJSON log files independently.  Run them side-by-side to get a complete
picture of whether a video server (go2rtc) has enough resources to operate:

| Binary | What it watches | Log file |
|--------|-----------------|----------|
| `process-monitor.exe` | Specific processes: spawn/exit events, per-process CPU & RAM | `proc_resources.jsonl` |
| `system-monitor.exe`  | System-wide headroom: free CPU, available RAM, free disk space | `sys_resources.jsonl` |

---

## Build

```
# Both binaries in one command
cargo build --release --target x86_64-pc-windows-msvc
```

Outputs:

```
target\release\process-monitor.exe
target\release\system-monitor.exe
```

No runtime dependencies — each ships as a single file.

---

## Usage

Both binaries accept the same arguments:

```
# With a console window (default)
process-monitor.exe C:\monitor\
system-monitor.exe  C:\monitor\

# Detach from console — no window (background / supervisor mode)
process-monitor.exe C:\monitor\ --no-console
system-monitor.exe  C:\monitor\ --no-console
```

`C:\monitor\` must contain `monitor.config.json`.
All log files are written into the same directory.

---

## Config (`monitor.config.json`)

Both monitors read the same file.  Each has its own section under `monitors`
so their intervals and thresholds are configured independently.

```json
{
  "log_rotation": {
    "max_file_size_mb": 10,
    "keep_files": 5
  },
  "monitors": {
    "process_monitor": { ... },
    "system_monitor":  { ... }
  }
}
```

Config changes are picked up automatically by both running monitors — no
restart needed.

### Shared: `log_rotation`

| Key | Default | Description |
|-----|---------|-------------|
| `max_file_size_mb` | 10 | Rotate the active log file when it exceeds this size |
| `keep_files` | 5 | Number of rotated files to keep (oldest are deleted) |

### `monitors.process_monitor`

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Set to `false` to skip this monitor on startup |
| `log_file` | `proc_resources.jsonl` | Base name for log files in the log dir |
| `resource_poll_interval_ms` | 5000 | How often to sample per-process CPU/RAM |
| `snapshot_interval_ms` | 60000 | How often to write a full process-tree snapshot |
| `watch_folders` | required | Absolute paths — every `.exe` found here is watched |
| `log.cpu_alert_threshold_percent` | null | Emit `cpu_alert` when a process exceeds this % |
| `log.memory_alert_mb` | null | Emit `memory_alert` when a process exceeds this MB |

### `monitors.system_monitor`

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Set to `false` to skip this monitor on startup |
| `log_file` | `sys_resources.jsonl` | Base name for log files in the log dir |
| `poll_interval_ms` | 30000 | How often to sample system resources (30 s recommended) |
| `watch_disks` | `[]` (all) | Mount points to report, e.g. `["C:\\"]`. Empty = all disks |
| `log.cpu_alert_free_percent` | null | Emit `cpu_headroom_alert` when free CPU drops below this % |
| `log.memory_alert_free_mb` | null | Emit `memory_headroom_alert` when available RAM drops below this MB |
| `log.disk_alert_free_gb` | null | Emit `disk_headroom_alert` when free space drops below this GB |

---

## Log file naming

Each monitor uses the same numbered-rotation scheme:

```
proc_resources.0.jsonl   ← oldest
proc_resources.1.jsonl
proc_resources.2.jsonl   ← active (highest number = current)

sys_resources.0.jsonl
sys_resources.1.jsonl    ← active
```

Each file starts with a `monitor_start` entry.  Rotation entries include
`"continued_from"` so readers can follow the chain backwards.

---

## Event reference

All events share the same envelope:

```json
{
  "ts":      "<ISO-8601 UTC>",
  "monitor": "process_monitor | system_monitor",
  "event":   "<event_name>",
  "level":   "INFO | WARN | ERROR",
  ... event-specific fields ...
}
```

### Shared events (both monitors)

#### `monitor_start`
Written on startup and after every log rotation.

```json
{ "pid": 4821, "log_file": "proc_resources.2.jsonl", "rotation": false }
```

#### `monitor_stop`
Written on clean shutdown before process exit.

```json
{ "pid": 4821, "reason": "shutdown", "exit_code": 0 }
```

#### `config_reloaded`
Config file was changed and successfully reloaded.

```json
{ "path": "C:\\monitor\\monitor.config.json" }
```

---

### process-monitor events

#### `process_spawned`
A new `.exe` appeared inside a watch folder.

```json
{ "pid": 1235, "name": "ffmpeg.exe", "exe_path": "C:\\go2rtc\\bin\\ffmpeg.exe" }
```

#### `process_exited`
A known process disappeared from the process list.

```json
{ "pid": 1235, "name": "ffmpeg.exe", "uptime_seconds": 1195 }
```

#### `resource_sample`
Written every `resource_poll_interval_ms`.

```json
{
  "processes": [
    {
      "pid": 1234, "name": "go2rtc.exe",
      "cpu_percent": 12.3, "memory_mb": 84.2,
      "handles": 312, "threads": 18
    }
  ],
  "total_cpu_percent": 12.3,
  "total_memory_mb":   84.2
}
```

#### `process_tree_snapshot`
Written every `snapshot_interval_ms`.

```json
{
  "count": 1,
  "processes": [
    {
      "pid": 1234, "name": "go2rtc.exe",
      "exe_path": "C:\\go2rtc\\go2rtc.exe",
      "started_at": "2026-03-20T09:45:00.000Z",
      "threads": 18, "memory_mb": 0.0
    }
  ]
}
```

#### `cpu_alert` / `memory_alert`
Per-process threshold exceeded — emitted once per offending sample.

```json
{ "msg": "go2rtc.exe cpu=91.2% exceeds threshold 80%" }
```

---

### system-monitor events

#### `system_resource_sample`
Written every `poll_interval_ms`.  Shows what is *available* to new workloads.

```json
{
  "cpu_used_percent":    34.5,
  "cpu_free_percent":    65.5,
  "memory_total_mb":     16384.0,
  "memory_used_mb":      9200.0,
  "memory_free_mb":      7184.0,
  "memory_free_percent": 43.8,
  "disks": [
    {
      "path": "C:\\",
      "total_gb":     476.84,
      "free_gb":      210.12,
      "free_percent": 44.06
    }
  ]
}
```

#### `cpu_headroom_alert`
Free CPU headroom fell below `log.cpu_alert_free_percent`.

```json
{ "msg": "CPU headroom 14.3% below threshold 20%" }
```

#### `memory_headroom_alert`
Available RAM fell below `log.memory_alert_free_mb`.

```json
{ "msg": "free RAM 412 MB below threshold 500 MB" }
```

#### `disk_headroom_alert`
Free disk space on a watched drive fell below `log.disk_alert_free_gb`.

```json
{ "msg": "disk C:\\ free 8.3 GB below threshold 10 GB" }
```

---

## Grep examples

```powershell
# All WARN/ERROR entries across both monitors
Get-ChildItem C:\monitor\*.jsonl | Get-Content |
  ConvertFrom-Json | Where-Object level -ne INFO

# Free CPU headroom over the last hour (system-monitor)
Get-Content C:\monitor\sys_resources.*.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq system_resource_sample |
  Select-Object ts, cpu_free_percent |
  Select-Object -Last 120   # 120 × 30 s = 1 hour

# Average available RAM over last 10 samples
Get-Content C:\monitor\sys_resources.0.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq system_resource_sample |
  Select-Object -Last 10 |
  ForEach-Object { $_.memory_free_mb } |
  Measure-Object -Average

# All processes spawned today (process-monitor)
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
