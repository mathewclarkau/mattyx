//! Golden wire-shape contract tests (issue: top-4 queue, D1).
//!
//! These pin the RESPONSE SHAPE — envelope plus the key names and JSON
//! types of each read-only verb family — so the socket contract cannot
//! drift silently. Values are deliberately not asserted (a session name,
//! version string, or screen text is expected to vary); only the shape
//! is. The source of truth is the handlers in `mux-core/src/server.rs`;
//! every assertion below was read off the handler, not invented.
//!
//! Deliberately deterministic: a stub PTY (`/bin/cat`) and no screen
//! timing, so a green run means the wire shape held, not that a race
//! happened to resolve.

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

/// Every response must carry `ok: true` and echo the request id.
fn assert_envelope(resp: &Value, id: u64) {
    assert_eq!(resp["id"], json!(id), "id must echo the request id: {resp}");
    assert_eq!(resp["ok"], json!(true), "expected ok:true: {resp}");
    assert!(resp.get("error").is_none(), "unexpected error field: {resp}");
}

/// A live mux with one default PTY workspace, plus its control socket.
/// RAII guard: dropping it removes the session's `.sock`/`.pid` pair
/// (issue #136 — this suite used to leak one session per test).
struct WireServer {
    mux: std::sync::Arc<Mux>,
    sock: std::path::PathBuf,
    surface: u64,
}

impl Drop for WireServer {
    fn drop(&mut self) {
        mux_core::server::cleanup(&self.sock);
    }
}

fn mux_with_workspace(prefix: &str) -> WireServer {
    mux_with_command(prefix, vec!["/bin/cat".to_string()])
}

