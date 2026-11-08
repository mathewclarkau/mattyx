//! Issue #176: OSC 7501 in-band agent status as an explicit
//! `AgentStateSource` tier.
//!
//! These are the end-to-end tests: a REAL PTY child (`/bin/sh -c`) writes
//! the escape sequence `ESC ] 7501 ; state=<state>:app=<name> ESC \` (and
//! the BEL-terminated variant) to its own output, and the assertions read
//! the resulting state back through the ordinary pane-state surfaces —
//! `Surface::agent_report`, `MuxEvent::AgentStateChanged`, `list-agents`,
//! `list-workspaces` (`agent_source`), and the socket `subscribe` stream.
//! Nothing here calls the watcher directly: A2/A3/A4 must exercise the
//! real PTY reader -> watcher -> `Mux::report_surface_agent` path, so a
//! bug in the byte framing or the reader plumbing cannot hide behind a
//! direct report.
//!
//! Determinism: `TMPDIR=/tmp` (set by the gate) plus a per-test-binary
//! isolated `XDG_RUNTIME_DIR`; the fixtures are `/bin/sh` scripts with
//! fixed sleeps so every transition is observable, no network anywhere.

#![cfg(unix)] // exercises unix PTY and AF_UNIX machinery

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mux_core::platform::transport;
use mux_core::{AgentState, AgentStateSource, Mux, MuxEvent, SurfaceOptions};
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

fn shell_opts(script: &str) -> SurfaceOptions {
    SurfaceOptions {
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]),
        ..Default::default()
    }
}

fn unique_session(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Point `XDG_RUNTIME_DIR` at a throwaway dir before any `serve(.., None)`:
/// the default (`platform::runtime_dir()`) is the operator's real
/// `/run/user/<uid>/mtyx-<uid>/` on a dev box, and tests must never touch it.
/// One dir per test binary; `serve()` re-reads the env on every call.
fn isolate_runtime_dir() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("mtyx-core-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
    });
}

fn connect(path: &Path) -> Box<dyn transport::Stream> {
    transport::connect(path).unwrap()
}

