# Rust Learning Guide — Monitor Codebase

> For developers coming from C++, C#, Java, or Kotlin.
> Each section points to real code in this project — start there, not at tutorials.

---

## Part 1 — Language Fundamentals

Read these in order. Each stage builds on the previous one.

---

### Stage 1 — Structs, Enums, and Impl Blocks

Rust has no classes. Structs hold data, `impl` blocks hold methods — like a class split in two.

| What to read | Why |
|---|---|
| [src/config.rs:13-69](src/config.rs#L13-L69) | Nested structs with `#[derive]` attributes — like C# records with auto-generated behaviour |
| [src/events.rs:22-28](src/events.rs#L22-L28) | Simple enum — like Java/Kotlin enums |
| [src/sampler.rs:52-58](src/sampler.rs#L52-L58) | `impl` block with a constructor `new()` — Rust has no `new` keyword, it is just a convention |
| [src/writer.rs:36-46](src/writer.rs#L36-L46) | Struct with private fields — same visibility rules as C#/Java |
| [src/bin/filebeat.rs:120-140](src/bin/filebeat.rs#L120-L140) | `#[serde(default)]` on individual fields — lets you add new fields to a persisted JSON struct without breaking existing state files that predate the field |

---

### Stage 2 — Traits — The Interface System

Rust traits are like Java interfaces or Kotlin interfaces but more powerful.
They are also how you get auto-generated behaviour via `#[derive]`.

| What to read | Why |
|---|---|
| [src/lib.rs:26](src/lib.rs#L26) | `T: serde::Serialize` — trait bound, exactly like Java/Kotlin generics `<T extends X>` |
| [src/events.rs:43-52](src/events.rs#L43-L52) | `impl<T: Serialize> LogEntry` — implementing methods on a generic type |
| [src/pdh_disk.rs:93-97](src/pdh_disk.rs#L93-L97) | `impl Drop for PdhDiskMonitor` — like C++ destructors or C# `IDisposable` |

---

### Stage 3 — Pattern Matching

Rust's `match` is far more powerful than C#/Java `switch`. It is exhaustive — the compiler forces you to handle every case.

| What to read | Why |
|---|---|
| [src/lib.rs:89-111](src/lib.rs#L89-L111) | `match` on `Result<T,E>` — the most common pattern you will write |
| [src/discovery.rs:147](src/discovery.rs#L147) | `if let Some(info) = ...` — ergonomic way to unwrap an `Option` (like Kotlin's `?.let {}`) |
| [src/discovery.rs:146](src/discovery.rs#L146) | `for &(pid, ref name, thread_count) in &snapshot` — destructuring in a loop |

---

### Stage 4 — Ownership and Borrowing

This is the core concept unique to Rust. Everything else builds on it.

**The three rules:**
- Every value has exactly one **owner**
- You can have many **immutable** borrows (`&T`) OR one **mutable** borrow (`&mut T`) — never both at once
- When the owner goes out of scope the value is dropped automatically — no GC, no `delete`

| What to read | Why |
|---|---|
| [src/writer.rs:101-107](src/writer.rs#L101-L107) | `&mut self` vs `&self` — mutable borrow means only one caller at a time |
| [src/discovery.rs:96-103](src/discovery.rs#L96-L103) | Iterating `&self.walk_folders` — borrowing, not consuming |

**[src/bin/process_monitor.rs:128-144](src/bin/process_monitor.rs#L128-L144)** — `move` transfers ownership into the closure (like C++ `[=]` capture):
```rust
thread::spawn(move || { ... })
```

**[src/lib.rs:69](src/lib.rs#L69)** — closure moves `file_tx` in, so the thread owns it exclusively:
```rust
move |res| { let _ = file_tx.send(res); }
```

---

### Stage 5 — Lifetimes

Lifetimes are only required when the compiler cannot figure out how long a borrow lives.
Most of the time they are elided automatically.

| What to read | Why |
|---|---|
| [src/events.rs:32-41](src/events.rs#L32-L41) | `LogEntry<'a, T>` holding `&'a str` — the `'a` says "the string I borrow lives at least as long as this struct" |

---

### Stage 6 — Error Handling

Rust has no exceptions. Functions return `Result<T, E>`.
The `anyhow` crate (used throughout) simplifies this for applications.

| What to read | Why |
|---|---|
| [src/discovery.rs:81](src/discovery.rs#L81) | `?` operator — early return on error, like `try`/`throw` but explicit at the call site |
| [src/discovery.rs:92-96](src/discovery.rs#L92-L96) | `.ok()?` — converts `Result` to `Option` then propagates `None` |
| [src/writer.rs:234](src/writer.rs#L234) | `let _ = std::fs::remove_file(path)` — explicitly ignoring an error |
| [src/bin/filebeat.rs](src/bin/filebeat.rs) `save_state` | `let _ = std::fs::write(...)` — same pattern: state persistence is best-effort; a write failure is not worth crashing the monitor |

**[src/config.rs:42-47](src/config.rs#L42-L47)** — wrapping errors with context, like chaining exceptions:
```rust
.with_context(|| format!("failed to read config from {}", path.display()))
```

---

### Stage 7 — Closures and Iterators

This will feel familiar from Kotlin/Java streams and C# LINQ,
but closures interact with the ownership system.

| What to read | Why |
|---|---|
| [src/discovery.rs:132-135](src/discovery.rs#L132-L135) | `.keys().filter(...).copied().collect()` — chained adapters like LINQ |
| [src/bin/system_monitor.rs:733-760](src/bin/system_monitor.rs#L733-L760) | `msg_fn: impl Fn(f64, f64) -> String` — passing a function as a parameter |
| [src/bin/filebeat.rs](src/bin/filebeat.rs) `poll_sources` | `paths.flatten()` on a glob iterator — `.flatten()` discards `Err` entries from the iterator, leaving only successful path matches |
| [src/bin/filebeat.rs](src/bin/filebeat.rs) `poll_sources` | `state.files.entry(key).or_insert(...)` — `HashMap::entry` API: get the value if it exists, insert a default if it does not, all in one step. Like `ConcurrentDictionary.GetOrAdd()` in C# |

**[src/discovery.rs:161-163](src/discovery.rs#L161-L163)** — basic iterator with a closure predicate:
```rust
.iter().any(|f| path_lower.starts_with(f.as_str()))
```

**[src/bin/process_monitor.rs:275-286](src/bin/process_monitor.rs#L275-L286)** — closure transforms each item, like LINQ `.Select()`:
```rust
.map(|p| ProcessSnapshotEntry { ... })
```

---

### Stage 8 — Practical Patterns: Persistent State and Hashing

`src/bin/filebeat.rs` is a good self-contained example of patterns that appear repeatedly in
real-world Rust services.

**Persisting state to a JSON file** — save/load without a database:

```rust
fn load_state(log_dir: &Path) -> ForwarderState {
    std::fs::read_to_string(path)
        .ok()                                  // Result → Option (None on any I/O error)
        .and_then(|s| serde_json::from_str(&s).ok())   // parse; None on bad JSON
        .unwrap_or_default()                   // fall back to empty state
}
```

The chain `ok().and_then(...).unwrap_or_default()` is idiomatic Rust for
"try this, and if anything goes wrong just use the zero value" — no try/catch, no if-chains.
See [src/bin/filebeat.rs](src/bin/filebeat.rs) `load_state`.

**Content fingerprinting with `DefaultHasher`** — identifying a line of text cheaply:

```rust
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
```

`DefaultHasher` lives in `std` — no external crate needed for a simple content hash.
The `Hash` trait (auto-derived on most standard types) does the work; `Hasher` accumulates
the result.  See [src/bin/filebeat.rs](src/bin/filebeat.rs) `hash_str` and `find_line_by_hash`.

**Environment variable expansion** — `std::env::var` returns `Result<String, VarError>`, so
chaining `.or_else` lets you try fallback names without nesting:

```rust
std::env::var(var_name)
    .or_else(|_| std::env::var(var_name.to_uppercase()))
    .unwrap_or_else(|_| format!("%{var_name}%"))
```

See [src/bin/filebeat.rs](src/bin/filebeat.rs) `expand_env_vars`.

---

### Stage 10 — Concurrency

Rust's ownership system makes data races a **compile error**, not a runtime crash.

| What to read | Why |
|---|---|
| [src/bin/process_monitor.rs:93](src/bin/process_monitor.rs#L93) | `Arc::new(RwLock::new(cfg))` — `Arc` = `shared_ptr`, `RwLock` = multiple readers OR one writer |
| [src/bin/process_monitor.rs:157-162](src/bin/process_monitor.rs#L157-L162) | `Arc<AtomicBool>` — atomic flag for a shutdown signal shared across threads |
| [src/lib.rs:58-88](src/lib.rs#L58-L88) | Channel-based messaging — the preferred Rust concurrency pattern (like Go channels) |
| [src/bin/process_monitor.rs:123](src/bin/process_monitor.rs#L123) | `bounded::<String>(512)` — bounded channel for backpressure |

---

### Stage 11 — Macros

Two kinds: **declarative** (`macro_rules!`) and **procedural** (`#[derive(...)]`). The project uses both.

| What to read | Why |
|---|---|
| [src/lib.rs:38-45](src/lib.rs#L38-L45) | `macro_rules! cprint!` — declarative macro (like a type-safe `#define` in C++) |
| [src/config.rs:13-22](src/config.rs#L13-L22) | `#[derive(Debug, Clone, Serialize, Deserialize)]` — proc macros generating code at compile time |
| [src/events.rs:60-65](src/events.rs#L60-L65) | `#[serde(skip_serializing_if = "Option::is_none")]` — attribute macros controlling code generation |

---

### Stage 12 — Unsafe and FFI (Windows API)

The advanced section. Rust's safety guarantees are opt-out with `unsafe`.
This project uses it to call Windows APIs directly.

| What to read | Why |
|---|---|
| [src/console.rs:15-23](src/console.rs#L15-L23) | Minimal `unsafe` block — calling a single Windows API function |
| [src/sampler.rs:82-129](src/sampler.rs#L82-L129) | `unsafe fn` wrapping Windows system calls |
| [src/pdh_gpu.rs:228-235](src/pdh_gpu.rs#L228-L235) | `std::slice::from_raw_parts(...)` — raw pointer to slice, the classic unsafe operation |
| [src/pdh_gpu.rs:252-282](src/pdh_gpu.rs#L252-L282) | Manual `alloc` / `dealloc` — only needed when talking to C APIs that return raw buffers |
| [src/discovery.rs:216-218](src/discovery.rs#L216-L218) | `OsString::from_wide()` — converting Windows UTF-16 strings to Rust strings |

---

## Part 2 — Project Structure and Build Pipeline

---

### Stage 1 — One Package, Five Executables

In C#/.NET you would have multiple `.csproj` files and a solution.
In Kotlin/Java you would have Gradle modules.
In Rust a single `Cargo.toml` describes everything — one package, multiple outputs.

```toml
[lib]                              ← one shared library
name = "process_monitor"
path = "src/lib.rs"

[[bin]]                            ← double bracket = array item
name = "process-monitor"           ← produces process-monitor.exe
path = "src/bin/process_monitor.rs"

[[bin]]
name = "system-monitor"            ← produces system-monitor.exe
...
[[bin]]
name = "filebeat"                  ← produces filebeat.exe
path = "src/bin/filebeat.rs"
```

[Cargo.toml:8-30](Cargo.toml#L8-L30)

**The analogy:**
- `[lib]` = a `.dll` / `.jar` compiled once and linked into everything else
- `[[bin]]` = a separate `main()` entry point like a separate `Program.cs` — produces its own `.exe`

All executables and the library are built from the same source tree with one `cargo build`.
On non-Windows platforms, binaries that depend on Windows APIs are excluded automatically via
`required-features` — see Stage 5 for how this works.

---

### Stage 2 — The Shared Library: `src/lib.rs`

[src/lib.rs:1-15](src/lib.rs#L1-L15) is the root of the shared library.
It does not contain logic itself — it declares which modules are public:

```rust
pub mod config;     // ← makes src/config.rs part of the library
pub mod console;
pub mod events;
pub mod writer;
```

The keyword `pub` makes a module visible to outside consumers — exactly like `public` in C#/Java.
Without it the module exists but nothing outside the library can use it.

The library also exposes three shared utilities used by every binary:

| Symbol | Location | Purpose |
|---|---|---|
| `pub fn send<T>()` | [src/lib.rs:26-32](src/lib.rs#L26-L32) | Serialise and push to the writer channel |
| `macro_rules! cprint!` | [src/lib.rs:38-45](src/lib.rs#L38-L45) | Conditional console printing |
| `pub fn watch_config()` | [src/lib.rs:54-113](src/lib.rs#L54-L113) | Config hot-reload thread |

---

### Stage 3 — How a Binary Imports the Library

Any binary imports shared code by the crate name:

```rust
use process_monitor::config::Config;
```

[src/bin/monitor_ui.rs:13](src/bin/monitor_ui.rs#L13)

The crate name (`process_monitor`) comes from `name` in `[lib]` in [Cargo.toml:9](Cargo.toml#L9).
This is identical to a `using` statement in C# or `import` in Kotlin —
except the compiler verifies at link time that only `pub` items are accessible.

---

### Stage 4 — Private Modules via `#[path]`

Some modules are not in the shared library — they are private to a single binary.
The file is included directly into the binary's compilation unit:

```rust
#[path = "../discovery.rs"]
mod discovery;

#[path = "../sampler.rs"]
mod sampler;
```

[src/bin/process_monitor.rs:24-27](src/bin/process_monitor.rs#L24-L27)

**The analogy:** This is like `#include` in C++. The compiler treats `discovery.rs` as if it were written inline inside `process_monitor.rs`. No other binary gets access to it via this mechanism.

**Why this split?**
`discovery.rs` and `sampler.rs` use Windows-specific unsafe code.
Putting them in the shared lib would force `monitor_ui` (a GUI app) and `monitor` (a process supervisor) to compile code they do not need.

---

### Stage 5 — Dependencies and Feature Flags

[Cargo.toml:34-74](Cargo.toml#L34-L74) controls which external crates and binaries are compiled in.

**Platform-conditional dependencies:**

```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [...] }
```

[Cargo.toml:62-74](Cargo.toml#L62-L74)

This crate is only compiled on Windows. On Linux/macOS it does not exist at all.
This is the Rust equivalent of `#ifdef _WIN32` in C++ but applied at the package level.

**Optional features:**

```toml
[features]
default         = ["process_monitor", "monitor_ui"]
process_monitor = []   # Windows-only binary
monitor_ui      = []   # GUI binary — exclude on headless targets
nvidia          = ["nvml-wrapper"]
```

[Cargo.toml:38-43](Cargo.toml#L38-L43)

Features serve two roles here:

1. **Gating entire binaries** — `process-monitor` and `monitor-ui` each declare `required-features`
   ([Cargo.toml:15](Cargo.toml#L15), [Cargo.toml:24](Cargo.toml#L24)).
   Cargo refuses to build those binaries unless the matching feature is active.
   On Linux you pass `--no-default-features` and those binaries are simply excluded from the build.

2. **Gating optional dependencies** — `nvml-wrapper` is only compiled when you request it:

```
cargo build --release --features nvidia
```

Inside `system_monitor.rs` the code is guarded with `#[cfg(feature = "nvidia")]` at
[src/bin/system_monitor.rs:57-58](src/bin/system_monitor.rs#L57-L58).
Inside `monitor.rs` children are gated with `#[cfg(feature = "process_monitor")]` and
`#[cfg(feature = "monitor_ui")]` at [src/bin/monitor.rs:186-192](src/bin/monitor.rs#L186-L192).

**The analogy:** Features are like C# conditional compilation symbols (`#if NVIDIA`) declared
in the project file. `required-features` is like a Gradle `compileOnly` dependency — the
output is simply not produced if the condition isn't met.

---

### Stage 6 — The Build Script: `build.rs`

[build.rs](build.rs) is a Rust convention. If a file named `build.rs` exists at the project root,
Cargo compiles and runs it **before** compiling any crate.
Here it copies the config template into the output directories so the binaries are ready to run immediately after `cargo build`.

```rust
fn main() {
    println!("cargo:rerun-if-changed=config/monitor.config.json");
    // copies config to target/{profile}/logs/ and logs/
}
```

[build.rs:16-35](build.rs#L16-L35)

The `println!("cargo:rerun-if-changed=...")` line is a special protocol —
it tells Cargo to only re-run this script when that specific file changes.

**The analogy:** This is like an MSBuild `<Target BeforeTargets="Build">` task in `.csproj`,
or a Gradle task that runs before `compileKotlin`.

---

### The Full Picture

```
Cargo.toml
│
├── build.rs                      ← runs first, copies config files
│
├── src/lib.rs                    ← compiled as "process_monitor" crate (shared library)
│   ├── config.rs                 │  pub: all binaries can import
│   ├── events.rs                 │
│   ├── writer.rs                 │
│   └── console.rs                │
│
├── src/discovery.rs              ← NOT in the lib; included via #[path] into process_monitor.rs only
├── src/sampler.rs                ← same
├── src/pdh_disk.rs               ← same (used by system_monitor.rs)
├── src/pdh_gpu.rs                ← same
│
└── src/bin/
    ├── process_monitor.rs        → process-monitor  (feature: process_monitor — Windows only)
    ├── system_monitor.rs         → system-monitor   (all platforms)
    ├── monitor_ui.rs             → monitor-ui       (feature: monitor_ui — GUI targets)
    ├── go2rtc_monitor.rs         → go2rtc-monitor   (all platforms)
    ├── filebeat.rs               → filebeat         (all platforms)
    └── monitor.rs                → monitor          (all platforms)
```

**Key rule to internalize:** In Rust there is no "one project per executable".
One `Cargo.toml` = one package. Multiple outputs are multiple entry points compiled from the same
source tree, sharing one dependency graph and one compiler invocation.

---

## Suggested Reading Order

```
1.  src/config.rs              structs, traits, serde — pure safe Rust, no complex logic
2.  src/events.rs              enums, generics, lifetimes
3.  src/lib.rs                 macros, channels, closures, shared utilities
4.  src/writer.rs              ownership, mut borrowing, file I/O
5.  src/discovery.rs           iterators, pattern matching, error handling
6.  src/bin/go2rtc_monitor.rs  straightforward real-world binary: HTTP polling, channels, hot-reload
7.  src/bin/filebeat.rs        persistent JSON state, HashMap::entry, hashing, env-var expansion
8.  src/bin/process_monitor.rs threads, Arc/RwLock, channels
9.  src/bin/system_monitor.rs  large real-world integration of all the above
10. src/sampler.rs             unsafe, Windows FFI
11. src/pdh_disk.rs            advanced unsafe, Drop trait
12. src/pdh_gpu.rs             raw memory allocation, pointer arithmetic
```

Start with [src/config.rs](src/config.rs) — coming from C#/Kotlin it will feel immediately readable.
`src/bin/filebeat.rs` is a good second binary to read after `go2rtc_monitor.rs` — it introduces
persistent state and content hashing using only `std`, and all the logic is straightforward
sequential file I/O with no platform-specific code.
