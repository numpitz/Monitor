# process-monitor

Rust 2024 · Windows 10+ · standalone `.exe`

Monitors processes found in configured watch folders and writes structured
NDJSON logs with resource metrics, spawn/exit events, and periodic tree
snapshots.

---

## Build

```
cargo build --release --target x86_64-pc-windows-msvc
```

The resulting binary is at `target/release/process-monitor.exe`.  
No runtime dependencies — ships as a single file.

---

## Usage

```
# With a console window (default)
process-monitor.exe C:\monitor\

# Detach from console — no window (background / supervisor mode)
process-monitor.exe C:\monitor\ --no-console
```

`C:\monitor\` must contain `monitor.config.json`.  
All log files are written into the same directory.

---

## Config (`monitor.config.json`)

| Key | Default | Description |
|-----|---------|-------------|
| `log_rotation.max_file_size_mb` | 10 | Rotate when file exceeds this size |
| `log_rotation.keep_files` | 5 | Number of rotated files to keep |
| `monitors.process_monitor.resource_poll_interval_ms` | 5000 | Resource sample frequency |
| `monitors.process_monitor.snapshot_interval_ms` | 60000 | Full tree snapshot frequency |
| `monitors.process_monitor.watch_folders` | required | Absolute paths to monitor |
| `monitors.process_monitor.log.cpu_alert_threshold_percent` | null | Emit alert above this % |
| `monitors.process_monitor.log.memory_alert_mb` | null | Emit alert above this MB |

Config changes are picked up automatically — no restart needed.

---

## Log file naming

```
proc_resources.0.jsonl   ← oldest
proc_resources.1.jsonl
proc_resources.2.jsonl   ← active (highest number = current)
```

Each file starts with a `monitor_start` entry.  Rotation entries include
`"continued_from"` so readers can follow the chain backwards.

---

## Event reference

All events share the same envelope:

```json
{
  "ts":      "<ISO-8601 UTC>",
  "monitor": "process_monitor",
  "event":   "<event_name>",
  "level":   "INFO | WARN | ERROR",
  ... event-specific fields ...
}
```

### `monitor_start`
Written on startup and after every log rotation.

```json
{
  "pid": 4821,
  "log_file": "proc_resources.2.jsonl",
  "rotation": false,
  "continued_from": null
}
```

### `monitor_stop`
Written on clean shutdown before process exit.

```json
{ "pid": 4821, "reason": "shutdown", "exit_code": 0 }
```

### `process_spawned`
A new `.exe` appeared inside a watch folder.

```json
{ "pid": 1235, "name": "ffmpeg.exe", "exe_path": "C:\\go2rtc\\bin\\ffmpeg.exe" }
```

### `process_exited`
A known process disappeared from the process list.

```json
{ "pid": 1235, "name": "ffmpeg.exe", "uptime_seconds": 1195 }
```

### `resource_sample`
Written every `resource_poll_interval_ms`.

```json
{
  "processes": [
    {
      "pid": 1234, "name": "go2rtc.exe",
      "cpu_percent": 12.3, "memory_mb": 84.2,
      "handles": 312, "threads": 18
    },
    {
      "pid": 1235, "name": "ffmpeg.exe",
      "cpu_percent": 34.1, "memory_mb": 121.5,
      "handles": 88, "threads": 6
    }
  ],
  "total_cpu_percent": 46.4,
  "total_memory_mb":   205.7
}
```

### `process_tree_snapshot`
Written every `snapshot_interval_ms`.

```json
{
  "count": 2,
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

### `cpu_alert` / `memory_alert`
Threshold exceeded — emitted once per offending sample.

```json
{ "msg": "go2rtc.exe cpu=91.2% exceeds threshold 80%" }
```

### `config_reloaded`
Config file was changed and successfully reloaded.

```json
{ "path": "C:\\monitor\\monitor.config.json" }
```

---

## Grep examples

```powershell
# All errors
Get-Content C:\monitor\proc_resources.0.jsonl |
  ConvertFrom-Json | Where-Object level -eq ERROR

# All spawned processes today
Get-Content C:\monitor\proc_resources.*.jsonl |
  ConvertFrom-Json | Where-Object event -eq process_spawned

# Average CPU over last 50 samples
Get-Content C:\monitor\proc_resources.0.jsonl |
  ConvertFrom-Json |
  Where-Object event -eq resource_sample |
  Select-Object -Last 50 |
  ForEach-Object { $_.total_cpu_percent } |
  Measure-Object -Average
```
