//! System clipboard helpers for the TUI.
//!
//! mtyx only *writes* the host clipboard via OSC 52. Reads shell out to
//! `wl-paste` (Wayland) or `xclip` (X11) so we can paste into browser panes
//! and inject clipboard images into PTY panes (issue #30 — Claude Code's
//! own clipboard read often fails inside nested terminal multiplexers).
//!
//! Clipboard child processes must never inherit the TUI's stdout/stderr
//! (issue #61): tools like `wl-copy` print multi-line diagnostics when
//! Wayland is unavailable, and those bytes corrupt the alternate-screen
//! frame so the session cannot be redrawn cleanly.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Read text from the desktop clipboard. Returns `None` if no tool is
/// available or the clipboard is empty/unreadable.
pub fn read_text() -> Option<String> {
    let bytes = read_clipboard_bytes(
        &["wl-paste", "--no-newline"],
        &["xclip", "-selection", "clipboard", "-o"],
    )?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.is_empty()).then_some(text)
}

/// Write text to the desktop clipboard via a system tool (`wl-copy` for
/// Wayland, `xclip` for X11). Returns true if a tool succeeded.
///
/// This is a fallback for when OSC 52 (the terminal-protocol clipboard
/// write) doesn't reach the host terminal — e.g. when mtyx is nested
/// inside another terminal multiplexer, run over SSH, or the host
/// terminal has `clipboard-write = deny`. OSC 52 is still tried first
/// by the caller (it works over SSH when the host allows it); this
/// function is the local-system safety net.
///
/// Child stdout/stderr are discarded so a failing tool cannot paint over
/// the TUI (issue #61). Tools that are clearly wrong for the session
/// (e.g. `wl-copy` with no Wayland display) are skipped entirely.
pub fn write_text(text: &str) -> bool {
    for tool in write_tools_for_session() {
        if try_write_with(tool, text) {
            return true;
        }
    }
    false
}

/// Read a PNG image from the desktop clipboard, if one is present.
pub fn read_image_png() -> Option<Vec<u8>> {
    // Prefer explicit image MIME types so we don't pull text as "image".
    if let Some(bytes) = read_clipboard_bytes(
        &["wl-paste", "--type", "image/png"],
        &["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
    ) {
        if looks_like_png(&bytes) {
            return Some(bytes);
        }
    }
    // Some compositors only expose image/jpeg or omit type filters.
    if let Some(bytes) = read_clipboard_bytes(
        &["wl-paste", "--type", "image/jpeg"],
        &["xclip", "-selection", "clipboard", "-t", "image/jpeg", "-o"],
    ) {
        if !bytes.is_empty() {
            return Some(bytes);
        }
    }
    None
}

/// Write `png` bytes to a unique temp file under the runtime dir and return
/// its absolute path. Caller is responsible for eventually cleaning up
/// (OS temp cleanup is fine for short-lived paste files).
pub fn write_paste_image(png_or_image: &[u8]) -> Option<PathBuf> {
    let dir = paste_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos();
    // Use a real image extension: agents (Claude Code) detect attachments by
    // file extension, so a `.img` file would not be recognised as an image.
    // The only non-PNG source is the `image/jpeg` clipboard branch, so any
    // non-PNG payload here is JPEG.
    let ext = if looks_like_png(png_or_image) { "png" } else { "jpg" };
    let path = dir.join(format!("paste-{}-{}.{}", std::process::id(), nanos, ext));
    std::fs::write(&path, png_or_image).ok()?;
    Some(path)
}

/// If the clipboard holds an image, materialize it and return a paste
/// payload Claude Code / agents understand: `@/abs/path.png`.
/// Returns `None` when no image is available (caller should fall through
/// to normal Ctrl+V / text paste).
pub fn image_paste_payload() -> Option<String> {
    let bytes = read_image_png()?;
    let path = write_paste_image(&bytes)?;
    Some(format!("@{}", path.display()))
}

fn paste_dir() -> Option<PathBuf> {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Some(Path::new(&runtime).join("mattyx").join("pastes"));
        }
    }
    Some(std::env::temp_dir().join("mtyx-pastes"))
}

fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Clipboard write backends, ordered for the current session.
///
/// Preference is driven by display env vars so we do not spawn a Wayland
/// tool on a pure X11 session (Linux Mint/Cinnamon, many Ubuntu installs)
/// and dump its connection error into the TUI.
fn write_tools_for_session() -> Vec<ClipboardWriteTool> {
    write_tools_for_env(env_is_set("WAYLAND_DISPLAY"), env_is_set("DISPLAY"))
}

