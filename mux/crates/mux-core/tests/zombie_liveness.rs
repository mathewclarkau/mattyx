//! A zombie (exited, not yet reaped) must not be reported as alive.
//!
//! This lives in its own test binary on purpose: `mux_core::process::reap_zombies`
//! reaps the test process's children with `waitpid(-1)`, so a zombie created
//! in the same binary as the other `process` tests can be reaped by a
//! concurrently-running test before this one observes it.

#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn zombie_is_reported_as_dead() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn short-lived child");
    let pid = child.id();

    // We deliberately do not wait() until the end, so the exited child stays
    // a zombie while we assert on it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !mux_core::process::is_zombie(pid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        mux_core::process::is_zombie(pid),
        "child {pid} should have exited into a zombie state"
    );
    assert!(
        !mux_core::process::is_alive(pid),
        "a zombie must not be reported as alive (pid {pid})"
    );

    let _ = child.wait();
}
