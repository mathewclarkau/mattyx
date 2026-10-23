#![cfg(unix)] // real PTY + /proc process evidence

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mux_core::{AgentState, AgentStateSource, Mux, SurfaceOptions};

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

/// Issue #140 AC1: the background heuristic poller watches a fake pi
/// pane through its working→idle transition and publishes a
/// `Heuristic`-tier report for BOTH states — idle included, because the
/// agent is identified (a bare shell stays unpublishable). AC2: once a
/// hook report lands, later poller ticks never downgrade it.
///
/// The fixture keeps a `pi> ` identity marker on screen in both phases
/// (the real TUI's prompt is always visible) and REDRAWS the screen
/// between states (`\033[2J\033[H`): the classifier ranks the strongest
/// marker anywhere in the viewport, so the working line must actually
/// leave the screen before the idle prompt can win. The redrawn
/// prompt carries text after it (`pi> ready`) because the plain-text
/// formatter trims trailing whitespace — a bare `pi> ` would read back
/// as `pi>` and match neither the detector nor the classifier.
#[test]
fn poller_publishes_working_then_idle_and_hooks_survive_later_ticks() {
    let mux = Mux::new(
        unique_session("issue140-poll"),
        shell_opts(
            "printf 'pi> Working… (esc to interrupt)\\n'; sleep 2; printf '\\033[2J\\033[Hpi> ready'; sleep 30",
        ),
    );
    let surface = mux.new_workspace(None, None).unwrap();
    mux.enable_heuristic_polling(Duration::from_millis(200));

    let working = wait_for(
        || {
            surface.agent_report().filter(|r| {
                r.state == AgentState::Working && r.source == AgentStateSource::Heuristic
            })
        },
        Duration::from_secs(5),
    )
    .expect("working/heuristic report within 5s of the marker appearing");
    assert_eq!(working.state, AgentState::Working);

    let idle = wait_for(
        || {
            surface.agent_report().filter(|r| {
                r.state == AgentState::Idle && r.source == AgentStateSource::Heuristic
            })
        },
        Duration::from_secs(5),
    )
    .expect("idle/heuristic report after the prompt redraw");
    assert_eq!(idle.state, AgentState::Idle);

    // AC2 live: a hook report wins over the heuristic tier, and later
    // poller ticks (the screen still shows `pi> `, so the poller keeps
    // classifying idle) must not downgrade it.
    mux.report_agent(surface.id, AgentState::Blocked, AgentStateSource::Hook, None, None, None)
        .unwrap();
    std::thread::sleep(Duration::from_millis(800)); // several poll intervals
    let report = surface.agent_report().expect("hook report applied");
    assert_eq!(report.state, AgentState::Blocked, "hook report must survive poller ticks");
    assert_eq!(report.source, AgentStateSource::Hook);

    mux.close_surface(surface.id);
}

/// Issue #140 AC1 (the negative half): a bare shell pane classifies
/// generic+idle, which the poller's publish policy never reports — no
/// chip spam on plain shells.
#[test]
fn poller_leaves_bare_shell_panes_unreported() {
    let mux = Mux::new(unique_session("issue140-bare"), shell_opts("printf '$ '; sleep 30"));
    let surface = mux.new_workspace(None, None).unwrap();
    mux.enable_heuristic_polling(Duration::from_millis(200));

    std::thread::sleep(Duration::from_millis(800));
    assert!(
        surface.agent_report().is_none(),
        "bare shell must not get a heuristic report, got {:?}",
        surface.agent_report()
    );

    mux.close_surface(surface.id);
}
