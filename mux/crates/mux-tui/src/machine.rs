//! Multi-machine SSH fleet support (issue #94).
//!
//! Two halves:
//!
//! 1. **The machine registry** — `mtyx machine add|list|remove` persists a
//!    label → `user@host` mapping under the mattyx config dir
//!    (`mux_core::platform::config_dir()`, 0600). It is deliberately tiny:
//!    a label, an SSH destination, nothing else. SSH aliases from
//!    `~/.ssh/config` work because the destination is passed to `ssh`
//!    verbatim (the same convention as `mux_core::remote_pty::RemoteSpec`).
//!
//! 2. **`--machine <label>` routing** — the global flag makes a control
//!    verb run against the *remote* host's mux server instead of the local
//!    one, with no TUI. The transport is `ssh -o BatchMode=yes <host> mtyx
//!    <verb> …`: a mux verb needs a mux server to run it, and the remote
//!    mtyx (headless or attended) owns exactly that. (`mtyx ssh <host>`
//!    boots `cmuxd-remote` for *PTY bytes* — see `ssh_bootstrap` and
//!    `mux_core::remote_pty` — which is the wrong layer for
//!    `list-workspaces`/`send`/`read-screen`: those read and write *mux
//!    state*, not a single pty.) The cached remote platform probe and
//!    binary handling in `ssh_bootstrap` is reused for the pre-flight
//!    reachability/OS check.
//!
//! Failure is explicit and never silently local. An unknown label prints
//! `unknown machine '<label>'` on stderr, exits nonzero, and — because
//! registry resolution happens *before* any socket work — never touches a
//! local socket (AC2, see `routing_never_falls_back_to_a_local_socket`).
//! A failed remote command exits with the remote status and its stderr;
//! the verb is never re-run locally.
//!
//! Windows SSH hosts are out of scope (`detect_remote_platform` only knows
//! Linux/Darwin, and `--machine` documents the same restriction).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// One saved machine. `target` is an SSH destination (`host` or
/// `user@host`) passed verbatim to `ssh`, so `~/.ssh/config` aliases and
/// per-host options apply unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub label: String,
    pub target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRegistry {
    #[serde(default)]
    pub machines: Vec<Machine>,
}

/// Bounded reconnect backoff for the remote verb transport: three
/// attempts total (issue #94), waiting 250ms before the second and 1s
/// before the third. Only *transport* failures (ssh could not run / the
/// remote command never started) are retried; a remote command that ran
/// and failed is reported as-is, never retried, so a failing verb cannot
/// be executed more than once.
pub const RECONNECT_ATTEMPTS: usize = 3;
pub const RECONNECT_DELAYS: [Duration; RECONNECT_ATTEMPTS - 1] =
    [Duration::from_millis(250), Duration::from_secs(1)];

// ---------- registry ----------

/// `<config>/machines.json`. Reuses the platform config-dir helper (which
/// itself honours `XDG_CONFIG_HOME` and the cmux-era rename compat).
pub fn registry_path() -> anyhow::Result<PathBuf> {
    let dir = mux_core::platform::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve a config directory ($HOME unset?)"))?;
    Ok(dir.join("machines.json"))
}

/// Load the registry from an explicit path. A missing file is an empty
/// registry (first run), but a *malformed* one is an error: silently
/// replacing a schema-drifted registry with `Default::default()` would
/// drop the user's other machines on the next write.
pub fn load_from(path: &Path) -> anyhow::Result<MachineRegistry> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MachineRegistry::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn load() -> anyhow::Result<MachineRegistry> {
    load_from(&registry_path()?)
}

/// Write the registry 0600 (and its parent dir 0700). Serializes to a
/// temp sibling and renames so a crash mid-write cannot truncate a good
/// registry.
pub fn save_to(path: &Path, registry: &MachineRegistry) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = mux_core::platform::restrict_directory(dir);
    }
    let text = serde_json::to_string_pretty(registry)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    // Tighten before the rename: the file is briefly visible at its final
    // name, never at a wider mode than 0600.
    let _ = mux_core::platform::restrict_file(&tmp);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolve a label to its SSH target, or the exact error the issue
/// requires. Split from any I/O so routing tests can exercise the
/// unknown-label path without a registry on disk.
pub fn resolve_target(registry: &MachineRegistry, label: &str) -> Result<String, String> {
    registry
        .machines
        .iter()
        .find(|m| m.label == label)
        .map(|m| m.target.clone())
        .ok_or_else(|| format!("unknown machine '{label}'"))
}

