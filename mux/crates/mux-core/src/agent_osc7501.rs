//! OSC 7501 in-band agent status (issue #176): a coding-agent CLI can
//! self-report its lifecycle to the terminal itself by writing
//! `ESC ] 7501 ; state=<state>:app=<name> ESC \` (or BEL-terminated)
//! to the pane's pty, no hook installed. A bare pane running a
//! 7501-speaking agent therefore shows working/waiting/done like a
//! hook-instrumented one.
//!
//! This is a best-effort side channel exactly like [`crate::notify`]'s
//! desktop-notification watcher: it scans the raw pty bytes a second
//! time, in parallel with the authoritative VT parse
//! (`term.vt_write`), and a missed edge case here can never corrupt
//! or desync actual terminal rendering. libghostty has no 7501 command
//! slot (its OSC enum tops out at types 0–22 and exposes no
//! raw-OSC callback), so the raw payload is parsed on the Rust side.
//!
//! Contract (pinned by the unit tests below and the integration tests
//! under `tests/`):
//!
//! - `state` values: `working` → [`AgentState::Working`], `waiting` →
//!   [`AgentState::Blocked`], `done` → [`AgentState::Done`]. Any other
//!   value — including mtyx's own `idle`/`unknown`, which no 7501
//!   emitter sends — is unknown, and an unknown state yields NO report
//!   so the pane's existing state is never changed by it.
//! - `app` is the agent's self-chosen name and is optional: a missing
//!   or empty `app=` reports [`None`], preserving the pane's
//!   established agent name (see [`crate::Surface::set_agent_report`]).
//! - Unknown `key=value` fields are ignored (forwards compatibility).
//! - Malformed payloads (no `state` field, non-UTF-8) yield NO report.
//! - Reports are NOT deduplicated here: the agent is the emitter and
//!   re-reports naturally (`working → waiting → done`); every
//!   well-formed sequence produces a report, and the pane applies them
//!   in order via the existing equal-tier overwrite rule.
//!
//! The watcher is incremental: a sequence may be split across
//! arbitrarily many `feed` calls (arbitrary `read()` boundaries), and
//! both ST (`ESC \`) and BEL terminators are recognized.

use crate::AgentState;

/// A parsed OSC 7501 agent-status report. `state` is the mapped
/// lifecycle state; `app` is the emitter's self-chosen agent name when
/// the sequence carried a non-empty `app=` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Osc7501Report {
    pub state: AgentState,
    pub app: Option<String>,
}

/// The OSC 7501 parameter prefix: everything an OSC sequence carries
/// after `ESC ]` must start with `7501;` for this watcher to care
/// (the check is byte-at-a-time, so a non-7501 sequence — OSC 0
/// titles, OSC 52 clipboard, kitty images — is abandoned without
/// buffering anything).
const PREFIX: &[u8] = b"7501;";

/// Cap on the bytes buffered for one in-flight 7501 payload, bounding
/// the watcher's memory contributions per pane. A 7501 sequence longer
/// than this cannot be a real agent-status report and is abandoned
/// (never reported) once the cap is hit; the scanner still consumes it
/// to its terminator so the next sequence parses cleanly.
const MAX_PAYLOAD_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Idle,
    /// Saw `ESC`; the next byte decides between an OSC opener and a
    /// different (irrelevant) escape sequence.
    SawEsc,
    /// Inside an OSC sequence's parameter bytes.
    InOsc,
    /// Saw `ESC` inside an OSC sequence: the next byte is either the
    /// `\` of the ST terminator or the sequence is abandoned.
    InOscSawEsc,
}

/// What the scanner is doing with the current sequence's payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Collect {
    /// Not a 7501 sequence (or abandoned): consume payload bytes
    /// without buffering them.
    Skip,
    /// `PREFIX` has matched this many leading parameter bytes so far.
    Prefix(usize),
    /// A complete `7501;` prefix matched; buffering the rest until the
    /// terminator so it can be parsed as a report.
    Payload(Vec<u8>),
}