/// One request → one response on a fresh connection (the `mtyx` CLI shape).
fn rpc(socket: &Path, request: Value) -> Value {
    let stream = connect(socket);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(20)));
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Read one JSON line, returning `None` on EOF or a read timeout (the
/// stream verbs' connections stay open, so a missing line is "nothing
/// arrived in the budget" rather than an error).
fn read_json_line(reader: &mut impl BufRead) -> Option<Value> {
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

/// The `agent_source` of the tab whose `surface` is `surface`, from a
/// `list-workspaces` snapshot (the sidebar path).
fn tab_agent_source(resp: &Value, surface: u64) -> Option<String> {
    for workspace in resp["data"]["workspaces"].as_array()? {
        for screen in workspace["screens"].as_array()? {
            for pane in screen["panes"].as_array()? {
                for tab in pane["tabs"].as_array()? {
                    if tab["surface"].as_u64() == Some(surface) {
                        return tab["agent_source"].as_str().map(str::to_string);
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// A2: a real PTY fixture drives Working -> Blocked -> Done through the
// pane-state path (no direct reports for the three OSC states).
// ---------------------------------------------------------------------------

/// `working` (ST-terminated, with `app=`) -> `waiting` (BEL-terminated,
/// no `app`) -> `done` (ST-terminated, no `app`). The sleeps between the
/// emissions keep each state observable through `Surface::agent_report`
/// even on a loaded runner; the 30s tail keeps the pane alive to assert on.
fn transition_fixture() -> &'static str {
    "printf '\\033]7501;state=working:app=test-agent\\033\\\\'; sleep 2; \
     printf '\\033]7501;state=waiting\\007'; sleep 2; \
     printf '\\033]7501;state=done\\033\\\\'; sleep 30"
}

#[test]
fn osc7501_pty_transitions_working_blocked_done() {
    let mux = Mux::new(unique_session("osc7501-transitions"), shell_opts(transition_fixture()));
    let surface = mux.new_workspace(None, None).unwrap();

    // working -> Working/osc7501, with `app=` becoming the report's name.
    let working = wait_for(
        || surface.agent_report().filter(|r| r.state == AgentState::Working),
        Duration::from_secs(10),
    )
    .expect("working/osc7501 report through the real pty path");
    assert_eq!(working.source, AgentStateSource::Osc7501);
    assert_eq!(working.agent.as_deref(), Some("test-agent"), "app= must populate the report name");

    // waiting -> Blocked/osc7501 (BEL-terminated variant).
    let blocked = wait_for(
        || surface.agent_report().filter(|r| r.state == AgentState::Blocked),
        Duration::from_secs(10),
    )
    .expect("blocked/osc7501 report after `waiting` (BEL)");
    assert_eq!(blocked.source, AgentStateSource::Osc7501);
    assert_eq!(
        blocked.agent.as_deref(),
        Some("test-agent"),
        "an absent app= must preserve the established report name"
    );

    // done -> Done/osc7501.
    let done = wait_for(
        || surface.agent_report().filter(|r| r.state == AgentState::Done),
        Duration::from_secs(10),
    )
    .expect("done/osc7501 report after `done`");
    assert_eq!(done.source, AgentStateSource::Osc7501);
    assert!(done.state_seq > blocked.state_seq, "each applied report advances state_seq");

    // The report is visible through the ordinary list path too.
    let agents = mux.list_agents(Some(surface.id), None);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].1.state, AgentState::Done);
    assert_eq!(agents[0].1.source, AgentStateSource::Osc7501);

    mux.close_surface(surface.id);
}

// ---------------------------------------------------------------------------
// A3: active Osc7501 outranks Hook > Socket > Detected > Heuristic, and a
// terminal `done` releases that authority to a newer lower-tier report
// (the `done-releases-authority` rule).
// ---------------------------------------------------------------------------

#[test]
fn osc7501_active_report_rejects_lower_tiers_and_done_releases_authority() {
    // `working` stays active for 4s (long enough for the synchronous
    // rejection assertions), then a terminal `done`, then a FRESH active
    // `working` 2s later to pin re-arming of the highest authority (the
    // gap keeps the post-done hook submission safely inside the window).
    let mux = Mux::new(
        unique_session("osc7501-precedence"),
        shell_opts(
            "printf '\\033]7501;state=working:app=test-agent\\033\\\\'; sleep 4; \
             printf '\\033]7501;state=done\\033\\\\'; sleep 2; \
             printf '\\033]7501;state=working\\033\\\\'; sleep 30",
        ),
    );
    let surface = mux.new_workspace(None, None).unwrap();

    let active = wait_for(
        || {
            surface.agent_report().filter(|r| {
                r.source == AgentStateSource::Osc7501 && r.state == AgentState::Working
            })
        },
        Duration::from_secs(10),
    )
    .expect("active osc7501/working report through the real pty path");
    let active_seq = active.state_seq;

    // While the OSC 7501 report is ACTIVE, every lower tier is rejected —
    // Hook, Socket, Detected, and Heuristic — with no state_seq bump.
    // Direct `report-agent` submissions keep this deterministic (no
    // dependence on the background heuristic poller).
    for source in [
        AgentStateSource::Hook,
        AgentStateSource::Socket,
        AgentStateSource::Detected,
        AgentStateSource::Heuristic,
    ] {
        let report = mux
            .report_agent(surface.id, AgentState::Blocked, source, None, None, None)
            .unwrap();
        assert_eq!(
            report.source,
            AgentStateSource::Osc7501,
            "lower tier {source:?} must be rejected while osc7501 is active"
        );
        assert_eq!(report.state, AgentState::Working, "the active state must be unchanged");
        assert_eq!(report.state_seq, active_seq, "a rejected report must not bump state_seq");
    }

    // A terminal Osc7501 `done` remains observable as Done/osc7501...
    let done = wait_for(
        || {
            surface.agent_report().filter(|r| {
                r.source == AgentStateSource::Osc7501 && r.state == AgentState::Done
            })
        },
        Duration::from_secs(10),
    )
    .expect("done/osc7501 report through the real pty path");
    assert!(done.state_seq > active_seq, "done is a fresh applied report");

    // ...but it releases authority: a NEWER lower-tier Hook report is
    // accepted, replaces the stale terminal report, and bumps state_seq.
    let hooked = mux
        .report_agent(surface.id, AgentState::Blocked, AgentStateSource::Hook, None, None, None)
        .unwrap();
    assert_eq!(hooked.source, AgentStateSource::Hook, "done must release authority to lower tiers");
    assert_eq!(hooked.state, AgentState::Blocked);
    assert!(hooked.state_seq > done.state_seq, "an accepted lower-tier report bumps state_seq");

    // The full ordering claim also pins Hook > Heuristic.
    let heuristic = mux
        .report_agent(surface.id, AgentState::Working, AgentStateSource::Heuristic, None, None, None)
        .unwrap();
    assert_eq!(heuristic.source, AgentStateSource::Hook, "heuristic must not override hook");
    assert_eq!(heuristic.state, AgentState::Blocked);
    assert_eq!(heuristic.state_seq, hooked.state_seq, "a rejected report must not bump state_seq");

    // A fresh ACTIVE Osc7501 report re-arms the highest authority: a
    // later hook report is rejected again with no state_seq bump.
    let rearmed = wait_for(
        || {
            surface.agent_report().filter(|r| {
                r.source == AgentStateSource::Osc7501 && r.state == AgentState::Working
            })
        },
        Duration::from_secs(10),
    )
    .expect("a fresh osc7501 working report re-arms highest authority");
    assert!(rearmed.state_seq > heuristic.state_seq, "the re-arm is a fresh applied report");
    let rejected = mux
        .report_agent(surface.id, AgentState::Blocked, AgentStateSource::Hook, None, None, None)
        .unwrap();
    assert_eq!(rejected.source, AgentStateSource::Osc7501, "re-armed osc7501 must reject hook again");
    assert_eq!(rejected.state, AgentState::Working);
    assert_eq!(rejected.state_seq, rearmed.state_seq, "a rejected report must not bump state_seq");

    mux.close_surface(surface.id);
}

// ---------------------------------------------------------------------------
// A4: the source marker reaches all four state surfaces for a REAL
// Osc7501 report (never a direct hook report).
// ---------------------------------------------------------------------------

#[test]
fn osc7501_source_reaches_core_events_wire_lists_and_socket_subscribe() {
    isolate_runtime_dir();
    let mux = Mux::new(
        unique_session("osc7501-surfaces"),
        // Alternate working/waiting so a live subscriber cannot miss the
        // single first event (and so the source, not a state transition,
        // is what each assertion pins).
        shell_opts(
            "while true; do printf '\\033]7501;state=working:app=test-agent\\033\\\\'; sleep 1; \
             printf '\\033]7501;state=waiting\\033\\\\'; sleep 1; done",
        ),
    );

    // Core events FIRST, then the socket subscribe, then the surface:
    // every receiver exists before the first OSC bytes can be reported, so
    // the assertions observe LIVE events rather than stale state.
    let events = mux.subscribe();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    let subscribe_stream = connect(&sock);
    subscribe_stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut subscribe_writer = subscribe_stream.try_clone_box().unwrap();
    let mut subscribe_reader = BufReader::new(subscribe_stream);
    writeln!(subscribe_writer, "{}", json!({"id": 1, "cmd": "subscribe"})).unwrap();
    // No surface exists yet, so the reply must be the next line; loop
    // anyway so a stray event can never be mistaken for the response.
    let reply = wait_for(
        || read_json_line(&mut subscribe_reader).filter(|value| value.get("ok").is_some()),
        Duration::from_secs(5),
    )
    .expect("subscribe reply");
    assert_eq!(reply["ok"], json!(true), "subscribe failed: {reply}");

    let surface = mux.new_workspace(None, None).unwrap();

    // (d) core `MuxEvent::AgentStateChanged` report carries `Osc7501`.
    let core_report = wait_for(
        || {
            events.try_iter().find_map(|event| match event {
                MuxEvent::AgentStateChanged { surface: id, report, .. }
                    if id == surface.id && report.source == AgentStateSource::Osc7501 =>
                {
                    Some(report)
                }
                _ => None,
            })
        },
        Duration::from_secs(10),
    )
    .expect("core AgentStateChanged with Osc7501 source");
    assert_eq!(core_report.source, AgentStateSource::Osc7501);

    // (c) socket `subscribe` emits `agent-state-changed` with
    // `source == "osc7501"`.
    let socket_event = wait_for(
        || {
            while let Some(value) = read_json_line(&mut subscribe_reader) {
                if value.get("event").and_then(Value::as_str) == Some("agent-state-changed")
                    && value.get("source").and_then(Value::as_str) == Some("osc7501")
                {
                    return Some(value);
                }
            }
            None
        },
        Duration::from_secs(10),
    )
    .expect("socket agent-state-changed with osc7501 source");
    assert_eq!(socket_event["source"], json!("osc7501"));
    assert_eq!(socket_event["surface"], json!(surface.id));

    // (a) list-agents / `agent list --json` wire record.
    let agent_record = wait_for(
        || {
            let resp = rpc(&sock, json!({"id": 2, "cmd": "list-agents"}));
            resp["data"]["agents"].as_array()?.iter().find_map(|agent| {
                (agent["surface"].as_u64() == Some(surface.id)
                    && agent["source"].as_str() == Some("osc7501"))
                .then(|| agent.clone())
            })
        },
        Duration::from_secs(10),
    )
    .expect("list-agents record with osc7501 source");
    assert_eq!(agent_record["source"], json!("osc7501"));
    assert!(agent_record["agent"].is_string(), "app= should surface as the record agent: {agent_record}");

    // (b) list-workspaces sidebar snapshot: `agent_source == "osc7501"`.
    let source = wait_for(
        || {
            let resp = rpc(&sock, json!({"id": 3, "cmd": "list-workspaces"}));
            tab_agent_source(&resp, surface.id)
        },
        Duration::from_secs(10),
    )
    .expect("list-workspaces agent_source with osc7501");
    assert_eq!(source, "osc7501");

    mux.close_surface(surface.id);
    mux_core::server::cleanup(&sock);
}
