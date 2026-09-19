//! Screen-derived agent-state classification (issue #96).
//!
//! `agent_detect.rs` (issue #78) answers *which* agent runs in a pane
//! from process + screen evidence. This module answers the next
//! question: given the pane's visible text, what lifecycle state is the
//! agent in? It exists because the only screen-derived
//! [`AgentStateSource::Detected`](crate::AgentStateSource) report today
//! is the OSC-9 notification watcher in `surface.rs`, which maps *any*
//! notification to `Blocked` — so a pane parked at a `y/n` confirmation
//! prompt was not reliably classified `blocked` without a hook.
//!
//! ## Conservative bias (the whole point)
//!
//! A false `blocked` refuses a valid send once #93's gating lands; a
//! false `idle` merely loses automation convenience. The classifier
//! therefore only ever returns [`AgentState::Blocked`] or
//! [`AgentState::Working`] when a marker is **unambiguous**, and falls
//! back to `Idle`/`Unknown` otherwise. It is a pure function of
//! `(agent, screen)` with no I/O, so it is cheap to run on every
//! detection pass and trivially testable against golden fixtures.
//!
//! ## Marker semantics
//!
//! Markers reuse the [`agent_detect`](crate::agent_detect) substring/globs
//! style (`*` wildcards), deliberately NOT regex: no workspace crate
//! links `regex`, and every marker the issue names is a literal.

use crate::agent_detect::text_matches;
use crate::AgentState;

/// What a pane's visible text can tell us about an agent's state.
///
/// Ordered by strength so the classifier can take the strongest signal
/// when a screen draws several markers at once (e.g. a confirmation
/// dialog that still shows the working spinner's footer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateSignal {
    /// Nothing recognisable.
    Unknown,
    /// A resting prompt — the agent is waiting for the user, not
    /// blocked on a permission dialog.
    Idle,
    /// A busy indicator (spinner, `esc to interrupt`).
    Working,
    /// An interactive confirmation the agent is waiting on.
    Blocked,
}

impl StateSignal {
    pub fn to_state(self) -> AgentState {
        match self {
            StateSignal::Blocked => AgentState::Blocked,
            StateSignal::Working => AgentState::Working,
            StateSignal::Idle => AgentState::Idle,
            StateSignal::Unknown => AgentState::Unknown,
        }
    }

    fn rank(self) -> u8 {
        match self {
            StateSignal::Unknown => 0,
            StateSignal::Idle => 1,
            StateSignal::Working => 2,
            StateSignal::Blocked => 3,
        }
    }
}

/// A single screen marker carrying a state signal for an agent.
///
/// `agent` is a registry name from `agent_detect/agents.json` plus the
/// synthetic `generic` bucket for "no detected agent"; `None` means the
/// marker applies to *every* agent (shared confirmation wording).
#[derive(Debug, Clone, Copy)]
struct StateMarker {
    agent: Option<&'static str>,
    signal: StateSignal,
    /// Substring with `*` wildcards (see `agent_detect::text_matches`).
    pattern: &'static str,
    case_insensitive: bool,
}

const fn m(agent: &'static str, signal: StateSignal, pattern: &'static str) -> StateMarker {
    StateMarker { agent: Some(agent), signal, pattern, case_insensitive: false }
}

const fn ci(agent: &'static str, signal: StateSignal, pattern: &'static str) -> StateMarker {
    StateMarker { agent: Some(agent), signal, pattern, case_insensitive: true }
}

/// Markers that apply to any agent identity (shared confirmation
/// wording only — a vague phrase like "confirm" alone would
/// false-positive on `blocked`).
const fn any(signal: StateSignal, pattern: &'static str) -> StateMarker {
    StateMarker { agent: None, signal, pattern, case_insensitive: true }
}

