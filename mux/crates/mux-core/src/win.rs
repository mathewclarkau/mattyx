//! Windows parity layer: job objects, console APIs and Toolhelp
//! snapshots standing in for the unix primitives used elsewhere.
//!
//! Mapping (see AGENTS.md and the Windows-parity plan):
//!
//! | unix primitive            | Windows replacement                     |
//! |---------------------------|-----------------------------------------|
//! | `PR_SET_PDEATHSIG`        | per-child job object with               |
//! |                           | `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`   |
//! | `kill(pid, 0)` liveness   | `OpenProcess` + `GetExitCodeProcess`   |
//! | `waitpid(-1, WNOHANG)`    | polled `WaitForSingleObject` on tracked |
//! |                           | handles                                 |
//! | `kill(-pgid, SIGTERM)`    | `TerminateJobObject` on the surface's   |
//! |                           | job                                     |
//! | `/proc/<pid>/cmdline`     | Toolhelp32 snapshot image names         |
//! |                           | (cmdline NOT visible — see below)       |
//!
//! Residual limitations, honestly stated:
//!
//! * **No graceful SIGTERM.** `GenerateConsoleCtrlEvent` only reaches
//!   processes attached to the *calling* console; a headless daemon has
//!   none. Surface teardown therefore goes straight to
//!   `TerminateJobObject` (the moral equivalent of SIGKILL for the
//!   tree). Shells get no chance to run shutdown hooks.
//! * **Job assignment is post-spawn.** portable-pty owns the
//!   `CreateProcess` call, so the child is assigned to its job after it
//!   starts. A grandchild the child spawns in that window escapes the
//!   job (the same race PDEATHSIG has between fork and prctl).
//! * **Toolhelp32 has no argv.** `/proc/<pid>/cmdline` has no analogue
//!   available to this process model; agent detection matches image
//!   names (`node.exe`) only, so patterns requiring full command lines
//!   (`claude --resume …`) cannot match on Windows.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use windows_sys::Win32::Foundation::{
    CloseHandle, INVALID_HANDLE_VALUE, STILL_ACTIVE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// pid -> kill-on-close job handle, one job per spawned surface child.
/// LazyLock because `HashMap::new` is not a const fn (E0015 in a plain
/// static); `SURFACE_JOBS.lock()` still works unchanged through Deref.
/// The Vec-backed registry below stays a const `Mutex::new` — `Vec::new`
/// IS const.
static SURFACE_JOBS: LazyLock<Mutex<HashMap<u32, isize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Open handles of directly-spawned children awaiting reaping
/// (`waitpid` analogue). HANDLE is pointer-sized; storing it as `isize`
/// avoids a Send wrapper (raw pointers are not Send, isize is).
static TRACKED_HANDLES: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// Wrap a HANDLE (a raw pointer in windows-sys) as an isize storable in
/// the statics above.
fn handle_as_usize(handle: windows_sys::Win32::Foundation::HANDLE) -> isize {
    handle as isize
}

fn usize_as_handle(raw: isize) -> windows_sys::Win32::Foundation::HANDLE {
    raw as windows_sys::Win32::Foundation::HANDLE
}

/// Put `pid` into a fresh job object with KILL_ON_JOB_CLOSE: when this
/// process dies (however rudely), the kernel closes every job handle we
/// hold and the whole child tree dies with us — the PDEATHSIG
/// equivalent. Returns false when the pid could not be opened or the
/// job could not be assigned (the child then survives daemon death;
/// documented degradation, never a spawn failure).
pub fn assign_pid_to_kill_on_close_job(pid: u32) -> bool {
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            CloseHandle(process);
            return false;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        let assigned = ok != 0 && AssignProcessToJobObject(job, process) != 0;
        CloseHandle(process);
        if !assigned {
            CloseHandle(job);
            return false;
        }
        let mut jobs = SURFACE_JOBS.lock().unwrap();
        jobs.insert(pid, handle_as_usize(job));
        true
    }
}