/// Same isolated runtime + RAII session as [`mux_with_workspace`], but the
/// seeded workspace runs an arbitrary command — used to drive real PTY
/// output (e.g. the OSC 7501 fixture) through the reader/watcher path.
fn mux_with_command(prefix: &str, command: Vec<String>) -> WireServer {
    isolate_runtime_dir();
    let mux = Mux::new(
        unique_session(prefix),
        SurfaceOptions {
            // A stub child so nothing depends on the user's login shell.
            command: Some(command),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();
    WireServer { mux, sock, surface: surface.id }
}

/// First pane id from the live tree, for verbs that target a pane.
fn first_pane(sock: &Path) -> u64 {
    let tree = rpc(sock, json!({"id": 1, "cmd": "list-workspaces"}));
    tree["data"]["workspaces"][0]["screens"][0]["panes"][0]["id"]
        .as_u64()
        .expect("workspace must expose a pane id")
}

#[test]
fn identify_envelope_and_data_shape() {
    let server = mux_with_workspace("wire-identify");
    let resp = rpc(&server.sock, json!({"id": 7, "cmd": "identify"}));
    assert_envelope(&resp, 7);

    let data = &resp["data"];
    assert!(data.is_object(), "identify data must be an object: {resp}");
    assert!(data["app"].is_string(), "app must be a string: {data}");
    assert!(data["version"].is_string(), "version must be a string: {data}");
    assert!(data["protocol"].is_number(), "protocol must be a number: {data}");
    assert!(data["session"].is_string(), "session must be a string: {data}");
    assert!(data["pid"].is_number(), "pid must be a number: {data}");
    let capabilities = &data["capabilities"];
    assert!(capabilities.is_object(), "capabilities must be an object: {data}");
    assert!(
        capabilities["input-ack"].is_boolean(),
        "capabilities.input-ack must be a bool: {capabilities}"
    );
    // Issue #176: additive boolean capability for in-band OSC 7501 agent
    // status. `PROTOCOL_VERSION` stays 8; peers discover support here.
    assert!(
        capabilities["osc7501"].is_boolean(),
        "capabilities.osc7501 must be a bool: {capabilities}"
    );
}

/// Issue #176: the OSC 7501 tier is a real wire value. A pane emitting the
/// sequence through its own PTY reports at `source: "osc7501"` in both the
/// `list-agents` record and the `list-workspaces` per-tab `agent_source`
/// snapshot. A direct `report-agent` is deliberately NOT used: the value
/// under test is the one parsed out of raw PTY bytes.
#[test]
fn osc7501_source_value_is_pinned_in_agent_and_workspace_payloads() {
    let server = mux_with_command(
        "wire-osc7501",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            // Alternate working/waiting so the record exists for the whole
            // 10s polling budget regardless of when the RPC lands.
            "while true; do printf '\\033]7501;state=working:app=wire-agent\\033\\\\'; \
             sleep 1; printf '\\033]7501;state=waiting\\033\\\\'; sleep 1; done"
                .to_string(),
        ],
    );

    let agent = wait_for(
        || {
            let resp = rpc(&server.sock, json!({"id": 20, "cmd": "list-agents"}));
            resp["data"]["agents"].as_array()?.iter().find_map(|agent| {
                (agent["surface"].as_u64() == Some(server.surface)
                    && agent["source"].as_str() == Some("osc7501"))
                .then(|| agent.clone())
            })
        },
        Duration::from_secs(10),
    )
    .expect("list-agents record must carry source osc7501");
    assert_eq!(agent["source"], json!("osc7501"));
    assert!(agent["state"].is_string(), "state must be a string: {agent}");
    assert_eq!(agent["agent"], json!("wire-agent"), "app= populates the record name");
    assert!(agent["updated_at_ms"].is_number(), "updated_at_ms must be a number: {agent}");
    assert!(agent["state_seq"].is_number(), "state_seq must be a number: {agent}");

    let source = wait_for(
        || {
            let resp = rpc(&server.sock, json!({"id": 21, "cmd": "list-workspaces"}));
            for workspace in resp["data"]["workspaces"].as_array()? {
                for screen in workspace["screens"].as_array()? {
                    for pane in screen["panes"].as_array()? {
                        for tab in pane["tabs"].as_array()? {
                            if tab["surface"].as_u64() == Some(server.surface) {
                                return tab["agent_source"].as_str().map(str::to_string);
                            }
                        }
                    }
                }
            }
            None
        },
        Duration::from_secs(10),
    )
    .expect("list-workspaces agent_source must be osc7501");
    assert_eq!(source, "osc7501");
}

#[test]
fn list_workspaces_shape() {
    let server = mux_with_workspace("wire-list-ws");
    let resp = rpc(&server.sock, json!({"id": 2, "cmd": "list-workspaces"}));
    assert_envelope(&resp, 2);

    let workspaces = &resp["data"]["workspaces"];
    let workspaces = workspaces.as_array().expect("data.workspaces must be an array");
    assert!(!workspaces.is_empty(), "expected the seeded workspace: {resp}");

    let ws = &workspaces[0];
    assert!(ws["id"].is_number(), "workspace id must be a number: {ws}");
    assert!(ws["name"].is_string(), "workspace name must be a string: {ws}");
    let screens = ws["screens"].as_array().expect("workspace screens must be an array");
    assert!(!screens.is_empty(), "expected a screen: {ws}");
    assert!(screens[0]["panes"].is_array(), "screen panes must be an array: {ws}");
    assert!(screens[0]["panes"][0]["id"].is_number(), "pane id must be a number: {ws}");
}

#[test]
fn get_resolved_config_shape() {
    let server = mux_with_workspace("wire-chrome");

    // No chrome registered yet: the handler substitutes an empty object.
    let resp = rpc(&server.sock, json!({"id": 3, "cmd": "get-resolved-config"}));
    assert_envelope(&resp, 3);
    assert!(resp["data"].is_object(), "get-resolved-config data must be an object: {resp}");

    // Registered chrome is passed through verbatim (the TUI owns the keys;
    // the wire contract is "an opaque object", so we pin round-tripping
    // rather than inventing a schema the server does not enforce).
    let chrome = json!({"theme": "dark", "tabs": {}, "sidebar": {}, "keys": []});
    server.mux.set_resolved_chrome(chrome.clone());
    let resp = rpc(&server.sock, json!({"id": 4, "cmd": "get-resolved-config"}));
    assert_envelope(&resp, 4);
    assert_eq!(resp["data"], chrome, "resolved chrome must round-trip verbatim");
}

#[test]
fn list_agents_shape() {
    let server = mux_with_workspace("wire-list-agents");
    let resp = rpc(&server.sock, json!({"id": 5, "cmd": "list-agents"}));
    assert_envelope(&resp, 5);
    assert!(resp["data"]["agents"].is_array(), "data.agents must be an array: {resp}");
}

#[test]
fn agent_pattern_list_shape() {
    let server = mux_with_workspace("wire-patterns");
    let resp = rpc(&server.sock, json!({"id": 6, "cmd": "agent-pattern-list"}));
    assert_envelope(&resp, 6);
    let patterns = resp["data"]["patterns"].as_array().expect("data.patterns must be an array");
    if let Some(pattern) = patterns.first() {
        assert!(pattern["name"].is_string(), "pattern name must be a string: {pattern}");
        assert!(pattern["kind"].is_string(), "pattern kind must be a string: {pattern}");
        assert!(pattern["pattern"].is_string(), "pattern pattern must be a string: {pattern}");
        assert!(pattern["confidence"].is_string(), "pattern confidence: {pattern}");
        assert!(pattern["case_insensitive"].is_boolean(), "pattern case_insensitive: {pattern}");
    }
}

#[test]
fn pane_worktree_list_shape() {
    let server = mux_with_workspace("wire-worktrees");
    let pane = first_pane(&server.sock);
    let resp = rpc(&server.sock, json!({"id": 8, "cmd": "pane-worktree-list", "pane": pane}));
    assert_envelope(&resp, 8);
    assert!(resp["data"]["worktrees"].is_array(), "data.worktrees must be an array: {resp}");
}

#[test]
fn layout_export_shape() {
    let server = mux_with_workspace("wire-layout");
    let resp = rpc(&server.sock, json!({"id": 9, "cmd": "layout-export"}));
    assert_envelope(&resp, 9);

    // The response `data` IS the LayoutDocument (no wrapper key).
    let doc = &resp["data"];
    assert!(doc.is_object(), "layout-export data must be an object: {resp}");
    assert!(doc["schema_version"].is_number(), "schema_version must be a number: {doc}");
    assert!(doc["mtyx_version"].is_string(), "mtyx_version must be a string: {doc}");
    let workspace = &doc["workspace"];
    assert!(workspace.is_object(), "workspace must be an object: {doc}");
    assert!(workspace["name"].is_string(), "workspace.name must be a string: {workspace}");
    assert!(workspace["active_screen"].is_number(), "workspace.active_screen: {workspace}");
    let screens = workspace["screens"].as_array().expect("workspace.screens must be an array");
    assert!(!screens.is_empty(), "expected a screen in the export: {workspace}");
    let screen = &screens[0];
    assert!(screen["layout"].is_object(), "screen.layout must be an object: {screen}");
    assert!(screen["panes"].is_array(), "screen.panes must be an array: {screen}");
    assert!(screen["panes"][0]["tabs"].is_array(), "pane.tabs must be an array: {screen}");
}

#[test]
fn read_screen_shape() {
    let server = mux_with_workspace("wire-read-screen");
    let resp =
        rpc(&server.sock, json!({"id": 10, "cmd": "read-screen", "surface": server.surface}));
    assert_envelope(&resp, 10);

    // server.rs:2117 — ReadScreen returns ONLY `{"text": <string>}`; there
    // is no `surface` key (agent-read is the verb that adds one).
    let data = resp["data"].as_object().expect("read-screen data must be an object");
    assert_eq!(data.len(), 1, "read-screen data must be exactly {{text}}: {resp}");
    assert!(data["text"].is_string(), "data.text must be a string: {resp}");
}

#[test]
fn notify_ack_shape() {
    let server = mux_with_workspace("wire-notify-ack");
    let resp = rpc(&server.sock, json!({"id": 11, "cmd": "notify-ack", "client": "golden"}));
    assert_envelope(&resp, 11);

    let data = &resp["data"];
    assert_eq!(data["client"], json!("golden"), "client must echo the request: {data}");
    assert!(data["acked"].is_array(), "data.acked must be an array: {data}");
    assert!(data["unread"].is_number(), "data.unread must be a number: {data}");
}

/// D1: `agent-register` reply is the agent-report record (same shape as
/// `report-agent`/`list-agents` entries). The registration runs at hook
/// authority so a fast-starting agent's first hook report cannot reject
/// it; the pinned part here is the KEY SET, not the values.
#[test]
fn agent_register_shape() {
    let server = mux_with_workspace("wire-agent-register");
    let resp = rpc(
        &server.sock,
        json!({"id": 12, "cmd": "agent-register", "surface": server.surface, "agent": "demo"}),
    );
    assert_envelope(&resp, 12);

    let data = &resp["data"];
    assert!(data["surface"].is_number(), "surface must be a number: {data}");
    assert!(data["state"].is_string(), "state must be a string: {data}");
    assert!(data["source"].is_string(), "source must be a string: {data}");
    assert!(data["agent"].is_string(), "agent must be a string: {data}");
    assert!(data["updated_at_ms"].is_number(), "updated_at_ms must be a number: {data}");
    assert!(data["state_seq"].is_number(), "state_seq must be a number: {data}");
}

/// D1: a duplicate name on another surface is rejected with the stable
/// `agent_name_taken` code (`mtyx agent start` maps it to exit 1).
#[test]
fn agent_register_rejects_duplicate_names_with_stable_code() {
    let server = mux_with_workspace("wire-agent-dup");
    let first = rpc(
        &server.sock,
        json!({"id": 13, "cmd": "agent-register", "surface": server.surface, "agent": "dup"}),
    );
    assert_envelope(&first, 13);

    let second_surface = server.mux.new_workspace(None, None).unwrap();
    let resp = rpc(
        &server.sock,
        json!({"id": 14, "cmd": "agent-register", "surface": second_surface.id, "agent": "dup"}),
    );
    assert_eq!(resp["ok"], json!(false), "duplicate register must fail: {resp}");
    assert_eq!(resp["code"], json!("agent_name_taken"), "stable error code: {resp}");
}

/// D1: `agent-prompt` reply, plus the `send` blocked gate contract —
/// ungated write, `agent_blocked` refusal (with the stable `code`
/// field) while blocked, and `force` bypassing it. The writes land in
/// the stub `/bin/cat` PTY, so this is shape-only.
#[test]
fn agent_prompt_shape_and_blocked_gate() {
    let server = mux_with_workspace("wire-agent-prompt");
    let reg = rpc(
        &server.sock,
        json!({"id": 15, "cmd": "agent-register", "surface": server.surface, "agent": "demo"}),
    );
    assert_envelope(&reg, 15);

    // Ungated (fresh registration reports `unknown`): text + CR write.
    let resp =
        rpc(&server.sock, json!({"id": 16, "cmd": "agent-prompt", "target": "demo", "text": "hi"}));
    assert_envelope(&resp, 16);
    assert!(resp["data"]["surface"].is_number(), "data.surface must be a number: {resp}");

    // A hook report AFTER the registration keeps the name (hooks never
    // carry `agent`; the ladder preserves it) — the factory race the
    // hook-authority registration exists to close.
    let blocked = rpc(
        &server.sock,
        json!({"id": 17, "cmd": "report-agent", "surface": server.surface, "state": "blocked", "source": "hook"}),
    );
    assert_envelope(&blocked, 17);
    assert_eq!(
        blocked["data"]["agent"],
        json!("demo"),
        "a hook report must not drop the registered name: {blocked}"
    );

    let refused =
        rpc(&server.sock, json!({"id": 18, "cmd": "agent-prompt", "target": "demo", "text": "hi"}));
    assert_eq!(refused["ok"], json!(false), "blocked prompt must fail: {refused}");
    assert_eq!(refused["code"], json!("agent_blocked"), "stable error code: {refused}");

    let forced = rpc(
        &server.sock,
        json!({"id": 19, "cmd": "agent-prompt", "target": "demo", "text": "hi", "force": true}),
    );
    assert_envelope(&forced, 19);
    assert!(forced["data"]["surface"].is_number(), "data.surface must be a number: {forced}");
}

/// Issue #144: CI drift check that `mux/docs/protocol.md`'s compatibility
/// matrix stays synchronized with `mux_core::PROTOCOL_VERSION` and names the
/// golden wire-contract tests that pin it.
#[test]
fn protocol_compat_matrix_in_docs_matches_protocol_version() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let protocol_md_path = manifest_dir.join("../../docs/protocol.md");
    let doc = std::fs::read_to_string(&protocol_md_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", protocol_md_path.display()));

    let ver = mux_core::server::PROTOCOL_VERSION;
    let expected_heading = format!("### Protocol Compatibility Matrix (`PROTOCOL_VERSION = {ver}`)");
    assert!(
        doc.contains(&expected_heading),
        "mux/docs/protocol.md is out of sync with mux_core::PROTOCOL_VERSION ({ver}); expected heading {expected_heading:?}"
    );
    assert!(
        doc.contains("<!-- PROTOCOL_COMPAT_MATRIX_START -->")
            && doc.contains("<!-- PROTOCOL_COMPAT_MATRIX_END -->"),
        "mux/docs/protocol.md must retain the PROTOCOL_COMPAT_MATRIX markers"
    );
    for required_token in [
        &format!("Protocol {ver} (Current)"),
        // The legacy-boundary column must move with PROTOCOL_VERSION too:
        // without this assert, a future bump would fix the heading but
        // leave a stale `Protocol <= <old-1>` column unnoticed.
        &format!("Protocol <= {}", ver - 1),
        &format!("Protocol {ver}+ (1.x Additive)"),
        "legacy_host_receipt_rejected",
        "wire_contract.rs",
        "agent_register_shape",
        "agent_prompt_shape_and_blocked_gate",
    ] {
        assert!(
            doc.contains(required_token),
            "protocol compat matrix in mux/docs/protocol.md is missing {required_token:?}"
        );
    }
}
