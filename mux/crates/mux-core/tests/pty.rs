#![cfg(unix)] // exercises unix PTY, /proc and AF_UNIX machinery

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ghostty_vt::RenderState;
use mux_core::platform::transport;
use mux_core::{
    AgentState, AgentStateSource, AttachFrame, DefaultColors, Mux, MuxEvent, Rgb, SplitDir,
    SurfaceOptions,
};

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

fn connect(path: &Path) -> Box<dyn transport::Stream> {
    transport::connect(path).unwrap()
}

fn read_json_line(reader: &mut impl BufRead) -> Option<serde_json::Value> {
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

/// Default surfaces must be a login shell (argv0 starts with `-`).
/// CrowdStrike Falcon IOA GenReverseShell kills a bare `/bin/bash`
/// attached to a PTY from an unsigned parent; login-shell argv0 is how
/// real terminals spawn and is what we have to match.
#[test]
#[cfg(target_os = "linux")]
fn default_shell_is_spawned_as_login_shell() {
    // Dash, not the user's bash: this test only checks argv0, and a
    // bare interactive bash is exactly what Falcon GenReverseShell kills.
    let _guard = PERSIST_ENV_LOCK.lock().unwrap();
    let previous_shell = std::env::var("SHELL").ok();
    std::env::set_var("SHELL", "/bin/sh");
    let mux = Mux::new(unique_session("test-login-shell"), SurfaceOptions::default());
    let surface = mux.new_workspace(None, None).unwrap();
    let pid = surface.child_pid().expect("pty child pid");
    // portable-pty fork+exec: /proc/pid/cmdline still shows the test
    // binary until execve lands. Wait for the login-shell argv0.
    let argv0 = wait_for(
        || {
            let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            let argv0 = bytes.split(|&b| b == 0).next().unwrap_or_default();
            let argv0 = String::from_utf8_lossy(argv0);
            argv0.starts_with('-').then(|| argv0.into_owned())
        },
        Duration::from_secs(3),
    );
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<exe unreadable: {e}>"));
    let alive = mux_core::process::is_alive(pid);
    assert!(
        argv0.is_some(),
        "default shell must be a login shell (argv0 starts with '-'), \
         pid={pid} alive={alive} comm={comm:?} exe={exe}"
    );
    mux.close_surface(surface.id);
    match previous_shell {
        Some(shell) => std::env::set_var("SHELL", shell),
        None => std::env::remove_var("SHELL"),
    }
}

#[test]
fn surface_runs_command_and_screen_updates() {
    let mux = Mux::new("test-pty", shell_opts("printf 'marker-42\\n'; sleep 30"));
    let events = mux.subscribe();
    let surface = mux.new_workspace(None, None).unwrap();

    // Output event arrives...
    let got = wait_for(
        || {
            events
                .try_iter()
                .find(|e| matches!(e, MuxEvent::SurfaceOutput(id) if *id == surface.id))
        },
        Duration::from_secs(10),
    );
    assert!(got.is_some(), "no SurfaceOutput event");

    // ...and the ghostty-backed screen contains the marker.
    let text = wait_for(
        || {
            let text = surface.with_terminal(|t| t.plain_text()).unwrap().unwrap();
            text.contains("marker-42").then_some(text)
        },
        Duration::from_secs(10),
    );
    assert!(text.is_some(), "marker never appeared on screen");

    mux.close_surface(surface.id);
}

#[test]
fn osc9_notification_from_real_pty_output_sets_detected_agent_state() {
    let mux = Mux::new(
        unique_session("test-osc9"),
        shell_opts("printf '\\033]9;Build failed\\007'; sleep 30"),
    );
    let events = mux.subscribe();
    let surface = mux.new_workspace(None, None).unwrap();

    let notification = wait_for(
        || {
            events.try_iter().find_map(|e| match e {
                MuxEvent::OscNotification { surface: id, title, body } if id == surface.id => {
                    Some((title, body))
                }
                _ => None,
            })
        },
        Duration::from_secs(10),
    );
    assert_eq!(notification, Some(("".to_string(), "Build failed".to_string())));

    let agents = mux.list_agents(Some(surface.id), None);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].1.state, AgentState::Blocked);
    assert_eq!(agents[0].1.source, AgentStateSource::Detected);

    // A detected report must not override an existing hook report - the
    // opposite direction of the authority rule covered in mux.rs's unit
    // tests, verified here against the real detection path.
    mux.report_agent(surface.id, AgentState::Working, AgentStateSource::Hook, None, None, None);
    let mux2 = Mux::new(
        unique_session("test-osc9-hook-priority"),
        shell_opts("printf '\\033]9;again\\007'; sleep 30"),
    );
    let events2 = mux2.subscribe();
    let surface2 = mux2.new_workspace(None, None).unwrap();
    mux2.report_agent(surface2.id, AgentState::Working, AgentStateSource::Hook, None, None, None);
    assert!(
        wait_for(
            || events2.try_iter().find(|e| matches!(e, MuxEvent::OscNotification { .. })),
            Duration::from_secs(10),
        )
        .is_some(),
        "notification event should still fire even when detection can't change agent state"
    );
    let agents2 = mux2.list_agents(Some(surface2.id), None);
    assert_eq!(
        agents2[0].1.state,
        AgentState::Working,
        "hook report must survive a later detection"
    );
    assert_eq!(agents2[0].1.source, AgentStateSource::Hook);

    mux.close_surface(surface.id);
    mux2.close_surface(surface2.id);
}

