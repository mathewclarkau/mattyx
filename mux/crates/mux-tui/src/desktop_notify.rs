//! Forwards an OSC 9 / OSC 777 / kitty desktop notification (detected by
//! mux-core, see `MuxEvent::OscNotification`) to the host desktop via
//! `notify-send`, the de facto Linux standard (part of libnotify).
//!
//! Fire-and-forget from the render loop's perspective: spawned without the
//! caller waiting so a slow or missing `notify-send` can never stall the
//! render loop, and a missing binary is silently ignored rather than shown
//! as an error (not every machine runs a notification daemon, e.g. over SSH
//! without X/Wayland forwarding). The spawned child is still reaped, just
//! off a background thread (never the render loop) - dropping a
//! `std::process::Child` does NOT reap it, and every unreaped `notify-send`
//! becomes a permanent zombie for the lifetime of the mtyx process. Verified
//! live: a long-running session with repeated OSC 9 notifications
//! accumulated one zombie per notification, unbounded, which is a
//! reasonable suspect for "crashes if left open a while" reports.

use std::process::{Command, Stdio};

pub fn send(pane_label: &str, title: &str, body: &str) {
    let summary =
        if title.is_empty() { pane_label.to_string() } else { format!("{pane_label}: {title}") };
    let child = Command::new("notify-send")
        .arg("--app-name=mtyx")
        .arg(&summary)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        // Reap on a throwaway thread, never the render loop. One thread per
        // notification is fine at the rate real notifications occur (a
        // human or agent triggering desktop alerts); it's still bounded and
        // self-cleaning, unlike the zombie it replaces.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
