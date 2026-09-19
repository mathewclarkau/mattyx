//! Issue #88: receipted terminal input (input-ACK capability) over the
//! control socket — capability record, confirmed-send ordering, the
//! oversized-input cap, and the structured ACK-timeout error.

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
    let mut stream = connect(socket);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Wait until the PTY child's /proc cmdline argv0 equals `argv0` —
/// proves an `sh -c '…; exec X'` script reached the exec (so anything
/// before it, e.g. `stty -echo`, has run) before we send input whose
/// non-echo we depend on. Matching argv0 exactly (not a substring)
/// matters: `sh -c 'stty -echo; exec cat …'` already contains "cat" in
/// its cmdline BEFORE the exec.
#[cfg(target_os = "linux")]
fn wait_for_child_argv0(child_pid: u32, argv0: &str) {
    let ok = wait_for(
        || {
            let cmdline = std::fs::read(format!("/proc/{child_pid}/cmdline")).ok()?;
            let first = cmdline.split(|&b: &u8| b == 0).next().unwrap_or_default();
            let name = String::from_utf8_lossy(first).trim_start_matches('-').to_string();
            (name == argv0).then_some(())
        },
        Duration::from_secs(5),
    );
    assert!(ok.is_some(), "child {child_pid} never exec'd {argv0:?}");
}

/// Issue #88 AC1 (capability record): `identify` bumps the protocol to 7
/// and advertises `input-ack`, and the shared client-side gate accepts it.
#[test]
fn identify_advertises_input_ack_capability() {
    let mux = Mux::new(
        unique_session("issue88-ident"),
        SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() },
    );
    let _surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    let ident = rpc(&sock, json!({"id": 1, "cmd": "identify"}));
    assert_eq!(ident["ok"], json!(true), "identify failed: {ident}");
    assert_eq!(ident["data"]["protocol"].as_u64(), Some(7), "protocol must be 7: {ident}");
    assert_eq!(
        ident["data"]["capabilities"]["input-ack"],
        json!(true),
        "capability record: {ident}"
    );
    assert!(mux_core::server::identify_has_input_ack(&ident["data"]));
    assert!(mux_core::server::require_input_ack_capability(&ident["data"]).is_ok());

    mux_core::server::cleanup(&sock);
}

