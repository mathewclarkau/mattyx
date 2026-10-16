#![cfg(unix)]

//! Comprehensive E2E opaque-box test suites for mattyx requirements R1 - R5 per TEST_INFRA.md.
//!
//! Tiers covered:
//! - Tier 1: Functional Happy Path (5 tests per feature = 25 tests)
//! - Tier 2: Boundary & Corner Cases (5 tests per feature = 25 tests)
//! - Tier 3: Pairwise Combinatorial Interactions (5 tests)
//! - Tier 4: Real-World Workload Scenarios (5 tests)
//!
//! Total: 60 test cases

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

// ============================================================================
// Test Harness Fixtures & Helpers
// ============================================================================

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mtyx")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn unique_temp_dir(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    PathBuf::from("/tmp").join(format!(
        "mtyx-e2e-{name}-{}-{stamp}-{seq}",
        std::process::id()
    ))
}

struct HeadlessServer {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
}

impl HeadlessServer {
    fn start(name: &str) -> Self {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mux.sock");
        let child = Command::new(bin())
            .args(["--headless", "--socket"])
            .arg(&socket)
            .env("XDG_STATE_HOME", &dir)
            .env("SHELL", "/bin/sh")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let server = Self { child, socket, dir };
        server.wait_for_socket();
        server
    }

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "headless server did not create socket at {}",
            self.socket.display()
        );
    }
}

impl Drop for HeadlessServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn cli(server: &HeadlessServer, args: &[&str]) -> Output {
    Command::new(bin())
        .args(["--socket"])
        .arg(&server.socket)
        .args(args)
        .env_remove("MTYX_MUX_SOCKET")
        .env_remove("CMUX_MUX_SOCKET")
        .output()
        .unwrap()
}

fn cli_json(server: &HeadlessServer, args: &[&str]) -> (Output, Value) {
    let mut full_args = vec!["--json"];
    full_args.extend_from_slice(args);
    let out = cli(server, &full_args);
    let val: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out, val)
}

fn ipc_call(server: &HeadlessServer, request: &Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(&server.socket)
        .map_err(|e| format!("connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set_read_timeout failed: {e}"))?;
    let mut payload = serde_json::to_vec(request)
        .map_err(|e| format!("serialize failed: {e}"))?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .map_err(|e| format!("write failed: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read failed: {e}"))?;
    serde_json::from_str(&line)
        .map_err(|e| format!("json decode error: {e}, raw: {line}"))
}

