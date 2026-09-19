//! Issue #93: agent lifecycle contract — blocked-send gating and
//! observed-transition waits.
//!
//! Two halves:
//!
//! * `send` to a pane whose effective agent state is `Blocked` (an
//!   explicit hook/socket report, or a screen-derived `Detected`
//!   classification) is refused with the structured `agent_blocked`
//!   error and writes NOTHING to the PTY. `force: true` bypasses it.
//! * `wait-agent-status` with `require_transition` only succeeds on an
//!   OBSERVED state change at/after the call, not on a cached state that
//!   already matched.

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

fn connect(path: &Path) -> Box<dyn transport::Stream> {
    transport::connect(path).unwrap()
}

/// One request → one response on a fresh connection (the `mtyx` CLI shape).
fn rpc(socket: &Path, request: Value) -> Value {
    let stream = connect(socket);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    // Waits may legitimately block; budget generously.
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(20)));
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn screen_text(surface: &mux_core::Surface) -> String {
    surface.with_terminal(|t| t.plain_text()).unwrap().unwrap()
}

/// AC1: a pane explicitly reported `blocked` refuses `send` with the
/// structured `agent_blocked` error and a `code` field, and NOTHING is
/// written to the PTY (the pane's screen is byte-identical afterwards).
#[test]
fn send_to_blocked_agent_returns_agent_blocked_no_input() {
    // A read loop that echoes every consumed line as `GOT:<line>`: if the
    // gated send had written anything, `GOT:` would appear.
    let mux = Mux::new(
        unique_session("issue93-gate"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; printf 'ready$ '; while IFS= read -r line; do echo \"GOT:$line\"; done"
                    .to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    // Wait for the read loop to be live (prompt drawn / stty ran).
    wait_for(|| screen_text(&surface).contains("ready$").then_some(()), Duration::from_secs(5))
        .expect("pane prompt never appeared");

    // Park the pane in a blocked state via an explicit report.
    let report = rpc(
        &sock,
        json!({"id": 1, "cmd": "report-agent", "surface": sid, "state": "blocked", "source": "hook"}),
    );
    assert_eq!(report["ok"], json!(true), "report-agent failed: {report}");

    let before = screen_text(&surface);
    let sent = rpc(&sock, json!({"id": 2, "cmd": "send", "surface": sid, "text": "hello\n"}));
    assert_eq!(sent["ok"], json!(false), "blocked send must fail: {sent}");
    assert_eq!(
        sent["code"],
        json!("agent_blocked"),
        "structured code must be agent_blocked: {sent}"
    );
    assert!(
        sent["error"].as_str().unwrap_or("").starts_with("agent_blocked"),
        "error string must carry the code prefix: {sent}"
    );
    // The sending byte sequence must NOT have reached the child.
    std::thread::sleep(Duration::from_millis(200));
    let after = screen_text(&surface);
    assert_eq!(before, after, "gated send changed the pane screen");
    assert!(!after.contains("GOT:hello"), "gated send reached the child: {after}");

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// AC1: `force: true` bypasses the gate and delivers the bytes (the
/// pre-#93 raw behaviour).
#[test]
fn send_with_force_bypasses_gate() {
    let mux = Mux::new(
        unique_session("issue93-force"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; printf 'ready$ '; while IFS= read -r line; do echo \"GOT:$line\"; done"
                    .to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    wait_for(|| screen_text(&surface).contains("ready$").then_some(()), Duration::from_secs(5))
        .expect("pane prompt never appeared");

    let report = rpc(
        &sock,
        json!({"id": 1, "cmd": "report-agent", "surface": sid, "state": "blocked", "source": "hook"}),
    );
    assert_eq!(report["ok"], json!(true), "report-agent failed: {report}");

    let sent = rpc(
        &sock,
        json!({"id": 2, "cmd": "send", "surface": sid, "text": "hello\n", "force": true, "confirm": false}),
    );
    assert_eq!(sent["ok"], json!(true), "forced send must succeed: {sent}");

    let seen = wait_for(
        || screen_text(&surface).contains("GOT:hello").then_some(()),
        Duration::from_secs(5),
    );
    assert!(seen.is_some(), "forced send never reached the child");

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// AC1: a screen-derived (`Detected`) blocked pane is gated too — the
/// user's "pane parked at a y/n prompt" case, with no hook installed.
/// Also pins that a plain (unreported, non-agent) shell pane is NOT
/// gated: `Unknown` stays ungated, which is what keeps every existing
/// `send` caller working.
#[test]
fn send_gates_screen_detected_blocked_but_not_plain_shell() {
    // Claude Code footer + a confirmation dialog: detection resolves
    // `claude`, and the #96 classifier makes that unambiguously Blocked.
    let mux = Mux::new(
        unique_session("issue93-screen"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; printf 'Claude Code\\nDo you want to proceed?\\n\\xe2\\x9d\\xaf 1. Yes\\n'; while IFS= read -r line; do echo \"GOT:$line\"; done"
                    .to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    wait_for(
        || screen_text(&surface).contains("Do you want to proceed?").then_some(()),
        Duration::from_secs(5),
    )
    .expect("dialog never appeared");

    let gated = rpc(&sock, json!({"id": 1, "cmd": "send", "surface": sid, "text": "go\n"}));
    assert_eq!(gated["ok"], json!(false), "detected-blocked pane must gate: {gated}");
    assert_eq!(gated["code"], json!("agent_blocked"), "code: {gated}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !screen_text(&surface).contains("GOT:go"),
        "gated send reached the child: {}",
        screen_text(&surface)
    );

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);

    // Control: a plain shell pane has no report and classifies Unknown,
    // so it must remain ungated.
    let mux = Mux::new(
        unique_session("issue93-plain"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; printf 'plainprompt '; while IFS= read -r line; do echo \"GOT:$line\"; done"
                    .to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    wait_for(
        || screen_text(&surface).contains("plainprompt").then_some(()),
        Duration::from_secs(5),
    )
    .unwrap_or_else(|| panic!("shell prompt never appeared; screen={:?}", screen_text(&surface)));
    let ok = rpc(&sock, json!({"id": 1, "cmd": "send", "surface": surface.id, "text": "hi\n"}));
    assert_eq!(ok["ok"], json!(true), "plain shell must stay ungated: {ok}");
    assert!(
        wait_for(|| screen_text(&surface).contains("GOT:hi").then_some(()), Duration::from_secs(5))
            .is_some(),
        "ungated send never reached the shell"
    );

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// AC1: `wait-agent-status` with `require_transition` demands an
/// OBSERVED change at/after the call — a cached state that already
/// matches must NOT satisfy it, while a later real transition must.
#[test]
fn wait_requires_observed_transition() {
    let mux = Mux::new(
        unique_session("issue93-wait"),
        SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    // Pre-existing `working` report (before the waiter starts).
    let report = rpc(
        &sock,
        json!({"id": 1, "cmd": "report-agent", "surface": sid, "state": "working", "source": "hook",
               "agent": "w93"}),
    );
    assert_eq!(report["ok"], json!(true), "report-agent failed: {report}");

    // require_transition + timeout 0: the cached match must NOT count.
    let miss = rpc(
        &sock,
        json!({"id": 2, "cmd": "wait-agent-status", "target": "w93", "state": "working",
               "timeout_ms": 0, "require_transition": true}),
    );
    assert_eq!(
        miss["ok"],
        json!(false),
        "cached state must not satisfy an observed-transition wait: {miss}"
    );

    // Now a REAL transition into `working` (via idle first) must satisfy it.
    let reporter_sock = sock.clone();
    let reporter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        let _ = rpc(
            &reporter_sock,
            json!({"id": 3, "cmd": "report-agent", "surface": sid, "state": "idle", "source": "hook"}),
        );
        std::thread::sleep(Duration::from_millis(150));
        rpc(
            &reporter_sock,
            json!({"id": 4, "cmd": "report-agent", "surface": sid, "state": "working", "source": "hook"}),
        )
    });

    let started = Instant::now();
    let hit = rpc(
        &sock,
        json!({"id": 5, "cmd": "wait-agent-status", "target": "w93", "state": "working",
               "timeout_ms": 5000, "require_transition": true}),
    );
    assert_eq!(hit["ok"], json!(true), "observed transition must satisfy the wait: {hit}");
    assert_eq!(hit["data"]["state"], json!("working"), "payload: {hit}");
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "must have waited for the real transition, took {:?}",
        started.elapsed()
    );
    reporter.join().unwrap();

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// AC1 (compat): without `require_transition` the pre-#93 immediate-match
/// behaviour is unchanged, so existing orchestrators keep working.
#[test]
fn wait_immediate_match_still_works_without_require_transition() {
    let mux = Mux::new(
        unique_session("issue93-wait-compat"),
        SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    let report = rpc(
        &sock,
        json!({"id": 1, "cmd": "report-agent", "surface": sid, "state": "done", "source": "hook",
               "agent": "compat93"}),
    );
    assert_eq!(report["ok"], json!(true), "report-agent failed: {report}");

    let hit = rpc(
        &sock,
        json!({"id": 2, "cmd": "wait-agent-status", "target": "compat93", "state": "done",
               "timeout_ms": 5000}),
    );
    assert_eq!(hit["ok"], json!(true), "immediate match must still succeed: {hit}");
    assert!(hit["data"]["elapsed_ms"].as_u64().unwrap_or(u64::MAX) < 2000, "not immediate: {hit}");

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}

/// Issue #93: `send --wait` reports success only after an OBSERVED
/// transition into working/blocked at/after the send; a pane that stays
/// idle times out (`activity_observed:false`) rather than falsely
/// succeeding.
#[test]
fn send_wait_requires_observed_activity() {
    let mux = Mux::new(
        unique_session("issue93-send-wait"),
        SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let sid = surface.id;

    // No agent state changes: the wait must time out with a structured
    // payload, not a transport error.
    let idle = rpc(
        &sock,
        json!({"id": 1, "cmd": "send", "surface": sid, "text": "x",
               "confirm": false, "wait_activity_ms": 300}),
    );
    assert_eq!(idle["ok"], json!(true), "send --wait should reply ok with a payload: {idle}");
    assert_eq!(
        idle["data"]["activity_observed"],
        json!(false),
        "no activity must report false: {idle}"
    );
    assert_eq!(idle["data"]["state"], Value::Null, "no state on timeout: {idle}");

    // A transition that lands AFTER the send satisfies the wait.
    let reporter_sock = sock.clone();
    let reporter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        rpc(
            &reporter_sock,
            json!({"id": 2, "cmd": "report-agent", "surface": sid, "state": "working", "source": "hook"}),
        )
    });
    let active = rpc(
        &sock,
        json!({"id": 3, "cmd": "send", "surface": sid, "text": "y",
               "confirm": false, "wait_activity_ms": 5000}),
    );
    assert_eq!(active["ok"], json!(true), "send --wait failed: {active}");
    assert_eq!(
        active["data"]["activity_observed"],
        json!(true),
        "must observe the transition: {active}"
    );
    assert_eq!(active["data"]["state"], json!("working"), "observed state: {active}");
    reporter.join().unwrap();

    mux.close_workspace(0);
    mux_core::server::cleanup(&sock);
}
