//! Windows console plumbing shared by the terminal probes
//! (`host_colors.rs`, `ui/graphics.rs`): timed stdin reads and VT-mode
//! detection. Everything here is cfg(windows); the unix equivalents use
//! poll(2) and are inlined at their call sites.
//!
//! Residual limitations, honestly stated:
//!
//! * **ConPTY assumption.** Under ConPTY stdin/stdout are pipes, which
//!   `WaitForSingleObject` can wait on. On a *legacy console* the std
//!   handles are console buffers, which are also waitable, but replies
//!   to escape-sequence queries may be delivered as cooked input lines
//!   rather than raw bytes — the probes then see nothing and fall back
//!   to defaults, never hang.
//! * **No mode changes.** The probes deliberately never call
//!   `SetConsoleMode` (crossterm owns terminal state); they only read
//!   what the current mode yields.

#![cfg(windows)]

use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

/// Is stdout attached to a console with VT processing enabled (or are
/// we inside Windows Terminal, detected via `WT_SESSION`)? Used to
/// decide whether escape-sequence queries can expect a reply at all.
pub fn stdout_is_vt_capable() -> bool {
    if std::env::var_os("WT_SESSION").is_some() {
        return true;
    }
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() {
            return false;
        }
        let mut mode: CONSOLE_MODE = 0;
        GetConsoleMode(handle, &mut mode) != 0 && mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
    }
}

/// Write bytes to stdout (best-effort; probe queries only).
pub fn write_stdout(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Read raw bytes from stdin until `timeout` elapses or `stop(bytes)`
/// says the awaited reply has arrived. Non-blocking between waits:
/// `WaitForSingleObject(.., 0)`-style polling in a 20 ms cadence, the
/// moral equivalent of the unix poll(2) loops. Never blocks past the
/// deadline.
pub fn read_stdin_until(timeout: Duration, stop: &dyn Fn(&[u8]) -> bool) -> Vec<u8> {
    let stdin = std::io::stdin();
    let handle = stdin.as_raw_handle();
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_ms = remaining.as_millis().min(20).min(u32::MAX as u128) as u32;
        let signaled = unsafe { WaitForSingleObject(handle, wait_ms) };
        if signaled == WAIT_TIMEOUT {
            continue;
        }
        if signaled != WAIT_OBJECT_0 {
            // Abandoned/error: treat as end-of-input.
            break;
        }
        let mut buf = [0u8; 1024];
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
        out.extend_from_slice(&buf[..read as usize]);
        if stop(&out) {
            break;
        }
    }
    out
}