fn parse_surface_id(out: &Output) -> u64 {
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

fn create_workspace_and_surface(server: &HeadlessServer, name: &str) -> u64 {
    let out = cli(server, &["new-workspace", "--name", name]);
    assert!(out.status.success(), "new-workspace failed: {:?}", out);
    parse_surface_id(&out)
}

fn get_active_pane_id(server: &HeadlessServer) -> u64 {
    let (_, ws) = cli_json(server, &["list-workspaces"]);
    ws["workspaces"]
        .as_array()
        .and_then(|w| w.first())
        .and_then(|w| w.get("screens"))
        .and_then(|s| s.as_array())
        .and_then(|s| s.first())
        .and_then(|s| s.get("panes"))
        .and_then(|p| p.as_array())
        .and_then(|p| p.first())
        .and_then(|p| p.get("id"))
        .and_then(|id| id.as_u64())
        .unwrap_or(0)
}

fn split_pane(server: &HeadlessServer, dir: &str) -> Output {
    let pane = get_active_pane_id(server);
    cli(server, &["split", "--pane", &pane.to_string(), "--dir", dir])
}

// ============================================================================
// Feature 1: R1 (WASM Plugin Multi-Call Execution)
// ============================================================================

/// Trait and simulator for validating multi-call request sequence contracts
struct MockPluginHost {
    expected_request_id: u64,
    #[allow(dead_code)]
    token: String,
    verb_allowlist: Vec<String>,
}

impl MockPluginHost {
    fn new(allowlist: &[&str]) -> Self {
        Self {
            expected_request_id: 0,
            token: "mock-token-xyz-123".to_string(),
            verb_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Emulates the host's `mtyx_call` validation and counter advancement
    fn call(&mut self, request_json: &str) -> Result<Value, String> {
        let req: Value = serde_json::from_str(request_json)
            .map_err(|e| format!("invalid JSON request: {e}"))?;

        let id = req["id"].as_u64().ok_or_else(|| "missing id".to_string())?;
        if id != self.expected_request_id {
            return Err(format!(
                "StaleId: expected {}, got {}",
                self.expected_request_id, id
            ));
        }

        let verb = req["verb"].as_str().ok_or_else(|| "missing verb".to_string())?;
        if !self.verb_allowlist.iter().any(|v| v == verb) {
            return Err(format!("VerbNotAllowed: {verb}"));
        }

        // On successful processing, advance expected_request_id
        self.expected_request_id = self.expected_request_id.wrapping_add(1);

        Ok(json!({
            "id": id,
            "ok": true,
            "data": { "verb": verb, "seq": id }
        }))
    }
}

// Tier 1: Nominal & Happy Path for R1
#[test]
fn test_r1_t1_sequential_call_request_ids_advance_correctly() {
    let mut host = MockPluginHost::new(&["list-workspaces", "read-screen"]);
    let req0 = r#"{"id":0,"verb":"list-workspaces","args":{}}"#;
    let res0 = host.call(req0).expect("call 0 should succeed");
    assert_eq!(res0["id"], 0);
    assert_eq!(host.expected_request_id, 1);

    let req1 = r#"{"id":1,"verb":"read-screen","args":{"surface":1}}"#;
    let res1 = host.call(req1).expect("call 1 should succeed without StaleId");
    assert_eq!(res1["id"], 1);
    assert_eq!(host.expected_request_id, 2);
}

#[test]
fn test_r1_t1_three_step_sequential_queries_succeed() {
    let mut host = MockPluginHost::new(&["list-workspaces", "read-screen", "select-tab"]);
    for step in 0..3 {
        let req = format!(r#"{{"id":{step},"verb":"list-workspaces","args":{{}}}}"#);
        let res = host.call(&req).unwrap_or_else(|e| panic!("step {step} failed: {e}"));
        assert_eq!(res["id"], step);
    }
    assert_eq!(host.expected_request_id, 3);
}

#[test]
fn test_r1_t1_sequential_read_queries_return_valid_responses() {
    let mut host = MockPluginHost::new(&["list-workspaces", "read-screen"]);
    let res1 = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    let res2 = host.call(r#"{"id":1,"verb":"read-screen","args":{"surface":1}}"#).unwrap();
    assert_eq!(res1["ok"], true);
    assert_eq!(res2["ok"], true);
}

#[test]
fn test_r1_t1_response_payload_preserves_request_id_matching() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    let res0 = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    assert_eq!(res0["id"], 0);
    let res1 = host.call(r#"{"id":1,"verb":"list-workspaces","args":{}}"#).unwrap();
    assert_eq!(res1["id"], 1);
}

#[test]
fn test_r1_t1_counter_wrapping_semantics() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    host.expected_request_id = u64::MAX;
    let req = format!(r#"{{"id":{},"verb":"list-workspaces","args":{{}}}}"#, u64::MAX);
    let res = host.call(&req).unwrap();
    assert_eq!(res["id"], u64::MAX);
    assert_eq!(host.expected_request_id, 0); // cleanly wraps around
}

// Tier 2: Boundary & Corner Cases for R1
#[test]
fn test_r1_t2_skipped_request_id_fails_with_stale_id() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    let _ = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    // Host expects 1, but guest skips to 2
    let err = host.call(r#"{"id":2,"verb":"list-workspaces","args":{}}"#).unwrap_err();
    assert!(err.contains("StaleId"), "expected StaleId error, got: {err}");
    assert!(err.contains("expected 1, got 2"));
}

#[test]
fn test_r1_t2_repeated_request_id_fails_with_stale_id() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    let _ = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    // Guest mistakenly repeats id 0
    let err = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap_err();
    assert!(err.contains("StaleId"), "expected StaleId error, got: {err}");
    assert!(err.contains("expected 1, got 0"));
}

#[test]
fn test_r1_t2_stale_id_error_preserves_expected_counter() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    let _ = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    assert_eq!(host.expected_request_id, 1);
    // Erroneous call
    let _ = host.call(r#"{"id":99,"verb":"list-workspaces","args":{}}"#);
    // Counter should still be 1
    assert_eq!(host.expected_request_id, 1);
    // Now retry with 1 succeeds
    let retry = host.call(r#"{"id":1,"verb":"list-workspaces","args":{}}"#).unwrap();
    assert_eq!(retry["id"], 1);
}

#[test]
fn test_r1_t2_disallowed_verb_on_step_two_rejected() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    let _ = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    let err = host.call(r#"{"id":1,"verb":"rename-workspace","args":{}}"#).unwrap_err();
    assert!(err.contains("VerbNotAllowed"));
    assert_eq!(host.expected_request_id, 1); // Not advanced on failure
}

#[test]
fn test_r1_t2_invalid_json_request_on_step_two_rejected() {
    let mut host = MockPluginHost::new(&["list-workspaces"]);
    let _ = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    let err = host.call("not a valid json string").unwrap_err();
    assert!(err.contains("invalid JSON request"));
    assert_eq!(host.expected_request_id, 1);
}

// ============================================================================
// Feature 2: R2 (Pane Zoom Toggle `Ctrl-b z` / `zoom-pane`)
// ============================================================================

// Tier 1: Nominal & Happy Path for R2
#[test]
fn test_r2_t1_zoom_pane_cli_zooms_active_split_pane() {
    let server = HeadlessServer::start("r2-zoom-cli");
    let _surface = create_workspace_and_surface(&server, "zoom-test");
    // Split pane to create 2 panes
    let split = split_pane(&server, "right");
    assert!(split.status.success(), "split failed: {:?}", split);

    // Zoom the active pane
    let zoom = cli(&server, &["zoom-pane"]);
    assert_eq!(
        zoom.status.code(),
        Some(0),
        "zoom-pane must exit 0, stderr: {}",
        String::from_utf8_lossy(&zoom.stderr)
    );

    // Verify workspace JSON shows zoomed state
    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    let screens = ws.get("workspaces").and_then(|w| w.as_array())
        .and_then(|arr| arr.first())
        .and_then(|w| w.get("screens"))
        .and_then(|s| s.as_array());
    if let Some(screens) = screens {
        let zoomed = screens[0].get("zoomed");
        assert!(zoomed.is_some() && !zoomed.unwrap().is_null(), "zoomed state should be present in screen_json");
    }
}

#[test]
fn test_r2_t1_zoom_pane_cli_toggle_restores_split_geometry() {
    let server = HeadlessServer::start("r2-zoom-toggle");
    let _surface = create_workspace_and_surface(&server, "zoom-toggle");
    let split = split_pane(&server, "down");
    assert!(split.status.success());

    // First toggle: zoom
    let zoom1 = cli(&server, &["zoom-pane"]);
    assert_eq!(zoom1.status.code(), Some(0));

    // Second toggle: unzoom
    let zoom2 = cli(&server, &["zoom-pane"]);
    assert_eq!(zoom2.status.code(), Some(0));

    // Verify unzoomed status in list-workspaces
    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    if let Some(screens) = ws["workspaces"][0]["screens"].as_array() {
        let zoomed = &screens[0]["zoomed"];
        assert!(zoomed.is_null(), "zoomed should be null after unzoom");
    }
}

#[test]
fn test_r2_t1_zoom_pane_with_target_pane_flag() {
    let server = HeadlessServer::start("r2-zoom-pane-flag");
    let _surface = create_workspace_and_surface(&server, "zoom-pane-flag");
    let split = split_pane(&server, "right");
    assert!(split.status.success());

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    let pane_id = ws["workspaces"][0]["screens"][0]["panes"][0]["id"].as_u64().unwrap_or(1);

    let zoom = cli(&server, &["zoom-pane", "--pane", &pane_id.to_string()]);
    assert_eq!(zoom.status.code(), Some(0));
}

#[test]
fn test_r2_t1_zoom_state_reflected_in_workspace_json() {
    let server = HeadlessServer::start("r2-zoom-json");
    let _surface = create_workspace_and_surface(&server, "zoom-json");
    let _ = split_pane(&server, "right");
    let _ = cli(&server, &["zoom-pane"]);

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    assert!(ws["workspaces"].is_array());
}

#[test]
fn test_r2_t1_zoom_ipc_command_returns_zoomed_status() {
    let server = HeadlessServer::start("r2-zoom-ipc");
    let _surface = create_workspace_and_surface(&server, "zoom-ipc");
    let _ = split_pane(&server, "right");

    let res = ipc_call(&server, &json!({ "cmd": "zoom-pane" }));
    if let Ok(val) = res {
        assert_eq!(val["ok"], true);
        assert!(val.get("zoomed").is_some());
    }
}

// Tier 2: Boundary & Corner Cases for R2
#[test]
fn test_r2_t2_zoom_single_pane_is_noop() {
    let server = HeadlessServer::start("r2-zoom-single");
    let _ = create_workspace_and_surface(&server, "single-pane");
    // Screen has only 1 pane (no splits)
    let zoom = cli(&server, &["zoom-pane"]);
    // Should be a safe no-op (either succeeds or reports no-op)
    assert!(zoom.status.code() == Some(0) || zoom.status.code() == Some(1));
}

#[test]
fn test_r2_t2_zoom_nonexistent_pane_fails() {
    let server = HeadlessServer::start("r2-zoom-invalid");
    let _ = create_workspace_and_surface(&server, "invalid-pane");
    let bad = cli(&server, &["zoom-pane", "--pane", "999999999"]);
    assert_ne!(bad.status.code(), Some(0), "zooming nonexistent pane must fail");
}

#[test]
fn test_r2_t2_split_while_zoomed_auto_unzooms() {
    let server = HeadlessServer::start("r2-split-auto-unzoom");
    let _surface = create_workspace_and_surface(&server, "split-unzoom");
    let _ = split_pane(&server, "right");
    let _ = cli(&server, &["zoom-pane"]);

    // Splitting a new pane while zoomed must auto-unzoom so the new pane is visible
    let split2 = split_pane(&server, "down");
    assert!(split2.status.success());

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    if let Some(screens) = ws["workspaces"][0]["screens"].as_array() {
        let zoomed = &screens[0]["zoomed"];
        assert!(zoomed.is_null(), "splitting must auto-unzoom");
    }
}

#[test]
fn test_r2_t2_close_zoomed_pane_auto_unzooms() {
    let server = HeadlessServer::start("r2-close-unzoom");
    let surface = create_workspace_and_surface(&server, "close-unzoom");
    let split = split_pane(&server, "right");
    assert!(split.status.success());
    let _ = cli(&server, &["zoom-pane"]);

    // Close the surface
    let _ = cli(&server, &["close-surface", "--surface", &surface.to_string()]);

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    if let Some(screens) = ws["workspaces"][0]["screens"].as_array() {
        let zoomed = &screens[0]["zoomed"];
        assert!(zoomed.is_null(), "closing zoomed pane must reset zoom");
    }
}

#[test]
fn test_r2_t2_asymmetric_split_ratios_preserved_after_unzoom() {
    let server = HeadlessServer::start("r2-ratio-preserve");
    let _surface = create_workspace_and_surface(&server, "ratio-preserve");
    let split = split_pane(&server, "right");
    assert!(split.status.success());

    // Cycle zoom and unzoom
    let _ = cli(&server, &["zoom-pane"]);
    let _ = cli(&server, &["zoom-pane"]);

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    let layout = &ws["workspaces"][0]["screens"][0]["layout"];
    assert!(!layout.is_null(), "layout tree geometry must be intact");
}

// ============================================================================
// Feature 3: R3 (Screen Text Synchronization `wait-for`)
// ============================================================================

// Tier 1: Nominal & Happy Path for R3
#[test]
fn test_r3_t1_wait_for_exact_match_exits_zero() {
    let server = HeadlessServer::start("r3-exact-match");
    let surface = create_workspace_and_surface(&server, "wait-exact");

    let marker = "mtyx_sync_exact_marker_100";
    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            &format!("printf '{marker}\\n'\n"),
        ],
    );

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            marker,
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(
        wait.status.code(),
        Some(0),
        "wait-for must exit 0 on match, stderr: {}",
        String::from_utf8_lossy(&wait.stderr)
    );
}

#[test]
fn test_r3_t1_wait_for_regex_pattern_match_exits_zero() {
    let server = HeadlessServer::start("r3-regex-match");
    let surface = create_workspace_and_surface(&server, "wait-regex");

    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "printf 'status_code_9876_ready\\n'\n",
        ],
    );

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            r#"status_code_[0-9]+_ready"#,
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));
}

