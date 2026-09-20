//! Swap/reattach recovery for the attach path (issue #69).
//!
//! When the session-manager overlay hands off to another session
//! (`leader S` -> pick row -> `Enter`), `run_attach` re-enters its loop and
//! calls `RemoteSession::connect` against the chosen socket. If that socket
//! is stale (the target daemon died between discovery and attach), the bare
//! `?` used to propagate the error all the way out to `main()`, exiting the
//! process to the shell instead of recovering in-process.
//!
//! This module holds the pure retry + decision logic, extracted so it is
//! unit-testable without a live TUI or a full socket handshake. `run_attach`
//! (in `main.rs`) wires these helpers into the swap loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::RemoteSession;

/// Retry a fallible op up to `retries` extra times, sleeping `backoff`
/// BEFORE each retry (never after the final failure). Always makes at least
/// one attempt. Generic over the op so the retry/timing logic is
/// unit-testable without sockets.
pub(crate) fn retry<F, T, E>(retries: u32, backoff: Duration, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
{
    let mut remaining = retries;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if remaining == 0 {
                    return Err(e);
                }
                remaining -= 1;
                std::thread::sleep(backoff);
            }
        }
    }
}

/// Connect to a session socket with up to `retries` retries, sleeping
/// `backoff` before each retry. Wraps `RemoteSession::connect` (which does
/// the full identify/subscribe handshake) so a transiently-unconnectable
/// socket gets a second chance before the caller gives up.
pub(crate) fn connect_with_retry(
    path: &Path,
    retries: u32,
    backoff: Duration,
) -> anyhow::Result<Arc<RemoteSession>> {
    retry(retries, backoff, || RemoteSession::connect(path))
}

/// What `run_attach` should do when a connect failed even after retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwapRecovery {
    /// No known-good socket to fall back to (a genuine first attach to a
    /// dead socket). The caller propagates the connect error -- exit 1,
    /// unchanged behavior.
    Propagate,
    /// A swap target died. Re-attach to `socket` (the last-known-good /
    /// origin socket) and surface `status` on the next TUI open so the
    /// user learns why the handoff failed.
    Recover { socket: PathBuf, status: String },
}