/// Issue #88 AC1 (negotiated gate, server round-trip): a confirmed send
/// gets a receipt against an echoing consumer. The pane runs a loop that
/// disables tty echo and only prints AFTER reading a line, so the receipt
/// proves real consumption — not the tty line-discipline echoing our
/// bytes back.
#[test]
fn confirmed_send_receives_receipt_after_child_consumption() {
    let mux = Mux::new(
        unique_session("issue88-receipt"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; while IFS= read -r line; do echo \"got:$line\"; done".to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    // Warm-up: an UNconfirmed send, then wait for its loop echo — proves
    // the read loop is live (stty ran) before we assert on receipts.
    let warm = rpc(&sock, json!({"id": 1, "cmd": "send", "surface": surface.id, "text": "warm\n"}));
    assert_eq!(warm["ok"], json!(true), "unconfirmed warm-up send failed: {warm}");
    let echoed = wait_for(
        || {
            let text = surface.with_terminal(|t| t.plain_text()).unwrap().unwrap();
            text.contains("got:warm").then_some(text)
        },
        Duration::from_secs(10),
    );
    assert!(echoed.is_some(), "read loop never echoed the warm-up line");

    // Confirmed send: response arrives only after the child printed, and
    // reports the receipt.
    let started = Instant::now();
    let confirmed = rpc(
        &sock,
        json!({"id": 2, "cmd": "send", "surface": surface.id, "text": "receipt\n", "confirm": true, "timeout_ms": 10_000}),
    );
    assert_eq!(confirmed["ok"], json!(true), "confirmed send failed: {confirmed}");
    assert_eq!(confirmed["data"]["confirmed"], json!(true));
    assert!(started.elapsed() >= Duration::from_millis(1));
    let echoed = wait_for(
        || {
            let text = surface.with_terminal(|t| t.plain_text()).unwrap().unwrap();
            text.contains("got:receipt").then_some(text)
        },
        Duration::from_secs(10),
    );
    assert!(echoed.is_some(), "confirmed line never echoed by the child");

    mux_core::server::cleanup(&sock);
}

/// Issue #88 AC1 (ACK ordering): concurrent confirmed sends to one
/// surface — submitted in a known order from separate connections —
/// resolve in submission order. Each send only receipts when the
/// echo-suppressed read loop consumes it, and the loop echoes `got:`
/// lines in the order it read them, so the screen order is the
/// consumption order. (The FIFO mechanism itself is pinned by
/// `ack_gate_advances_in_ticket_order_and_skips_abandoned` in
/// surface.rs.)
#[test]
fn input_ack_ordering() {
    let mux = Mux::new(
        unique_session("issue88-order"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; while IFS= read -r line; do echo \"got:$line\"; done".to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    // Warm-up (same as above): prove the read loop is live first.
    let warm = rpc(&sock, json!({"id": 1, "cmd": "send", "surface": surface.id, "text": "warm\n"}));
    assert_eq!(warm["ok"], json!(true));
    let live = wait_for(
        || {
            surface
                .with_terminal(|t| t.plain_text())
                .unwrap()
                .unwrap()
                .contains("got:warm")
                .then_some(())
        },
        Duration::from_secs(10),
    );
    assert!(live.is_some(), "read loop never became live");

    const SENDERS: usize = 4;
    let mut handles = Vec::new();
    for i in 0..SENDERS {
        let sock = sock.clone();
        let surface_id = surface.id;
        handles.push(std::thread::spawn(move || {
            let stream = connect(&sock);
            let mut writer = stream.try_clone_box().unwrap();
            let mut reader = BufReader::new(stream);
            writeln!(
                writer,
                r#"{{"id":1,"cmd":"send","surface":{surface_id},"text":"line-{i}\n","confirm":true,"timeout_ms":10000}}"#
            )
            .unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            (i, serde_json::from_str::<Value>(&line).unwrap())
        }));
        // Stagger submissions so ticket order (taken when the server
        // thread picks up the request) deterministically matches this
        // submission order.
        std::thread::sleep(Duration::from_millis(150));
    }
    for handle in handles {
        let (i, response) = handle.join().unwrap();
        assert_eq!(response["ok"], json!(true), "confirmed send {i} failed: {response}");
        assert_eq!(
            response["data"]["confirmed"],
            json!(true),
            "send {i} not receipted: {response}"
        );
    }

    let text = wait_for(
        || {
            let text = surface.with_terminal(|t| t.plain_text()).unwrap().unwrap();
            (0..SENDERS).all(|i| text.contains(&format!("got:line-{i}"))).then_some(text)
        },
        Duration::from_secs(10),
    )
    .expect("not every confirmed line was consumed");
    let positions: Vec<Option<usize>> =
        (0..SENDERS).map(|i| text.find(&format!("got:line-{i}"))).collect();
    let mut sorted = positions.clone();
    sorted.sort();
    assert!(
        sorted.windows(2).all(|w| w[0].unwrap() < w[1].unwrap()),
        "confirmed sends resolved out of submission order; positions {positions:?} in:\n{text}"
    );

    mux_core::server::cleanup(&sock);
}

/// Issue #88 AC3 (oversized input): a confirmed send above
/// MAX_CONFIRMED_SEND_BYTES is rejected up front with the structured
/// `oversized_input` code (machine `code` field AND a prefixed error
/// string), while the same size unconfirmed still succeeds — the
/// pre-#88 fire-and-forget path is unchanged and uncapped.
#[test]
fn oversized_input_rejected() {
    let mux = Mux::new(
        unique_session("issue88-oversize"),
        SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    let too_big = "x".repeat(mux_core::server::MAX_CONFIRMED_SEND_BYTES + 1);
    let confirmed = rpc(
        &sock,
        json!({"id": 1, "cmd": "send", "surface": surface.id, "text": too_big, "confirm": true, "timeout_ms": 500}),
    );
    assert_eq!(confirmed["ok"], json!(false), "oversized confirmed send must fail: {confirmed}");
    assert_eq!(confirmed["code"].as_str(), Some("oversized_input"), "structured code: {confirmed}");
    assert!(
        confirmed["error"].as_str().unwrap().contains("oversized_input"),
        "error string carries the code prefix: {confirmed}"
    );

    // Exactly at the cap is accepted (receipt via cat's echo); over the
    // cap unconfirmed still works — pre-#88 behavior unchanged.
    let at_cap = "y".repeat(mux_core::server::MAX_CONFIRMED_SEND_BYTES);
    let edge = rpc(
        &sock,
        json!({"id": 2, "cmd": "send", "surface": surface.id, "text": at_cap, "confirm": true, "timeout_ms": 10_000}),
    );
    assert_eq!(edge["ok"], json!(true), "cap-sized confirmed send failed: {edge}");

    let unconfirmed =
        rpc(&sock, json!({"id": 3, "cmd": "send", "surface": surface.id, "text": too_big}));
    assert_eq!(unconfirmed["ok"], json!(true), "unconfirmed sends stay uncapped: {unconfirmed}");

    mux_core::server::cleanup(&sock);
}

/// Issue #88 AC2/AC4 (ACK timeout): against a consumer that neither
/// echoes nor exits (`stty -echo; exec cat > /dev/null`), a confirmed
/// send fails with the structured `input_ack_timeout` code after the
/// requested timeout — the bytes were written (the pane is still
/// runnable) — and unconfirmed sends to the same pane return immediately.
#[test]
#[cfg(target_os = "linux")] // relies on /proc/<pid>/cmdline for the exec check
fn input_ack_timeout_is_structured_error() {
    let mux = Mux::new(
        unique_session("issue88-timeout"),
        SurfaceOptions {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "stty -echo; exec cat > /dev/null".to_string(),
            ]),
            ..Default::default()
        },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock = mux_core::server::serve(mux.clone(), None).unwrap();

    // stty -echo must be in effect before we send, or the line discipline
    // would echo the input and produce a (spurious) receipt.
    let pid = surface.child_pid().expect("pty child pid");
    wait_for_child_argv0(pid, "cat");

    let started = Instant::now();
    let confirmed = rpc(
        &sock,
        json!({"id": 1, "cmd": "send", "surface": surface.id, "text": "no-echo\n", "confirm": true, "timeout_ms": 400}),
    );
    assert_eq!(confirmed["ok"], json!(false), "confirmed send must time out: {confirmed}");
    assert_eq!(
        confirmed["code"].as_str(),
        Some("input_ack_timeout"),
        "structured code: {confirmed}"
    );
    assert!(
        confirmed["error"].as_str().unwrap().contains("input_ack_timeout"),
        "error string carries the code prefix: {confirmed}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "the reply must wait out the receipt timeout, took {:?}",
        started.elapsed()
    );

    // Fire-and-forget to the same silent pane returns immediately.
    let unconfirmed = rpc(
        &sock,
        json!({"id": 2, "cmd": "send", "surface": surface.id, "text": "still-silent\n"}),
    );
    assert_eq!(unconfirmed["ok"], json!(true), "unconfirmed send failed: {unconfirmed}");

    mux_core::server::cleanup(&sock);
}

/// Issue #88 AC1 (legacy daemon refusal): the client-side capability
/// gate refuses confirmed send against an identify payload without the
/// `input-ack` record (protocol 6), with the structured
/// `legacy_host_receipt_rejected` code and the --no-confirm remedy. (The
/// CLI wiring of this gate is exercised end-to-end in mux-tui's
/// tests/cli.rs against a fake v6 daemon.)
#[test]
fn legacy_host_receipt_rejected() {
    let legacy_identify = json!({
        "app": "mtyx", "version": "0.0.0", "protocol": 6,
        "session": "main", "pid": 12345,
    });
    let err = mux_core::server::require_input_ack_capability(&legacy_identify).unwrap_err();
    assert_eq!(err.code, "legacy_host_receipt_rejected");
    let message = err.to_string();
    assert!(message.starts_with("legacy_host_receipt_rejected:"), "{message}");
    assert!(message.contains("--no-confirm"), "must name the remedy: {message}");
    assert!(message.contains("protocol 6"), "{message}");

    // A daemon advertising the record passes; a v7 daemon without the
    // record is still refused (the record, not the number, is the gate).
    let capable = json!({ "app": "mtyx", "protocol": 7, "capabilities": { "input-ack": true } });
    assert!(mux_core::server::require_input_ack_capability(&capable).is_ok());
    let no_record = json!({ "app": "mtyx", "protocol": 7 });
    assert_eq!(
        mux_core::server::require_input_ack_capability(&no_record).unwrap_err().code,
        "legacy_host_receipt_rejected"
    );
}