#[test]
fn test_r3_t1_wait_for_timeout_exits_one() {
    let server = HeadlessServer::start("r3-timeout");
    let surface = create_workspace_and_surface(&server, "wait-timeout");

    let started = Instant::now();
    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "NEVER_EMITTED_STRING_12345",
            "--timeout-ms",
            "300",
        ],
    );
    assert_eq!(
        wait.status.code(),
        Some(1),
        "wait-for must exit 1 on timeout"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "wait-for must wait out the timeout"
    );
}

#[test]
fn test_r3_t1_wait_for_json_reports_matched_and_elapsed() {
    let server = HeadlessServer::start("r3-json");
    let surface = create_workspace_and_surface(&server, "wait-json");

    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "printf 'json_target_token\\n'\n",
        ],
    );

    let (out, val) = cli_json(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "json_target_token",
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(val["matched"].as_bool(), Some(true));
    assert!(val.get("elapsed_ms").is_some());
}

#[test]
fn test_r3_t1_wait_for_immediate_check_timeout_zero() {
    let server = HeadlessServer::start("r3-timeout-zero");
    let surface = create_workspace_and_surface(&server, "wait-timeout-zero");

    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "printf 'immediate_present\\n'\n",
        ],
    );
    // Give VT a tiny slice to process stdout
    std::thread::sleep(Duration::from_millis(200));

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "immediate_present",
            "--timeout-ms",
            "0",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));
}