/// A streaming OSC 7501 scanner over raw pty output. One instance per
/// PTY surface; feed it every raw read chunk (both terminators,
/// arbitrary chunk boundaries).
///
/// ```
/// use mux_core::agent_osc7501::Osc7501Watcher;
/// use mux_core::AgentState;
///
/// let mut watcher = Osc7501Watcher::new();
/// let reports = watcher.feed(b"\x1b]7501;state=working:app=acme\x1b\\");
/// assert_eq!(reports.len(), 1);
/// assert_eq!(reports[0].state, AgentState::Working);
/// assert_eq!(reports[0].app.as_deref(), Some("acme"));
/// ```
pub struct Osc7501Watcher {
    scan: Scan,
    collect: Collect,
}

impl Osc7501Watcher {
    pub fn new() -> Self {
        Osc7501Watcher { scan: Scan::Idle, collect: Collect::Skip }
    }

    /// Scans a chunk of raw pty output, returning every OSC 7501
    /// agent-status report whose terminating byte (BEL, or the `\` of
    /// an ST) fell within it. Sequences that continue past the chunk
    /// stay pending in the watcher; nothing is lost by an `ESC ]`
    /// opener landing at the very end of a chunk.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Osc7501Report> {
        let mut found = Vec::new();
        for &byte in bytes {
            match self.scan {
                Scan::Idle => {
                    if byte == 0x1b {
                        self.scan = Scan::SawEsc;
                    }
                }
                Scan::SawEsc => {
                    if byte == b']' {
                        self.scan = Scan::InOsc;
                        self.collect = Collect::Prefix(0);
                    } else {
                        self.scan = Scan::Idle;
                    }
                }
                Scan::InOsc => match byte {
                    0x07 => {
                        if let Some(report) = self.finish_sequence() {
                            found.push(report);
                        }
                    }
                    0x1b => self.scan = Scan::InOscSawEsc,
                    _ => self.push_payload(byte),
                },
                Scan::InOscSawEsc => {
                    if byte == b'\\' {
                        if let Some(report) = self.finish_sequence() {
                            found.push(report);
                        }
                    } else {
                        // Anything but `\` after an ESC mid-OSC isn't a
                        // valid ST; abandon this sequence rather than
                        // misparse it (mirrors `notify::OscWatcher`).
                        self.scan = Scan::Idle;
                        self.collect = Collect::Skip;
                    }
                }
            }
        }
        found
    }

    /// Consume one payload byte of the current OSC sequence. Only
    /// bytes of a sequence whose parameters still (or already) match
    /// [`PREFIX`] are buffered.
    fn push_payload(&mut self, byte: u8) {
        self.collect = match std::mem::replace(&mut self.collect, Collect::Skip) {
            Collect::Skip => Collect::Skip,
            Collect::Prefix(matched) if matched < PREFIX.len() => {
                if byte == PREFIX[matched] {
                    Collect::Prefix(matched + 1)
                } else {
                    Collect::Skip
                }
            }
            // The full `7501;` prefix just matched: this byte is the
            // sequence's first payload byte.
            Collect::Prefix(_) => Collect::Payload(vec![byte]),
            Collect::Payload(mut buf) if buf.len() < MAX_PAYLOAD_BYTES => {
                buf.push(byte);
                Collect::Payload(buf)
            }
            Collect::Payload(_) => Collect::Skip,
        };
    }

    /// The current sequence terminated (BEL or ST): parse a buffered
    /// 7501 payload into a report, if it is a well-formed one, and
    /// reset the scanner.
    fn finish_sequence(&mut self) -> Option<Osc7501Report> {
        self.scan = Scan::Idle;
        match std::mem::replace(&mut self.collect, Collect::Skip) {
            Collect::Payload(buf) => parse_report(&buf),
            _ => None,
        }
    }
}

impl Default for Osc7501Watcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one complete 7501 payload (the bytes after `7501;`).
///
/// Fields are `:`-separated `key=value` pairs. Returns `None` — no
/// report, no state change — when no `state` key carries a recognized
/// value or the bytes aren't valid UTF-8.
fn parse_report(payload: &[u8]) -> Option<Osc7501Report> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut state = None;
    let mut app = None;
    for segment in text.split(':') {
        let Some((key, value)) = segment.split_once('=') else {
            // Not a key=value field (e.g. a trailing stray after an
            // app name that contained a colon). Ignored.
            continue;
        };
        match key {
            "state" => {
                // Recognized values only: an unknown state leaves the
                // pane's state alone (no report) rather than guessing.
                if let Some(mapped) = parse_state(value) {
                    state = Some(mapped);
                }
            }
            "app" => app = (!value.is_empty()).then(|| value.to_string()),
            // Unknown keys are tolerated for forwards compatibility.
            _ => {}
        }
    }
    state.map(|state| Osc7501Report { state, app })
}

