#![cfg(target_os = "linux")]

//! Issue #139 AC3, suite layer: prove that no `mtyx` daemon /
//! socket-watchdog process from THIS test run outlives it.
//!
//! Attribution is by the `MTYX_TEST_RUN` env tag: a process counts as
//! ours only when its environ carries the exact `MTYX_TEST_RUN=<tag>`
//! entry AND its cmdline mentions `mtyx` (the daemon and its watchdog
//! both inherit the tag and both name `mtyx` in argv). The box may carry
//! pre-existing leaked daemons from older runs and the operator's real
//! session daemons — none of them carry this run's tag, so the check
//! never false-positives on them.
//!
//! Ordering caveat: cargo's test-target execution ORDER is undocumented.
//! This target may run before (not just after) the daemon-spawning
//! targets, in which case it trivially passes — it only ever matches its
//! own run's tag either way. For the full-suite statement, run the suite
//! with a unique tag and then this target explicitly afterwards:
//!
//! Grace window: the scan can fire within ~50ms of the previous test
//! binary exiting, while that binary's LAST socket-watchdog is still
//! inside its 200ms exit-on-daemon-death window — so the scan re-polls.
//!
//! ```text
//! MTYX_TEST_RUN=<unique> cargo test -p mux-tui
//! MTYX_TEST_RUN=<same>    cargo test -p mux-tui --test no_leaked_daemons
//! ```

use std::fs;

#[test]
fn no_tagged_mtyx_processes_outlive_the_run() {
    let tag = match std::env::var("MTYX_TEST_RUN") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!(
                "skipping: MTYX_TEST_RUN is not set — rerun with a unique tag \
                 (MTYX_TEST_RUN=<unique> cargo test …) to arm the leak check"
            );
            return;
        }
    };
    let needle = format!("MTYX_TEST_RUN={tag}");

    // Grace window (see module doc): pass the instant the scan is clean;
    // otherwise re-scan every 100ms until the 10s deadline and only then
    // assert with the final list. A mid-exit watchdog vanishes <=200ms
    // in; a real leak never disappears, so it can't be masked.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let offenders = loop {
        let offenders = scan_offenders(&needle);
        if offenders.is_empty() || std::time::Instant::now() >= deadline {
            break offenders;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    assert!(
        offenders.is_empty(),
        "processes from this test run (MTYX_TEST_RUN={tag}) are still live:\n{}",
        offenders
            .iter()
            .map(|(pid, cmdline)| format!("  pid {pid}: {cmdline}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// One /proc sweep: live (non-zombie) processes whose environ carries
/// the exact `MTYX_TEST_RUN` entry and whose cmdline mentions `mtyx`.
fn scan_offenders(needle: &str) -> Vec<(u32, String)> {
    let me = std::process::id();
    let mut offenders: Vec<(u32, String)> = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return offenders; // no /proc: nothing to assert on
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == me {
            continue;
        }
        // A zombie holds no resources; an ancestor subreaper that adopts
        // an orphaned test daemon may never reap it (issue #139 scout
        // R7), so Z must not count as a leftover.
        if is_zombie(pid) {
            continue;
        }
        // Read failures race processes exiting (or another uid) — ignore.
        let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        // Exact tag bytes: compare whole NUL-separated environ entries so
        // a different run's tag is never a substring match.
        if !environ.split(|b| *b == 0).any(|entry| entry == needle.as_bytes()) {
            continue;
        }
        let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline.contains("mtyx") {
            offenders.push((pid, cmdline));
        }
    }
    offenders
}

/// Zombie state from /proc's stat file. `comm` may contain spaces and
/// parentheses, so parse after the LAST `)` — the next field is state.
fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    match stat.rsplit_once(')') {
        Some((_, tail)) => tail.trim_start().starts_with('Z'),
        None => false,
    }
}
