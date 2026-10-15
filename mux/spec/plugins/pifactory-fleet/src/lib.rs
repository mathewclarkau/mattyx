//! pifactory-fleet: wasm32-unknown-unknown adapter for mattyx plugins.
//!
//! This is the runtime artifact for the pifactory-fleet example plugin
//! shipped at `mux/spec/plugins/pifactory-fleet/`. It compiles to
//! `bin/fleet.wasm` (via `build.sh`) and the mtyx loader
//! (`mux/crates/mux-tui/src/plugin_host.rs`) instantiates it with the
//! manifest's `fuel`, `memory_mib`, and `max_runtime_ms` budgets.
//!
//! Adapter model: thin. The plugin takes one user verb, decides which
//! mtyx_call(s) to make, and returns. The actual fleet decision
//! (which roles to spawn, which prompts to send) is made by the
//! calling operator, not by the plugin. This mirrors the
//! `cmux_dispatch_worker_pane*` family of helpers in
//! `scripts/cmux-panel-lib.sh` (the pifactory repo's shell glue),
//! which is itself a thin shell wrapper around the mtyx CLI verbs.
//!
//! ## ABI
//!
//! The mtyx loader exposes three host imports (see
//! `mux/crates/mux-tui/src/plugin_host.rs::define_host_imports`):
//!
//! ```text
//! mtyx_token() -> u64            // high 32 = ptr, low 32 = len
//! mtyx_call(req_ptr, req_len, out_ptr, out_cap) -> i32   // returns resp len, or -1 on err
//! mtyx_log(level, ptr, len)       // 0=info, 1=warn, 2=error
//! ```
//!
//! Plus standard WASI preview1 imports. We only import `proc_exit`
//! (so `_mtyx_plugin_main` returns with a real exit code) and the
//! `args_get` / `args_sizes_get` family (so we can read argv[1] = the
//! verb the user typed).
//!
//! ## Single-call-per-invocation
//!
//! The loader's `expected_request_id` starts at 0 and is never
//! incremented (a known loader limitation; see the plugin's README).
//! Each verb here therefore makes at most one `mtyx_call` and uses
//! `id: 0` in the request. A multi-call plugin would fail the
//! second call with `StaleId`.
//!
//! ## Memory
//!
//! Two static byte buffers for the mtyx_call request and response.
//! Sized at the manifest's `memory_mib` default (64 MiB) minus what
//! the rest of the WASM linear memory needs — well over what the
//! plugin ever needs to transfer.
//!
//! ## Building
//!
//! See `build.sh` in this directory:
//!
//! ```bash
//! ./build.sh
//! ```
//!
//! Generates `bin/fleet.wasm` (a wasm32-unknown-unknown cdylib).

#![no_std]
#![no_main]
// wasmtime instantiates one plugin WASM per call on a single
// execution thread, so `static mut` references are safe in this
// context — the static-mut lint flags the general "no aliasing"
// rule, which only matters when other threads could observe the
// same mutable global. The plugin has no other threads.
#![allow(static_mut_refs)]

use core::panic::PanicInfo;
use core::ptr;

// ---------- mtyx host imports ----------

#[link(wasm_import_module = "mtyx")]
extern "C" {
    /// Returns a packed pointer/length to the per-call auth token
    /// bytes. The plugin does NOT need to free the memory; the host
    /// does after the call.
    #[link_name = "token"]
    fn mtyx_token() -> u64;
    /// Sends a JSON request to mtyx through the dispatcher. Returns
    /// the response byte length on success (>=0), or -1 on error
    /// (out-of-cap, invalid JSON, validation failure, etc.). On a
    /// validation failure the host writes a structured error response
    /// to `out_ptr` regardless, so the plugin can read it.
    #[link_name = "call"]
    fn mtyx_call(req_ptr: i32, req_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    /// Emits a log line tagged with the plugin name. Plugin-side
    /// stderr/stdout are not wired up; this is the only diagnostics
    /// channel.
    #[link_name = "log"]
    fn mtyx_log(level: i32, ptr: i32, len: i32);
}

// ---------- WASI preview1 imports ----------
//
// `add_to_linker_sync` in `wasmtime_wasi::preview1` imports the full
// preview1 surface. The loader will reject our module if any host
// import is missing, so we declare the ones we actually call. The
// loader does not validate the import *count* (it wires all of
// preview1 in), but for portability we only import the slice we use.

#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn proc_exit(exit_code: i32);
    /// Writes the argument pointer array to `argv` (each slot is a
    /// 4-byte wasm32 pointer) and the (NUL-terminated) argument
    /// string bytes to `argv_buf`.
    fn args_get(argv: *mut i32, argv_buf: *mut u8) -> i32;
    /// Writes the argument count to `argc` and the total bytes of
    /// argument data (including NUL terminators) to `argv_buf_size`.
    fn args_sizes_get(argc: *mut i32, argv_buf_size: *mut i32) -> i32;
}

