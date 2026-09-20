//! Ambient agent detection (issue #78).
//!
//! Detects which AI coding agent (claude code, codex, pi, opencode,
//! cursor, aider, …) is running in a pane by combining two evidence
//! sources:
//!
//! 1. **Process evidence** — the pane PTY's child process tree
//!    (`/proc/<pid>/comm` + `/proc/<pid>/cmdline`), matched against
//!    process patterns on a *token* basis (a bare `pi` pattern matches
//!    the `pi` CLI but not `spider` or `pip`).
//! 2. **Screen evidence** — the pane's visible terminal text
//!    (`plain_text()`), matched against screen markers as substrings
//!    with `*` wildcards (e.g. Claude Code's "Claude Code" footer,
//!    codex's `codex>` prompt, pi's `pi> ` prompt).
//!
//! This is the *detection* half of the agent-state loop; it complements,
//! and does not replace, the self-report verbs `report-agent` /
//! `list-agents`. Detection results are informational (name +
//! confidence + evidence) and are cached per surface; they do NOT set
//! `AgentState` (the `AgentStateSource::Detected` tier exists for a
//! follow-up once the agent_status work lands).
//!
//! Patterns ship as a bundled JSON file ([`agents.json`]); users extend
//! the registry at runtime via the `agent-pattern-add` socket command
//! (`mtyx agent-pattern add <name> --pattern <marker>`). Pattern
//! semantics are substring/glob (`*` wildcard), deliberately NOT regex:
//! no runtime crate in the workspace links `regex`, and every marker
//! the issue names is a literal.

use anyhow::Context;
use serde::Deserialize;

const BUNDLED_PATTERNS_JSON: &str = include_str!("agent_detect/agents.json");

/// Where a pattern looks for its marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatternKind {
    /// Matched against tokens of a process's comm/cmdline (issue #78 AC3).
    Process,
    /// Matched as a substring (with `*` wildcards) of the visible screen.
    Screen,
}

impl PatternKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PatternKind::Process => "process",
            PatternKind::Screen => "screen",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "process" => Some(PatternKind::Process),
            "screen" => Some(PatternKind::Screen),
            _ => None,
        }
    }
}

/// Detection confidence, ordered `Low < Medium < High`. A process match
/// is conventionally `High`; a distinctive screen footer/prompt marker
/// `Medium`; a weak substring `Low`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Confidence::Low),
            "medium" => Some(Confidence::Medium),
            "high" => Some(Confidence::High),
            _ => None,
        }
    }
}

/// One detection pattern for one agent. `pattern` is a substring with
/// optional `*` wildcards (see the module doc: not regex).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentPattern {
    /// Agent name reported on detection (e.g. `claude`). Must not be
    /// `unknown` (reserved for "no match").
    pub name: String,
    pub kind: PatternKind,
    pub pattern: String,
    pub confidence: Confidence,
    #[serde(default)]
    pub case_insensitive: bool,
}

impl AgentPattern {
    /// Validate a (user-supplied) pattern. Returns a human-readable
    /// error rather than panicking, so the socket handler can reject
    /// bad input with a clean `ok:false` (issue #78 AC4).
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("agent name cannot be empty".to_string());
        }
        if self.name != self.name.trim() {
            return Err("agent name cannot have leading/trailing whitespace".to_string());
        }
        if self.name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "agent name {:?} cannot contain whitespace or control characters",
                self.name
            ));
        }
        if self.name == "unknown" {
            return Err("agent name 'unknown' is reserved".to_string());
        }
        if self.pattern.is_empty() {
            return Err("pattern cannot be empty".to_string());
        }
        Ok(())
    }

    /// Whether this screen pattern matches the pane's visible text.
    pub fn matches_screen(&self, text: &str) -> bool {
        text_matches(text, &self.pattern, self.case_insensitive)
    }

    /// Whether this process pattern matches a process-tree entry.
    /// Matching is per-token (see [`tokens`]): a bare `pi` pattern
    /// matches the `pi` CLI's argv but never `spider` or `pip`.
    pub fn matches_process(&self, evidence: &ProcessEvidence) -> bool {
        let haystacks = [evidence.comm.as_str(), evidence.cmdline.as_str()];
        haystacks
            .iter()
            .flat_map(|text| tokens(*text))
            .any(|token| token_matches(token, &self.pattern, self.case_insensitive))
    }
}