/// The marker table. Grouped by agent so a reviewer can see at a glance
/// what each agent's `blocked`/`working` evidence is; the fixture suite
/// (`tests/agent_state_fixtures.rs`) is the executable spec for it.
///
/// Ordering within a group does not matter — every marker is tested and
/// the strongest signal wins. Per-agent confirmation wordings are
/// verbatim where possible; they drift across agent versions, and an
/// unknown string degrades to Idle/Unknown, which is the safe direction.
static MARKERS: &[StateMarker] = &[
    // -------- Claude Code --------------------------------------------
    // Confirmation dialogs (`❯ 1. Yes` select + footer). Both current
    // and older footers are listed; the select cursor glyph is the
    // strongest single signal.
    m("claude", StateSignal::Blocked, "Enter to confirm"),
    m("claude", StateSignal::Blocked, "Esc to cancel"),
    m("claude", StateSignal::Blocked, "Do you want to proceed?"),
    m("claude", StateSignal::Blocked, "Do you want to make this edit?"),
    m("claude", StateSignal::Blocked, "❯ 1. Yes"),
    m("claude", StateSignal::Blocked, "❯ 1. Allow"),
    m("claude", StateSignal::Blocked, "Yes, and don't ask again"),
    m("claude", StateSignal::Blocked, "Allow this tool"),
    m("claude", StateSignal::Blocked, "Waiting for user confirmation"),
    // Busy indicators. `esc to interrupt` is the canonical one.
    ci("claude", StateSignal::Working, "esc to interrupt"),
    ci("claude", StateSignal::Working, "ctrl+c to interrupt"),
    m("claude", StateSignal::Working, "· Working…"),
    m("claude", StateSignal::Working, "Thinking…"),
    // The resting input prompt: a bare `> ` prompt in the boxed input
    // area. Deliberately last in the group and weak on its own —
    // `blocked`/`working` markers outrank it, so a dialog that still
    // shows the prompt underneath stays blocked.
    m("claude", StateSignal::Idle, "│ > "),
    m("claude", StateSignal::Idle, "> Try \""),
    m("claude", StateSignal::Idle, "? for shortcuts"),
    // -------- Pi ------------------------------------------------------
    // The interactive TUI's `pi> ` prompt at rest is IDLE, never
    // blocked (the issue calls this out explicitly). Only an explicit
    // confirmation dialog counts as blocked.
    m("pi", StateSignal::Idle, "pi> "),
    m("pi", StateSignal::Blocked, "Enter to confirm"),
    m("pi", StateSignal::Blocked, "Esc to cancel"),
    m("pi", StateSignal::Blocked, "❯ Yes"),
    m("pi", StateSignal::Blocked, "Allow this"),
    m("pi", StateSignal::Blocked, "Do you want to proceed?"),
    m("pi", StateSignal::Blocked, "Approve this action"),
    ci("pi", StateSignal::Working, "esc to interrupt"),
    m("pi", StateSignal::Working, "Thinking…"),
    // -------- Codex ---------------------------------------------------
    // Codex's exec-approval prompt. `Allow command?` is the canonical
    // wording; the `y/n` footer is a secondary corroboration.
    m("codex", StateSignal::Blocked, "Allow command?"),
    m("codex", StateSignal::Blocked, "Allow execution of"),
    m("codex", StateSignal::Blocked, "Do you want to run"),
    m("codex", StateSignal::Blocked, "❯ Yes"),
    m("codex", StateSignal::Blocked, "Yes (y)"),
    ci("codex", StateSignal::Blocked, "allow command"),
    m("codex", StateSignal::Idle, "codex>"),
    ci("codex", StateSignal::Working, "esc to interrupt"),
    m("codex", StateSignal::Working, "Thinking…"),
    m("codex", StateSignal::Working, "Working…"),
    // -------- Grok ----------------------------------------------------
    m("grok", StateSignal::Blocked, "Allow command?"),
    m("grok", StateSignal::Blocked, "Do you want to allow"),
    m("grok", StateSignal::Blocked, "❯ Yes"),
    m("grok", StateSignal::Blocked, "Enter to confirm"),
    m("grok", StateSignal::Blocked, "Esc to cancel"),
    ci("grok", StateSignal::Blocked, "permission"),
    ci("grok", StateSignal::Working, "esc to interrupt"),
    m("grok", StateSignal::Working, "Thinking…"),
    // -------- opencode / cursor / aider (registry names) --------------
    // Minimal: these agents report state via their hooks, so screen
    // classification only needs the shared confirmation wording to be
    // useful. No agent-specific block markers are invented here.
    ci("opencode", StateSignal::Working, "esc to interrupt"),
    ci("cursor", StateSignal::Working, "esc to interrupt"),
    ci("aider", StateSignal::Working, "esc to interrupt"),
    m("aider", StateSignal::Blocked, "Enter to confirm"),
    m("aider", StateSignal::Blocked, "Esc to cancel"),
    // -------- generic / unknown pane ----------------------------------
    // A plain shell must NEVER be blocked. Shell resting prompts are
    // idle; absence of evidence is unknown. There is deliberately no
    // generic `blocked` marker: an unrecognised pane degrades to
    // Idle/Unknown, which is the conservative direction.
    m("generic", StateSignal::Idle, "$ "),
    m("generic", StateSignal::Idle, "# "),
    m("generic", StateSignal::Idle, "% "),
    m("generic", StateSignal::Idle, "❯ "),
    m("generic", StateSignal::Idle, "λ "),
    // Shared confirmations apply to any *identified* agent: the footer
    // wording is agent-agnostic and unambiguous, but an unidentified
    // pane (a plain shell) must never be classified blocked from screen
    // text alone, so `None` here means "any bucket except generic".
    // Claude/Codex/Pi each also list the wording explicitly, so this
    // row is belt-and-braces for agent names we do not enumerate.
    any(StateSignal::Blocked, "Enter to confirm · Esc to cancel"),
    any(StateSignal::Blocked, "Enter to confirm"),
    any(StateSignal::Blocked, "Esc to cancel"),
];