/// Pure decision: given the last-known-good socket and the socket that just
/// failed, decide `Propagate` vs `Recover`. Returns `Propagate` when there
/// is no fallback, OR when the fallback IS the failed socket (prevents an
/// infinite recovery loop if the origin itself died).
pub(crate) fn plan_swap_recovery(
    last_good: Option<&Path>,
    failed: &Path,
    failed_name: &str,
) -> SwapRecovery {
    match last_good {
        Some(good) if good != failed => SwapRecovery::Recover {
            socket: good.to_path_buf(),
            status: format!("could not attach to {} - socket unreachable", failed_name),
        },
        _ => SwapRecovery::Propagate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    // --- `retry`: generic timing/counting -------------------------------

    /// T1: a first-try success makes exactly one call and skips the backoff.
    #[test]
    fn retry_succeeds_first_try_calls_once() {
        let count = AtomicU32::new(0);
        let start = Instant::now();
        let out: Result<u32, ()> = retry(1, Duration::from_millis(2), || {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(7)
        });
        let elapsed = start.elapsed();
        assert_eq!(out, Ok(7));
        assert_eq!(count.load(Ordering::SeqCst), 1, "first-try Ok must call op once");
        assert!(
            elapsed < Duration::from_millis(2),
            "no sleep before the first/only attempt, took {:?}",
            elapsed
        );
    }

    /// T2: an always-Err op with one retry fires the backoff exactly once
    /// (between the two attempts), so elapsed >= backoff.
    #[test]
    fn retry_retries_once_then_errors() {
        let count = AtomicU32::new(0);
        let backoff = Duration::from_millis(2);
        let start = Instant::now();
        let out: Result<u32, ()> = retry(1, backoff, || {
            count.fetch_add(1, Ordering::SeqCst);
            Err(())
        });
        let elapsed = start.elapsed();
        assert!(out.is_err(), "always-Err must return Err");
        assert_eq!(count.load(Ordering::SeqCst), 2, "retries=1 => 2 total attempts");
        assert!(elapsed >= backoff, "sleep must fire once between attempts, took {:?}", elapsed);
    }

    /// T3: an op that fails then succeeds on the second call returns Ok
    /// after exactly two attempts.
    #[test]
    fn retry_succeeds_on_second_attempt() {
        let count = AtomicU32::new(0);
        let start = Instant::now();
        let out: Result<u32, ()> = retry(1, Duration::from_millis(2), || {
            let n = count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(())
            } else {
                Ok(9)
            }
        });
        let elapsed = start.elapsed();
        assert_eq!(out, Ok(9));
        assert_eq!(count.load(Ordering::SeqCst), 2, "two attempts before Ok");
        assert!(
            elapsed >= Duration::from_millis(2),
            "one backoff sleep before the retry, took {:?}",
            elapsed
        );
    }

    /// T4: retries=0 means exactly one attempt and no sleep, even with a
    /// large backoff.
    #[test]
    fn retry_zero_retries_calls_once_no_sleep() {
        let count = AtomicU32::new(0);
        let start = Instant::now();
        let out: Result<u32, ()> = retry(0, Duration::from_millis(50), || {
            count.fetch_add(1, Ordering::SeqCst);
            Err(())
        });
        let elapsed = start.elapsed();
        assert!(out.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1, "no retries => one call");
        assert!(elapsed < Duration::from_millis(50), "no sleep when retries=0, took {:?}", elapsed);
    }

    // --- `plan_swap_recovery`: pure decision -----------------------------

    /// T5: no last-known-good socket (a genuine first attach) must still
    /// propagate the error -- exit 1, unchanged behavior.
    #[test]
    fn plan_propagates_when_no_last_good() {
        assert_eq!(
            plan_swap_recovery(None, Path::new("/x/dead.sock"), "dead"),
            SwapRecovery::Propagate,
        );
    }

    /// T6: a swap target died; recover to the last-known-good socket and
    /// surface the status string. (Plain hyphen, per issue #69 acceptance.)
    #[test]
    fn plan_recovers_to_last_good_with_status() {
        assert_eq!(
            plan_swap_recovery(
                Some(Path::new("/run/origin.sock")),
                Path::new("/run/dead.sock"),
                "dead",
            ),
            SwapRecovery::Recover {
                socket: Path::new("/run/origin.sock").into(),
                status: "could not attach to dead - socket unreachable".to_string(),
            },
        );
    }

    /// T7: if the last-known-good IS the failed socket (origin itself
    /// died), propagate to avoid an infinite recovery loop.
    #[test]
    fn plan_propagates_when_last_good_equals_failed() {
        assert_eq!(
            plan_swap_recovery(
                Some(Path::new("/run/origin.sock")),
                Path::new("/run/origin.sock"),
                "origin",
            ),
            SwapRecovery::Propagate,
        );
    }

    // --- `connect_with_retry`: real transport ---------------------------

    /// T8: a stale file (not a listening socket) is retried once with the
    /// production 250 ms backoff before the final error. Lower-bound only
    /// (CI variance); the timing proves the retry fired.
    #[test]
    fn connect_with_retry_dead_socket_retried_once_and_errors() {
        let dir = unique_temp_dir("reattach-dead");
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("dead.sock");
        // Non-socket file: UnixStream::connect returns ENOTSOCK/ECONNREFUSED,
        // same stale-file technique as cli.rs::serve_recovers_from_stale_socket.
        fs::write(&socket, b"").unwrap();

        let start = Instant::now();
        let result = connect_with_retry(&socket, 1, Duration::from_millis(250));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "connecting to a stale file must error");
        assert!(
            elapsed >= Duration::from_millis(240),
            "one ~250ms retry must have fired, took {:?}",
            elapsed
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// T9: a path that was never created errors fast with no retries and
    /// no sleep.
    #[test]
    fn connect_with_retry_missing_path_errors_fast() {
        let dir = unique_temp_dir("reattach-missing");
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.sock");

        let start = Instant::now();
        let result = connect_with_retry(&missing, 0, Duration::from_millis(250));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "missing path must error");
        assert!(elapsed < Duration::from_millis(50), "retries=0 => no sleep, took {:?}", elapsed);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Minimal temp dir helper (mirrors tests/cli.rs::unique_temp_dir).
    fn unique_temp_dir(name: &str) -> PathBuf {
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        PathBuf::from("/tmp").join(format!("mtyx-reattach-{name}-{}-{stamp}", std::process::id()))
    }
}