/// One process in the pane's PTY process tree, as read from `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEvidence {
    pub pid: u32,
    /// `/proc/<pid>/comm` (argv0 basename, 15-char kernel limit).
    pub comm: String,
    /// `/proc/<pid>/cmdline` with NULs turned into spaces.
    pub cmdline: String,
    /// `/proc/<pid>/stat` field 22 (starttime in clock ticks), used for
    /// the issue's "prefer the most-recent process" tie-break. `None`
    /// when the entry vanished or the field couldn't be parsed.
    pub starttime: Option<u64>,
}

/// The result of one detection run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// Detected agent name, or `"unknown"` when nothing matched above
    /// the configured threshold.
    pub agent: String,
    /// Match confidence; `None` for an `unknown` detection.
    pub confidence: Option<Confidence>,
    /// Human-readable evidence line naming what triggered the match
    /// (issue #78 AC1).
    pub evidence: String,
}

impl Detection {
    pub fn unknown(evidence: impl Into<String>) -> Self {
        Detection { agent: "unknown".to_string(), confidence: None, evidence: evidence.into() }
    }

    pub fn is_unknown(&self) -> bool {
        self.agent == "unknown"
    }
}

/// Runtime detection settings, pushed into [`crate::Mux`] by the TUI
/// host from the `[[agent_detection]]` config table (issue #78 AC7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectionSettings {
    pub enabled: bool,
    pub min_confidence: Confidence,
}

impl Default for DetectionSettings {
    fn default() -> Self {
        DetectionSettings { enabled: true, min_confidence: Confidence::Low }
    }
}

#[derive(Deserialize)]
struct PatternFile {
    agents: Vec<AgentPattern>,
}

/// The bundled pattern registry, parsed from [`agents.json`]. Parse
/// errors propagate (repo rule: never silently default on a parse
/// path) — a corrupt bundled file is a build bug, not user input.
pub fn bundled_patterns() -> anyhow::Result<Vec<AgentPattern>> {
    let file: PatternFile = serde_json::from_str(BUNDLED_PATTERNS_JSON)
        .context("parsing bundled agent patterns (agent_detect/agents.json)")?;
    Ok(file.agents)
}

/// Run detection over collected evidence (issue #78 AC3/AC5).
///
/// Scoring: any process match beats any screen match (a live agent
/// process is stronger than text that might be scrollback); within
/// process matches, the most recently spawned process (largest
/// `/proc/<pid>/stat` starttime) wins, and on a starttime tie the agent
/// that ALSO has screen evidence wins (the closest implementable
/// approximation of the issue's "prefer the most recent output" —
/// output bytes cannot be attributed to individual descendant
/// processes). Within screen matches, the highest confidence wins.
///
/// A best candidate below `threshold` is filtered to `unknown`.
pub fn detect(
    process: &[ProcessEvidence],
    screen_text: &str,
    patterns: &[AgentPattern],
    threshold: Confidence,
) -> Detection {
    // Screen-evidence agents, for the process-tie break (AC5's "most
    // recent output" approximation).
    let screen_agents: Vec<&str> = patterns
        .iter()
        .filter(|p| p.kind == PatternKind::Screen && p.matches_screen(screen_text))
        .map(|p| p.name.as_str())
        .collect();

    // Every (process pattern, matching process) pair.
    let candidates: Vec<(usize, &AgentPattern, &ProcessEvidence)> = patterns
        .iter()
        .enumerate()
        .filter(|(_, p)| p.kind == PatternKind::Process)
        .flat_map(|(pi, pattern)| {
            process
                .iter()
                .filter(move |ev| pattern.matches_process(ev))
                .map(move |ev| (pi, pattern, ev))
        })
        .collect();

    // AC5 tie-break, in order: most-recently-spawned process (largest
    // starttime) → agent that also has screen evidence → registry order.
    let best_process = candidates.iter().min_by(|a, b| {
        use std::cmp::Ordering;
        match b.2.starttime.cmp(&a.2.starttime) {
            Ordering::Equal => {
                let a_screen = screen_agents.contains(&a.1.name.as_str());
                let b_screen = screen_agents.contains(&b.1.name.as_str());
                match b_screen.cmp(&a_screen) {
                    Ordering::Equal => a.0.cmp(&b.0),
                    other => other,
                }
            }
            other => other,
        }
    });

    // Threshold is the evidence bar for any *single* match (review fix
    // F3), not a class filter: an above-threshold process match outranks
    // everything (per AC3); a below-threshold process match does NOT
    // discard a viable above-threshold screen match — we fall through
    // and try the screen branch. An above-threshold screen match with
    // no process evidence also wins.
    if let Some((_, pattern, evidence)) = best_process.filter(|(_, p, _)| p.confidence >= threshold)
    {
        return Detection {
            agent: pattern.name.clone(),
            confidence: Some(pattern.confidence),
            evidence: format!("process '{}' (pid {})", pattern.pattern, evidence.pid),
        };
    }

    // No (or below-threshold) process match: fall back to the highest-
    // confidence screen marker (pattern order breaks ties, keeping it
    // deterministic — `min_by` keeps the FIRST minimum, so indices
    // compare reversed).
    let best_screen = patterns
        .iter()
        .enumerate()
        .filter(|(_, p)| p.kind == PatternKind::Screen && p.matches_screen(screen_text))
        .min_by(|(ia, a), (ib, b)| b.confidence.cmp(&a.confidence).then(ib.cmp(ia)));
    match best_screen {
        Some((_, pattern)) if pattern.confidence >= threshold => Detection {
            agent: pattern.name.clone(),
            confidence: Some(pattern.confidence),
            evidence: format!("screen marker '{}'", pattern.pattern),
        },
        _ => Detection::unknown(match best_process {
            Some((_, pattern, _)) => format!(
                "process match '{}' below confidence threshold and no screen marker met it",
                pattern.pattern
            ),
            None => "no agent markers above confidence threshold".to_string(),
        }),
    }
}

