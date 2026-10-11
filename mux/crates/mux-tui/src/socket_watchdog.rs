//! External companion that removes a session's `.sock` / `.pid` after the
//! daemon dies — including the SIGKILL case where signal handlers and
//! `atexit` never run (issue #27).
//!
//! The daemon spawns `mtyx socket-watchdog --pid <daemon> --socket <path>`
//! as a detached child right after binding. The watchdog polls until the
//! target PID is gone (or is no longer a mtyx process — PID-reuse guard),
//! then unlinks both files and exits. On graceful shutdown the daemon
//! already unlinks; the watchdog's second unlink is a no-op.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use mux_core::server::{is_mtyx_process, is_process_alive, pid_path};

const POLL_MS: u64 = 200;
/// Upper bound matching issue #27 acceptance (≤5s). Used only in tests /
/// docs; the loop itself has no timeout while the daemon lives.
#[allow(dead_code)]
pub const MAX_CLEANUP_SECS: u64 = 5;

/// Spawn a detached watchdog watching `daemon_pid` + `socket_path`.
/// Failures are swallowed: a missing watchdog only means SIGKILL leaves
/// stale files (the pre-#27 behaviour), never blocks daemon start.
pub fn spawn(daemon_pid: u32, socket_path: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let socket = socket_path.display().to_string();
    let pid_s = daemon_pid.to_string();
    let _ = Command::new(exe)
        .args(["socket-watchdog", "--pid", &pid_s, "--socket", &socket])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Entry point for `mtyx socket-watchdog ...`. Returns a process exit code.
pub fn run(args: &[String]) -> i32 {
    let mut pid: Option<u32> = None;
    let mut socket: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pid" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("mtyx socket-watchdog: --pid needs a value");
                    return 2;
                };
                match v.parse::<u32>() {
                    Ok(p) if p > 1 => pid = Some(p),
                    _ => {
                        eprintln!("mtyx socket-watchdog: invalid --pid {v}");
                        return 2;
                    }
                }
            }
            "--socket" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("mtyx socket-watchdog: --socket needs a value");
                    return 2;
                };
                socket = Some(PathBuf::from(v));
            }
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            other => {
                eprintln!("mtyx socket-watchdog: unknown argument {other}");
                print_usage();
                return 2;
            }
        }
        i += 1;
    }

    let Some(pid) = pid else {
        eprintln!("mtyx socket-watchdog: --pid is required");
        return 2;
    };
    let Some(socket) = socket else {
        eprintln!("mtyx socket-watchdog: --socket is required");
        return 2;
    };

    watch_until_dead(pid, &socket);
    cleanup_files(&socket);
    0
}

fn print_usage() {
    eprint!(
        "\
mtyx socket-watchdog — remove a session socket after the daemon dies

USAGE:
  mtyx socket-watchdog --pid <daemon-pid> --socket <path>

Not intended for interactive use; the daemon spawns this automatically.
"
    );
}

/// Block until `pid` is dead or no longer a mtyx process (PID reuse).
fn watch_until_dead(pid: u32, _socket: &Path) {
    loop {
        if !is_process_alive(pid) {
            break;
        }
        // PID reused by something that is not mtyx → treat as dead so we
        // don't hold the socket forever for the wrong owner.
        if !is_mtyx_process(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

fn cleanup_files(socket: &Path) {
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(pid_path(socket));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::Instant;

    #[test]
    #[cfg(target_os = "linux")]
    fn watchdog_unlinks_after_target_dies() {
        let dir = std::env::temp_dir().join(format!(
            "mtyx-wd-test-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("sess.sock");
        let pid_file = dir.join("sess.pid");
        // Dummy files the watchdog should remove.
        fs::write(&socket, b"").unwrap();
        // Spawn a short-lived process to watch.
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        fs::write(&pid_file, format!("{pid}\n")).unwrap();

        // Run the watch loop in-process against a process we kill.
        let sock = socket.clone();
        let handle = std::thread::spawn(move || {
            watch_until_dead(pid, &sock);
            cleanup_files(&sock);
        });

        std::thread::sleep(Duration::from_millis(100));
        let _ = child.kill();
        let _ = child.wait();

        handle.join().expect("watchdog thread");

        assert!(!socket.exists(), "socket should be unlinked");
        assert!(!pid_file.exists(), "pid file should be unlinked");
        let _ = fs::remove_dir_all(&dir);
    }
}
