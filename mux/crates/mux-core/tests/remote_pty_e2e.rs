#![cfg(unix)] // exercises unix PTY, /proc and AF_UNIX machinery

//! Exercises `mux-core`'s remote-pty support (`remote_pty.rs`) against a
//! real SSH target. Needs infrastructure this crate's default `cargo test`
//! can't assume (a reachable sshd, key-based auth already set up, and a
//! `cmuxd-remote` binary built for that host), so every test here is a
//! silent no-op unless both env vars are set:
//!
//! ```text
//! MTYX_MUX_TEST_SSH_HOST=localhost \
//! MTYX_MUX_TEST_REMOTE_BIN=/path/to/cmuxd-remote \
//! cargo test -p mux-core --test remote_pty_e2e
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mux_core::{Mux, RemoteSpec, SurfaceOptions};

fn wait_for<T>(mut f: impl FnMut() -> Option<T>, timeout: Duration) -> Option<T> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn remote_spec(session_id: &str) -> Option<RemoteSpec> {
    let host = std::env::var("MTYX_MUX_TEST_SSH_HOST").ok()?;
    let bin = std::env::var("MTYX_MUX_TEST_REMOTE_BIN").ok()?;
    Some(RemoteSpec {
        host,
        slot: format!("mux-mux-test-{}", std::process::id()),
        session_id: session_id.to_string(),
        local_binary_path: PathBuf::from(bin),
    })
}

fn screen_text(surface: &mux_core::Surface) -> String {
    surface.with_terminal(|t| t.plain_text()).unwrap().unwrap()
}

#[test]
fn remote_workspace_runs_a_real_command_over_ssh() {
    let Some(spec) = remote_spec("e2e-fresh") else {
        eprintln!("skipping remote_pty_e2e: set MTYX_MUX_TEST_SSH_HOST/MTYX_MUX_TEST_REMOTE_BIN");
        return;
    };
    let mux = Mux::new("remote-e2e-fresh", SurfaceOptions::default());
    let surface = mux.new_remote_workspace(spec, Some("remote-ws".to_string()), None).unwrap();

    surface.write_bytes(b"echo REMOTE-PTY-HELLO\n").unwrap();
    let text = wait_for(
        || screen_text(&surface).contains("REMOTE-PTY-HELLO").then(|| screen_text(&surface)),
        Duration::from_secs(15),
    );
    assert!(text.is_some(), "remote command output never appeared");

    mux.with_state(|s| {
        assert_eq!(s.workspaces.len(), 1);
        assert_eq!(s.workspaces[0].name, "remote-ws");
    });

    mux.shutdown();
}

#[test]
fn remote_session_survives_detach_and_reattach_across_a_new_mux() {
    let Some(spec) = remote_spec("e2e-persist") else {
        eprintln!("skipping remote_pty_e2e: set MTYX_MUX_TEST_SSH_HOST/MTYX_MUX_TEST_REMOTE_BIN");
        return;
    };

    {
        let mux = Mux::new("remote-e2e-persist-a", SurfaceOptions::default());
        let surface = mux.new_remote_workspace(spec.clone(), None, None).unwrap();
        surface.write_bytes(b"export MUX_MUX_MARKER=e2e-42\n").unwrap();
        // Give the remote shell a moment to actually apply the export
        // before we detach (write_bytes only confirms the daemon queued
        // it, not that the shell finished processing it).
        wait_for(|| screen_text(&surface).contains('$').then_some(()), Duration::from_secs(10));
        // close_surface -> Surface::kill() -> ChildKiller::kill() -> a
        // remote surface sends pty.detach, not pty.close: the shell must
        // still be running after this for the whole test to mean anything.
        mux.close_surface(surface.id);
        mux.shutdown();
    }

    // A brand new Mux (simulating a full local process restart) with the
    // SAME session_id must reattach to the still-running remote shell.
    {
        let mux = Mux::new("remote-e2e-persist-b", SurfaceOptions::default());
        let surface = mux.new_remote_workspace(spec, None, None).unwrap();
        surface.write_bytes(b"echo MARKER=$MUX_MUX_MARKER\n").unwrap();
        let text = wait_for(
            || screen_text(&surface).contains("MARKER=e2e-42").then(|| screen_text(&surface)),
            Duration::from_secs(15),
        );
        assert!(
            text.is_some(),
            "reattached shell did not have the exported variable - session did not survive"
        );
        mux.close_surface(surface.id);
        mux.shutdown();
    }
}

/// `XDG_STATE_HOME` is process-global; only one test in this binary uses
/// it, but guard anyway in case that changes.
static PERSIST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn remote_workspace_survives_the_automatic_persist_and_restore_flow() {
    let Some(spec) = remote_spec("e2e-auto-persist") else {
        eprintln!("skipping remote_pty_e2e: set MTYX_MUX_TEST_SSH_HOST/MTYX_MUX_TEST_REMOTE_BIN");
        return;
    };
    let _guard = PERSIST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("mux-remote-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &dir);
    let session = format!("remote-auto-persist-{}", std::process::id());

    {
        let mux = Mux::new(session.clone(), SurfaceOptions::default());
        mux.enable_persistence();
        let surface =
            mux.new_remote_workspace(spec, Some("auto-remote".to_string()), None).unwrap();
        surface.write_bytes(b"export MUX_MUX_AUTO_MARKER=auto-99\n").unwrap();
        wait_for(|| screen_text(&surface).contains('$').then_some(()), Duration::from_secs(10));

        let snapshot_path = mux_core::platform::session_snapshot_path(&session);
        assert!(
            wait_for(|| snapshot_path.exists().then_some(()), Duration::from_secs(5)).is_some(),
            "enable_persistence never wrote a snapshot"
        );
        // No close_surface here - shutdown() (persistence-gated) writes
        // the final snapshot, and Drop on RemoteConn detaches, exactly
        // like a real crash/restart would (not a clean pane close).
        mux.shutdown();
    }

    // A fresh Mux for the same *mux-mux* session (not the same variable,
    // a brand new one) that never called new_remote_workspace itself -
    // restore_session must reconstruct the remote workspace on its own
    // from the snapshot alone.
    {
        let mux = Mux::new(session, SurfaceOptions::default());
        mux.restore_session();
        let (ws_name, surface_id) = mux.with_state(|s| {
            assert_eq!(s.workspaces.len(), 1);
            let ws = &s.workspaces[0];
            let pane = ws.screens[0].active_pane;
            (ws.name.clone(), s.panes[&pane].tabs[0])
        });
        assert_eq!(ws_name, "auto-remote");
        let surface = mux.surface(surface_id).unwrap();
        assert!(surface.remote_spec().is_some(), "restored tab should still be a remote surface");

        surface.write_bytes(b"echo MARKER=$MUX_MUX_AUTO_MARKER\n").unwrap();
        let text = wait_for(
            || screen_text(&surface).contains("MARKER=auto-99").then(|| screen_text(&surface)),
            Duration::from_secs(15),
        );
        assert!(text.is_some(), "automatic restore did not reattach to the surviving remote shell");

        mux.close_surface(surface.id);
        mux.shutdown();
    }

    std::env::remove_var("XDG_STATE_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}