// ---------- panic handler ----------

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Translate a Rust panic into a clean WASI exit. The host does
    // not surface panics as PluginError::Trap because wasmtime
    // unwinds them; proc_exit here is the documented way to set the
    // exit code from a no_std panic handler.
    unsafe {
        proc_exit(2);
    }
    // Unreachable; satisfies the `-> !` return type.
    loop {}
}

// ---------- static buffers ----------

/// Scratch space for the JSON request we send via `mtyx_call`. Each
/// verb's request fits in a few hundred bytes (see the constants in
/// `verb_request`); 512 is plenty with room to grow.
static mut REQUEST_BUF: [u8; 512] = [0; 512];

/// Scratch space for the JSON response we read back from
/// `mtyx_call`. Worst case is a `list-workspaces` JSON dump, which
/// fits well under 8 KiB for a pifactory-sized fleet.
static mut RESPONSE_BUF: [u8; 8192] = [0; 8192];

/// Scratch space for argv strings (read by WASI `args_get`).
static mut ARGV_BUF: [u8; 512] = [0; 512];

// ---------- argv parsing ----------

/// Read argv[1] (the verb the user typed, e.g. "ping") into `out`.
/// Returns the byte length copied, or -1 if no such arg or the buffer
/// is too small.
fn read_argv1(out: &mut [u8]) -> i32 {
    let mut argc: i32 = 0;
    let mut argv_buf_size: i32 = 0;
    let rc = unsafe {
        args_sizes_get(
            &mut argc as *mut i32,
            &mut argv_buf_size as *mut i32,
        )
    };
    if rc != 0 || argc < 2 {
        return -1;
    }
    let argv_buf_cap = unsafe { ARGV_BUF.len() } as i32;
    if argv_buf_size > argv_buf_cap || out.len() < argv_buf_size as usize {
        return -1;
    }
    // argv layout: argc pointers followed by argc*N bytes of string data.
    // We need a stack buffer for the pointers; for a pifactory-fleet verb
    // we never have more than ~8 argv entries, so 8 i32 slots suffice.
    let mut argv_ptrs: [i32; 8] = [0; 8];
    let rc = unsafe {
        args_get(
            argv_ptrs.as_mut_ptr(),
            ARGV_BUF.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return -1;
    }
    // Copy argv[1] into out. WASI does not NUL-terminate string
    // pointers; we know the total size from args_sizes_get, so copy
    // until we hit the end of argv[1]'s slice or run out of out.
    let argv_buf_base = unsafe { ARGV_BUF.as_ptr() } as usize;
    let start = argv_ptrs[1] as usize - argv_buf_base;
    let mut end = start;
    while end < argv_buf_cap as usize && unsafe { ARGV_BUF[end] } != 0 {
        end += 1;
    }
    let len = end - start;
    if len > out.len() {
        return -1;
    }
    unsafe {
        ptr::copy_nonoverlapping(ARGV_BUF.as_ptr().add(start), out.as_mut_ptr(), len);
    }
    len as i32
}

// ---------- verb dispatch ----------

/// Dispatch the user verb to one mtyx_call. Returns the mtyx_call
/// response length on success (>=0), or -1 if the verb is unknown or
/// the call failed. On a successful mtyx_call the response bytes are
/// in RESPONSE_BUF[0..len].
fn dispatch(verb: &[u8]) -> i32 {
    if eq(verb, b"ping") {
        call_identify()
    } else if eq(verb, b"status") {
        call_list_workspaces()
    } else if eq(verb, b"deploy") {
        call_new_workspace(b"fleet-scout")
    } else if eq(verb, b"dispatch") {
        call_new_workspace(b"fleet-worker")
    } else if eq(verb, b"rollback") {
        // Close the first workspace we can target; a multi-step
        // close would need loader-side id-increment support.
        call_close_workspace()
    } else {
        log_error(b"unknown verb");
        -1
    }
}

fn call_identify() -> i32 {
    const REQ: &[u8] = b"{\"id\":0,\"verb\":\"identify\",\"args\":{}}";
    send_request(REQ)
}

fn call_list_workspaces() -> i32 {
    const REQ: &[u8] = b"{\"id\":0,\"verb\":\"list-workspaces\",\"args\":{}}";
    send_request(REQ)
}

fn call_new_workspace(role: &[u8]) -> i32 {
    // Build: {"id":0,"verb":"new-workspace","args":{"name":"<role>"}}
    let prefix: &[u8] = b"{\"id\":0,\"verb\":\"new-workspace\",\"args\":{\"name\":\"";
    let suffix: &[u8] = b"\"}}";
    let total = prefix.len() + role.len() + suffix.len();
    if total > unsafe { REQUEST_BUF.len() } {
        log_error(b"request too large");
        return -1;
    }
    unsafe {
        let dst = &mut REQUEST_BUF[..total];
        let mut i = 0;
        dst[i..i + prefix.len()].copy_from_slice(prefix);
        i += prefix.len();
        dst[i..i + role.len()].copy_from_slice(role);
        i += role.len();
        dst[i..i + suffix.len()].copy_from_slice(suffix);
    }
    send_request_bytes(total)
}

fn call_close_workspace() -> i32 {
    // The plugin is single-call; we close by name (the "fleet lead"
    // workspace, the conventional first member of a deployed fleet).
    // Multi-workspace rollback is a manifest-side concern; the loader
    // limit on mtyx_call count (one per invocation) prevents a true
    // multi-call rollback without the loader-side fix.
    const REQ: &[u8] = b"{\"id\":0,\"verb\":\"close-workspace\",\"args\":{\"name\":\"fleet-scout\"}}";
    send_request(REQ)
}

/// Send a pre-built request and copy the response into RESPONSE_BUF.
/// Returns the response byte length or -1 on failure.
fn send_request(req: &[u8]) -> i32 {
    if req.len() > unsafe { REQUEST_BUF.len() } {
        log_error(b"request too large");
        return -1;
    }
    unsafe {
        REQUEST_BUF[..req.len()].copy_from_slice(req);
    }
    send_request_bytes(req.len())
}

fn send_request_bytes(req_len: usize) -> i32 {
    let resp_len = unsafe {
        mtyx_call(
            REQUEST_BUF.as_ptr() as i32,
            req_len as i32,
            RESPONSE_BUF.as_ptr() as i32,
            RESPONSE_BUF.len() as i32,
        )
    };
    if resp_len < 0 {
        log_error(b"mtyx_call failed");
        return -1;
    }
    log_info_bytes(unsafe { RESPONSE_BUF.as_ptr() }, resp_len as usize);
    resp_len
}

// ---------- helpers ----------

fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && {
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

fn log_error(msg: &[u8]) {
    unsafe { mtyx_log(2, msg.as_ptr() as i32, msg.len() as i32) }
}

fn log_info_bytes(ptr: *const u8, len: usize) {
    unsafe { mtyx_log(0, ptr as i32, len as i32) }
}

/// Read the mtyx per-call auth token into `out`. The host mints a
/// fresh token at every plugin invocation (see
/// `plugin_host::mint_token`) and validates every mtyx_call against
/// it; the plugin doesn't *need* the token (the host validates
/// each call) but we log it on startup so a developer running the
/// plugin under `wasmtime` directly can confirm the token mint is
/// firing.
fn read_token_into(out: &mut [u8; 64]) -> usize {
    let packed = unsafe { mtyx_token() };
    let ptr = (packed >> 32) as usize as *const u8;
    let len = (packed & 0xFFFF_FFFF) as usize;
    let n = if len > out.len() { out.len() } else { len };
    unsafe {
        ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), n);
    }
    n
}