/// The agent bucket a name belongs to. Unknown / empty names map to
/// `generic`, matching `detect-agent`'s `"unknown"` sentinel.
fn bucket(agent: &str) -> &str {
    if agent.is_empty() || agent == "unknown" {
        "generic"
    } else {
        agent
    }
}

/// Classify a pane's visible text into an [`AgentState`] for `agent`
/// (a registry name from `agent_detect/agents.json`, or `"unknown"` /
/// `""` for a pane with no detected agent).
///
/// Returns `Idle`/`Unknown` unless an unambiguous `Blocked` or `Working`
/// marker is present. Blocked outranks Working outranks Idle when one
/// screen draws several markers — a confirmation dialog is the strongest
/// possible evidence, and a working spinner's `esc to interrupt` footer
/// must not mask it.
pub fn classify_agent_state(agent: &str, screen: &str) -> AgentState {
    classify_signal(agent, screen).to_state()
}

/// The same classification, exposing the [`StateSignal`] so callers can
/// apply the "only lower to blocked/working" policy via
/// [`is_actionable`] without re-deriving it from the state.
pub fn classify_signal(agent: &str, screen: &str) -> StateSignal {
    if screen.is_empty() {
        return StateSignal::Unknown;
    }
    let bucket = bucket(agent);
    let mut best = StateSignal::Unknown;
    for marker in MARKERS {
        // `None` = any identified agent (never `generic`); `Some(name)`
        // = only that bucket.
        if let Some(name) = marker.agent {
            if name != bucket {
                continue;
            }
        } else if bucket == "generic" {
            continue;
        }
        if text_matches(screen, marker.pattern, marker.case_insensitive)
            && marker.signal.rank() > best.rank()
        {
            best = marker.signal;
        }
    }
    // Note: an explicit per-agent `Idle` marker is returned as-is. Only
    // `blocked`/`working` are ever *published* as a `Detected` report
    // (see [`is_actionable`]), so an idle classification here cannot
    // overwrite a hook's `Idle` — it is informational to the caller.
    best
}

/// Whether a signal is strong enough to publish a `Detected`-tier
/// report. Only `Blocked`/`Working` qualify: those are the states the
/// classifier can *improve*, and the conservative bias forbids letting a
/// screen observation downgrade a hook/socket report to `Idle`/`Done`.
pub fn is_actionable(signal: StateSignal) -> bool {
    matches!(signal, StateSignal::Blocked | StateSignal::Working)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_maps_unknown_and_empty_to_generic() {
        assert_eq!(bucket(""), "generic");
        assert_eq!(bucket("unknown"), "generic");
        assert_eq!(bucket("claude"), "claude");
    }

    #[test]
    fn unknown_agent_never_reaches_an_agent_specific_blocker() {
        // "Do you want to proceed?" is a Claude/Pi blocker; a generic pane
        // showing the same words (e.g. echoed by a shell) must stay idle.
        assert_eq!(
            classify_agent_state("generic", "Do you want to proceed?\n$ "),
            AgentState::Idle
        );
        assert_eq!(classify_agent_state("unknown", "Do you want to proceed?"), AgentState::Unknown);
        assert_eq!(classify_agent_state("claude", "Do you want to proceed?"), AgentState::Blocked);
    }

    #[test]
    fn strongest_signal_wins_regardless_of_marker_order() {
        // A screen that shows the working footer AND the confirmation
        // dialog is blocked, not working.
        let screen = "Thinking… (esc to interrupt)\nDo you want to proceed?\n❯ 1. Yes\n";
        assert_eq!(classify_agent_state("claude", screen), AgentState::Blocked);
        assert_eq!(classify_signal("claude", screen), StateSignal::Blocked);
    }

    #[test]
    fn empty_screen_is_unknown_for_every_agent() {
        for agent in ["claude", "pi", "codex", "grok", "generic", "unknown", ""] {
            assert_eq!(classify_agent_state(agent, ""), AgentState::Unknown, "agent {agent:?}");
        }
    }
}