pub fn add(registry: &mut MachineRegistry, label: &str, target: &str) -> Result<(), String> {
    if label.is_empty() || target.is_empty() {
        return Err("machine label and target must be non-empty".to_string());
    }
    if let Some(existing) = registry.machines.iter_mut().find(|m| m.label == label) {
        // Idempotent update: re-adding a label repoints it rather than
        // erroring, which is what an operator re-running `machine add`
        // after a host rename expects.
        existing.target = target.to_string();
        return Ok(());
    }
    registry.machines.push(Machine { label: label.to_string(), target: target.to_string() });
    Ok(())
}

pub fn remove(registry: &mut MachineRegistry, label: &str) -> Result<(), String> {
    let before = registry.machines.len();
    registry.machines.retain(|m| m.label != label);
    if registry.machines.len() == before {
        return Err(format!("unknown machine '{label}'"));
    }
    Ok(())
}

// ---------- remote verb transport ----------

/// A remote transport: run `mtyx <verb>` on a host, inheriting stdio.
/// Trait-ified so the routing tests can inject a fake instead of needing
/// a real SSH host (issue #94: "do NOT require a real SSH host in tests").
pub trait RemoteRunner {
    /// Returns `(exit_code, stderr)`. `Ok` means the remote command ran
    /// (whatever its status); `Err` means the transport itself failed and
    /// the call may be retried.
    fn run(&self, target: &str, argv: &[String]) -> anyhow::Result<RemoteOutcome>;
}

pub struct RemoteOutcome {
    pub exit_code: i32,
    pub stderr: String,
}

/// The real transport: `ssh -o BatchMode=yes <target> mtyx <argv…>`.
/// `BatchMode=yes` keeps a missing key from hanging on a password prompt.
pub struct SshRunner;

impl RemoteRunner for SshRunner {
    fn run(&self, target: &str, argv: &[String]) -> anyhow::Result<RemoteOutcome> {
        // The remote command is a single argv vector handed to ssh after
        // the `mtyx` program name — no shell string interpolation of
        // caller data. ssh (not a shell) performs any remote-word
        // splitting, and `mtyx` argv arrives intact.
        let status = Command::new("ssh")
            .args(["-o", "BatchMode=yes"])
            .arg(target)
            .arg("mtyx")
            .args(argv)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow::anyhow!("failed to invoke ssh for {target}: {e}"))?;
        Ok(RemoteOutcome {
            exit_code: status.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&status.stderr).into_owned(),
        })
    }
}

/// ssh's own exit status for a connection/transport failure. ssh reserves
/// 255 for itself (connection refused/timeout, auth failure, host key
/// mismatch) and never propagates a remote command's 255 as its own — so
/// this unambiguously means "the connection dropped", which is what the
/// bounded backoff retries.
const SSH_TRANSPORT_EXIT: i32 = 255;

/// Route one verb to `target`, retrying only a transport failure with
/// bounded backoff. Threading the runner in makes the retry policy and
/// the "no local fallback" contract directly testable.
pub fn run_remote<F>(
    runner: &dyn RemoteRunner,
    target: &str,
    argv: &[String],
    mut sleep: F,
    stderr: &mut dyn Write,
) -> i32
where
    F: FnMut(Duration),
{
    let mut attempt = 0;
    loop {
        let outcome = match runner.run(target, argv) {
            Ok(outcome) => outcome,
            Err(e) => {
                if attempt + 1 >= RECONNECT_ATTEMPTS {
                    let _ = writeln!(
                        stderr,
                        "mtyx: {e} (after {RECONNECT_ATTEMPTS} attempts, giving up)"
                    );
                    return 1;
                }
                sleep(RECONNECT_DELAYS[attempt]);
                attempt += 1;
                continue;
            }
        };
        // ssh could not reach the host (exit 255): a dropped connection,
        // so retry with backoff. Retain the last stderr so a run that
        // exhausts its attempts still reports why.
        if outcome.exit_code == SSH_TRANSPORT_EXIT {
            if attempt + 1 >= RECONNECT_ATTEMPTS {
                if !outcome.stderr.trim().is_empty() {
                    let _ = write!(stderr, "{}", outcome.stderr);
                } else {
                    let _ = writeln!(
                        stderr,
                        "mtyx: ssh to {target} failed (exit {SSH_TRANSPORT_EXIT}) after \
                         {RECONNECT_ATTEMPTS} attempts"
                    );
                }
                return outcome.exit_code;
            }
            sleep(RECONNECT_DELAYS[attempt]);
            attempt += 1;
            continue;
        }
        // The remote command ran. Its status is the verb's status: report
        // it (with the remote's stderr) and never re-execute a failure.
        if outcome.exit_code != 0 && !outcome.stderr.trim().is_empty() {
            let _ = write!(stderr, "{}", outcome.stderr);
        }
        return outcome.exit_code;
    }
}