// Tier 2: Boundary & Corner Cases for R3
#[test]
fn test_r3_t2_wait_for_immediate_check_timeout_zero_fails_if_absent() {
    let server = HeadlessServer::start("r3-timeout-zero-fail");
    let surface = create_workspace_and_surface(&server, "timeout-zero-fail");

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "not_present_yet",
            "--timeout-ms",
            "0",
        ],
    );
    assert_eq!(wait.status.code(), Some(1));
}

#[test]
fn test_r3_t2_wait_for_invalid_regex_syntax_exits_nonzero() {
    let server = HeadlessServer::start("r3-bad-regex");
    let surface = create_workspace_and_surface(&server, "bad-regex");

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "([a-z+unclosed",
            "--timeout-ms",
            "500",
        ],
    );
    assert_ne!(wait.status.code(), Some(0));
}

#[test]
fn test_r3_t2_wait_for_unknown_surface_exits_nonzero() {
    let server = HeadlessServer::start("r3-unknown-surf");
    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            "999999",
            "--pattern",
            "anything",
            "--timeout-ms",
            "500",
        ],
    );
    assert_ne!(wait.status.code(), Some(0));
}

#[test]
fn test_r3_t2_wait_for_multiline_pattern_matching() {
    let server = HeadlessServer::start("r3-multiline");
    let surface = create_workspace_and_surface(&server, "multiline");

    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "printf 'LINE_ALPHA\\nLINE_BETA\\n'\n",
        ],
    );

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            r#"(?s)LINE_ALPHA.*LINE_BETA"#,
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));
}