fn write_tools_for_env(wayland: bool, x11: bool) -> Vec<ClipboardWriteTool> {
    let mut tools = Vec::with_capacity(2);
    match (wayland, x11) {
        (true, false) => tools.push(ClipboardWriteTool::WlCopy),
        (false, true) => tools.push(ClipboardWriteTool::Xclip),
        (true, true) => {
            // Nested/mixed sessions (e.g. XWayland): try Wayland first.
            tools.push(ClipboardWriteTool::WlCopy);
            tools.push(ClipboardWriteTool::Xclip);
        }
        (false, false) => {
            // Headless/SSH with no display: still try both silently so a
            // forwarded or late-set display has a chance; failures stay
            // quiet (stdout/stderr nulled).
            tools.push(ClipboardWriteTool::WlCopy);
            tools.push(ClipboardWriteTool::Xclip);
        }
    }
    tools
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardWriteTool {
    WlCopy,
    Xclip,
}

fn try_write_with(tool: ClipboardWriteTool, text: &str) -> bool {
    let mut cmd = match tool {
        ClipboardWriteTool::WlCopy => Command::new("wl-copy"),
        ClipboardWriteTool::Xclip => {
            let mut c = Command::new("xclip");
            c.args(["-selection", "clipboard"]);
            c
        }
    };
    // Critical: never inherit the TUI's terminal fds. wl-copy prints
    // "Failed to connect to a Wayland server…" on stderr when
    // WAYLAND_DISPLAY is wrong/missing; that corrupts the alt-screen
    // frame and leaves the session unusable until restart (issue #61).
    let mut child =
        match cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(c) => c,
            Err(_) => return false,
        };
    // Close stdin after writing so tools that read until EOF (wl-copy,
    // xclip) exit instead of hanging the TUI on child.wait().
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        // stdin dropped here → EOF
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Try Wayland args first when a Wayland display is present, otherwise
/// prefer X11, then fall back. Both branches use `output()` so child
/// stderr is captured rather than painted onto the TUI.
fn read_clipboard_bytes(wl_args: &[&str], x_args: &[&str]) -> Option<Vec<u8>> {
    let wayland = env_is_set("WAYLAND_DISPLAY");
    let x11 = env_is_set("DISPLAY");
    match (wayland, x11) {
        (true, false) => run_capture(wl_args).or_else(|| run_capture(x_args)),
        (false, true) => run_capture(x_args),
        _ => run_capture(wl_args).or_else(|| run_capture(x_args)),
    }
}

fn run_capture(args: &[&str]) -> Option<Vec<u8>> {
    if args.is_empty() {
        return None;
    }
    // `output()` already captures stdout+stderr into memory, so a failing
    // wl-paste cannot corrupt the TUI the way write_text used to.
    let output = Command::new(args[0]).args(&args[1..]).output().ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_png_detects_signature() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0u8; 16]);
        assert!(looks_like_png(&bytes));
        assert!(!looks_like_png(b"not a png"));
        assert!(!looks_like_png(b""));
    }

    #[test]
    fn write_paste_image_roundtrips() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"fake-png-body");
        let path = write_paste_image(&bytes).expect("write");
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn image_paste_payload_none_when_no_image_tool_or_empty() {
        // Without a real image on the clipboard this returns None; we only
        // assert it does not panic.
        let _ = image_paste_payload();
    }

    #[test]
    fn pure_x11_session_skips_wl_copy() {
        // Linux Mint / Cinnamon default: DISPLAY set, WAYLAND_DISPLAY unset.
        // Spawning wl-copy here is what painted the error over the TUI in #61.
        assert_eq!(write_tools_for_env(false, true), vec![ClipboardWriteTool::Xclip]);
    }

    #[test]
    fn pure_wayland_session_skips_xclip_first() {
        assert_eq!(write_tools_for_env(true, false), vec![ClipboardWriteTool::WlCopy]);
    }

    #[test]
    fn mixed_session_prefers_wayland_then_x11() {
        assert_eq!(
            write_tools_for_env(true, true),
            vec![ClipboardWriteTool::WlCopy, ClipboardWriteTool::Xclip]
        );
    }

    #[test]
    fn headless_session_tries_both_silently() {
        assert_eq!(
            write_tools_for_env(false, false),
            vec![ClipboardWriteTool::WlCopy, ClipboardWriteTool::Xclip]
        );
    }

    #[test]
    fn write_text_does_not_panic_without_tools() {
        // On a CI/headless box neither tool may work; must not panic or
        // write to the terminal.
        let _ = write_text("issue-61 regression probe");
    }
}
