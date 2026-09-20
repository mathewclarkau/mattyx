//! Issue #86: peer authentication on the daemon control socket.
//!
//! The daemon authenticates each accepted control connection via
//! `SO_PEERCRED` (Linux) and default-denies anything that is not an exact
//! uid match (or root). These tests cover the positive path end-to-end
//! over a real AF_UNIX socket and the negative path at the decision/wire
//! seam.
//!
//! ## Why the foreign-uid test is not an end-to-end socket connection
//!
//! `SO_PEERCRED` reflects the *connecting* process's kernel-attested uid.
//! Producing a genuinely mismatched uid therefore requires running the
//! client under a second uid, which needs `CAP_SETUID` (root). The CI/dev
//! environment this suite runs in has a single unprivileged uid and no
//! way to escalate (the harness explicitly blocks privilege escalation),
//! so a real mismatched-uid connection is impossible here. Per the issue's
//! acceptance criteria we unit-test the pure decision function across the
//! full matrix — including the critical invariant that a *lookup error is
//! never a match* — and pin the exact structured denial the real handler
//! writes, using the same `server::peer_auth_denial_json` the handler
//! calls. The positive path is exercised over a real socket below.

#![cfg(unix)] // SO_PEERCRED is a unix/Linux surface

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mux_core::platform::{self, transport, PeerAuthDecision};
use mux_core::{server, Mux, SurfaceOptions};
use serde_json::{json, Value};

static SERIAL: AtomicU64 = AtomicU64::new(0);