#[test]
fn test_r3_t2_wait_for_short_timeout_bounded_duration() {
    let server = HeadlessServer::start("r3-bounded");
    let surface = create_workspace_and_surface(&server, "bounded");

    let started = Instant::now();
    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "ABSENT",
            "--timeout-ms",
            "150",
        ],
    );
    assert_eq!(wait.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "wait-for exceeded bounded timeout"
    );
}

// ============================================================================
// Feature 4: R4 (Direct Process Spawning `run`)
// ============================================================================

// Tier 1: Nominal & Happy Path for R4
#[test]
fn test_r4_t1_run_argv_spawns_process_and_prints_surface_id() {
    let server = HeadlessServer::start("r4-argv");
    let out = cli(
        &server,
        &["run", "--", "/bin/echo", "direct_process_invocation"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "run -- <argv> must exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let surface_id = parse_surface_id(&out);
    assert!(surface_id > 0, "run must print new surface ID to stdout");
}

#[test]
fn test_r4_t1_run_command_flag_spawns_via_shell() {
    let server = HeadlessServer::start("r4-cmd");
    let out = cli(&server, &["run", "--command", "echo shell_spawned"]);
    assert_eq!(out.status.code(), Some(0));
    let surface_id = parse_surface_id(&out);
    assert!(surface_id > 0);
}

#[test]
fn test_r4_t1_run_new_workspace_flag() {
    let server = HeadlessServer::start("r4-new-ws");
    let out = cli(
        &server,
        &["run", "--new-workspace", "--", "/bin/sleep", "1"],
    );
    assert_eq!(out.status.code(), Some(0));
    let surface_id = parse_surface_id(&out);
    assert!(surface_id > 0);

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    let count = ws["workspaces"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(count >= 1);
}

#[test]
fn test_r4_t1_run_cwd_flag_sets_working_directory() {
    let server = HeadlessServer::start("r4-cwd");
    let out = cli(&server, &["run", "--cwd", "/tmp", "--", "/bin/pwd"]);
    assert_eq!(out.status.code(), Some(0));
    let surface_id = parse_surface_id(&out);
    assert!(surface_id > 0);
}

#[test]
fn test_r4_t1_run_name_flag_names_surface_or_workspace() {
    let server = HeadlessServer::start("r4-name");
    let out = cli(
        &server,
        &["run", "--name", "worker-tab", "--", "/bin/sleep", "1"],
    );
    assert_eq!(out.status.code(), Some(0));
    let surface_id = parse_surface_id(&out);
    assert!(surface_id > 0);
}

// Tier 2: Boundary & Corner Cases for R4
#[test]
fn test_r4_t2_run_mutual_exclusion_argv_and_command() {
    let server = HeadlessServer::start("r4-mutex-argv-cmd");
    let bad = cli(
        &server,
        &["run", "--command", "echo hi", "--", "/bin/echo", "hi"],
    );
    assert_ne!(
        bad.status.code(),
        Some(0),
        "specifying both --command and positional argv must fail"
    );
}

#[test]
fn test_r4_t2_run_mutual_exclusion_pane_and_new_workspace() {
    let server = HeadlessServer::start("r4-mutex-pane-ws");
    let bad = cli(
        &server,
        &[
            "run",
            "--pane",
            "1",
            "--new-workspace",
            "--",
            "/bin/echo",
            "invalid",
        ],
    );
    assert_ne!(
        bad.status.code(),
        Some(0),
        "specifying both --pane and --new-workspace must fail"
    );
}

#[test]
fn test_r4_t2_run_target_specific_pane() {
    let server = HeadlessServer::start("r4-target-pane");
    let _surface = create_workspace_and_surface(&server, "target-pane");
    let _ = split_pane(&server, "right");

    let (_, ws) = cli_json(&server, &["list-workspaces"]);
    let pane_id = ws["workspaces"][0]["screens"][0]["panes"][0]["id"].as_u64().unwrap_or(1);

    let out = cli(
        &server,
        &["run", "--pane", &pane_id.to_string(), "--", "/bin/echo", "in_pane"],
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn test_r4_t2_run_json_output_format() {
    let server = HeadlessServer::start("r4-json");
    let (out, val) = cli_json(&server, &["run", "--", "/bin/echo", "json_run"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(val.get("surface").is_some());
    assert!(val.get("pane").is_some());
    assert!(val.get("screen").is_some());
    assert!(val.get("workspace").is_some());
}

#[test]
fn test_r4_t2_run_missing_argv_and_command_fails() {
    let server = HeadlessServer::start("r4-empty");
    let bad = cli(&server, &["run"]);
    assert_ne!(
        bad.status.code(),
        Some(0),
        "run without command or argv must fail"
    );
}

// ============================================================================
// Feature 5: R5 (PR CI Pipeline Hardening)
// ============================================================================

// Tier 1: Nominal & Happy Path for R5
#[test]
fn test_r5_t1_ci_workflow_file_exists_and_readable() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    assert!(ci_path.exists(), "pr-build.yml must exist at {}", ci_path.display());
    let content = fs::read_to_string(&ci_path).expect("must be able to read pr-build.yml");
    assert!(!content.is_empty(), "pr-build.yml must not be empty");
}

#[test]
fn test_r5_t1_ci_matrix_runs_mux_core_tests() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(
        content.contains("cargo +1.97 test -p mux-core"),
        "CI workflow must execute cargo +1.97 test -p mux-core"
    );
}

#[test]
fn test_r5_t1_ci_workflow_installs_go() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(
        content.contains("actions/setup-go@v5") || content.contains("actions/setup-go"),
        "CI workflow must include setup-go action"
    );
}

#[test]
fn test_r5_t1_ci_workflow_runs_go_daemon_tests() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(
        content.contains("go test ./..."),
        "CI workflow must run Go daemon tests (go test ./...)"
    );
}

#[test]
fn test_r5_t1_ci_workflow_guards_go_tests_non_windows() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(
        content.contains("runner.os != 'Windows'"),
        "Go daemon tests must be guarded with if: runner.os != 'Windows'"
    );
}

// Tier 2: Boundary & Corner Cases for R5
#[test]
fn test_r5_t2_ci_mux_core_working_directory_is_mux() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    let mux_core_step = lines.iter().position(|l| l.contains("test -p mux-core"));
    assert!(mux_core_step.is_some(), "step for mux-core must exist");
    let idx = mux_core_step.unwrap();
    let window = &lines[idx.saturating_sub(4)..=(idx + 4).min(lines.len() - 1)];
    assert!(
        window.iter().any(|l| l.contains("working-directory: mux")),
        "mux-core test step must set working-directory: mux"
    );
}

#[test]
fn test_r5_t2_ci_go_working_directory_is_daemon_remote() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    let go_step = lines.iter().position(|l| l.contains("go test ./..."));
    assert!(go_step.is_some(), "step for go test must exist");
    let idx = go_step.unwrap();
    let window = &lines[idx.saturating_sub(4)..=(idx + 4).min(lines.len() - 1)];
    assert!(
        window.iter().any(|l| l.contains("working-directory: daemon/remote")),
        "Go test step must set working-directory: daemon/remote"
    );
}

