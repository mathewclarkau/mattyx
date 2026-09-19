//! Issue #85: `wait-ready` post-spawn health check over the control
//! socket. A headless orchestrator spawns a pane and sends a command;
//! `wait-ready` blocks until BOTH a prompt (or detected agent) is on the
//! screen AND the pane PTY has a running process-tree child, or gives up
//! at `--timeout` with `ready:false` and a nonzero CLI exit.

#![cfg(unix)] // exercises unix PTY, /proc and AF_UNIX machinery

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mux_core::platform::transport;
use mux_core::{Mux, SurfaceOptions};
use serde_json::{json, Value};

fn unique_session(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

fn connect(path: &Path) -> Box<dyn transport::Stream> {
    transport::connect(path).unwrap()
}

/// One request → one response on a fresh connection (the `mtyx` CLI shape).
fn rpc(socket: &Path, request: Value) -> Value {
    let stream = connect(socket);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    // Generous read timeout: `wait-ready` may legitimately block for the
    // `timeout_ms` we pass.
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(20)));
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// AC1: a shell that prints a prompt and keeps a live child is reported
/// ready with spawn metadata (`child.pid`/`child.comm` + prompt_seen).
#[test]
fn wait_ready_reports_ready_with_child_after_send() {
    // `sh -c 'printf "$ "; sleep 30'` draws a `$ ` prompt (the generic
    // idle marker) and forks `sleep`, so both readiness halves hold.
    // The script stays as the PTY child; `sleep` is its descendant.
    let mux = Mux::new(
        unique_session("issue85-ready"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'ready$ '; sleep 30".to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    // The orchestrator's normal pre-flight: identify + send a command.
    let ident = rpc(&sock, json!({"id": 1, "cmd": "identify"}));
    assert_eq!(ident["ok"], json!(true), "identify failed: {ident}");
    let sent = rpc(&sock, json!({"id": 2, "cmd": "send", "surface": sid, "text": "echo hi"}));
    assert_eq!(sent["ok"], json!(true), "send failed: {sent}");

    let resp =
        rpc(&sock, json!({"id": 3, "cmd": "wait-ready", "surface": sid, "timeout_ms": 5000}));
    assert_eq!(resp["ok"], json!(true), "wait-ready failed: {resp}");
    let data = &resp["data"];
    assert_eq!(data["ready"], json!(true), "expected ready: {data}");
    assert_eq!(data["surface"], json!(sid));
    assert_eq!(data["prompt_seen"], json!(true), "prompt not seen: {data}");
    let child = &data["child"];
    assert!(child.is_object(), "expected a detected child: {data}");
    assert!(child["pid"].as_u64().unwrap_or(0) > 0, "child pid: {data}");
    assert!(!child["comm"].as_str().unwrap_or("").is_empty(), "child comm: {data}");
    assert!(data["elapsed_ms"].is_u64(), "elapsed_ms: {data}");

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// AC2: a pane whose command never draws a prompt is NOT ready; the
/// reply is still `ok:true` (structured) but `ready:false`, and the CLI
/// turns that into a nonzero exit (covered in mux-tui's unit tests).
#[test]
fn wait_ready_times_out_not_ready_for_wedged_pane() {
    // `sleep 30` as the direct PTY child: a live process-tree child
    // exists, but the screen never shows a prompt, so readiness must
    // stay false after the (short) timeout.
    let mux = Mux::new(
        unique_session("issue85-wedged"),
        SurfaceOptions {
            command: Some(vec!["/bin/sleep".to_string(), "30".to_string()]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    let start = std::time::Instant::now();
    let resp =
        rpc(&sock, json!({"id": 1, "cmd": "wait-ready", "surface": surface.id, "timeout_ms": 300}));
    assert_eq!(resp["ok"], json!(true), "wait-ready should be ok:true on timeout: {resp}");
    let data = &resp["data"];
    assert_eq!(data["ready"], json!(false), "wedged pane must not be ready: {data}");
    assert_eq!(data["prompt_seen"], json!(false), "no prompt expected: {data}");
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "should have waited for the timeout, took {:?}",
        start.elapsed()
    );

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// AC2 (killed child): once the PTY child is gone the pane can never be
/// ready — `child` is null even if a prompt was drawn earlier.
#[test]
fn wait_ready_child_null_after_child_exit() {
    // Print a prompt-like marker, then exit immediately: the screen may
    // still show `$ ` but the process tree is empty.
    let mux = Mux::new(
        unique_session("issue85-exit"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'done$ '; exit 0".to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    // Let the child exit and be reaped.
    std::thread::sleep(Duration::from_millis(400));
    let resp =
        rpc(&sock, json!({"id": 1, "cmd": "wait-ready", "surface": surface.id, "timeout_ms": 200}));
    let data = &resp["data"];
    assert_eq!(data["ready"], json!(false), "exited child must not be ready: {data}");
    assert_eq!(data["child"], Value::Null, "no child should be detected: {data}");

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}