/// Resolve `--machine <label>` and route the verb remotely. Resolution
/// happens first and returns `1` with the `unknown machine` message
/// *before* the transport is ever consulted, so no local socket is
/// touched on the failure path.
pub fn route_verb(registry: &MachineRegistry, label: &str, argv: &[String]) -> i32 {
    let target = match resolve_target(registry, label) {
        Ok(target) => target,
        Err(msg) => {
            eprintln!("mtyx: {msg}");
            return 1;
        }
    };
    run_remote(&SshRunner, &target, argv, |d| std::thread::sleep(d), &mut std::io::stderr())
}

// ---------- `mtyx machine …` subcommand group ----------

const USAGE: &str =
    "usage: mtyx machine <add <label> <user@host> | list [--json] | remove <label>>";

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") | Some("help") | None => {
            println!("{USAGE}");
            0
        }
        Some("add") => {
            let (Some(label), Some(target)) = (args.get(1), args.get(2)) else {
                eprintln!("mtyx: {USAGE}");
                return 2;
            };
            if args.len() != 3 {
                eprintln!("mtyx: {USAGE}");
                return 2;
            }
            if label.starts_with('-') || target.starts_with('-') {
                eprintln!("mtyx: {USAGE}");
                return 2;
            }
            let mut registry = match load() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("mtyx: could not read machine registry: {e}");
                    return 1;
                }
            };
            if let Err(msg) = add(&mut registry, label, target) {
                eprintln!("mtyx: {msg}");
                return 2;
            }
            if let Err(e) = save_to(&registry_path().expect("config dir resolved above"), &registry)
            {
                eprintln!("mtyx: could not write machine registry: {e}");
                return 1;
            }
            0
        }
        Some("list") => {
            let json_out = args[1..].iter().any(|a| a == "--json");
            let registry = match load() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("mtyx: could not read machine registry: {e}");
                    return 1;
                }
            };
            if json_out {
                let payload = json!({ "machines": registry.machines });
                match serde_json::to_string_pretty(&payload) {
                    Ok(s) => {
                        println!("{s}");
                        0
                    }
                    Err(e) => {
                        eprintln!("mtyx: {e}");
                        1
                    }
                }
            } else {
                for m in &registry.machines {
                    println!("{}\t{}", m.label, m.target);
                }
                0
            }
        }
        Some("remove") => {
            let Some(label) = args.get(1) else {
                eprintln!("mtyx: {USAGE}");
                return 2;
            };
            if args.len() != 2 {
                eprintln!("mtyx: {USAGE}");
                return 2;
            }
            let mut registry = match load() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("mtyx: could not read machine registry: {e}");
                    return 1;
                }
            };
            if let Err(msg) = remove(&mut registry, label) {
                eprintln!("mtyx: {msg}");
                return 1;
            }
            if let Err(e) = save_to(&registry_path().expect("config dir resolved above"), &registry)
            {
                eprintln!("mtyx: could not write machine registry: {e}");
                return 1;
            }
            0
        }
        Some(other) => {
            eprintln!("mtyx: unknown machine subcommand {other:?}\n{USAGE}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mtyx-machine-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn add_list_remove_round_trip() {
        let mut r = MachineRegistry::default();
        add(&mut r, "box1", "user@host").unwrap();
        add(&mut r, "box2", "other").unwrap();
        assert_eq!(resolve_target(&r, "box1").unwrap(), "user@host");
        assert_eq!(r.machines.len(), 2);

        remove(&mut r, "box1").unwrap();
        assert_eq!(resolve_target(&r, "box1").unwrap_err(), "unknown machine 'box1'");
        assert_eq!(r.machines.len(), 1);
        assert_eq!(r.machines[0].label, "box2");
    }

    #[test]
    fn readd_repoints_rather_than_duplicating() {
        let mut r = MachineRegistry::default();
        add(&mut r, "box1", "user@old").unwrap();
        add(&mut r, "box1", "user@new").unwrap();
        assert_eq!(r.machines.len(), 1);
        assert_eq!(resolve_target(&r, "box1").unwrap(), "user@new");
    }

    #[test]
    fn remove_unknown_label_is_an_error() {
        let mut r = MachineRegistry::default();
        assert_eq!(remove(&mut r, "nope").unwrap_err(), "unknown machine 'nope'");
    }

    #[test]
    fn unknown_label_resolution_names_the_label() {
        let r = MachineRegistry::default();
        let err = resolve_target(&r, "typo").unwrap_err();
        assert!(err.contains("unknown machine 'typo'"), "got {err:?}");
    }

    #[test]
    fn registry_survives_a_disk_round_trip() {
        let dir = scratch("roundtrip");
        let path = dir.join("machines.json");
        let mut r = MachineRegistry::default();
        add(&mut r, "box1", "user@host").unwrap();
        save_to(&path, &r).unwrap();
        assert_eq!(load_from(&path).unwrap(), r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_registry_is_empty_not_an_error() {
        let dir = scratch("missing");
        assert_eq!(load_from(&dir.join("machines.json")).unwrap(), MachineRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_registry_is_an_error_not_silently_empty() {
        // Schema drift must surface: silently defaulting would drop every
        // machine on the next write.
        let dir = scratch("malformed");
        let path = dir.join("machines.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_from(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_registry_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        let path = dir.join("machines.json");
        let mut r = MachineRegistry::default();
        add(&mut r, "box1", "user@host").unwrap();
        save_to(&path, &r).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "machine registry must be private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- routing / transport stubs ---

    struct FakeRunner {
        outcomes: RefCell<Vec<anyhow::Result<RemoteOutcome>>>,
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<anyhow::Result<RemoteOutcome>>) -> Self {
            FakeRunner { outcomes: RefCell::new(outcomes), calls: RefCell::new(Vec::new()) }
        }
    }

    impl RemoteRunner for FakeRunner {
        fn run(&self, target: &str, argv: &[String]) -> anyhow::Result<RemoteOutcome> {
            self.calls.borrow_mut().push((target.to_string(), argv.to_vec()));
            self.outcomes.borrow_mut().remove(0)
        }
    }

    #[test]
    fn run_remote_passes_the_target_and_argv_through() {
        let runner =
            FakeRunner::new(vec![Ok(RemoteOutcome { exit_code: 0, stderr: String::new() })]);
        let mut err = Vec::new();
        let code =
            run_remote(&runner, "user@box", &["list-workspaces".to_string()], |_| {}, &mut err);
        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert_eq!(
            runner.calls.borrow()[0],
            ("user@box".to_string(), vec!["list-workspaces".to_string()])
        );
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    #[test]
    fn run_remote_retries_transport_failures_with_backoff_then_gives_up() {
        let runner = FakeRunner::new(vec![
            Err(anyhow::anyhow!("connection refused")),
            Err(anyhow::anyhow!("connection refused")),
            Err(anyhow::anyhow!("connection refused")),
        ]);
        let mut slept = Vec::new();
        let mut err = Vec::new();
        let code =
            run_remote(&runner, "box", &["identify".to_string()], |d| slept.push(d), &mut err);
        assert_eq!(code, 1);
        assert_eq!(runner.calls.borrow().len(), RECONNECT_ATTEMPTS);
        assert_eq!(slept, RECONNECT_DELAYS.to_vec());
        let msg = String::from_utf8(err).unwrap();
        assert!(msg.contains("giving up"), "got {msg:?}");
    }

    #[test]
    fn run_remote_retry_succeeds_after_transient_failure() {
        let runner = FakeRunner::new(vec![
            Err(anyhow::anyhow!("connection refused")),
            Ok(RemoteOutcome { exit_code: 0, stderr: String::new() }),
        ]);
        let mut slept = Vec::new();
        let mut err = Vec::new();
        let code =
            run_remote(&runner, "box", &["identify".to_string()], |d| slept.push(d), &mut err);
        assert_eq!(code, 0);
        assert_eq!(slept, vec![RECONNECT_DELAYS[0]]);
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    #[test]
    fn a_failing_remote_command_is_reported_once_and_never_retried() {
        // The remote command ran (transport Ok) and failed: the verb must
        // not be re-executed, and its stderr must pass through.
        let runner = FakeRunner::new(vec![Ok(RemoteOutcome {
            exit_code: 3,
            stderr: "cannot connect to session socket\n".to_string(),
        })]);
        let mut err = Vec::new();
        let code = run_remote(&runner, "box", &["send".to_string()], |_| {}, &mut err);
        assert_eq!(code, 3);
        assert_eq!(runner.calls.borrow().len(), 1);
        assert_eq!(String::from_utf8(err).unwrap(), "cannot connect to session socket\n");
    }

    #[test]
    fn ssh_transport_exit_255_is_retried_like_a_dropped_connection() {
        // ssh's own failure status (255) means the connection never
        // established: retry with backoff, then surface the remote stderr
        // once the attempts are exhausted.
        let refused = || {
            Ok(RemoteOutcome {
                exit_code: SSH_TRANSPORT_EXIT,
                stderr: "ssh: connect to host box port 22: Connection refused\n".to_string(),
            })
        };
        let runner = FakeRunner::new(vec![refused(), refused(), refused()]);
        let mut slept = Vec::new();
        let mut err = Vec::new();
        let code =
            run_remote(&runner, "box", &["identify".to_string()], |d| slept.push(d), &mut err);
        assert_eq!(code, SSH_TRANSPORT_EXIT);
        assert_eq!(runner.calls.borrow().len(), RECONNECT_ATTEMPTS);
        assert_eq!(slept, RECONNECT_DELAYS.to_vec());
        let msg = String::from_utf8(err).unwrap();
        assert!(msg.contains("Connection refused"), "got {msg:?}");
    }

    #[test]
    fn ssh_transport_exit_255_then_success_returns_zero() {
        let runner = FakeRunner::new(vec![
            Ok(RemoteOutcome { exit_code: SSH_TRANSPORT_EXIT, stderr: "refused\n".to_string() }),
            Ok(RemoteOutcome { exit_code: 0, stderr: String::new() }),
        ]);
        let mut slept = Vec::new();
        let mut err = Vec::new();
        let code =
            run_remote(&runner, "box", &["identify".to_string()], |d| slept.push(d), &mut err);
        assert_eq!(code, 0);
        assert!(err.is_empty(), "a successful retry must not print the prior failure");
        assert_eq!(slept, vec![RECONNECT_DELAYS[0]]);
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    /// AC2's "no local socket is ever contacted" contract. Resolution
    /// happens before the transport, so an unknown label must return the
    /// unknown-machine error and touch nothing. The registry file and
    /// config dir are pointed at an empty scratch tree, and the test
    /// asserts the message is *not* a socket/connect error.
    #[test]
    fn unknown_label_never_falls_back_to_a_local_socket() {
        let dir = scratch("no-local");
        let empty_config = dir.join("config");
        std::fs::create_dir_all(&empty_config).unwrap();
        let prev_config = std::env::var_os("XDG_CONFIG_HOME");
        let prev_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_CONFIG_HOME", &empty_config);
        std::env::set_var("XDG_RUNTIME_DIR", dir.join("runtime")); // exists nowhere

        let mut r = MachineRegistry::default();
        add(&mut r, "box1", "user@host").unwrap();

        let target = resolve_target(&r, "typo");
        match prev_config {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_runtime {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }

        let err = target.unwrap_err();
        assert_eq!(err, "unknown machine 'typo'");
        // The distinguishing assertion: not any kind of transport error.
        assert!(!err.contains("socket"), "must not be a socket error: {err:?}");
        assert!(!err.contains("connect"), "must not be a connect error: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The routing wrapper resolves before it dispatches: an unknown
    /// label therefore produces the unknown-machine exit without the
    /// transport ever being built. This is what keeps a typo from
    /// silently hitting the local socket via the real `SshRunner`.
    #[test]
    fn route_verb_unknown_label_exits_nonzero_matching_the_resolver() {
        let mut r = MachineRegistry::default();
        add(&mut r, "box1", "user@host").unwrap();
        let code = route_verb(&r, "typo", &["list-workspaces".to_string()]);
        assert_eq!(code, 1);
    }
}
