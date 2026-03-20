//! Console-window management.
//!
//! The binary is compiled as a *console subsystem* application so it
//! shows a CMD window by default.  Passing `--no-console` at startup
//! calls `FreeConsole()` to detach from the console before any output
//! is produced, making it behave like a background process.
//!
//! The detachment is imperceptible (no visible flash) when launched
//! via the supervisor, Task Scheduler, or a shortcut with
//! "Run: minimised".

/// Detach from the console window.
/// After this call `println!` and `eprintln!` write nowhere — all
/// output must go to the log file.
pub fn detach() {
    #[cfg(windows)]
    unsafe {
        // Ignore the result — if there is no console to detach from
        // (e.g. launched from a GUI parent), FreeConsole returns false
        // but that is harmless.
        let _ = windows::Win32::System::Console::FreeConsole();
    }
}