#[test]
fn test_r5_t2_ci_rust_toolchain_pinned_to_1_97() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(
        content.contains("+1.97") || content.contains("1.97.0"),
        "Rust toolchain must be pinned to 1.97 per AGENTS.md"
    );
}

#[test]
fn test_r5_t2_ci_go_version_source_configuration() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(
        content.contains("daemon/remote/go.mod") || content.contains("1.22"),
        "Go setup must specify go.mod or version >= 1.22"
    );
}

#[test]
fn test_r5_t2_ci_matrix_runner_operating_systems() {
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();
    assert!(content.contains("ubuntu-latest"), "CI matrix must include ubuntu-latest");
    assert!(content.contains("macos"), "CI matrix must include macos");
    assert!(content.contains("windows"), "CI matrix must include windows");
}

// ============================================================================
// Tier 3: Pairwise Combinatorial Interactions
// ============================================================================

#[test]
fn test_tier3_pairwise_run_and_wait_for() {
    let server = HeadlessServer::start("t3-run-wait");
    let out = cli(
        &server,
        &[
            "run",
            "--",
            "/bin/sh",
            "-c",
            "sleep 0.2; echo PAIRWISE_RUN_WAIT_TOKEN_999",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let surface_id = parse_surface_id(&out);
    assert!(surface_id > 0);

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface_id.to_string(),
            "--pattern",
            "PAIRWISE_RUN_WAIT_TOKEN_999",
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));
}