/// Collect process evidence for a pane's PTY child tree: the child
/// itself plus every descendant (`process::all_descendants`). Empty on
/// platforms without a process listing or when there is no local child
/// (browser / remote panes).
///
/// Windows: a Toolhelp32 snapshot walk of the child's descendants
/// (`win::descendant_processes`). Residual limitation: only image
/// names are visible — no cmdline, no start time — so process patterns
/// that need full command lines (`claude --resume …`) cannot match,
/// and the most-recently-spawned tie-break degenerates to registry
/// order. Documented, not hidden.
pub fn collect_process_evidence(child_pid: Option<u32>) -> Vec<ProcessEvidence> {
    #[cfg(target_os = "linux")]
    {
        let Some(root) = child_pid else { return Vec::new() };
        let mut pids = vec![root];
        pids.extend(crate::process::all_descendants(root));
        pids.into_iter().filter_map(process_evidence_for_pid).collect()
    }
    #[cfg(windows)]
    {
        let Some(root) = child_pid else { return Vec::new() };
        crate::win::descendant_processes(root)
            .into_iter()
            .map(|(pid, image)| ProcessEvidence {
                pid,
                comm: image,
                cmdline: String::new(),
                // Toolhelp32 exposes no start time, so encode "unknown"
                // (`None`) rather than a fabricated 0 — the field's
                // contract on unix is already "None when it couldn't be
                // read". Every Windows row then ties on starttime and
                // the most-recently-spawned tie-break degenerates to
                // screen evidence / registry order, exactly as
                // documented above.
                starttime: None,
            })
            .collect()
    }
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    {
        let _ = child_pid;
        Vec::new()
    }
}

/// Read `/proc` evidence for one pid. `None` when the process is gone
/// (raced exit) or neither comm nor cmdline is readable.
#[cfg(target_os = "linux")]
fn process_evidence_for_pid(pid: u32) -> Option<ProcessEvidence> {
    let comm = read_comm(pid).unwrap_or_default();
    let cmdline = read_cmdline(pid).unwrap_or_default();
    if comm.is_empty() && cmdline.is_empty() {
        return None;
    }
    Some(ProcessEvidence { pid, comm, cmdline, starttime: read_starttime(pid) })
}

#[cfg(target_os = "linux")]
fn read_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

/// `/proc/<pid>/cmdline` is NUL-separated; render as a space-joined
/// argv string for token matching.
#[cfg(target_os = "linux")]
fn read_cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text = String::from_utf8_lossy(&raw);
    Some(text.trim_end_matches('\0').replace('\0', " "))
}

/// starttime is field 22 of `/proc/<pid>/stat`. `comm` (field 2) is
/// parenthesised and may itself contain spaces/parens, so parse from
/// AFTER the LAST `')'` — the `)` itself must not count as a token or
/// every field index shifts by one and field 21 (itrealvalue, always 0
/// on modern kernels) is read instead (review fix F1). Field 22 is then
/// the 20th whitespace-separated token (fields 3..=22).
#[cfg(target_os = "linux")]
fn read_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_starttime(&stat)
}

