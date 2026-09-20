//! Issue #96: golden agent-state fixtures + screen-based state classifier.
//!
//! Data-driven suite: every case in
//! `tests/fixtures/agent_state/manifest.json` names a screen capture, the
//! agent running in the pane, and the state the classifier must return.
//! The classifier itself is a pure function
//! (`mux_core::agent_state_classify::classify_agent_state`), so this needs
//! no PTY and no terminal emulator — the fixtures are literal screen
//! text.
//!
//! Why here (mux-core) rather than mux-tui: the classifier lives in
//! `mux-core`, and the plan's `cargo test -p mux-tui` note predates that
//! placement. `cargo test -p mux-core` is the natural home; mux-tui's own
//! integration gate still runs the wire-level `detect-agent` tests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mux_core::agent_state_classify::{classify_agent_state, classify_signal, is_actionable};
use mux_core::AgentState;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_state")
}

fn read_fixture(name: &str) -> String {
    let path = fixture_dir().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()))
}

fn parse_state(s: &str) -> AgentState {
    AgentState::parse(s).unwrap_or_else(|| panic!("unknown expected state {s:?}"))
}

/// The whole golden-fixture table. Any regression in the marker tables
/// shows up here as a named failure with the exact screen text.
#[test]
fn golden_fixtures_classify_expected_state() {
    let manifest = read_fixture("manifest.json");
    let manifest: Value = serde_json::from_str(&manifest).expect("manifest.json parses");
    let cases = manifest["cases"].as_array().expect("manifest has a cases array");
    assert!(cases.len() >= 10, "expected a broad fixture set, got {}", cases.len());

    let mut failures = Vec::new();
    for case in cases {
        let file = case["file"].as_str().expect("case.file");
        let agent = case["agent"].as_str().expect("case.agent");
        let expected = parse_state(case["expected"].as_str().expect("case.expected"));
        let screen = read_fixture(file);
        let got = classify_agent_state(agent, &screen);
        if got != expected {
            failures.push(format!(
                "{file} (agent {agent:?}): expected {expected:?}, got {got:?}\n--- screen ---\n{screen}\n--- end ---"
            ));
        }
    }
    assert!(failures.is_empty(), "fixture mismatches:\n{}", failures.join("\n\n"));
}

/// The manifest must not silently lose coverage: every expected state is
/// represented, every named agent has at least one case, and every
/// fixture file on disk is referenced.
#[test]
fn manifest_covers_every_state_agent_and_fixture_file() {
    let manifest: Value =
        serde_json::from_str(&read_fixture("manifest.json")).expect("manifest.json parses");
    let cases = manifest["cases"].as_array().unwrap();

    let states: BTreeSet<&str> = cases.iter().filter_map(|c| c["expected"].as_str()).collect();
    assert_eq!(
        states,
        ["blocked", "idle", "working"].into_iter().collect(),
        "manifest must cover blocked, idle and working"
    );

    let agents: BTreeSet<&str> = cases.iter().filter_map(|c| c["agent"].as_str()).collect();
    for required in ["claude", "pi", "codex", "grok", "generic"] {
        assert!(agents.contains(required), "manifest needs a {required} case");
    }

    let referenced: BTreeSet<String> =
        cases.iter().filter_map(|c| c["file"].as_str().map(String::from)).collect();
    let on_disk: BTreeSet<String> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir readable")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            (name.ends_with(".screen")).then_some(name)
        })
        .collect();
    assert_eq!(referenced, on_disk, "manifest and fixture directory disagree");
}

/// AC2, explicit: Claude Code's confirmation footer → `blocked`. This is
/// the case the issue names by hand.
#[test]
fn claude_enter_to_confirm_footer_is_blocked() {
    let screen = read_fixture("claude__confirm_enter_to_confirm.screen");
    assert!(
        screen.contains("Enter to confirm · Esc to cancel"),
        "fixture must carry the current Claude footer verbatim"
    );
    assert_eq!(classify_agent_state("claude", &screen), AgentState::Blocked);
}

/// AC2, explicit: Pi's interactive TUI at rest is `idle`, NOT `blocked`.
/// The issue calls this out because the naive "any interactive prompt is
/// blocked" rule misfires here.
#[test]
fn pi_interactive_tui_at_rest_is_not_blocked() {
    let screen = read_fixture("pi__idle_prompt_at_rest.screen");
    let state = classify_agent_state("pi", &screen);
    assert_ne!(state, AgentState::Blocked, "pi at rest must never classify blocked");
    assert_eq!(state, AgentState::Idle);
}

/// A plain shell is never `blocked`, even when its *output* happens to
/// contain confirmation wording — the classifier must not be fooled by
/// scrollback echoing the exact phrase a dialog would draw.
#[test]
fn generic_shell_is_never_blocked_even_with_confirm_wording_in_scrollback() {
    let screen = read_fixture("generic__shell_echoes_confirm_wording.screen");
    assert_eq!(classify_agent_state("generic", &screen), AgentState::Idle);
    assert_eq!(classify_agent_state("unknown", &screen), AgentState::Idle);
    assert_eq!(classify_agent_state("", &screen), AgentState::Idle);
}

/// Blocked outranks working on a single screen: a confirmation dialog
/// drawn over a spinner footer is still blocked.
#[test]
fn blocked_outranks_working_when_both_markers_present() {
    let screen = "✻ Thinking… (esc to interrupt)\nDo you want to proceed?\n❯ 1. Yes\nEnter to confirm · Esc to cancel\n";
    assert_eq!(classify_agent_state("claude", screen), AgentState::Blocked);
}

/// Conservative bias: unknown/empty screens and unrecognised text never
/// produce `blocked`.
#[test]
fn unknown_screen_text_never_blocks() {
    assert_eq!(classify_agent_state("claude", ""), AgentState::Unknown);
    assert_eq!(classify_agent_state("pi", ""), AgentState::Unknown);
    assert_eq!(
        classify_agent_state("claude", "just some unrelated terminal output\n"),
        AgentState::Unknown
    );
    assert_eq!(
        classify_agent_state("codex", "compile warning: unused variable\n"),
        AgentState::Unknown
    );
}

/// The `Detected`-tier publish gate: only blocked/working are actionable,
/// so a screen observation can never downgrade a hook/socket report to
/// idle/unknown.
#[test]
fn only_blocked_and_working_signals_are_actionable() {
    assert!(is_actionable(classify_signal("claude", "Do you want to proceed?")));
    assert!(is_actionable(classify_signal("claude", "esc to interrupt")));
    assert!(!is_actionable(classify_signal("generic", "$ ")));
    assert!(!is_actionable(classify_signal("pi", "pi> ")));
    assert!(!is_actionable(classify_signal("claude", "")));
}