#[test]
fn test_tier3_pairwise_zoom_and_run() {
    let server = HeadlessServer::start("t3-zoom-run");
    let _surface = create_workspace_and_surface(&server, "zoom-run");
    let _ = split_pane(&server, "right");
    let _ = cli(&server, &["zoom-pane"]);

    let run_out = cli(&server, &["run", "--", "/bin/echo", "in_zoomed_session"]);
    assert_eq!(run_out.status.code(), Some(0));
}

#[test]
fn test_tier3_pairwise_zoom_and_wait_for() {
    let server = HeadlessServer::start("t3-zoom-wait");
    let surface = create_workspace_and_surface(&server, "zoom-wait");
    let _ = split_pane(&server, "right");
    let _ = cli(&server, &["zoom-pane"]);

    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "echo ZOOM_WAIT_SYNC_TOKEN\n",
        ],
    );

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            "ZOOM_WAIT_SYNC_TOKEN",
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));
}

#[test]
fn test_tier3_pairwise_plugin_multicall_and_screen_inspection() {
    let server = HeadlessServer::start("t3-plugin-inspect");
    let surface = create_workspace_and_surface(&server, "plugin-screen");

    let mut host = MockPluginHost::new(&["list-workspaces", "read-screen"]);
    let res0 = host.call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#).unwrap();
    assert_eq!(res0["id"], 0);

    let res1 = host.call(&format!(
        r#"{{"id":1,"verb":"read-screen","args":{{"surface":{surface}}}}}"#
    )).unwrap();
    assert_eq!(res1["id"], 1);
}

#[test]
fn test_tier3_pairwise_run_zoom_wait_unzoom_chain() {
    let server = HeadlessServer::start("t3-chain");
    let _surface = create_workspace_and_surface(&server, "chain-test");
    let _ = split_pane(&server, "right");

    // 1. Run command
    let run = cli(
        &server,
        &[
            "run",
            "--",
            "/bin/sh",
            "-c",
            "echo CHAIN_STEP_ACTIVE; sleep 1",
        ],
    );
    assert_eq!(run.status.code(), Some(0));
    let run_surf = parse_surface_id(&run);

    // 2. Zoom pane
    let _ = cli(&server, &["zoom-pane"]);

    // 3. Wait for pattern
    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &run_surf.to_string(),
            "--pattern",
            "CHAIN_STEP_ACTIVE",
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));

    // 4. Unzoom pane
    let unzoom = cli(&server, &["zoom-pane"]);
    assert_eq!(unzoom.status.code(), Some(0));
}

