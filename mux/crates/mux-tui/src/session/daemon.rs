//! Spawn a detached headless session daemon (issue #107).
//!
//! A local TUI used to own the mux in-process, so prefix `d` quit the
//! process and took the session with it. Interactive `mtyx` now starts
//! (or reuses) a background daemon and attaches as a client, so detach
//! leaves something to reattach to.
//!
//! The daemon is a fresh `mtyx --headless` child, not a `fork()` of the
//! TUI: by the time the mux is running the process is multithreaded
//! (PTY readers, accept thread), and fork-without-exec is unsafe.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(25);

/// Argv for a detached headless child (not including argv0).
pub(crate) fn headless_argv(
    session: &str,
    socket: &Path,
    term: Option<&str>,
) -> Vec<std::ffi::OsString> {
    let mut args = vec![
        "--headless".into(),
        "--session".into(),
        session.into(),
        "--socket".into(),
        socket.as_os_str().to_os_string(),
    ];
    if let Some(term) = term {
        args.push("--term".into());
        args.push(term.into());
    }
    args
}

/// If `socket` is already a live session, do nothing. Otherwise spawn a
/// detached headless daemon bound to that path and wait until it is
/// connectable. Kills the child on timeout or early exit.
pub(crate) fn ensure_session_daemon(
    exe: &Path,
    session: &str,
    socket: &Path,
    term: Option<&str>,
) -> anyhow::Result<()> {
    if mux_core::server::is_session_socket_live(socket) {
        return Ok(());
    }
    let mut child = spawn_detached_headless(exe, session, socket, term)?;
    match wait_until_ready(socket, &mut child, READY_TIMEOUT) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(err)
        }
    }
}

fn spawn_detached_headless(
    exe: &Path,
    session: &str,
    socket: &Path,
    term: Option<&str>,
) -> anyhow::Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.args(headless_argv(session, socket, term))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_stdio_session(&mut cmd);
    cmd.spawn().map_err(|err| {
        anyhow::anyhow!("spawning session daemon ({} --headless): {err}", exe.display())
    })
}

/// Put the child in its own session so closing the TUI's terminal does
/// not SIGHUP the daemon. No-op beyond stdio on Windows (no SIGHUP).
fn detach_stdio_session(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe. This runs between
        // fork and exec, so only async-signal-safe calls are allowed.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        const FLAGS: u32 = 0x00000008 | 0x00000200 | 0x08000000;
        cmd.creation_flags(FLAGS);
    }
}

/// Block until `socket` is a live session, the child exits, or `timeout`.
pub(crate) fn wait_until_ready(
    socket: &Path,
    child: &mut Child,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if mux_core::server::is_session_socket_live(socket) {
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!(
                    "session daemon exited before the control socket was ready ({status})"
                );
            }
            Ok(None) => {}
            Err(err) => anyhow::bail!("polling session daemon: {err}"),
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for session daemon at {}", socket.display());
        }
        std::thread::sleep(READY_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("mtyx-daemon-{name}-{}-{stamp}", std::process::id()))
    }

    fn argv_strings(args: &[std::ffi::OsString]) -> Vec<String> {
        args.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn headless_argv_includes_session_socket_and_optional_term() {
        let socket = Path::new("/tmp/agents.sock");
        assert_eq!(
            argv_strings(&headless_argv("agents", socket, None)),
            vec!["--headless", "--session", "agents", "--socket", "/tmp/agents.sock"]
        );
        let with_term = argv_strings(&headless_argv("agents", socket, Some("xterm-ghostty")));
        assert_eq!(&with_term[with_term.len() - 2..], ["--term", "xterm-ghostty"]);
    }

    #[cfg(unix)]
    #[test]
    fn wait_until_ready_errors_when_child_exits() {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let err =
            wait_until_ready(Path::new("/nope/missing.sock"), &mut child, Duration::from_secs(2))
                .expect_err("dead child must not look ready");
        let msg = err.to_string();
        assert!(msg.contains("exited"), "error was: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn wait_until_ready_succeeds_once_socket_listens() {
        use std::os::unix::net::UnixListener;

        let dir = unique_temp_dir("ready");
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        // Keep the listener alive so connect() succeeds. No pid file:
        // is_session_socket_live treats a connectable socket without a
        // pid file as live.
        let mut child = Command::new("sleep").arg("10").spawn().expect("spawn sleep");
        let result = wait_until_ready(&socket, &mut child, Duration::from_secs(2));
        let _ = child.kill();
        let _ = child.wait();
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
        result.expect("listening socket must count as ready");
    }

    #[cfg(unix)]
    #[test]
    fn wait_until_ready_times_out_if_socket_never_appears() {
        let mut child = Command::new("sleep").arg("10").spawn().expect("spawn sleep");
        let err =
            wait_until_ready(Path::new("/nope/never.sock"), &mut child, Duration::from_millis(80))
                .expect_err("must time out");
        let _ = child.kill();
        let _ = child.wait();
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "error was: {msg}");
    }
}