// ---------- entrypoint ----------

/// Plugin entrypoint. The mtyx loader tries `_mtyx_plugin_main`
/// first (project convention) and falls back to `_cmux_plugin_main`
/// and `_start` if absent
/// (`mux/crates/mux-tui/src/plugin_host.rs::invoke`). Exporting
/// `_mtyx_plugin_main` matches what the README documents.
///
/// Returns nothing (`()`) — the loader's typed signature is
/// `fn() -> ()`. We use `proc_exit` to surface errors so the
/// process exit code matches the failure.
#[no_mangle]
pub extern "C" fn _mtyx_plugin_main() {
    // Surface the per-call token to mtyx_log so a developer
    // running the plugin outside of `mtyx` can confirm token mint
    // is firing. The token itself is 32 hex chars; the loader
    // produces it from `mint_token()` and discards it after this
    // call returns.
    let mut token: [u8; 64] = [0; 64];
    let n = read_token_into(&mut token);
    log_info_bytes(token.as_ptr(), n);

    let mut verb: [u8; 64] = [0; 64];
    let n = read_argv1(&mut verb);
    if n <= 0 {
        log_error(b"missing verb argument");
        unsafe { proc_exit(2) }
    }
    let rc = dispatch(&verb[..n as usize]);
    if rc < 0 {
        unsafe { proc_exit(1) }
    }
    unsafe { proc_exit(0) }
}

/// Compatibility entrypoint for loaders expecting `_cmux_plugin_main`.
#[no_mangle]
pub extern "C" fn _cmux_plugin_main() {
    _mtyx_plugin_main()
}

// Fallback entrypoint for WASI runtimes that expect `_start`. Same
// body as `_mtyx_plugin_main`.
#[no_mangle]
pub extern "C" fn _start() {
    _mtyx_plugin_main()
}