#[cfg(target_os = "linux")]
fn parse_starttime(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

/// Screen-marker semantics: substring match where `*` in the pattern
/// spans any run of characters (segments must appear in order). A
/// pattern without `*` is a plain substring test.
fn text_matches(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let (haystack, needle) = if case_insensitive {
        (text.to_lowercase(), pattern.to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let segments: Vec<&str> = needle.split('*').collect();
    if segments.len() == 1 {
        return haystack.contains(&needle);
    }
    // Wildcard: each non-empty segment must occur after the previous
    // one's end; the spans between are the `*`s.
    let mut search_from = 0;
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        match haystack[search_from..].find(segment) {
            Some(pos) => search_from += pos + segment.len(),
            None => return false,
        }
    }
    true
}

/// Process-pattern semantics: exact token equality for a bare pattern
/// (with optional case folding), or the wildcard substring semantics
/// above applied within a single token when the pattern has `*`s.
fn token_matches(token: &str, pattern: &str, case_insensitive: bool) -> bool {
    if pattern.contains('*') {
        return text_matches(token, pattern, case_insensitive);
    }
    if case_insensitive {
        token.eq_ignore_ascii_case(pattern)
    } else {
        token == pattern
    }
}

/// Split text into word tokens: maximal runs of alphanumeric
/// characters plus `-`, `_`, `.`. `/usr/bin/pi` yields `usr`, `bin`,
/// `pi` — so a bare `pi` pattern matches the pi CLI's argv without
/// false-positiving on `spider` or `pip`.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_pat(name: &str, pattern: &str) -> AgentPattern {
        AgentPattern {
            name: name.into(),
            kind: PatternKind::Screen,
            pattern: pattern.into(),
            confidence: Confidence::Medium,
            case_insensitive: false,
        }
    }

    fn proc_pat(name: &str, pattern: &str) -> AgentPattern {
        AgentPattern {
            name: name.into(),
            kind: PatternKind::Process,
            pattern: pattern.into(),
            confidence: Confidence::High,
            case_insensitive: false,
        }
    }

    fn ev(pid: u32, comm: &str, cmdline: &str, starttime: Option<u64>) -> ProcessEvidence {
        ProcessEvidence { pid, comm: comm.into(), cmdline: cmdline.into(), starttime }
    }

    /// T1 (plan §4): the bundled registry parses and covers the top-6
    /// agents, each with at least one process and one screen pattern.
    #[test]
    fn bundled_patterns_parse_and_cover_top_six_agents() {
        let patterns = bundled_patterns().expect("bundled agents.json should parse");
        let mut names: Vec<&str> = patterns.iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names, vec!["aider", "claude", "codex", "cursor", "opencode", "pi"]);
        for name in names {
            assert!(
                patterns.iter().any(|p| p.name == name && p.kind == PatternKind::Process),
                "{name} needs at least one process pattern"
            );
            assert!(
                patterns.iter().any(|p| p.name == name && p.kind == PatternKind::Screen),
                "{name} needs at least one screen pattern"
            );
        }
    }

    /// T2: a screen prompt marker yields the agent with the pattern's
    /// confidence and an evidence line naming the marker.
    #[test]
    fn screen_marker_match_returns_agent_confidence_and_evidence() {
        let patterns = bundled_patterns().unwrap();
        let detection = detect(&[], "hello\npi> what next\n", &patterns, Confidence::Low);
        assert_eq!(detection.agent, "pi");
        assert_eq!(detection.confidence, Some(Confidence::Medium));
        assert!(detection.evidence.contains("pi> "), "evidence was {:?}", detection.evidence);
        assert!(detection.evidence.contains("screen"), "evidence was {:?}", detection.evidence);
    }

    /// T3: a live agent process outranks a mere screen marker (issue
    /// AC5's "claude child AND pi child" shape, process vs screen half).
    #[test]
    fn process_match_outranks_screen_match() {
        let patterns = bundled_patterns().unwrap();
        let process = vec![ev(10, "claude", "claude", Some(100))];
        let detection = detect(&process, "codex> idle prompt", &patterns, Confidence::Low);
        assert_eq!(detection.agent, "claude");
        assert_eq!(detection.confidence, Some(Confidence::High));
        assert!(detection.evidence.contains("pid 10"), "evidence was {:?}", detection.evidence);
    }

    /// T4 (AC5 tie-break): two live agent processes → the most recently
    /// spawned wins; on a starttime tie, the agent that also has screen
    /// evidence wins.
    #[test]
    fn tie_break_prefers_most_recent_process() {
        let patterns = bundled_patterns().unwrap();
        // codex spawned later (larger starttime) even though claude's
        // pid is lower.
        let process =
            vec![ev(10, "claude", "claude", Some(100)), ev(20, "codex", "codex", Some(200))];
        let detection = detect(&process, "", &patterns, Confidence::Low);
        assert_eq!(detection.agent, "codex");

        // Equal starttimes: codex also has screen evidence → codex wins.
        let process =
            vec![ev(10, "claude", "claude", Some(200)), ev(20, "codex", "codex", Some(200))];
        let detection = detect(&process, "codex> thinking", &patterns, Confidence::Low);
        assert_eq!(detection.agent, "codex");

        // Equal starttimes, no screen evidence either: deterministic
        // (first pattern in the registry wins) — must not be "unknown".
        let process =
            vec![ev(20, "codex", "codex", Some(200)), ev(10, "claude", "claude", Some(200))];
        let detection = detect(&process, "", &patterns, Confidence::Low);
        assert!(!detection.is_unknown());
    }

    /// T5: no evidence at all → `unknown`; and a below-threshold match
    /// is filtered to `unknown`.
    #[test]
    fn no_evidence_yields_unknown_with_empty_agent() {
        let patterns = bundled_patterns().unwrap();
        let detection = detect(&[], "$ \n", &patterns, Confidence::Low);
        assert_eq!(detection.agent, "unknown");
        assert_eq!(detection.confidence, None);

        // opencode's screen marker is deliberately low-confidence, so a
        // medium threshold filters it out.
        let detection = detect(&[], "opencode v1.2", &patterns, Confidence::Medium);
        assert_eq!(detection.agent, "unknown");
    }

    /// T6: the confidence threshold drops matches strictly below it.
    #[test]
    fn min_confidence_threshold_filters_low_matches() {
        let patterns = vec![AgentPattern {
            name: "weak".into(),
            kind: PatternKind::Screen,
            pattern: "weakmarker".into(),
            confidence: Confidence::Low,
            case_insensitive: false,
        }];
        let below = detect(&[], "weakmarker visible", &patterns, Confidence::Medium);
        assert_eq!(below.agent, "unknown");
        let at = detect(&[], "weakmarker visible", &patterns, Confidence::Low);
        assert_eq!(at.agent, "weak");
        assert_eq!(at.confidence, Some(Confidence::Low));
    }

    /// T7: matcher semantics pinned — `*` wildcards and the
    /// case-insensitive flag, for both screen and process patterns, plus
    /// the token-exactness that keeps `pi` from matching `spider`.
    #[test]
    fn wildcard_and_case_insensitive_patterns_match() {
        // Screen wildcard: segments must appear in order.
        let mut pat = screen_pat("wild", "my*mark");
        assert!(
            pat.matches_screen("noise MY stuff MARK noise") == false,
            "case-sensitive by default"
        );
        pat.case_insensitive = true;
        assert!(pat.matches_screen("noise MY stuff MARK noise"));
        assert!(!pat.matches_screen("noise mark stuff my"), "segments must appear in order");
        assert!(pat.matches_screen("mymark"), "'*' spans zero or more characters");

        // Process patterns match on tokens: bare `pi` never matches
        // `spider`/`pip`, but matches the pi CLI's argv.
        let pi = proc_pat("pi", "pi");
        assert!(pi.matches_process(&ev(7, "node", "node /usr/bin/pi", None)));
        assert!(!pi.matches_process(&ev(7, "spider", "spider crawl web", None)));
        assert!(!pi.matches_process(&ev(7, "pip", "pip install x", None)));

        // Process wildcard + case-insensitivity.
        let mut node_cli = proc_pat("x", "node-*");
        assert!(node_cli.matches_process(&ev(8, "sh", "sh -c /opt/node-cli run", None)));
        assert!(!node_cli.matches_process(&ev(8, "node", "node", None)));
        node_cli.case_insensitive = true;
        assert!(node_cli.matches_process(&ev(8, "sh", "sh -c /opt/NODE-CLI run", None)));
    }

    /// T8: user-supplied patterns are validated with errors, not panics.
    #[test]
    fn bad_custom_pattern_is_rejected_not_panicked() {
        let ok = screen_pat("myagent", "MYMARKER>");
        assert!(ok.validate().is_ok());

        assert!(screen_pat("", "MYMARKER>").validate().is_err());
        assert!(screen_pat("myagent", "").validate().is_err());
        assert!(screen_pat("my agent", "MYMARKER>").validate().is_err());
        assert!(screen_pat("unknown", "x").validate().is_err(), "name 'unknown' is reserved");
        assert!(proc_pat("ok", "claude").validate().is_ok());
    }

    /// Review fix F1: `parse_starttime` must read field 22 (starttime),
    /// not field 21 (itrealvalue — obsolete, hardcoded 0 on modern
    /// kernels). The off-by-one came from slicing from the `)` itself,
    /// which keeps `)` as token 0 and shifts every field index by one.
    /// Synthetic `/proc/<pid>/stat` lines pin the parse, including the
    /// classic trap: a parenthesised comm containing spaces AND nested
    /// parens (only the LAST `)` ends the comm).
    #[test]
    #[cfg(target_os = "linux")]
    fn parse_starttime_reads_field_22_with_parenthesised_comm() {
        // Fields after the final ')': 3=state ... 21=itrealvalue,
        // 22=starttime. Here: itrealvalue=3, starttime=8888, vsize=999999.
        let stat = "1234 (python (child)) S 1 1234 1234 0 -1 4194560 100 0 0 0 7 5 0 0 20 0 7 3 8888 999999 200 140737488355312 1 0 0 0 0 0 0 0";
        assert_eq!(parse_starttime(stat), Some(8888), "nested-paren comm");

        // A comm with spaces but no nested parens, and a zero starttime
        // (a process spawned before the kernel booted is impossible, but
        // 0 is a legal field value to round-trip).
        let stat = "42 (mtyx agent d) S 1 42 42 0 -1 4194560 1 0 0 0 1 1 0 0 20 0 1 0 0 4096 200 0 0 0 0 0 0 0";
        assert_eq!(parse_starttime(stat), Some(0), "space-containing comm");

        // A plain comm: itrealvalue=0 must NOT be mistaken for starttime.
        let stat = "7 (sleep) S 1 7 7 0 -1 4194560 1 0 0 0 1 1 0 0 20 0 1 0 72660241 2228224 200 0 0 0 0 0 0";
        assert_eq!(parse_starttime(stat), Some(72660241), "plain comm");

        // Degenerate shapes: no closing paren / too few fields.
        assert_eq!(parse_starttime("7 (slee"), None);
        assert_eq!(parse_starttime("7 (sleep) S 1"), None);
    }

    /// Review fix F1, end-to-end half: a LIVE child's starttime must
    /// parse nonzero. With the off-by-one this read itrealvalue, which
    /// is 0 for every process on modern kernels — flattening AC5's
    /// most-recent-process tie-break into an all-way tie in production
    /// (the pure-parse test above couldn't catch that alone).
    #[test]
    #[cfg(target_os = "linux")]
    fn read_starttime_returns_nonzero_for_live_process() {
        use std::process::Command;
        let mut child = Command::new("/bin/sleep").arg("30").spawn().expect("spawn sleep");
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut starttime = None;
        while std::time::Instant::now() < deadline {
            if let Some(value) = read_starttime(pid) {
                starttime = Some(value);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let value = starttime.expect("starttime should parse for a live child");
        assert!(value > 0, "starttime must be nonzero (field 22, not itrealvalue=0); got {value}");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Review fix F3: a below-threshold process match must not discard a
    /// viable above-threshold screen match. The threshold is the evidence
    /// bar for *any* single match (not a class filter): a user-added
    /// low-confidence process pattern plus a bundled medium-confidence
    /// screen marker, with `min_confidence = "medium"`, should report
    /// the screen match — not `unknown`.
    #[test]
    fn below_threshold_process_match_falls_back_to_screen_match() {
        let mut patterns = bundled_patterns().unwrap();
        patterns.push(AgentPattern {
            name: "weakproc".into(),
            kind: PatternKind::Process,
            pattern: "sleep".into(),
            confidence: Confidence::Low,
            case_insensitive: false,
        });
        let process = vec![ev(5, "sleep", "sleep 999", Some(10))];
        let detection = detect(&process, "codex> thinking", &patterns, Confidence::Medium);
        assert_eq!(detection.agent, "codex");
        assert_eq!(detection.confidence, Some(Confidence::Medium));
        assert!(detection.evidence.contains("codex>"), "evidence: {:?}", detection.evidence);

        // Sanity: with the threshold dropped to low, the (now-above-threshold)
        // process match beats the screen match (process outranks screen).
        let detection = detect(&process, "codex> thinking", &patterns, Confidence::Low);
        assert_eq!(detection.agent, "weakproc");
        assert_eq!(detection.confidence, Some(Confidence::Low));
    }
}