/// Tear down a surface: `TerminateJobObject` on the job assigned at
/// spawn (the kill(-pgid) analogue — every process in the job dies,
/// including grandchildren that stayed inside it). Falls back to
/// `TerminateProcess` on the bare pid when no job was recorded (spawn
/// raced, or the registry entry was already reaped). There is
/// deliberately no graceful phase; see the module limitations.
pub fn terminate_pid_tree(pid: u32) -> bool {
    let job_raw = SURFACE_JOBS.lock().unwrap().remove(&pid);
    unsafe {
        if let Some(raw) = job_raw {
            let terminated = TerminateJobObject(usize_as_handle(raw), 1) != 0;
            CloseHandle(usize_as_handle(raw));
            return terminated;
        }
        let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return false;
        }
        let terminated = TerminateProcess(process, 1) != 0;
        CloseHandle(process);
        terminated
    }
}

/// `kill(pid, 0)` analogue: does the process exist and still run?
/// `OpenProcess` failing with access-denied counts as alive (the EPERM
/// convention: the pid is occupied by something we may not query).
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            // ERROR_ACCESS_DENIED means "exists but not ours"; anything
            // else (ERROR_INVALID_PARAMETER in practice) means "gone".
            return windows_sys::Win32::Foundation::GetLastError() == 5;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(process, &mut exit_code);
        CloseHandle(process);
        // STILL_ACTIVE (259) is an NTSTATUS (i32) in windows-sys while
        // GetExitCodeProcess writes u32; the value fits u32 trivially,
        // so compare as u32.
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
}

/// Hard-kill one pid (`kill(pid, SIGKILL)` analogue).
pub fn terminate_pid(pid: u32) -> bool {
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return false;
        }
        let terminated = TerminateProcess(process, 1) != 0;
        CloseHandle(process);
        terminated
    }
}

/// Register a child process handle for the `waitpid(-1, WNOHANG)`
/// analogue. Takes ownership: the handle is closed once signalled.
pub fn track_child_handle(handle: windows_sys::Win32::Foundation::HANDLE) {
    TRACKED_HANDLES.lock().unwrap().push(handle_as_usize(handle));
}

/// Non-blocking reap: poll every tracked handle, close and drop the
/// ones that have exited. Uses `WaitForSingleObject(.., 0)` per handle
/// (a single `WaitForMultipleObjects` batch is bounded at
/// MAXIMUM_WAIT_OBJECTS anyway, and the handle count here is small).
pub fn reap_tracked_handles() {
    let mut handles = TRACKED_HANDLES.lock().unwrap();
    handles.retain(|raw| {
        let signalled = unsafe { WaitForSingleObject(usize_as_handle(*raw), 0) == WAIT_OBJECT_0 };
        if signalled {
            unsafe { CloseHandle(usize_as_handle(*raw)) };
        }
        !signalled
    });
}

/// One process row from a Toolhelp32 snapshot. `image` is the exe
/// basename, lower-cased with the `.exe` suffix stripped so pattern
/// matching lines up with unix `comm` values (`node.exe` == `node`).
pub struct SnapshotProcess {
    pub pid: u32,
    pub parent_pid: u32,
    pub image: String,
}

/// Full-system process snapshot via `CreateToolhelp32Snapshot`.
fn snapshot_processes() -> Vec<SnapshotProcess> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let len =
                    entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                let image = name.to_ascii_lowercase().trim_end_matches(".exe").to_string();
                out.push(SnapshotProcess {
                    pid: entry.th32ProcessID,
                    parent_pid: entry.th32ParentProcessID,
                    image,
                });
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// Image name (basename, lowercase, no `.exe`) of one pid, via a fresh
/// snapshot. `None` when the pid is gone.
pub fn process_image_name(pid: u32) -> Option<String> {
    snapshot_processes().into_iter().find(|p| p.pid == pid).map(|p| p.image)
}

/// The pid and image name of `root` plus every descendant, via
/// parent-pid links in a snapshot. This is the `/proc`-walk analogue
/// used for agent detection; cmdline evidence is NOT available (see
/// the module limitations).
pub fn descendant_processes(root: u32) -> Vec<(u32, String)> {
    let snapshot = snapshot_processes();
    let by_pid: HashMap<u32, &SnapshotProcess> = snapshot.iter().map(|p| (p.pid, p)).collect();
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    seen.insert(root);
    while let Some(pid) = stack.pop() {
        if let Some(process) = by_pid.get(&pid) {
            out.push((process.pid, process.image.clone()));
            for child in &snapshot {
                if child.parent_pid == pid && seen.insert(child.pid) {
                    stack.push(child.pid);
                }
            }
        }
    }
    out
}