fn scratch_socket(label: &str) -> PathBuf {
    let n = SERIAL.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("mtyx_peer_auth_{}_{}_{}", std::process::id(), n, label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir.join("session.sock")
}

fn rpc(socket: &Path, request: Value) -> Value {
    let stream = transport::connect(socket).expect("connect");
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    serde_json::from_str(&line).expect("parse response")
}

// ---------------------------------------------------------------------
// AC1: the same-uid path is unaffected — a real client connects and a
// verb succeeds.
// ---------------------------------------------------------------------

#[test]
fn same_uid_accepted() {
    let socket = scratch_socket("same_uid");
    // The decision for our own uid must be Accept, and the host must be a
    // credential-capable transport (Linux) for the check to be armed.
    let us = platform::daemon_uid().expect("unix host has a uid");
    assert!(
        platform::transport_supports_peer_creds(),
        "this test expects a credential-capable transport"
    );
    assert_eq!(platform::peer_auth_decision(Ok(us), Some(us), true), PeerAuthDecision::Accept);

    let opts = SurfaceOptions::default();
    let mux = Mux::new(&format!("peer-auth-same-{}", std::process::id()), opts);
    let bound = server::serve(mux.clone(), Some(socket.clone())).expect("serve");
    assert_eq!(bound, socket);

    // A real same-uid connection gets a normal, successful response.
    let identified = rpc(&socket, json!({"id": 1, "cmd": "identify"}));
    assert_eq!(identified["ok"], json!(true), "identify should succeed: {identified}");
    assert_eq!(identified["data"]["app"], json!("mtyx"));

    // A second, independent connection is also accepted (no one-shot
    // lockout / no hang from the peer check).
    let listed = rpc(&socket, json!({"id": 2, "cmd": "list-workspaces"}));
    assert_eq!(listed["ok"], json!(true), "list-workspaces should succeed: {listed}");

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(server::pid_path(&socket));
}

// ---------------------------------------------------------------------
// AC2: the negative path — a foreign uid is denied with the exact
// structured response and no verb is dispatched.
//
// (See the module docs: a real second-uid client is impossible here
// without CAP_SETUID, so this drives the pure decision the handler uses
// and pins the response the handler writes.)
// ---------------------------------------------------------------------

#[test]
fn foreign_uid_rejected_with_structured_response() {
    let us = platform::daemon_uid().expect("unix host has a uid");
    let foreign = if us == u32::MAX { us - 1 } else { us + 1 };

    // The decision: a foreign uid on a credential-capable transport is a
    // rejection that names the offending uid.
    let decision = platform::peer_auth_decision(Ok(foreign), Some(us), true);
    assert_eq!(decision, PeerAuthDecision::RejectForeign { uid: foreign });

    // The handler's own response builder produces the exact structured
    // denial: id null, ok false, "peer uid <N> rejected".
    let denial = server::peer_auth_denial_json(&decision).expect("foreign uid must deny");
    assert_eq!(denial["id"], Value::Null);
    assert_eq!(denial["ok"], json!(false));
    assert_eq!(denial["error"], json!(format!("peer uid {foreign} rejected")));
    // No `data`/`code` on a peer-auth denial.
    assert!(denial.get("data").is_none());
    assert!(denial.get("code").is_none());

    // CRITICAL invariant: a uid *lookup error* on a credential-capable
    // transport is NEVER treated as a match — it is a hard reject, even
    // though there is no uid to name.
    let lookup_err = std::io::Error::new(std::io::ErrorKind::Other, "getsockopt failed");
    let lookup_decision = platform::peer_auth_decision(Err(&lookup_err), Some(us), true);
    assert_ne!(lookup_decision, PeerAuthDecision::Accept);
    assert_eq!(lookup_decision, PeerAuthDecision::RejectLookupError);
    let lookup_denial = server::peer_auth_denial_json(&lookup_decision).expect("must deny");
    assert_eq!(lookup_denial["ok"], json!(false));

    // Root (uid 0) is the one permitted foreign uid (policy).
    assert_eq!(platform::peer_auth_decision(Ok(0), Some(us), true), PeerAuthDecision::Accept);

    // And a real same-uid connection on a live daemon still works — the
    // negative path does not break the positive one.
    let socket = scratch_socket("foreign_uid");
    let mux =
        Mux::new(&format!("peer-auth-foreign-{}", std::process::id()), SurfaceOptions::default());
    server::serve(mux.clone(), Some(socket.clone())).expect("serve");
    let ok = rpc(&socket, json!({"id": 1, "cmd": "identify"}));
    assert_eq!(ok["ok"], json!(true));
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(server::pid_path(&socket));
}

// ---------------------------------------------------------------------
// AC1/AC3: a stale socket (file present, nothing listening, dead pidfile)
// is not treated as live and `serve()` reclaims it rather than bailing.
// ---------------------------------------------------------------------

#[test]
fn stale_socket_rejected() {
    let socket = scratch_socket("stale");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

    // A stale socket dirent with no listener: not live.
    std::fs::write(&socket, b"not really a socket").unwrap();
    assert!(!server::is_session_socket_live(&socket), "a non-socket dirent is not live");

    // Create a real, unbound AF_UNIX socket file whose owner pidfile
    // points at a dead process: exists, not connectable -> not live.
    std::fs::remove_file(&socket).ok();
    {
        use std::os::unix::net::UnixListener;
        let l = UnixListener::bind(&socket).unwrap();
        drop(l); // closes the listener; the dirent remains (stale)
    }
    // A dead pid makes the liveness probe fail even if a connect raced.
    let pid_file = server::pid_path(&socket);
    std::fs::write(&pid_file, "999999\n").unwrap();
    assert!(
        !server::is_session_socket_live(&socket),
        "a socket whose pidfile names a dead process is not live"
    );

    // serve() must reclaim the stale socket instead of bailing.
    let mux =
        Mux::new(&format!("peer-auth-stale-{}", std::process::id()), SurfaceOptions::default());
    let bound = server::serve(mux.clone(), Some(socket.clone()))
        .expect("serve must reclaim a stale socket");
    assert_eq!(bound, socket);
    assert!(socket.exists());
    let identified = rpc(&socket, json!({"id": 1, "cmd": "identify"}));
    assert_eq!(identified["ok"], json!(true));

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&pid_file);
}

// ---------------------------------------------------------------------
// AC3: binding an explicit `--socket /tmp/x.sock` must NOT chmod the
// parent (/tmp, or any pre-existing dir we do not own) to 0700.
// ---------------------------------------------------------------------

#[test]
fn explicit_socket_does_not_chmod_foreign_parent() {
    use std::os::unix::fs::PermissionsExt;

    // Real /tmp check: the historical bug chmod'ed the parent of an
    // explicit `--socket /tmp/x.sock` to 0700. /tmp is root-owned and
    // world-writable (1777) here, so the bind must leave it untouched
    // (and must not error out because it could not chmod it).
    let tmp = PathBuf::from("/tmp");
    let before = std::fs::metadata(&tmp).map(|m| m.permissions().mode() & 0o7777).unwrap();
    let sock = tmp.join(format!(
        "mtyx-peer-auth-{}-{}.sock",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mux =
        Mux::new(&format!("peer-auth-bind-{}", std::process::id()), SurfaceOptions::default());
    // The bind itself may succeed or warn; either way it must not error
    // out because it could not chmod /tmp, and must not chmod /tmp.
    let _ = server::serve(mux.clone(), Some(sock.clone()));
    let after = std::fs::metadata(&tmp).map(|m| m.permissions().mode() & 0o7777).unwrap();
    assert_eq!(before, after, "/tmp permissions must be untouched by an explicit --socket bind");

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(server::pid_path(&sock));
}