/// herdr's three-state vocabulary mapped onto mtyx's `AgentState`.
fn parse_state(value: &str) -> Option<AgentState> {
    match value {
        "working" => Some(AgentState::Working),
        "waiting" => Some(AgentState::Blocked),
        "done" => Some(AgentState::Done),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: AgentState, app: Option<&str>) -> Osc7501Report {
        Osc7501Report { state, app: app.map(str::to_string) }
    }

    #[test]
    fn parses_st_terminated_working_with_app() {
        let mut watcher = Osc7501Watcher::new();
        let found = watcher.feed(b"\x1b]7501;state=working:app=acme\x1b\\");
        assert_eq!(found, vec![report(AgentState::Working, Some("acme"))]);
    }

    #[test]
    fn parses_bel_terminated_done_without_app() {
        let mut watcher = Osc7501Watcher::new();
        // `app` missing entirely → None (missing app is allowed).
        let found = watcher.feed(b"\x1b]7501;state=done\x07");
        assert_eq!(found, vec![report(AgentState::Done, None)]);
    }

    #[test]
    fn waiting_maps_to_blocked() {
        let mut watcher = Osc7501Watcher::new();
        let found = watcher.feed(b"\x1b]7501;state=waiting:app=herdr\x07");
        assert_eq!(found, vec![report(AgentState::Blocked, Some("herdr"))]);
    }

    #[test]
    fn empty_app_value_reports_none() {
        let mut watcher = Osc7501Watcher::new();
        let found = watcher.feed(b"\x1b]7501;state=working:app=\x07");
        assert_eq!(found, vec![report(AgentState::Working, None)]);
    }

    /// The hard case: a sequence split at arbitrary read boundaries —
    /// including one-by-one — must still parse exactly once, delivered
    /// at the terminator byte.
    #[test]
    fn sequence_split_across_feeds_byte_by_byte() {
        let mut watcher = Osc7501Watcher::new();
        let mut all = Vec::new();
        for byte in b"noise \x1b]7501;state=working:app=acme\x1b\\" {
            all.extend(watcher.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(all, vec![report(AgentState::Working, Some("acme"))]);

        // …and across odd multi-byte chunk boundaries too: no chunk
        // boundary can half-deliver a sequence.
        let mut watcher = Osc7501Watcher::new();
        let mut all = Vec::new();
        for chunk in b"\x1b]7501;state=done:app=acme\x07garbage".chunks(3) {
            all.extend(watcher.feed(chunk));
        }
        assert_eq!(all, vec![report(AgentState::Done, Some("acme"))]);
    }

    #[test]
    fn multiple_reports_in_one_chunk_arrive_in_order() {
        let mut watcher = Osc7501Watcher::new();
        let found = watcher.feed(
            b"\x1b]7501;state=working:app=a\x1b\\ mid \x1b]7501;state=waiting:app=a\x07 \x1b]7501;state=done:app=a\x1b\\",
        );
        assert_eq!(
            found,
            vec![
                report(AgentState::Working, Some("a")),
                report(AgentState::Blocked, Some("a")),
                report(AgentState::Done, Some("a")),
            ]
        );
    }

    /// Issue #176 contract: repeated identical reports are each
    /// delivered — no dedup (the pane applies them in order via the
    /// existing equal-tier rule).
    #[test]
    fn repeated_identical_reports_are_each_emitted() {
        let mut watcher = Osc7501Watcher::new();
        let found = watcher.feed(b"\x1b]7501;state=working:app=a\x07\x1b]7501;state=working:app=a\x07");
        assert_eq!(found, vec![report(AgentState::Working, Some("a")), report(AgentState::Working, Some("a"))]);
    }

    #[test]
    fn unknown_state_and_malformed_payloads_yield_no_report() {
        let mut watcher = Osc7501Watcher::new();
        // Unknown state value.
        assert!(watcher.feed(b"\x1b]7501;state=bogus:app=a\x07").is_empty());
        // mtyx's own wire states are not 7501 emitters' vocabulary.
        assert!(watcher.feed(b"\x1b]7501;state=idle\x07").is_empty());
        assert!(watcher.feed(b"\x1b]7501;state=blocked\x07").is_empty());
        assert!(watcher.feed(b"\x1b]7501;state=unknown\x07").is_empty());
        // No state field at all.
        assert!(watcher.feed(b"\x1b]7501;app=a\x07").is_empty());
        // Empty payload, no key=value anything.
        assert!(watcher.feed(b"\x1b]7501;\x07").is_empty());
        // Non-UTF-8 payload.
        assert!(watcher.feed(b"\x1b]7501;state=work\xffing\x07").is_empty());
        // The pane's watcher is still healthy afterwards.
        let found = watcher.feed(b"\x1b]7501;state=done:app=a\x07");
        assert_eq!(found, vec![report(AgentState::Done, Some("a"))]);
    }

    #[test]
    fn unknown_keys_are_ignored_but_state_still_parsed() {
        let mut watcher = Osc7501Watcher::new();
        // Reversed field order + unknown keys: tolerated.
        let found = watcher.feed(b"\x1b]7501;app=acme:extra=1:state=working\x07");
        assert_eq!(found, vec![report(AgentState::Working, Some("acme"))]);
    }

    #[test]
    fn non_7501_and_plain_escape_sequences_are_ignored() {
        let mut watcher = Osc7501Watcher::new();
        // OSC 9 / OSC 0 / OSC 52 / OSC 8: not ours.
        assert!(watcher.feed(b"\x1b]9;Build failed\x07").is_empty());
        assert!(watcher.feed(b"\x1b]0;my shell\x07").is_empty());
        assert!(watcher.feed(b"\x1b]52;c;AAAA\x1b\\").is_empty());
        assert!(watcher.feed(b"\x1b]8;id=x;https://example.invalid\x1b\\").is_empty());
        // 7501-shaped TEXT without an `ESC ]` opener is just text.
        assert!(watcher.feed(b"7501;state=working:app=a\x07").is_empty());
        // A prefix close-but-not-7501 (OSC 7500/75101) is not ours.
        assert!(watcher.feed(b"\x1b]7500;state=working\x07").is_empty());
        assert!(watcher.feed(b"\x1b]75101;state=working\x07").is_empty());
        // CSI and DCS noise around a real report.
        let found = watcher.feed(b"\x1b[2J\x1b]7501;state=done:app=a\x07\x1b[?25h");
        assert_eq!(found, vec![report(AgentState::Done, Some("a"))]);
    }

    /// An `ESC` mid-sequence that isn't the ST opener abandons the
    /// sequence (no report) without desyncing the scanner: the next
    /// well-formed sequence still parses.
    #[test]
    fn esc_without_backslash_mid_sequence_abandons_and_recovers() {
        let mut watcher = Osc7501Watcher::new();
        assert!(watcher.feed(b"\x1b]7501;state=working:app=a\x1bX").is_empty());
        // Abandoned mid-prefix behaves identically.
        assert!(watcher.feed(b"\x1b]750\x1bX").is_empty());
        let found = watcher.feed(b"\x1b]7501;state=done:app=a\x07");
        assert_eq!(found, vec![report(AgentState::Done, Some("a"))]);
    }

    #[test]
    fn oversized_payload_is_abandoned_and_scanner_recovers() {
        let mut watcher = Osc7501Watcher::new();
        let mut chunk = b"\x1b]7501;state=working:".to_vec();
        chunk.extend(std::iter::repeat_n(b'a', MAX_PAYLOAD_BYTES + 16));
        chunk.extend_from_slice(b"\x1b\\"); // the emitter still terminates it
        assert!(watcher.feed(&chunk).is_empty());
        let found = watcher.feed(b"\x1b]7501;state=done:app=a\x07");
        assert_eq!(found, vec![report(AgentState::Done, Some("a"))]);
    }

    /// The ST terminator (`ESC \`) itself may straddle feeds.
    #[test]
    fn st_terminator_split_across_feeds() {
        let mut watcher = Osc7501Watcher::new();
        assert!(watcher.feed(b"\x1b]7501;state=working:app=a\x1b").is_empty());
        let found = watcher.feed(b"\\");
        assert_eq!(found, vec![report(AgentState::Working, Some("a"))]);
    }
}
