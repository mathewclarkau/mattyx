//! Issue #92: durable per-pane notifications with per-client read state.
//!
//! End-to-end over the control socket: a notification emitted while NO
//! client is attached survives in the per-pane ring; a later `subscribe`
//! replays it as a `notification` event; `notify-ack` marks it read for
//! that client; a second `subscribe` with the same client id replays
//! nothing. The desktop-emission side channel (`MuxEvent::OscNotification`)
//! is untouched — see `pty.rs`'s existing OSC tests.

#![cfg(unix)] // exercises unix PTY and AF_UNIX machinery

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mux_core::platform::transport;
use mux_core::{Mux, SurfaceOptions};
use serde_json::{json, Value};

fn wait_for<T>(mut f: impl FnMut() -> Option<T>, timeout: Duration) -> Option<T> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn unique_session(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

fn shell_opts(script: &str) -> SurfaceOptions {
    SurfaceOptions {
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]),
        ..Default::default()
    }
}

fn connect(path: &Path) -> Box<dyn transport::Stream> {
    transport::connect(path).unwrap()
}

/// One request → one response on a fresh connection.
fn rpc(socket: &Path, request: Value) -> Value {
    let stream = connect(socket);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Read one JSON line, returning `None` on EOF or a read timeout (the
/// stream verbs' connections stay open, so a missing line is "nothing
/// arrived in the budget" rather than an error).
fn read_line(reader: &mut impl BufRead) -> Option<Value> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => serde_json::from_str(&line).ok(),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            None
        }
        Err(e) => panic!("socket read failed: {e}"),
    }
}

/// AC1: emit while nobody is attached → a later `subscribe --client A`
/// replays it → `notify-ack` → a second subscribe with A replays nothing,
/// while a different client B still sees it.
#[test]
fn durable_notification_replays_then_acks_suppress_replay() {
    let mux = Mux::new(
        unique_session("issue92-durable"),
        shell_opts("printf '\\033]9;Build failed\\007'; sleep 30"),
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    // Emitted while NO client is attached: wait until the ring holds it.
    let stored = wait_for(
        || {
            let records = mux.notifications_for(surface.id);
            (!records.is_empty()).then_some(records)
        },
        Duration::from_secs(10),
    )
    .expect("notification was not recorded in the pane ring");
    assert_eq!(stored.len(), 1);
    let record_id = stored[0].id;
    assert_eq!(stored[0].body, "Build failed");

    // First subscribe with client A: the record is replayed as a
    // `notification` event before the reply line.
    let stream = connect(&sock);
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(writer, "{}", json!({"id": 1, "cmd": "subscribe", "client": "cli-A"})).unwrap();
    let replayed = read_line(&mut reader).expect("expected a replayed notification event");
    assert_eq!(replayed["event"], json!("notification"), "got: {replayed}");
    assert_eq!(replayed["surface"], json!(surface.id));
    assert_eq!(replayed["id"], json!(record_id));
    assert_eq!(replayed["body"], json!("Build failed"));
    drop(writer);
    drop(reader);

    // Ack it for A.
    let ack =
        rpc(&sock, json!({"id": 2, "cmd": "notify-ack", "surface": surface.id, "client": "cli-A"}));
    assert_eq!(ack["ok"], json!(true), "ack failed: {ack}");
    assert_eq!(ack["data"]["acked"][0]["notification_id"], json!(record_id));
    assert_eq!(ack["data"]["unread"], json!(0), "nothing unread after ack: {ack}");

    // Second subscribe with A: the reply line arrives, and NO
    // `notification` event precedes or follows it within the budget.
    let stream2 = connect(&sock);
    stream2.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    let mut writer2 = stream2.try_clone_box().unwrap();
    let mut reader2 = BufReader::new(stream2);
    writeln!(writer2, "{}", json!({"id": 3, "cmd": "subscribe", "client": "cli-A"})).unwrap();
    let mut saw_notification = false;
    while let Some(value) = read_line(&mut reader2) {
        assert_ne!(
            value["event"],
            json!("notification"),
            "acked client must not be replayed: {value}"
        );
        if value.get("ok").is_some() {
            break;
        }
        saw_notification = true;
    }
    assert!(!saw_notification, "unexpected pre-reply events after ack");

    // A DIFFERENT client B has its own read state and still sees it.
    let stream3 = connect(&sock);
    stream3.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut writer3 = stream3.try_clone_box().unwrap();
    let mut reader3 = BufReader::new(stream3);
    writeln!(writer3, "{}", json!({"id": 4, "cmd": "subscribe", "client": "cli-B"})).unwrap();
    let replayed_b = read_line(&mut reader3).expect("client B should still see the record");
    assert_eq!(replayed_b["event"], json!("notification"));
    assert_eq!(replayed_b["id"], json!(record_id));

    mux_core::server::cleanup(&sock);
}

/// AC1 (ack shape): `notify-ack` with no surface acks every pane with
/// stored notifications, and a re-ack is a no-op.
#[test]
fn notify_ack_all_is_idempotent() {
    let mux = Mux::new(unique_session("issue92-ack-all"), SurfaceOptions::default());
    let surface = mux.new_workspace(None, None).unwrap();
    mux.record_notification(surface.id, String::new(), "one".into());
    mux.record_notification(surface.id, String::new(), "two".into());
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    let first = rpc(&sock, json!({"id": 1, "cmd": "notify-ack", "client": "z"}));
    assert_eq!(first["ok"], json!(true));
    assert_eq!(first["data"]["acked"][0]["surface"], json!(surface.id));
    assert_eq!(first["data"]["unread"], json!(0));

    let second = rpc(&sock, json!({"id": 2, "cmd": "notify-ack", "client": "z"}));
    assert_eq!(second["data"]["acked"].as_array().unwrap().len(), 0, "re-ack is a no-op");

    mux_core::server::cleanup(&sock);
}
