//! Unix process-tree helpers for orphan reaping.
//!
//! When a pane's shell exits (or is killed), grandchildren that double-forked
//! or were backgrounded can outlive the direct PTY child. Combined with
//! [`set_child_subreaper`], this module lets mtyx inherit those orphans and
//! terminate the whole tree on surface kill / mux shutdown.
//!
//! Linux walks `/proc`; macOS walks `proc_listchildpids`/`proc_pidinfo`.
//! Windows does not enumerate here — pane teardown uses per-surface job
//! objects (`win.rs`) instead, and readiness uses `win::descendant_processes`.
//!
//! See issue #28.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Make this process a subreaper so orphaned descendants reparent here
/// instead of PID 1. No-op / returns false on non-Linux.
///
/// Safe to call more than once. Requires Linux 3.4+.
pub fn set_child_subreaper() -> bool {
    #[cfg(target_os = "linux")]
    {
        // PR_SET_CHILD_SUBREAPER = 36
        let rc = unsafe { libc::prctl(36, 1i64, 0, 0, 0) };
        rc == 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Whether `pid` is currently alive (same semantics as server::is_process_alive).
///
/// Windows: `OpenProcess` + `GetExitCodeProcess` probe (see `win.rs`);
/// access-denied counts as alive, mirroring the unix EPERM convention.
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if res == 0 {
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(windows)]
    {
        crate::win::is_process_alive(pid)
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = pid;
        false
    }
}

/// Direct children of `pid` (Linux `/proc/<pid>/task/<pid>/children`, with
/// a `/proc` scan fallback; macOS `proc_listchildpids`). Empty on Windows.
///
/// Windows residual limitation: there is no per-parent child listing
/// used here — pane children are torn down via per-surface job objects
/// (see `win.rs`), so the /proc-tree walk this feeds on unix has no
/// Windows caller. Returns empty rather than a Toolhelp approximation,
/// because approximating "children of this pid" with parent-pid links
/// is unreliable when pids are reused mid-walk. Readiness on Windows
/// goes through `win::descendant_processes` instead, which snapshots
/// the tree atomically via Toolhelp32.
pub fn direct_children(pid: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        // The per-task `children` file lists children of THAT task
        // (thread), not of the thread group: children forked by worker
        // threads (every socket-driven pane spawn in the daemon) and
        // orphans reparented to a subreaper appear on other tasks'
        // lists, never the leader's. Reading only the leader's file
        // returned a stale partial list whenever any such child existed,
        // silently skipping the rest (and making the
        // `direct_children_finds_spawned_child` test order-dependent).
        // Union every task's list instead; fall back to a full /proc
        // scan when no list is readable at all.
        let task_dir = format!("/proc/{pid}/task");
        let mut kids: Vec<u32> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&task_dir) {
            for entry in entries.flatten() {
                let children_path = entry.path().join("children");
                let Ok(contents) = std::fs::read_to_string(&children_path) else {
                    continue;
                };
                for token in contents.split_whitespace() {
                    if let Ok(child) = token.parse::<u32>() {
                        if !kids.contains(&child) {
                            kids.push(child);
                        }
                    }
                }
            }
        }
        if !kids.is_empty() {
            return kids;
        }
        scan_proc_for_children(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_direct_children(pid)
    }
    #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
    {
        let _ = pid;
        Vec::new()
    }
}