#[test]
fn surface_resize_reports_whether_the_size_changed() {
    let mux = Mux::new(unique_session("test-resize-bool"), shell_opts("sleep 30"));
    let surface = mux.new_workspace(None, Some((80, 24))).unwrap();

    assert!(!surface.resize(80, 24));
    assert_eq!(surface.size(), (80, 24));
    assert!(surface.resize(100, 40));
    assert_eq!(surface.size(), (100, 40));
    assert!(!surface.resize(100, 40));
    assert!(surface.resize(0, 0));
    assert_eq!(surface.size(), (1, 1));
    assert!(!surface.resize(0, 0));

    mux.close_surface(surface.id);
}

/// Issue #99: a surface spawned with no explicit size and no client
/// attached uses the 120x40 headless default (adapting to a
/// `MTYX_MUX_VT_SIZE` override when one is exported), an explicit
/// `SurfaceOptions` geometry (what mux.json `headless.vt_size` layers on
/// in `run_server`) wins, and a later attach-style resize still moves
/// the surface cleanly.
#[test]
fn headless_spawn_uses_default_geometry_and_attach_resize_still_works() {
    let expected = std::env::var("MTYX_MUX_VT_SIZE")
        .ok()
        .and_then(|value| mux_core::parse_vt_size(&value))
        .unwrap_or((120, 40));

    // Default: no size passed down the spawn path.
    let mux = Mux::new(unique_session("test-headless-geometry"), shell_opts("sleep 30"));
    let surface = mux.new_workspace(None, None).unwrap();
    assert_eq!(surface.size(), expected, "headless default geometry");

    // Override: explicit geometry yields exactly that size.
    let opts = SurfaceOptions {
        command: Some(vec!["/bin/cat".to_string()]),
        cols: 100,
        rows: 30,
        ..Default::default()
    };
    let mux2 = Mux::new(unique_session("test-headless-geometry-override"), opts);
    let surface2 = mux2.new_workspace(None, None).unwrap();
    assert_eq!(surface2.size(), (100, 30), "explicit vt_size override");

    // Attach-style resize from the default geometry still applies.
    assert!(surface.resize(80, 50));
    assert_eq!(surface.size(), (80, 50));

    mux.close_surface(surface.id);
    mux2.close_surface(surface2.id);
}

#[test]
fn surface_exit_reaps_tree_and_emits_event() {
    let opts =
        SurfaceOptions { command: Some(vec!["/usr/bin/true".to_string()]), ..Default::default() };
    let mux = Mux::new("test-exit", opts);
    let events = mux.subscribe();
    let surface = mux.new_workspace(None, None).unwrap();

    let got = wait_for(
        || {
            events
                .try_iter()
                .find(|e| matches!(e, MuxEvent::SurfaceExited(id) if *id == surface.id))
        },
        Duration::from_secs(10),
    );
    assert!(got.is_some(), "no SurfaceExited event");
    assert!(surface.is_dead());
    // The mux reaps exited surfaces itself; the emptied workspace is gone.
    let reaped = wait_for(
        || mux.with_state(|s| s.workspaces.is_empty().then_some(())),
        Duration::from_secs(10),
    );
    assert!(reaped.is_some(), "exited surface not reaped from tree");
}

