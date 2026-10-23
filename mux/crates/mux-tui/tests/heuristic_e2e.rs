#![cfg(unix)] // real PTY daemon + /bin/sh fake-agent fixtures

//! Issue #140 AC1 end-to-end evidence: with `[[agent_detection]]
//! poll_interval_ms = 200` the daemon's heuristic poller watches a fake
//! pi pane through its working→idle transition and the sidebar's data
//! source (`list-workspaces` → `tabs[].agent_state`/`agent_source`)
//! shows both states with `source: "heuristic"`, within the 5s budget
//! and with no client attached. A bare `/bin/sh` pane in the same
//! session stays `agent_state: null` (no chip spam). Harness copied
//! from `tests/cli.rs`'s `HeadlessServer` (only the opt-in-with-config
//! constructor is needed here; `cli.rs`'s default `start` pins the
//! poller OFF for determinism).

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct HeadlessServer {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
}

impl HeadlessServer {
    fn start_with_config(name: &str, contents: &str) -> Self {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mux.sock");
        let config = dir.join("mux.toml");
        fs::write(&config, contents).unwrap();
        let child = Command::new(bin())
            .args(["--headless", "--socket"])
            .arg(&socket)
            .env("MTYX_MUX_CONFIG", &config)
            .env("XDG_STATE_HOME", &dir)
            // Dash, not bash: CLI tests only need printf/echo/sleep.
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
            if self.socket.exists() && std::os::unix::net::UnixStream::connect(&self.socket).is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("headless server did not create socket at {}", self.socket.display());
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

fn unique_temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    PathBuf::from("/tmp").join(format!("mtyx-heuristic-{name}-{}-{stamp}", std::process::id()))
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mtyx")
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success, got status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The tab record for `surface` in `--json list-workspaces` — the exact
/// object the TUI's `parse_tree` consumes for the sidebar chip.
fn list_tab(server: &HeadlessServer, surface: u64) -> serde_json::Value {
    let out = cli(server, &["--json", "list-workspaces"]);
    assert_success(&out);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    value["workspaces"]
        .as_array()
        .expect("workspaces array")
        .iter()
        .flat_map(|ws| ws["screens"].as_array().into_iter().flatten())
        .flat_map(|screen| screen["panes"].as_array().into_iter().flatten())
        .flat_map(|pane| pane["tabs"].as_array().into_iter().flatten())
        .find(|tab| tab["surface"].as_u64() == Some(surface))
        .cloned()
        .unwrap_or_else(|| panic!("surface {surface} not in list-workspaces"))
}

/// Poll the sidebar's data source until the tab matches `(state,
/// source)`, or panic with the last observed tab.
fn wait_for_tab_state(
    server: &HeadlessServer,
    surface: u64,
    state: &str,
    source: &str,
    budget: Duration,
) {
    let deadline = Instant::now() + budget;
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        last = list_tab(server, surface);
        if last["agent_state"].as_str() == Some(state)
            && last["agent_source"].as_str() == Some(source)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "surface {surface} never reached agent_state={state:?} agent_source={source:?} within {:?}; last tab: {last}",
        budget
    );
}

/// Issue #140 AC1: a transitioning fake pi pane — working marker, then a
/// screen redraw with the resting prompt — shows working→idle in the
/// sidebar data source with `source: "heuristic"` (the poller's tier),
/// inside the 5s freshness budget. AC2 ride-along: nothing here installs
/// hooks, so the heuristic tier is exactly what a zero-config pane gets.
/// The bare `/bin/sh` pane never gets a report (generic+idle is not
/// publishable), and `detect-agents` reports the heuristic source.
#[test]
fn heuristic_poller_e2e_working_then_idle_in_sidebar_data() {
    let server = HeadlessServer::start_with_config(
        "e2e",
        "[[agent_detection]]\npoll_interval_ms = 200\n",
    );

    // The agent pane: pi identity marker + working footer on screen,
    // then (after a sleep long enough for several 200ms polls) a full
    // redraw showing the resting prompt. The prompt carries text after
    // it because the plain-text formatter trims trailing whitespace.
    let workspace = cli(&server, &["new-workspace", "--name", "agent"]);
    assert_success(&workspace);
    let surface: u64 = String::from_utf8(workspace.stdout).unwrap().trim().parse().unwrap();

    let send = cli(
        &server,
        &[
            "send",
            "--surface",
            &surface.to_string(),
            "--text",
            "printf 'pi> Working… (esc to interrupt)\\n'; sleep 2; printf '\\033[2J\\033[Hpi> ready\\n'\n",
        ],
    );
    assert_success(&send);

    wait_for_tab_state(&server, surface, "working", "heuristic", Duration::from_secs(5));
    wait_for_tab_state(&server, surface, "idle", "heuristic", Duration::from_secs(5));

    // The report the poller published is the one the agent verbs see:
    // `wait-agent-status` matches the cached idle report.
    let wait = cli(
        &server,
        &[
            "wait-agent-status",
            "--target",
            &surface.to_string(),
            "--status",
            "idle",
            "--timeout",
            "2000",
        ],
    );
    assert_success(&wait);

    // AC3: `detect-agents` carries the additive detections map with
    // source "heuristic" for the pane.
    let detect = cli(&server, &["--json", "detect-agents"]);
    assert_success(&detect);
    let value: serde_json::Value = serde_json::from_slice(&detect.stdout).unwrap();
    let id = surface.to_string();
    assert_eq!(
        value["detections"][&id]["source"].as_str(),
        Some("heuristic"),
        "detect-agents detections map: {value}"
    );
    assert_eq!(value["detections"][&id]["agent"].as_str(), Some("pi"));
    assert_eq!(value["detections"][&id]["state"].as_str(), Some("idle"));
    // The legacy name map is unchanged.
    assert_eq!(value["agents"][&id].as_str(), Some("pi"));

    // The negative half (AC1 "bare panes"): a plain /bin/sh pane in the
    // same session, polled at the same 200ms, stays report-less —
    // generic+idle never publishes, so no chip spam on non-agent panes.
    let bare = cli(&server, &["new-workspace", "--name", "bare"]);
    assert_success(&bare);
    let bare_surface: u64 = String::from_utf8(bare.stdout).unwrap().trim().parse().unwrap();
    // Give the poller several ticks to (wrongly) publish, if it would.
    let marker = cli(
        &server,
        &[
            "send",
            "--surface",
            &bare_surface.to_string(),
            "--text",
            "printf 'bare-shell-marker\\n'\n",
        ],
    );
    assert_success(&marker);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let out = cli(&server, &["read-screen", "--surface", &bare_surface.to_string()]);
        assert_success(&out);
        if String::from_utf8_lossy(&out.stdout).contains("bare-shell-marker") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(700));
    let tab = list_tab(&server, bare_surface);
    assert_eq!(
        tab["agent_state"],
        serde_json::Value::Null,
        "bare shell must not get a heuristic report, got {tab}"
    );
}