/// macOS `direct_children` via `proc_listchildpids`. Two-call pattern:
/// first probe the byte count with a zero-size buffer, allocate a
/// `Vec<libc::pid_t>` with headroom (children can be born between the
/// two calls), second call, truncate to the returned count. Returns
/// empty on any error — callers (`all_descendants`, `kill_process_tree`,
/// `kill_remaining_children`) degrade to "no children seen" rather than
/// panicking, which is the safe side for orphan reaping.
#[cfg(target_os = "macos")]
fn macos_direct_children(pid: u32) -> Vec<u32> {
    use std::mem;
    // First call: returns the byte count needed. A NULL/zero-size
    // buffer is the documented "size query" spelling.
    let needed =
        unsafe { libc::proc_listchildpids(pid as libc::pid_t, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let needed = needed as usize;
    let elem = mem::size_of::<libc::pid_t>();
    // Slack for children spawned between the two calls (proc_listchildpids
    // is not atomic). +16 is the same headroom libproc callers commonly
    // use; the second call's returned count is the source of truth.
    let cap = needed / elem + 16;
    let mut buf: Vec<libc::pid_t> = Vec::with_capacity(cap);
    let got = unsafe {
        libc::proc_listchildpids(
            pid as libc::pid_t,
            buf.as_mut_ptr() as *mut _,
            (cap * elem) as libc::c_int,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    let got = got as usize / elem;
    // SAFETY: proc_listchildpids wrote `got` pid_t elements starting at
    // buf.as_ptr(); the Vec was allocated with >= got capacity. We never
    // read uninitialized memory beyond `got`.
    unsafe { buf.set_len(got) };
    buf.into_iter()
        .filter(|&c| c > 0)
        .map(|c| c as u32)
        .collect()
}

#[cfg(target_os = "linux")]
fn scan_proc_for_children(pid: u32) -> Vec<u32> {
    let mut kids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return kids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(child_pid) = name.parse::<u32>() else { continue };
        if child_pid == pid {
            continue;
        }
        let stat_path = format!("/proc/{child_pid}/stat");
        let Ok(stat) = std::fs::read_to_string(&stat_path) else {
            continue;
        };
        // /proc/pid/stat: "pid (comm) state ppid ..."
        // comm may contain spaces/parens; ppid is the first field after
        // the final ')' of the comm.
        if let Some(close) = stat.rfind(')') {
            let after = &stat[close + 1..];
            let mut fields = after.split_whitespace();
            // state, ppid
            let _state = fields.next();
            if let Some(ppid_s) = fields.next() {
                if ppid_s.parse::<u32>().ok() == Some(pid) {
                    kids.push(child_pid);
                }
            }
        }
    }
    kids
}

/// All descendants of `root` (not including `root` itself), depth-first.
pub fn all_descendants(root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    seen.insert(root);
    while let Some(pid) = stack.pop() {
        for child in direct_children(pid) {
            if seen.insert(child) {
                out.push(child);
                stack.push(child);
            }
        }
    }
    out
}

/// Reap any zombie children of this process (non-blocking).
///
/// Windows: polls the tracked child handles (`win::reap_tracked_handles`)
/// — the `waitpid(-1, WNOHANG)` analogue.
pub fn reap_zombies() {
    #[cfg(unix)]
    {
        loop {
            let mut status: libc::c_int = 0;
            let rc = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if rc <= 0 {
                break;
            }
        }
    }
    #[cfg(windows)]
    {
        crate::win::reap_tracked_handles();
    }
}

/// Send `sig` to every pid in `pids`. Ignores ESRCH / EPERM.
///
/// Windows residual limitation: `GenerateConsoleCtrlEvent` only
/// reaches processes sharing the caller's console, and a headless
/// daemon has none, so there is no graceful phase — both the SIGTERM
/// and SIGKILL spellings hard-terminate (`TerminateProcess`). Surface
/// teardown prefers the job-object path (`win::terminate_pid_tree`);
/// this is the fallback for pids outside any tracked job.
fn signal_all(pids: &[u32], sig: libc::c_int) {
    for &pid in pids {
        if pid == 0 {
            continue;
        }
        signal_pid(pid, sig);
    }
}

/// Signal/terminate one pid. Unix: `kill(2)`; Windows: hard
/// `TerminateProcess` (no cross-console graceful signal exists — see
/// `signal_all`'s doc comment for the residual limitation).
#[cfg(unix)]
fn signal_pid(pid: u32, sig: libc::c_int) {
    let _ = unsafe { libc::kill(pid as libc::pid_t, sig) };
}

#[cfg(windows)]
fn signal_pid(pid: u32, sig: libc::c_int) {
    let _ = sig;
    crate::win::terminate_pid(pid);
}

#[cfg(all(not(unix), not(windows)))]
fn signal_pid(_pid: u32, _sig: libc::c_int) {}

/// Terminate `root` and every descendant: SIGTERM, wait up to `grace`,
/// then SIGKILL survivors. Also reaps zombies along the way.
///
/// Does nothing if `root` is 0 or is this process.
pub fn kill_process_tree(root: u32) {
    let self_pid = std::process::id();
    if root == 0 || root == self_pid {
        return;
    }

    // Snapshot tree; re-walk after SIGTERM for late-spawned kids.
    let mut targets: HashSet<u32> = all_descendants(root).into_iter().collect();
    targets.insert(root);
    targets.remove(&self_pid);

    let list: Vec<u32> = targets.into_iter().collect();
    signal_all(&list, libc::SIGTERM);

    let grace = Duration::from_secs(2);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        reap_zombies();
        // Catch late reparents / new children under still-living nodes.
        let mut still = false;
        for &pid in &list {
            if is_alive(pid) {
                still = true;
                break;
            }
        }
        // Also pull any new descendants of still-living roots.
        for d in all_descendants(root) {
            if d != self_pid && is_alive(d) {
                still = true;
                signal_pid(d, libc::SIGTERM);
            }
        }
        if !still && !is_alive(root) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Final hard kill of anything still around in the tree. Unix:
    // SIGKILL. Windows: route through the job-object teardown —
    // `TerminateJobObject` on the per-surface job, falling back to
    // `TerminateProcess` for pids outside any tracked job (see win.rs) —
    // because libc on windows has no SIGKILL constant at all, and the
    // job path is the kill(-pgid) analogue per win.rs's mapping table.
    let mut survivors = all_descendants(root);
    survivors.push(root);
    survivors.retain(|&p| p != self_pid && is_alive(p));
    #[cfg(unix)]
    signal_all(&survivors, libc::SIGKILL);
    #[cfg(windows)]
    for &pid in &survivors {
        crate::win::terminate_pid_tree(pid);
    }
    // Brief wait for SIGKILL to take effect.
    let hard_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < hard_deadline {
        reap_zombies();
        if survivors.iter().all(|&p| !is_alive(p)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    reap_zombies();
}

/// Kill every remaining child process of *this* process (and their
/// descendants). Used on mux shutdown after surface kills so anything
/// reparented via the subreaper is cleaned up.
pub fn kill_remaining_children() {
    let self_pid = std::process::id();
    let kids = direct_children(self_pid);
    for kid in kids {
        kill_process_tree(kid);
    }
    reap_zombies();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    #[cfg(target_os = "linux")]
    fn kill_process_tree_reaps_background_grandchild() {
        // Shape matching issue #28 acceptance:
        //   sh -c 'sleep 999 & wait'  — sleep is a grandchild that would
        //   normally reparent to init if sh exits first.
        let enabled = set_child_subreaper();
        assert!(enabled, "PR_SET_CHILD_SUBREAPER should succeed on Linux");

        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 999 & exec sleep 998"])
            .spawn()
            .expect("spawn shell tree");
        let root = child.id();

        // Wait until the background sleep is visible as a descendant.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found_bg = false;
        while Instant::now() < deadline {
            let desc = all_descendants(root);
            // Either sleep 999 is under root, or (if exec replaced sh) we
            // still have the root sleep 998 alive.
            if is_alive(root) {
                // Look for any sleep-named descendant via /proc.
                for d in &desc {
                    if let Ok(cmd) = std::fs::read_to_string(format!("/proc/{d}/cmdline")) {
                        if cmd.contains("sleep") {
                            found_bg = true;
                            break;
                        }
                    }
                }
                // Even without finding the bg sleep yet, proceed once root is up.
                if found_bg || Instant::now() > deadline - Duration::from_secs(1) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(is_alive(root), "root should still be alive before kill");

        // Snapshot every descendant so we can assert none survive.
        let mut watched = all_descendants(root);
        watched.push(root);

        kill_process_tree(root);
        let _ = child.try_wait();

        // Nothing in the original tree (or reparented to us) should remain.
        for pid in watched {
            assert!(!is_alive(pid), "pid {pid} should be dead after kill_process_tree");
        }

        // Also assert no leftover "sleep 999" from this test.
        // (Best-effort: only fail if we can still see a descendant we tracked.)
        reap_zombies();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn direct_children_finds_spawned_child() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().expect("spawn sleep");
        let pid = child.id();
        // Give the kernel a moment to publish the child in /proc.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut found = false;
        while Instant::now() < deadline {
            let kids = direct_children(std::process::id());
            if kids.contains(&pid) {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(found, "expected {pid} in direct_children of self");
        let _ = child.kill();
        let _ = child.wait();
        reap_zombies();
    }
}