#[test]
fn control_socket_round_trip() {
    let mux =
        Mux::new(unique_session("test-sock"), shell_opts("printf 'socket-check\\n'; sleep 30"));
    let surface = mux.new_workspace(None, None).unwrap();

    let sock_path = mux_core::server::serve(mux.clone(), None).unwrap();
    let stream = connect(&sock_path);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);

    let mut line = String::new();

    writeln!(writer, r#"{{"id":1,"cmd":"identify"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["app"], "mtyx");

    line.clear();
    writeln!(writer, r#"{{"id":2,"cmd":"list-workspaces"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true);
    let screen = &v["data"]["workspaces"][0]["screens"][0];
    assert_eq!(screen["panes"][0]["tabs"][0]["surface"], surface.id);
    assert_eq!(screen["active"], true);

    // Rename the workspace, its screen, and its pane over the socket.
    let ws_id = v["data"]["workspaces"][0]["id"].as_u64().unwrap();
    let screen_id = screen["id"].as_u64().unwrap();
    let pane_id = screen["panes"][0]["id"].as_u64().unwrap();
    let surface_id = screen["panes"][0]["tabs"][0]["surface"].as_u64().unwrap();
    for (id, cmd) in [
        (
            3,
            format!(
                r#"{{"id":3,"cmd":"rename-workspace","workspace":{ws_id},"name":"renamed-ws"}}"#
            ),
        ),
        (4, format!(r#"{{"id":4,"cmd":"rename-pane","pane":{pane_id},"name":"renamed-pane"}}"#)),
        (
            5,
            format!(
                r#"{{"id":5,"cmd":"rename-screen","screen":{screen_id},"name":"renamed-screen"}}"#
            ),
        ),
        (
            6,
            format!(
                r#"{{"id":6,"cmd":"rename-surface","surface":{surface_id},"name":"renamed-tab"}}"#
            ),
        ),
    ] {
        line.clear();
        writeln!(writer, "{cmd}").unwrap();
        reader.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true, "request {id} failed: {line}");
    }
    line.clear();
    writeln!(writer, r#"{{"id":7,"cmd":"list-workspaces"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["data"]["workspaces"][0]["name"], "renamed-ws");
    let screen = &v["data"]["workspaces"][0]["screens"][0];
    assert_eq!(screen["name"], "renamed-screen");
    assert_eq!(screen["panes"][0]["name"], "renamed-pane");
    assert_eq!(screen["panes"][0]["tabs"][0]["name"], "renamed-tab");

    // New tab in the pane: two tabs, second active.
    line.clear();
    writeln!(writer, r#"{{"id":8,"cmd":"new-tab","pane":{pane_id}}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "new-tab failed: {line}");
    let second_tab = v["data"]["surface"].as_u64().unwrap();

    line.clear();
    writeln!(
        writer,
        r#"{{"id":81,"cmd":"move-tab","surface":{surface_id},"pane":{pane_id},"index":2}}"#
    )
    .unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "move-tab failed: {line}");

    line.clear();
    writeln!(writer, r#"{{"id":82,"cmd":"list-workspaces"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tabs = v["data"]["workspaces"][0]["screens"][0]["panes"][0]["tabs"].as_array().unwrap();
    assert_eq!(tabs[0]["surface"], second_tab);
    assert_eq!(tabs[1]["surface"], surface_id);

    line.clear();
    writeln!(
        writer,
        r#"{{"id":83,"cmd":"move-tab","surface":{surface_id},"pane":{pane_id},"index":2}}"#
    )
    .unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "same-position move-tab failed: {line}");

    // Split and resize the split ratio over the socket.
    line.clear();
    writeln!(writer, r#"{{"id":9,"cmd":"split","pane":{pane_id},"dir":"right"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "split failed: {line}");

    line.clear();
    writeln!(writer, r#"{{"id":10,"cmd":"set-ratio","pane":{pane_id},"dir":"right","ratio":0.7}}"#)
        .unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "set-ratio failed: {line}");

    // New screen in the workspace: two screens, second active.
    line.clear();
    writeln!(writer, r#"{{"id":11,"cmd":"new-screen"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "new-screen failed: {line}");

    line.clear();
    writeln!(writer, r#"{{"id":11,"cmd":"list-workspaces"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let ws = &v["data"]["workspaces"][0];
    let pane = &ws["screens"][0]["panes"][0];
    assert_eq!(pane["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(pane["active_tab"], 1);
    let ratio = ws["screens"][0]["layout"]["ratio"].as_f64().unwrap();
    assert!((ratio - 0.7).abs() < 0.0001, "layout ratio was {ratio}");
    assert_eq!(ws["screens"].as_array().unwrap().len(), 2);
    assert_eq!(ws["screens"][1]["active"], true);

    line.clear();
    writeln!(writer, r#"{{"id":12,"cmd":"new-workspace","name":"second"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "new-workspace failed: {line}");

    line.clear();
    writeln!(writer, r#"{{"id":13,"cmd":"move-workspace","workspace":{ws_id},"index":2}}"#)
        .unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "move-workspace failed: {line}");

    line.clear();
    writeln!(writer, r#"{{"id":14,"cmd":"list-workspaces"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let workspaces = v["data"]["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[1]["id"], ws_id);

    line.clear();
    writeln!(writer, r#"{{"id":15,"cmd":"move-workspace","workspace":{ws_id},"index":1}}"#)
        .unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "same-position move-workspace failed: {line}");

    // Wait for the marker to hit the screen, then read it over the socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        line.clear();
        writeln!(writer, r#"{{"id":12,"cmd":"read-screen","surface":{}}}"#, surface.id).unwrap();
        reader.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true, "read-screen failed: {line}");
        if v["data"]["text"].as_str().unwrap_or("").contains("socket-check") {
            break;
        }
        assert!(Instant::now() < deadline, "marker never visible via socket");
        std::thread::sleep(Duration::from_millis(50));
    }

    mux.close_workspace(ws_id);
    mux_core::server::cleanup(&sock_path);
}

#[test]
fn control_socket_set_default_colors_merges_fields() {
    let opts = SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() };
    let mux = Mux::new(format!("test-colors-{}", std::process::id()), opts);
    let sock_path = mux_core::server::serve(mux.clone(), None).unwrap();
    let stream = connect(&sock_path);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    writeln!(writer, r##"{{"id":1,"cmd":"set-default-colors","fg":"#010203"}}"##).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "set-default-colors failed: {line}");
    assert_eq!(
        mux.default_colors(),
        DefaultColors { fg: Some(Rgb { r: 1, g: 2, b: 3 }), bg: None }
    );

    line.clear();
    writeln!(writer, r##"{{"id":2,"cmd":"set-default-colors","bg":"#131415"}}"##).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true, "set-default-colors failed: {line}");
    assert_eq!(
        mux.default_colors(),
        DefaultColors {
            fg: Some(Rgb { r: 1, g: 2, b: 3 }),
            bg: Some(Rgb { r: 0x13, g: 0x14, b: 0x15 }),
        }
    );

    line.clear();
    writeln!(writer, r##"{{"id":3,"cmd":"set-default-colors","bg":"#bad"}}"##).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], false, "bad color unexpectedly accepted: {line}");

    mux_core::server::cleanup(&sock_path);
}

/// Issue #35: `send` with a known shell resets the pane's input buffer
/// (leading `\n`) before metacharacter-leading text, while `raw` (the
/// default) writes bytes verbatim. We run `/bin/cat` so the PTY echoes
/// exactly the bytes written, and read them back via the terminal.
#[test]
fn send_shell_sanitises_text_and_raw_passes_through() {
    let mux = Mux::new(
        unique_session("test-send-shell"),
        SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() },
    );
    let surface = mux.new_workspace(None, None).unwrap();
    let sock_path = mux_core::server::serve(mux.clone(), None).unwrap();
    let stream = connect(&sock_path);
    let mut writer = stream.try_clone_box().unwrap();
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    let mut send = |writer: &mut Box<dyn transport::Stream>,
                    id: u64,
                    shell: &str,
                    text: &str|
     -> serde_json::Value {
        writeln!(
            writer,
            r##"{{"id":{id},"cmd":"send","surface":{},"text":{},"shell":"{shell}"}}"##,
            surface.id,
            serde_json::to_string(text).unwrap()
        )
        .unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    // raw first so we can assert it got no leading newline.
    assert_eq!(send(&mut writer, 1, "raw", "$marker-raw\n")["ok"], true);
    assert_eq!(send(&mut writer, 2, "fish", "$marker-fish\n")["ok"], true);
    assert_eq!(send(&mut writer, 3, "bash", "$marker-bash\n")["ok"], true);

    // The terminal sees the echoed bytes: raw verbatim (first line, no
    // blank line before it), fish/bash with a leading newline (blank line
    // before the marker).
    let text = wait_for(
        || {
            let text = surface.with_terminal(|t| t.plain_text()).unwrap().unwrap();
            (text.contains("marker-raw")
                && text.contains("marker-fish")
                && text.contains("marker-bash"))
            .then_some(text)
        },
        Duration::from_secs(10),
    )
    .expect("markers never appeared");
    assert!(text.starts_with("$marker-raw"), "raw must be verbatim, got: {text:?}");
    assert!(text.contains("\n\n$marker-fish"), "fish needs a leading newline, got: {text:?}");
    assert!(text.contains("\n\n$marker-bash"), "bash needs a leading newline, got: {text:?}");

    mux_core::server::cleanup(&sock_path);
}

#[test]
fn control_socket_broadcasts_surface_resized_once_per_changed_size() {
    let mux = Mux::new(unique_session("test-resize-event"), shell_opts("sleep 30"));
    let surface = mux.new_workspace(None, Some((80, 24))).unwrap();

    let sock_path = mux_core::server::serve(mux.clone(), None).unwrap();
    let subscribe_stream = connect(&sock_path);
    subscribe_stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let mut subscribe_writer = subscribe_stream.try_clone_box().unwrap();
    let mut subscribe_reader = BufReader::new(subscribe_stream);

    let command_stream = connect(&sock_path);
    let mut command_writer = command_stream.try_clone_box().unwrap();
    let mut command_reader = BufReader::new(command_stream);

    writeln!(subscribe_writer, r#"{{"id":1,"cmd":"subscribe"}}"#).unwrap();
    let response = wait_for(|| read_json_line(&mut subscribe_reader), Duration::from_secs(5))
        .expect("subscribe response");
    assert_eq!(response["ok"], true, "subscribe failed: {response}");

    writeln!(
        command_writer,
        r#"{{"id":2,"cmd":"resize-surface","surface":{},"cols":103,"rows":29}}"#,
        surface.id
    )
    .unwrap();
    let mut line = String::new();
    command_reader.read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "resize failed: {line}");

    let event = wait_for(
        || {
            while let Some(value) = read_json_line(&mut subscribe_reader) {
                if value.get("event").and_then(|v| v.as_str()) == Some("surface-resized") {
                    return Some(value);
                }
            }
            None
        },
        Duration::from_secs(5),
    )
    .expect("no surface-resized event");
    assert_eq!(event["surface"], surface.id);
    assert_eq!(event["cols"], 103);
    assert_eq!(event["rows"], 29);
    assert_eq!(surface.size(), (103, 29));

    line.clear();
    writeln!(
        command_writer,
        r#"{{"id":3,"cmd":"resize-surface","surface":{},"cols":103,"rows":29}}"#,
        surface.id
    )
    .unwrap();
    command_reader.read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "repeated resize failed: {line}");

    let repeated = wait_for(
        || {
            while let Some(value) = read_json_line(&mut subscribe_reader) {
                if value.get("event").and_then(|v| v.as_str()) == Some("surface-resized") {
                    return Some(value);
                }
            }
            None
        },
        Duration::from_millis(300),
    );
    assert!(repeated.is_none(), "same-size resize emitted another event: {repeated:?}");

    mux.close_surface(surface.id);
    mux_core::server::cleanup(&sock_path);
}

#[test]
fn default_colors_apply_to_existing_and_future_surfaces() {
    let opts = SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() };
    let mux = Mux::new("test-default-colors", opts);
    let first = mux.new_workspace(None, None).unwrap();

    let colors = DefaultColors {
        fg: Some(Rgb { r: 0x01, g: 0x02, b: 0x03 }),
        bg: Some(Rgb { r: 0x13, g: 0x14, b: 0x15 }),
    };
    mux.set_default_colors(colors);

    let mut first_state = RenderState::new().unwrap();
    first.snapshot(&mut first_state).unwrap();
    assert_eq!(
        first_state.default_colors(),
        (Rgb { r: 0x13, g: 0x14, b: 0x15 }, Rgb { r: 0x01, g: 0x02, b: 0x03 })
    );

    let second = mux.new_tab(None, None, None).unwrap();
    let mut second_state = RenderState::new().unwrap();
    second.snapshot(&mut second_state).unwrap();
    assert_eq!(
        second_state.default_colors(),
        (Rgb { r: 0x13, g: 0x14, b: 0x15 }, Rgb { r: 0x01, g: 0x02, b: 0x03 })
    );

    mux.close_surface(first.id);
    mux.close_surface(second.id);
}

#[test]
fn attach_stream_replays_then_streams_without_duplication() {
    let mux = Mux::new(
        "test-attach",
        shell_opts(
            "printf 'before-attach\\n'; read line; printf 'after-%s\\n' \"$line\"; sleep 30",
        ),
    );
    let surface = mux.new_workspace(None, None).unwrap();

    // Wait until the pre-attach output landed in the terminal.
    let ok = wait_for(
        || {
            surface
                .with_terminal(|t| t.plain_text())
                .unwrap()
                .unwrap()
                .contains("before-attach")
                .then_some(())
        },
        Duration::from_secs(10),
    );
    assert!(ok.is_some());

    let attach = surface.attach_stream().unwrap();
    assert!(attach.cols > 0 && attach.rows > 0);

    // The replay reproduces pre-attach content in a fresh terminal.
    let mut mirror =
        ghostty_vt::Terminal::new(attach.cols, attach.rows, 1000, ghostty_vt::Callbacks::default())
            .unwrap();
    mirror.vt_write(&attach.replay);
    assert!(mirror.plain_text().unwrap().contains("before-attach"));

    // Post-attach output arrives on the stream, not duplicated in the
    // replay we already applied.
    surface.write_bytes(b"attach\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match attach.stream.recv_timeout(Duration::from_millis(200)) {
            Ok(AttachFrame::Output(chunk)) => {
                mirror.vt_write(&chunk);
                if mirror.plain_text().unwrap().contains("after-attach") {
                    break;
                }
            }
            Ok(AttachFrame::Resized { cols, rows, replay }) => {
                assert!(!replay.is_empty());
                mirror =
                    ghostty_vt::Terminal::new(cols, rows, 1000, ghostty_vt::Callbacks::default())
                        .unwrap();
                mirror.vt_write(&replay);
            }
            Err(_) => assert!(Instant::now() < deadline, "stream never delivered output"),
        }
    }
    let text = mirror.plain_text().unwrap();
    assert_eq!(text.matches("before-attach").count(), 1, "duplicated replay: {text}");

    mux.close_surface(surface.id);
}

#[test]
fn attach_stream_orders_resize_between_output_frames() {
    let mux = Mux::new(unique_session("test-attach-resize"), shell_opts("cat"));
    let surface = mux.new_workspace(None, None).unwrap();
    let attach = surface.attach_stream().unwrap();

    surface.write_bytes(b"before-resize\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match attach.stream.recv_timeout(Duration::from_millis(200)) {
            Ok(AttachFrame::Output(bytes))
                if bytes.windows(b"before-resize".len()).any(|w| w == b"before-resize") =>
            {
                break
            }
            Ok(_) => {}
            Err(_) => assert!(Instant::now() < deadline, "before output never arrived"),
        }
    }

    mux.resize_surface(surface.id, 100, 40).unwrap();
    let resized = wait_for(
        || match attach.stream.recv_timeout(Duration::from_millis(200)) {
            Ok(AttachFrame::Resized { cols, rows, replay }) => {
                assert!(!replay.is_empty());
                Some((cols, rows))
            }
            Ok(_) | Err(_) => None,
        },
        Duration::from_secs(5),
    )
    .expect("resize marker");
    assert_eq!(resized, (100, 40));

    surface.write_bytes(b"after-resize\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match attach.stream.recv_timeout(Duration::from_millis(200)) {
            Ok(AttachFrame::Output(bytes))
                if bytes.windows(b"after-resize".len()).any(|w| w == b"after-resize") =>
            {
                break
            }
            Ok(AttachFrame::Resized { .. }) => panic!("unexpected second resize marker"),
            Ok(_) => {}
            Err(_) => assert!(Instant::now() < deadline, "after output never arrived"),
        }
    }

    mux.close_surface(surface.id);
}

#[test]
fn new_tab_on_empty_headless_session_creates_workspace() {
    // A headless session receives new-tab before any workspace exists;
    // it must create a workspace around the new tab instead of panicking.
    let opts = SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() };
    let mux = Mux::new("test-headless", opts);
    let surface = mux.new_tab(None, None, None).unwrap();
    mux.with_state(|s| {
        assert_eq!(s.workspaces.len(), 1);
        assert_eq!(s.panes.len(), 1);
    });

    // Unknown pane ids error without leaking a surface.
    let before = mux.surface_count();
    assert!(mux.new_tab(Some(9999), None, None).is_err());
    assert_eq!(mux.surface_count(), before);

    mux.close_surface(surface.id);
}

/// `XDG_STATE_HOME` is process-global; tests that set it must not run
/// concurrently with each other.
static PERSIST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn restore_session_with_no_snapshot_is_a_silent_noop() {
    let _guard = PERSIST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("mux-persist-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &dir);

    let mux = Mux::new(unique_session("persist-empty"), shell_opts("sleep 30"));
    mux.restore_session();
    mux.with_state(|s| assert_eq!(s.workspaces.len(), 0));

    std::env::remove_var("XDG_STATE_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_persists_layout_and_cwd_across_simulated_restart() {
    let _guard = PERSIST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("mux-persist-full-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &dir);

    let session = unique_session("persist-full");
    let custom_cwd = dir.to_str().unwrap().to_string();
    const WS_NAME: &str = "restored-ws";
    const PANE_NAME: &str = "restored-pane";
    const TAB_NAME: &str = "restored-tab";

    {
        let mux = Mux::new(session.clone(), shell_opts("sleep 30"));
        mux.enable_persistence();

        let surface0 = mux.new_workspace(Some(WS_NAME.to_string()), None).unwrap();
        let pane0 = mux.with_state(|s| s.pane_of(surface0.id).unwrap());
        let surface1 = mux.split(pane0, SplitDir::Right, None).unwrap();
        let pane1 = mux.with_state(|s| s.pane_of(surface1.id).unwrap());
        assert!(mux.set_ratio(pane0, SplitDir::Right, 0.3));
        assert!(mux.rename_pane(pane1, PANE_NAME.to_string()));

        // A second tab with an explicit, spawn-time cwd - reliable to
        // assert on later without depending on shell OSC 7 support.
        let extra_tab = mux.new_tab(Some(pane1), Some(custom_cwd.clone()), None).unwrap();
        assert!(mux.rename_surface(extra_tab.id, TAB_NAME.to_string()));
        mux.select_tab(Some(pane1), Some(1), None);

        // enable_persistence's background writer should pick up the
        // TreeChanged burst above on its own, debounced.
        let snapshot_path = mux_core::platform::session_snapshot_path(&session);
        assert!(
            wait_for(|| snapshot_path.exists().then_some(()), Duration::from_secs(5)).is_some(),
            "enable_persistence never wrote a snapshot"
        );

        mux.shutdown(); // also writes a final, guaranteed-fresh snapshot
    }

    // A fresh Mux for the same session name simulates the daemon
    // restarting: nothing here is shared with the instance above.
    let mux2 = Mux::new(session.clone(), shell_opts("sleep 30"));
    mux2.restore_session();

    let ws_name = mux2.with_state(|s| s.workspaces[0].name.clone());
    assert_eq!(ws_name, WS_NAME);

    let mut pane_ids = Vec::new();
    mux2.with_state(|s| s.workspaces[0].screens[0].root.pane_ids(&mut pane_ids));
    assert_eq!(pane_ids.len(), 2, "the split survived restore");

    let restored_pane1 = mux2.with_state(|s| {
        s.panes.values().find(|p| p.name.as_deref() == Some(PANE_NAME)).unwrap().id
    });
    let tabs = mux2.with_state(|s| s.panes[&restored_pane1].tabs.clone());
    assert_eq!(tabs.len(), 2, "the extra tab survived restore");
    assert_eq!(
        mux2.with_state(|s| s.panes[&restored_pane1].active_tab),
        1,
        "the active tab index survived restore"
    );

    let restored_extra_tab = mux2.surface(tabs[1]).unwrap();
    assert_eq!(restored_extra_tab.cwd().as_deref(), Some(custom_cwd.as_str()));
    assert_eq!(restored_extra_tab.name().as_deref(), Some(TAB_NAME));

    mux2.shutdown();
    std::env::remove_var("XDG_STATE_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #87: a snapshot written by `save` must be 0600 and its
/// `sessions/` dir 0700, whatever the ambient umask.
#[test]
#[cfg(unix)]
fn snapshot_file_is_0600_and_dir_is_0700() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = PERSIST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("mux-persist-perms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &dir);

    let session = unique_session("persist-perms");
    {
        let mux = Mux::new(session.clone(), shell_opts("sleep 30"));
        mux.enable_persistence();
        mux.new_workspace(Some("perms-ws".to_string()), None).unwrap();
        mux.shutdown(); // writes a final, guaranteed snapshot
    }

    let path = mux_core::platform::session_snapshot_path(&session);
    assert!(path.exists(), "snapshot missing at {}", path.display());
    let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(file_mode, 0o600, "snapshot file must be 0600, got {file_mode:o}");
    let dir_mode = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o7777;
    assert_eq!(dir_mode, 0o700, "sessions dir must be 0700, got {dir_mode:o}");

    std::env::remove_var("XDG_STATE_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #87: a snapshot whose mode was loosened to 0777 must be refused
/// whole — the restore launches nothing, leaving an empty tree.
#[test]
#[cfg(unix)]
fn restored_daemon_rejects_world_readable_state() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = PERSIST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("mux-persist-0777-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &dir);

    let session = unique_session("persist-0777");
    {
        let mux = Mux::new(session.clone(), shell_opts("sleep 30"));
        mux.enable_persistence();
        mux.new_workspace(Some("world-readable-ws".to_string()), None).unwrap();
        mux.shutdown();
    }

    let path = mux_core::platform::session_snapshot_path(&session);
    assert!(path.exists());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();

    let mux2 = Mux::new(session.clone(), shell_opts("sleep 30"));
    mux2.restore_session();
    mux2.with_state(|s| {
        assert_eq!(s.workspaces.len(), 0, "0777 snapshot must not be restored");
    });

    mux2.shutdown();
    std::env::remove_var("XDG_STATE_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #28 acceptance: spawning a background `sleep` under a pane shell,
/// then shutting down the mux, leaves zero leftover processes from that tree.
#[test]
#[cfg(target_os = "linux")]
fn shutdown_kills_background_grandchild_sleep() {
    let _ = mux_core::process::set_child_subreaper();

    // Shape: shell backgrounds sleep 999, then becomes sleep 998 so the
    // surface stays alive until mux.shutdown().
    let mux = Mux::new(
        unique_session("issue28-tree"),
        SurfaceOptions {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 999 & exec sleep 998".into()]),
            ..Default::default()
        },
    );
    let _surface = mux.new_workspace(None, None).unwrap();

    // Collect PIDs we care about (any sleep 998/999 that is a descendant
    // of this test process via the subreaper / surface child).
    let self_pid = std::process::id();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut watched: Vec<u32> = Vec::new();
    while Instant::now() < deadline {
        watched = mux_core::process::all_descendants(self_pid)
            .into_iter()
            .filter(|&pid| {
                std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                    .map(|c| c.contains("sleep") && (c.contains("999") || c.contains("998")))
                    .unwrap_or(false)
            })
            .collect();
        if !watched.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!watched.is_empty(), "expected sleep 998/999 descendant(s) before shutdown");

    mux.shutdown();

    let leftover: Vec<u32> =
        watched.into_iter().filter(|&pid| mux_core::process::is_alive(pid)).collect();
    assert!(leftover.is_empty(), "leftover sleep PIDs after mux.shutdown(): {leftover:?}");
}