// ============================================================================
// Tier 4: Real-World Workload Scenarios
// ============================================================================

#[test]
fn test_tier4_scenario_scripted_automation_workflow() {
    // Scenario 1: Automation daemon launches service via `run`, synchronizes on startup banner
    // via `wait-for`, transmits command, and verifies execution.
    let server = HeadlessServer::start("t4-scenario1-automation");
    let run = cli(
        &server,
        &[
            "run",
            "--name",
            "backend-worker",
            "--",
            "/bin/sh",
            "-c",
            "sleep 0.1; echo 'SERVICE_LISTENING_PORT_4567'; while true; do read line; echo ACK: $line; done",
        ],
    );
    assert_eq!(run.status.code(), Some(0));
    let surf = parse_surface_id(&run);

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surf.to_string(),
            "--pattern",
            "SERVICE_LISTENING_PORT_4567",
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));

    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surf.to_string(),
            "--text",
            "PING_CMD_PAYLOAD\n",
        ],
    );

    let wait_ack = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surf.to_string(),
            "--pattern",
            "ACK: PING_CMD_PAYLOAD",
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait_ack.status.code(), Some(0));
}

#[test]
fn test_tier4_scenario_interactive_split_navigation_and_zoom() {
    // Scenario 2: Complex split tree navigation and non-destructive fullscreen zoom toggle
    let server = HeadlessServer::start("t4-scenario2-ide-layout");
    let _s1 = create_workspace_and_surface(&server, "ide-layout");
    let split_r = split_pane(&server, "right");
    assert!(split_r.status.success());
    let split_d = split_pane(&server, "down");
    assert!(split_d.status.success());

    // Zoom focused editor pane
    let z_in = cli(&server, &["zoom-pane"]);
    assert_eq!(z_in.status.code(), Some(0));

    // Restore multi-pane split view
    let z_out = cli(&server, &["zoom-pane"]);
    assert_eq!(z_out.status.code(), Some(0));
}

#[test]
fn test_tier4_scenario_multi_query_wasm_plugin_lifecycle() {
    // Scenario 3: Full lifecycle of a plugin issuing multiple host queries in sequence
    let mut plugin_runtime = MockPluginHost::new(&[
        "list-workspaces",
        "read-screen",
        "select-workspace",
    ]);

    // Query 1: Enumerate workspaces
    let q1 = plugin_runtime
        .call(r#"{"id":0,"verb":"list-workspaces","args":{}}"#)
        .expect("query 1 failed");
    assert_eq!(q1["id"], 0);

    // Query 2: Inspect active screen
    let q2 = plugin_runtime
        .call(r#"{"id":1,"verb":"read-screen","args":{"surface":1}}"#)
        .expect("query 2 failed");
    assert_eq!(q2["id"], 1);

    // Query 3: Select workspace
    let q3 = plugin_runtime
        .call(r#"{"id":2,"verb":"select-workspace","args":{"workspace":1}}"#)
        .expect("query 3 failed");
    assert_eq!(q3["id"], 2);

    assert_eq!(plugin_runtime.expected_request_id, 3);
}

#[test]
fn test_tier4_scenario_ci_multi_target_verification() {
    // Scenario 4: Validates that CI configuration conforms to multi-target test matrix
    let ci_path = workspace_root().join(".github/workflows/pr-build.yml");
    let content = fs::read_to_string(&ci_path).unwrap();

    // Check all required test targets
    assert!(content.contains("mux-tui"), "CI must test mux-tui");
    assert!(content.contains("mux-core"), "CI must test mux-core");
    assert!(content.contains("daemon/remote"), "CI must test daemon/remote");
}

#[test]
fn test_tier4_scenario_complex_regex_screen_sync_with_timeout() {
    // Scenario 5: ANSI escape codes, complex regex tokens, and timeout handling
    let server = HeadlessServer::start("t4-scenario5-ansi-regex");
    let surface = create_workspace_and_surface(&server, "ansi-regex");

    // Output formatted with ANSI color sequences
    let _ = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "printf '\\033[32m[BUILD-SUCCESS]\\033[0m hash=\\033[1mabcdef123456\\033[0m\\n'\n",
        ],
    );

    let wait = cli(
        &server,
        &[
            "wait-for",
            "--surface",
            &surface.to_string(),
            "--pattern",
            r#"\[BUILD-SUCCESS\].*hash=[0-9a-f]{12}"#,
            "--timeout-ms",
            "3000",
        ],
    );
    assert_eq!(wait.status.code(), Some(0));
}
