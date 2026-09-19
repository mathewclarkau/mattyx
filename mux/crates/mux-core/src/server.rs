//! Control socket: a JSON-lines protocol over the platform transport.
//!
//! This is the attach surface for external frontends (the mtyx app, the
//! bundled `mtyx attach` client, scripts). One JSON request per line;
//! every request gets one JSON response line. Two commands additionally
//! turn the connection full-duplex:
//!
//! - `subscribe` — the server pushes `{"event":...}` lines (tree-changed,
//!   surface-output, surface-exited, title-changed, bell) interleaved
//!   with responses.
//! - `attach-surface` — PTYs receive `{"event":"vt-state"}` with a
//!   base64 VT replay followed by live `{"event":"output"}` pty bytes.
//!   Browsers receive `{"event":"browser-state"}` with optional latest
//!   frame followed by live `{"event":"frame"}` PNG payloads.
//!
//! ```text
//! {"id":1,"cmd":"identify"}
//! {"id":1,"ok":true,"data":{"app":"mtyx","session":"main",...}}
//! ```
//!
//! ## `rename-session` (issue #63)
//!
//! `mtyx rename-session --old X --new Y` connects to the `X` socket and
//! sends `{"cmd":"rename-session","new_name":"Y"}`. The daemon renames
//! THIS session in place: `rename(2)` the `.sock` and `.pid` to the new
//! names, flip `Mux.session`, reparent the snapshot file, and keep serving.
//! The listener **never rebinds**: on a bound `AF_UNIX` `SOCK_STREAM`
//! socket, `rename(2)` reparents the dirent while the kernel keeps the
//! listener bound to the inode (pinned by `unix_socket_survives_rename`),
//! so the daemon stays reachable only at the new path.
//!
//! Ordering: pid file moves first; the socket rename is the commit point
//! (only it changes reachability), so a pid-rename failure bails before
//! anything is committed. Partial failure is self-healing. A LIVE target
//! is refused (`session "Y" already exists`); a STALE target is cleared.
//!
//! **Lifetime guarantee (AC4):** existing panes keep the `MTYX_MUX_SOCKET`
//! they inherited at spawn (the old path) for their lifetime — this is
//! intentional, not a bug. Panes spawned AFTER the rename inherit the new
//! path (`Mux::refresh_socket_env` rewrites the env on every spawn from
//! `socket_path()`, the single source of truth). The startup-path socket
//! watchdog still points at the original path; a SIGKILL after rename is
//! handled by the next `serve()` stale-clear / `kill-stale` (an L3
//! follow-up can respawn the watchdog for the new path).
//!
//! ## Confirmed (receipted) input — `send --confirm` (issue #88)
//!
//! Protocol 7 adds an `input-ack` capability (negotiated via the
//! `capabilities` record in the `identify` response) and a confirmed mode
//! for `send`: `"confirm": true` (plus optional `"timeout_ms"`, default
//! [`DEFAULT_INPUT_ACK_TIMEOUT_MS`], capped at [`MAX_INPUT_ACK_TIMEOUT_MS`])
//! returns success only after the daemon OBSERVES the input consumed —
//! the practical receipt is: bytes written to the PTY AND the surface
//! echoed/advanced (the reader thread applied output: see
//! `PtySurface::output_epoch`), or the child exited, within the timeout.
//! This is a documented heuristic, NOT a byte-exact consumption proof; see
//! `Surface::write_bytes_confirmed`.
//!
//! Ordering: confirmed sends serialize per-surface on a FIFO ticket, so
//! concurrent confirmed sends to one surface resolve in submission order
//! (`PtySurface::ack_gate`). Unconfirmed send (the wire default; the
//! pre-#88 behavior) bypasses the gate and is unchanged.
//!
//! Capability gate: a client that wants confirmed send must check
//! `identify` first ([`require_input_ack_capability`]); against a daemon
//! lacking the capability it gets a structured
//! `legacy_host_receipt_rejected` error, never a silent downgrade. The
//! CLI's `send` does this automatically (`--confirm` is its DEFAULT;
//! `--no-confirm` preserves fire-and-forget).
//!
//! Error codes (extra `"code"` field on error responses; the `error`
//! string keeps a `"code: message"` prefix for string-matching callers):
//!
//! - `oversized_input` — confirmed send payload exceeds
//!   [`MAX_CONFIRMED_SEND_BYTES`] (1 MiB). Rejected up front rather than
//!   applying receipt backpressure to an unbounded write.
//! - `input_ack_timeout` — no receipt within the timeout (or the send
//!   never reached its FIFO turn). Bytes were written unless the message
//!   says otherwise; delivery is unproven, not failed.
//! - `legacy_host_receipt_rejected` — emitted by the CLIENT-side gate
//!   when the daemon lacks `input-ack`; documented here as part of the
//!   capability contract (this server never emits it).

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{IconName, Screen, State};
use crate::platform::{self, transport};
use crate::{
    assign_short_ids, AttachFrame, DefaultColors, Mux, MuxEvent, Node, PaneId, Rgb, ScreenId,
    SplitDir, SurfaceId, SurfaceKind, WorkspaceId,
};

/// Control-socket protocol version. 7 adds the `input-ack` capability
/// (confirmed/receipted `send`, issue #88); see the module docs. Bumped
/// from 6 (rename-session, attach `resized` replay events). Older
/// clients keep working against this daemon: their requests deserialize
/// unchanged (all new `send` fields are serde-defaulted) and unconfirmed
/// send behaves exactly as before.
pub const PROTOCOL_VERSION: u32 = 7;

/// Issue #88: hard cap on a CONFIRMED send payload (bytes of `text` plus
/// decoded `bytes`, before the optional CR / shell-sanitisation prefix).
/// Oversized confirmed input is rejected with a structured
/// `oversized_input` error instead of applying receipt backpressure to an
/// unbounded write. Unconfirmed sends are NOT capped (pre-#88 behavior is
/// unchanged).
pub const MAX_CONFIRMED_SEND_BYTES: usize = 1024 * 1024;

/// Issue #88: default `send --confirm` receipt timeout.
pub const DEFAULT_INPUT_ACK_TIMEOUT_MS: u64 = 5_000;

/// Issue #88: server-side cap on the confirmed-send receipt timeout, so a
/// leaked waiter can't park on its connection thread forever (mirrors
/// [`MAX_AGENT_WAIT_MS`]).
pub const MAX_INPUT_ACK_TIMEOUT_MS: u64 = 60_000;

/// Issue #88: the input-ACK capability key in the `identify` response's
/// `capabilities` object. A daemon reporting it implements confirmed
/// (receipted) input for `send` (see the module docs).
pub const CAP_INPUT_ACK: &str = "input-ack";

/// True when an `identify` response's `data` advertises the input-ACK
/// capability (issue #88).
pub fn identify_has_input_ack(identify: &Value) -> bool {
    identify.get("capabilities").and_then(|c| c.get(CAP_INPUT_ACK)).and_then(Value::as_bool)
        == Some(true)
}

/// Issue #88 CLIENT-side capability gate: refuse a confirmed send against
/// a daemon that lacks input-ACK (no `capabilities` record, e.g. protocol
/// <= 6) instead of silently downgrading to fire-and-forget. The bundled
/// CLI calls this on an `identify` pre-flight before every confirmed
/// `send`; the error carries the structured code
/// `legacy_host_receipt_rejected`.
pub fn require_input_ack_capability(identify: &Value) -> Result<(), ServerError> {
    if identify_has_input_ack(identify) {
        return Ok(());
    }
    let protocol = identify.get("protocol").and_then(Value::as_u64).unwrap_or(0);
    Err(ServerError::new(
        "legacy_host_receipt_rejected",
        format!(
            "daemon (protocol {protocol}) does not advertise the input-ACK capability,              so a confirmed-send receipt cannot be negotiated; pass --no-confirm for              fire-and-forget delivery"
        ),
    ))
}

/// Issue #88: structured socket error — a stable machine-readable `code`
/// plus a human message. Serialized as an extra `"code"` field on the
/// error response line (old clients ignore it), and the `error` string
/// keeps a `"<code>: <message>"` prefix so string-matching callers see
/// the code too.
#[derive(Debug, Clone)]
pub struct ServerError {
    /// Stable machine-readable error kind (see the module docs for the
    /// issue-#88 codes).
    pub code: &'static str,
    pub message: String,
}

impl ServerError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        ServerError { code, message: message.into() }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServerError {}

/// Default socket path for a session.
pub fn default_socket_path(session: &str) -> PathBuf {
    platform::runtime_dir().join(format!("{session}.sock"))
}

/// PID file path corresponding to a socket path.
pub fn pid_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("pid")
}

/// Check if a process ID is currently alive.
pub fn is_process_alive(pid: u32) -> bool {
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
        // OpenProcess + GetExitCodeProcess probe (access-denied counts
        // as alive, the EPERM convention); see win.rs.
        crate::win::is_process_alive(pid)
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        true
    }
}

/// Check if a process ID is alive AND is a mtyx process.
///
/// Windows: matches the process image name (mtyx/cmux) via a
/// Toolhelp32 snapshot. Weaker than the unix cmdline check (an
/// unrelated process named `mtyx.exe` also matches), but Windows pid
/// reuse is aggressive enough that the check still catches the common
/// stale-pidfile case; documented degradation.
pub fn is_cmux_process(pid: u32) -> bool {
    if !is_process_alive(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let cmdline_path = format!("/proc/{pid}/cmdline");
        if let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) {
            // Accept both the canonical `mtyx` name and the `cmux` transition
            // alias (Cargo.toml keeps a [[bin]] so old wrappers keep working).
            cmdline.contains("mtyx") || cmdline.contains("cmux")
        } else {
            false
        }
    }
    #[cfg(windows)]
    {
        match crate::win::process_image_name(pid) {
            Some(image) => image.contains("mtyx") || image.contains("cmux"),
            None => false,
        }
    }
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    {
        true
    }
}

/// Check if a session socket path is live (connectable AND process is alive if pidfile present).
pub fn is_session_socket_live(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    if transport::connect(socket_path).is_err() {
        return false;
    }
    let pid_p = pid_path(socket_path);
    if pid_p.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_p) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                if !is_cmux_process(pid) {
                    return false;
                }
            }
        }
    }
    true
}

/// Client-side socket resolution for a session (rename compat).
///
/// Probes the canonical `mtyx-<uid>/<session>.sock` first; if it is not
/// live, probes a LIVE cmux-era `cmux-<uid>/<session>.sock` (probe
/// only — nothing is ever created) so a client keeps talking to a
/// pre-rename server until it is restarted under the new name. When
/// neither is live the canonical path is returned, so the connect
/// error names where a new server is expected. The server bind path
/// (`serve`) deliberately keeps [`default_socket_path`] — new sockets
/// are only ever created under the canonical dir.
pub fn client_socket_path(session: &str) -> PathBuf {
    let canonical = default_socket_path(session);
    let canonical_live = is_session_socket_live(&canonical);
    let legacy = platform::legacy_runtime_dir().join(format!("{session}.sock"));
    let legacy_live = is_session_socket_live(&legacy);
    platform::pick_runtime_socket(canonical, legacy, canonical_live, legacy_live)
}

/// Reject session names that are unsafe as filesystem path components.
/// The name becomes `<name>.sock` / `<name>.pid` /
/// `$XDG_STATE_HOME/mattyx/sessions/<name>.json`, so a `/` or `\0` is a
/// path-traversal / NUL-injection vector (AGENTS.md review checklist).
/// Called from BOTH the CLI (`run_rename_session`, client-side defence)
/// and the server (`RenameSession` handler, the security authority).
pub fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') {
        anyhow::bail!("session name cannot contain a path separator");
    }
    if name.contains('\0') {
        anyhow::bail!("session name cannot contain NUL");
    }
    if name.chars().any(|c| c.is_control()) {
        anyhow::bail!("session name cannot contain control characters");
    }
    if name != name.trim() {
        anyhow::bail!("session name cannot have leading/trailing whitespace");
    }
    if matches!(name, "." | "..") {
        anyhow::bail!("session name cannot be \".\" or \"..\"");
    }
    if name.len() > 255 {
        anyhow::bail!("session name too long (max 255)");
    }
    Ok(())
}

#[derive(Deserialize)]
struct Request {
    id: Option<Value>,
    #[serde(flatten)]
    cmd: Command,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Command {
    Identify,
    ListWorkspaces,
    Send {
        surface: SurfaceId,
        #[serde(default)]
        text: Option<String>,
        /// Base64-encoded raw bytes, written verbatim to the pty.
        #[serde(default)]
        bytes: Option<String>,
        /// If true, append a literal CR (0x0D) to the written bytes — used to
        /// submit a fish REPL buffer when dispatching into a mtyx pane from
        /// a non-interactive context (e.g. another agent via `mtyx send`).
        /// Without this, fish's multi-line mode holds the text in its input
        /// buffer and waits for a real CR keystroke that mtyx's regular
        /// `send` does not deliver. Added 2026-07-09 to support the
        /// pifactory-fleet interactive-pi worker dispatch pattern
        /// (`scripts/cmux-panel-lib.sh`'s `cmux_dispatch_worker_pane_interactive`).
        #[serde(default)]
        send_cr: Option<bool>,
        /// Shell-aware input sanitisation (issue #35): one of `auto`,
        /// `fish`, `bash`, `zsh`, `sh`, `nu`, or `raw`. `raw` (default,
        /// when absent) writes bytes verbatim, preserving pre-#35
        /// behaviour. A known shell gets a leading `\n` prefixed to `text`
        /// when it starts with a shell metacharacter or contains an
        /// unclosed quote, so `$ pwd\n` is typed literally into a fish
        /// pane instead of being interpreted by the shell's line editor.
        /// `auto` resolves the pane's shell from `/proc/<child-pid>/cmdline`
        /// on Linux and falls back to `raw` on lookup failure or non-Linux.
        #[serde(default)]
        shell: Option<String>,
        /// Issue #88: request a RECEIPT — the reply is sent only after the
        /// daemon observes the input consumed (surface echo/advance or
        /// child exit within `timeout_ms`). Requires the `input-ack`
        /// capability (protocol 7+); absent/false keeps the pre-#88
        /// fire-and-forget write (the wire default, so old requests and
        /// old daemons behave exactly as before).
        #[serde(default)]
        confirm: Option<bool>,
        /// Issue #88: receipt timeout in milliseconds for
        /// `confirm: true`. Defaults to [`DEFAULT_INPUT_ACK_TIMEOUT_MS`],
        /// capped at [`MAX_INPUT_ACK_TIMEOUT_MS`] server-side. `0` is
        /// rejected (a confirmed send must wait at least 1 ms).
        #[serde(default)]
        timeout_ms: Option<u64>,
        /// Issue #93: bypass the blocked-send gate. Absent/false means a
        /// surface whose effective agent state is `Blocked` refuses the
        /// send with the structured `agent_blocked` error and NO bytes
        /// are written; `true` restores the pre-#93 raw behaviour.
        #[serde(default)]
        force: Option<bool>,
        /// Issue #93: observed-transition success. When set, the reply is
        /// sent only after the target agent is observed to transition
        /// (a strictly-newer `state_seq`) into `working`/`blocked` at/after
        /// this call's start, or `wait_activity_ms` elapses. `None` keeps
        /// the fire-and-forget reply. A `send` that lands while the pane
        /// is already `working` but produces no new report still times out
        /// — the transition must be *observed*, not merely implied.
        #[serde(default)]
        wait_activity_ms: Option<u64>,
    },
    ReadScreen {
        surface: SurfaceId,
    },
    /// One-shot VT replay of the surface's current state (base64).
    VtState {
        surface: SurfaceId,
    },
    /// New tab in a pane (default: the active pane).
    NewTab {
        #[serde(default)]
        pane: Option<PaneId>,
        #[serde(default)]
        cwd: Option<String>,
        /// Expected content size in cells (spawn-at-size avoids shell
        /// redraw artifacts).
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        /// Issue #76: explicit child argv (agent start) — absent means
        /// the default login shell. Recorded at spawn so `layout-export`
        /// can replay it.
        #[serde(default)]
        command: Option<Vec<String>>,
        /// Issue #76: extra env for the child, as a JSON object of
        /// string → string.
        #[serde(default)]
        env: Option<BTreeMap<String, String>>,
        /// Create a git worktree for this branch and spawn the tab
        /// inside it (issue #77 AC4). Takes precedence over `cwd` for
        /// the spawn directory; `cwd` (or the pane's working dir) still
        /// selects the repository to branch from.
        #[serde(default)]
        branch: Option<String>,
        /// Display label for the worktree record (issue #77 AC1).
        #[serde(default)]
        label: Option<String>,
    },
    NewBrowserTab {
        url: String,
        #[serde(default)]
        pane: Option<PaneId>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    SetCellPixels {
        #[serde(alias = "width_px")]
        width_px: u16,
        #[serde(alias = "height_px")]
        height_px: u16,
    },
    BrowserMouse {
        surface: SurfaceId,
        kind: String,
        #[serde(alias = "x_px")]
        x_px: f64,
        #[serde(alias = "y_px")]
        y_px: f64,
        #[serde(default)]
        button: Option<String>,
        #[serde(default, alias = "click_count")]
        click_count: Option<u32>,
    },
    BrowserWheel {
        surface: SurfaceId,
        #[serde(alias = "x_px")]
        x_px: f64,
        #[serde(alias = "y_px")]
        y_px: f64,
        #[serde(alias = "delta_y_px")]
        delta_y_px: f64,
    },
    BrowserKey {
        surface: SurfaceId,
        kind: String,
        key: String,
        code: String,
        #[serde(alias = "windows_virtual_key_code")]
        windows_virtual_key_code: u32,
        modifiers: u32,
        #[serde(default)]
        text: Option<String>,
    },
    BrowserInsertText {
        surface: SurfaceId,
        text: String,
    },
    BrowserNavigate {
        surface: SurfaceId,
        url: String,
    },
    BrowserBack {
        surface: SurfaceId,
    },
    BrowserForward {
        surface: SurfaceId,
    },
    BrowserReload {
        surface: SurfaceId,
    },
    BrowserActivate {
        surface: SurfaceId,
    },
    NewWorkspace {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    /// New workspace whose tab is a `cmuxd-remote` session over SSH
    /// instead of a local shell (see `remote_pty.rs`). Building/caching
    /// the daemon binary for the remote's OS/arch is the caller's job
    /// (typically `mtyx ssh <host>`, not this socket API directly);
    /// `local_binary_path` must already point at one.
    NewRemoteWorkspace {
        host: String,
        slot: String,
        session_id: String,
        local_binary_path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    /// Return the server's resolved presentation chrome (theme/tabs/
    /// sidebar/keys) so a thin-client `mtyx attach --apply-local-config`
    /// can layer its local `Overlay` on top of the server config rather
    /// than replacing it with the laptop's own config (issue #40,
    /// blocker 1). See `mux-tui`'s `Config::resolved_chrome_value`/
    /// `Config::from_server_chrome` for the round-trip shape.
    GetResolvedConfig,
    /// New screen in a workspace (default: the active one).
    NewScreen {
        #[serde(default)]
        workspace: Option<WorkspaceId>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    Split {
        pane: PaneId,
        /// "right" or "down"
        dir: String,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        /// Issue #76: explicit argv/env for the new pane's first tab.
        #[serde(default)]
        command: Option<Vec<String>>,
        #[serde(default)]
        env: Option<BTreeMap<String, String>>,
        /// Create a git worktree for this branch and spawn the new pane
        /// inside it (issue #77 AC4).
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    SetRatio {
        pane: PaneId,
        /// "right" or "down"
        dir: String,
        ratio: f32,
    },
    MoveTab {
        surface: SurfaceId,
        pane: PaneId,
        index: usize,
    },
    MoveWorkspace {
        workspace: WorkspaceId,
        index: usize,
    },
    SetDefaultColors {
        #[serde(default)]
        fg: Option<String>,
        #[serde(default)]
        bg: Option<String>,
    },
    /// Close one tab.
    CloseSurface {
        surface: SurfaceId,
    },
    /// Close a pane and all its tabs.
    ClosePane {
        pane: PaneId,
    },
    CloseScreen {
        screen: ScreenId,
    },
    CloseWorkspace {
        workspace: WorkspaceId,
        /// Issue #100: also close this workspace's worktree-child
        /// workspaces. Absent (older clients) or `false` closes only the
        /// target and reports the surviving children in the response.
        #[serde(default)]
        group: bool,
    },
    RenamePane {
        pane: PaneId,
        /// Empty clears the name (falls back to the tab title).
        name: String,
    },
    RenameSurface {
        surface: SurfaceId,
        /// Empty clears the name (falls back to the generated tab label).
        name: String,
    },
    RenameScreen {
        screen: ScreenId,
        /// Empty clears the name (falls back to the screen number).
        name: String,
    },
    RenameWorkspace {
        workspace: WorkspaceId,
        name: String,
    },
    /// `colour: Some(hex)` sets the workspace color; `colour: None` (an
    /// explicit `null` or the key absent) clears it.
    SetWorkspaceColor {
        workspace: WorkspaceId,
        #[serde(default)]
        colour: Option<String>,
    },
    /// Set the status icon on a workspace, defaulting to the active one.
    SetStatus {
        #[serde(default)]
        workspace: Option<WorkspaceId>,
        icon: String,
    },
    /// Positional CLI shorthand which creates a missing named workspace.
    WorkspaceColor {
        name: String,
        color: String,
    },
    /// Emits a transient `flash` event to subscribers. `surface` is
    /// advisory (not validated against the workspace) and just passed
    /// through.
    TriggerFlash {
        workspace: WorkspaceId,
        #[serde(default)]
        surface: Option<SurfaceId>,
    },
    ResizeSurface {
        surface: SurfaceId,
        cols: u16,
        rows: u16,
    },
    FocusPane {
        pane: PaneId,
    },
    /// Select a tab within a pane (default: the active pane).
    SelectTab {
        #[serde(default)]
        pane: Option<PaneId>,
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        delta: Option<isize>,
    },
    /// Select a screen within the active workspace.
    SelectScreen {
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        delta: Option<isize>,
    },
    SelectWorkspace {
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        delta: Option<isize>,
    },
    /// Stream mux events on this connection.
    Subscribe,
    /// Stream a surface: vt-state event followed by live output events.
    AttachSurface {
        surface: SurfaceId,
    },
    /// Scroll a surface's viewport by a row delta (negative is up).
    ScrollSurface {
        surface: SurfaceId,
        delta: isize,
    },
    /// Reports agent state for a surface. Hook-sourced reports have
    /// authority over socket-sourced ones (see `spec/commands.md`).
    /// `source` defaults to `"socket"` when absent (issue #75: in-pane
    /// self-reports omit it; hooks pass `"hook"` explicitly and keep
    /// their override authority). `agent` names the pane for the
    /// name-addressed verbs; `message` is free-text context (issue #75).
    ReportAgent {
        surface: SurfaceId,
        state: String,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Known agent-status records, optionally filtered.
    ListAgents {
        #[serde(default)]
        surface: Option<SurfaceId>,
        #[serde(default)]
        state: Option<String>,
    },
    /// Ambient agent detection on one surface (issue #78 AC1): walk the
    /// pane PTY's process tree + scrape the visible screen against the
    /// pattern registry, cache the result, and return it with a
    /// confidence and the evidence line that triggered the match.
    DetectAgent {
        surface: SurfaceId,
    },
    /// Ambient detection on every live surface in one call (issue #78
    /// AC2): `{"agents": {"<surface>": "<agent>"}}` for fleet
    /// dashboards. Keys are surface ids — the mtyx pane-content ids
    /// (this repo's model is Workspace → Screen → Pane → Surface).
    DetectAgents,
    /// Add a user pattern to the live registry (issue #78 AC4). Patterns
    /// are substring/glob (`*` wildcard), not regex. `kind` defaults to
    /// `screen`; `confidence` defaults to `medium`.
    AgentPatternAdd {
        name: String,
        pattern: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        confidence: Option<String>,
        #[serde(default)]
        case_insensitive: Option<bool>,
    },
    /// List the effective pattern registry (bundled + user adds).
    AgentPatternList,
    /// Remove every user-added pattern named `name`.
    AgentPatternRemove {
        name: String,
    },
    /// Create a git worktree for `branch` and `cd` the pane's active
    /// tab into it (issue #77 AC1). On failure the error propagates as
    /// `ok:false` and the pane is untouched (AC7).
    PaneWorktreeCreate {
        pane: PaneId,
        branch: String,
        #[serde(default)]
        label: Option<String>,
    },
    /// Every worktree attached to a pane over its lifetime (issue #77
    /// AC2), in creation order.
    PaneWorktreeList {
        pane: PaneId,
    },
    /// Tear down one of a pane's worktrees: `git worktree remove` +
    /// `prune`, then drop the record (issue #77 AC3).
    PaneWorktreeRemove {
        pane: PaneId,
        branch: String,
    },
    /// Issue #75 AC3: read a pane's screen by agent name or surface id.
    /// `source` is one of "visible" (default), "recent", or
    /// "recent-unwrapped"; `recent`/`recent-unwrapped` are currently the
    /// same active-screen content as "visible" (scrollback is not yet
    /// surfaced by the VT formatter — see spec/commands.md), with
    /// "recent-unwrapped" undoing soft line-wraps. `lines` tails the
    /// last N lines (default 40).
    /// Issue #75 AC3: read a pane by agent name or surface id. `source`
    /// is one of "visible" (default: viewport rows only), "recent" (a
    /// bottom-anchored window including scrollback), or
    /// "recent-unwrapped" (the recent window with soft line-wraps
    /// re-joined). `lines` tails the last N lines (default 40).
    AgentRead {
        target: String,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        lines: Option<usize>,
    },
    /// Issue #75 AC4: type literal text into a pane addressed by agent
    /// name or surface id, WITHOUT a trailing CR — the caller submits
    /// separately (e.g. `send --text "" --send-cr`). Shell sanitisation
    /// mirrors `send` (`raw` default).
    AgentSend {
        target: String,
        text: String,
        #[serde(default)]
        shell: Option<String>,
    },
    /// Issue #75 AC5: block until the target agent's reported state
    /// reaches `state`, or `timeout_ms` elapses (exit-1 timeout error).
    /// `timeout_ms: 0` is a single immediate check. Capped server-side
    /// at [`MAX_AGENT_WAIT_MS`] so a leaked waiter can't park on its
    /// connection thread forever.
    ///
    /// Issue #93 adds `require_transition`: when true, a cached state
    /// matching `state` whose `state_seq` predates the call does NOT
    /// satisfy the wait — only an observed transition at/after the call
    /// does. Absent/false keeps the pre-#93 immediate-match behaviour.
    WaitAgentStatus {
        target: String,
        state: String,
        timeout_ms: u64,
        #[serde(default)]
        require_transition: Option<bool>,
    },
    /// Issue #85: block until a surface is *ready* — its screen shows a
    /// recognised prompt (or an agent) AND its PTY has a running
    /// process-tree child — or `timeout_ms` elapses. Read-only
    /// observation: it never writes to the pane or mutates mux state.
    ///
    /// The reply is always `ok:true` with a structured payload (see
    /// [`wait_ready_json`]); a timeout is `{"ready": false, ...}`, NOT
    /// an error, so a headless orchestrator can read the JSON and
    /// decide. The CLI maps `ready:false` to a nonzero exit (AC2).
    /// `timeout_ms` defaults to [`DEFAULT_WAIT_READY_MS`] when absent
    /// (backward compat: a pre-#85 daemon hits serde's unknown-variant
    /// path) and is capped at [`MAX_AGENT_WAIT_MS`] so a leaked waiter
    /// cannot park on its connection thread forever.
    WaitReady {
        surface: SurfaceId,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Rename THIS daemon's session (issue #63): atomically move its
    /// `.sock`/`.pid` to the new name, update the logical session name,
    /// and best-effort reparent the persisted snapshot file. The listener
    /// keeps accepting at the NEW path — `rename(2)` on a bound `AF_UNIX`
    /// socket reparents the dirent while the kernel keeps the listener
    /// bound to the inode (pinned by the `unix_socket_survives_rename`
    /// unit test), so the daemon never rebinds. Carries only `new_name`:
    /// the daemon is authoritative about its own identity. Issued by
    /// `mtyx rename-session --old X --new Y` after the CLI has connected
    /// to the old socket. Backward compatible (no protocol-version bump):
    /// old servers hit serde's unknown-variant path; the attach client
    /// never emits it. Response:
    /// `{"session":"bar","socket_path":"...","pid":<daemon-pid>}`.
    RenameSession {
        new_name: String,
    },
    /// Issue #76: export one workspace's tab/pane/agent-argv topology as
    /// a versioned `LayoutDocument` (the response `data` IS the document).
    /// `workspace` resolves name-first, then numeric workspace id; an
    /// omitted field means the active workspace.
    LayoutExport {
        #[serde(default)]
        workspace: Option<String>,
    },
    /// Issue #76: export every workspace in the session, as
    /// `{"files":[{"filename":"<sanitized>.json","document":{...}}]}`
    /// for the CLI's `--output-dir` fan-out.
    LayoutExportAll,
    /// Issue #76: replay a layout document under `workspace` (created if
    /// missing, AC2). The document is structurally parsed by serde (parse
    /// errors propagate as `bad request`); `validate()` then hard-fails
    /// any schema/geometry drift (AC7) before a single pane is spawned.
    LayoutApply {
        workspace: String,
        document: crate::layout_doc::LayoutDocument,
    },
}

#[derive(Serialize)]
struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Issue #88: stable machine-readable error code, present only when
    /// the handler failed with a [`ServerError`] (e.g. `oversized_input`,
    /// `input_ack_timeout`). Old clients ignore the extra field.
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

/// Line-oriented shared writer: responses and event streams interleave
/// whole lines.
#[derive(Clone)]
struct LineWriter(Arc<Mutex<Box<dyn transport::Stream>>>);

impl LineWriter {
    fn send(&self, value: &Value) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        let mut stream = self.0.lock().unwrap();
        stream.write_all(&bytes)
    }
}

/// Issue #86: `std::fs::create_dir_all` that reports whether it actually
/// created the leaf directory (vs. finding it already present). Needed so
/// the bind path can distinguish "a dir we just made" (safe to chmod
/// 0700) from "someone else's pre-existing dir" (e.g. `/tmp`).
fn create_dir_all_tracked(dir: &Path) -> std::io::Result<bool> {
    if dir.is_dir() {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    Ok(true)
}

/// Issue #86: does this process own `dir` (same uid as the daemon)?
/// Used to decide whether restricting the directory to 0700 is a
/// legitimate hardening step or an intrusion into someone else's tree.
/// Non-unix: always false (no uid concept; `restrict_permissions` is a
/// no-op there anyway).
#[cfg(unix)]
fn dir_owned_by_us(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(dir) else { return false };
    meta.uid() == unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn dir_owned_by_us(_dir: &Path) -> bool {
    false
}

/// Bind the socket and serve connections on background threads.
pub fn serve(mux: Arc<Mux>, path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let explicit = path.is_some();
    let path = path.unwrap_or_else(|| default_socket_path(&mux.session_name()));
    if let Some(dir) = path.parent() {
        // Issue #86 bind-path guard: only chmod a parent directory we own.
        // `create_dir_all` is tracked so "did we just create it" is known;
        // for an explicit `--socket /tmp/x.sock` the parent (/tmp) already
        // exists and is NOT ours, so chmod'ing it 0700 would be a
        // system-breaking side effect. In that case we best-effort the
        // restrict (ignore failure) and warn instead of hard-failing; the
        // socket itself is still chmod 0600 below and peer-authed above.
        let created = create_dir_all_tracked(dir)?;
        let default_runtime_dir = !explicit;
        if created || default_runtime_dir || dir_owned_by_us(dir) {
            platform::restrict_directory(dir)?;
        } else {
            eprintln!(
                "mtyx: warning: {dir} was not created by this process; leaving its \
                 permissions alone (socket keeps 0600)",
                dir = dir.display()
            );
            let _ = platform::restrict_directory(dir);
        }
    }
    let pid_p = pid_path(&path);
    // Refuse to clobber a live socket; remove a stale one.
    if path.exists() || pid_p.exists() {
        if is_session_socket_live(&path) {
            anyhow::bail!(
                "session socket {} is already in use (another instance running?)",
                path.display()
            );
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&pid_p);
    }
    let listener = transport::listen(&path)?;
    // Record the bound socket path as the single source of truth the rename
    // handler mutates and that cleanup/spawn read (issue #63). Set before
    // the accept thread spawns so it is visible to any client connection.
    mux.set_socket_path(path.clone());
    platform::restrict_file(&path)?;

    std::fs::write(&pid_p, format!("{}\n", std::process::id()))?;
    platform::restrict_file(&pid_p)?;

    std::thread::Builder::new().name("mux-server".into()).spawn(move || loop {
        let Ok(stream) = listener.accept() else { continue };
        let mux = mux.clone();
        let _ = std::thread::Builder::new()
            .name("mux-conn".into())
            .spawn(move || handle_connection(mux, stream));
    })?;
    Ok(path)
}

/// Issue #86: the exact JSON denial written on a rejected control
/// connection, or `None` when the decision is not a rejection (accept no
/// response, unsupported falls through to the filesystem boundary).
///
/// Kept as one pure function so the wire shape is pinned by a test and the
/// handler cannot drift from it. `id` is explicitly `null` (unlike
/// [`Response`], which omits a `None` id) because the plan specifies
/// `{"id":null,"ok":false,"error":"peer uid <N> rejected"}` exactly.
pub fn peer_auth_denial_json(decision: &platform::PeerAuthDecision) -> Option<Value> {
    match decision {
        platform::PeerAuthDecision::RejectForeign { uid } => Some(json!({
            "id": Value::Null,
            "ok": false,
            "error": format!("peer uid {uid} rejected"),
        })),
        platform::PeerAuthDecision::RejectLookupError => Some(json!({
            "id": Value::Null,
            "ok": false,
            "error": "peer authentication failed".to_string(),
        })),
        platform::PeerAuthDecision::Accept | platform::PeerAuthDecision::Unsupported => None,
    }
}

fn handle_connection(mux: Arc<Mux>, stream: Box<dyn transport::Stream>) {
    let Ok(write_half) = stream.try_clone_box() else { return };
    let writer = LineWriter(Arc::new(Mutex::new(write_half)));

    // Issue #86: peer-authenticate BEFORE any Request is parsed. The
    // decision itself is pure (`platform::peer_auth_decision`); all we do
    // here is gather the inputs and execute its verdict.
    let peer = stream.peer_uid();
    let decision = platform::peer_auth_decision(
        peer.as_ref().map(|uid| *uid),
        platform::daemon_uid(),
        platform::transport_supports_peer_creds(),
    );
    let denial = peer_auth_denial_json(&decision);
    match decision {
        platform::PeerAuthDecision::Accept => {}
        platform::PeerAuthDecision::RejectForeign { uid } => {
            eprintln!("mtyx: rejected control connection from peer uid {uid}");
            if let Some(denial) = &denial {
                let _ = writer.send(denial);
            }
            return;
        }
        platform::PeerAuthDecision::RejectLookupError => {
            let detail = match &peer {
                Ok(_) => "no daemon uid available".to_string(),
                Err(e) => e.to_string(),
            };
            eprintln!(
                "mtyx: rejected control connection: peer-credential lookup failed ({detail})"
            );
            if let Some(denial) = &denial {
                let _ = writer.send(denial);
            }
            return;
        }
        platform::PeerAuthDecision::Unsupported => {
            // Documented interim: no peer-cred surface on this transport
            // (Windows named pipes / non-Linux unix). Fall back to the
            // filesystem-permissions boundary (runtime dir 0700, socket
            // 0600) and say so exactly once.
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                eprintln!(
                    "mtyx: warning: peer-credential authentication is unavailable on this \
                     transport; relying on filesystem permissions (runtime dir 0700, socket 0600)"
                );
            });
        }
    }

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let id = req.id.clone();
                match handle_command(&mux, req.cmd, &writer) {
                    Ok(data) => {
                        Response { id, ok: true, data: Some(data), error: None, code: None }
                    }
                    // Issue #88: a structured ServerError also carries its
                    // code on the wire (see Response::code).
                    Err(e) => Response {
                        code: e.downcast_ref::<ServerError>().map(|se| se.code.to_string()),
                        id,
                        ok: false,
                        data: None,
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) => Response {
                id: None,
                ok: false,
                data: None,
                error: Some(format!("bad request: {e}")),
                code: None,
            },
        };
        let Ok(value) = serde_json::to_value(&response) else { break };
        if writer.send(&value).is_err() {
            break;
        }
    }
}

fn node_json(node: &Node) -> Value {
    match node {
        Node::Leaf(id) => json!({ "type": "leaf", "pane": id }),
        Node::Split { dir, ratio, a, b } => json!({
            "type": "split",
            "dir": match dir { SplitDir::Right => "right", SplitDir::Down => "down" },
            "ratio": ratio,
            "a": node_json(a),
            "b": node_json(b),
        }),
    }
}

fn pane_json(state: &State, id: PaneId, short_ids: &HashMap<u64, String>) -> Value {
    let Some(pane) = state.panes.get(&id) else {
        return json!({ "id": id, "dead": true });
    };
    json!({
        "id": id,
        "short_id": short_ids.get(&id).cloned().unwrap_or_default(),
        "name": pane.name,
        "active_tab": pane.active_tab,
        "worktrees": pane.worktrees.iter().map(worktree_record_json).collect::<Vec<_>>(),
        "tabs": pane.tabs.iter().map(|sid| {
            let surface = state.surfaces.get(sid);
            json!({
                "surface": sid,
                "short_id": short_ids.get(sid).cloned().unwrap_or_default(),
                "kind": surface.map(|s| s.kind().as_str()).unwrap_or("pty"),
                "browser_source": surface.and_then(|s| s.browser_source().map(|source| source.as_str())),
                "browser_status": surface.and_then(|s| s.browser_status().map(|status| status.as_str())),
                "browser_error": surface.and_then(|s| s.browser_status().and_then(|status| status.error())),
                "browser_frames_stalled": surface.and_then(|s| s.browser_frames_stalled()),
                "name": surface.and_then(|s| s.name()),
                "title": surface.map(|s| s.title()).unwrap_or_default(),
                "cwd": surface.and_then(|s| s.cwd()),
                "agent_state": surface.and_then(|s| s.agent_report()).map(|r| r.state.as_str()),
                "agent_session": surface.and_then(|s| s.agent_report()).and_then(|r| r.session.clone()),
                // Issue #78: the last ambient detection result (name +
                // confidence), cached by `detect-agent`/`detect-agents`. A
                // cached `unknown` detection reports as null so dashboards
                // can distinguish "never detected" from "detected unknown".
                "agent_name": surface
                    .and_then(|s| s.detected_agent())
                    .filter(|d| !d.is_unknown())
                    .map(|d| d.agent),
                "agent_confidence": surface
                    .and_then(|s| s.detected_agent())
                    .filter(|d| !d.is_unknown())
                    .and_then(|d| d.confidence)
                    .map(|c| c.as_str()),
                // Issue #75 AC6: `agent_status` is always a string
                // (default "unknown" for bare panes), unlike the
                // nullable back-compat `agent_state` above.
                "agent_status": surface
                    .and_then(|s| s.agent_report())
                    .map(|r| r.state.as_str())
                    .unwrap_or("unknown"),
                "agent_message": surface.and_then(|s| s.agent_report()).and_then(|r| r.message.clone()),
                "agent_updated_at_ms": surface.and_then(|s| s.agent_report()).map(|r| r.updated_at_ms),
                "size": surface.map(|s| {
                    let (c, r) = s.size();
                    json!({"cols": c, "rows": r})
                }),
                "dead": surface.map(|s| s.is_dead()).unwrap_or(true),
            })
        }).collect::<Vec<_>>(),
    })
}

fn screen_json(
    state: &State,
    screen: &Screen,
    active: bool,
    short_ids: &HashMap<u64, String>,
) -> Value {
    let mut pane_ids = Vec::new();
    screen.root.pane_ids(&mut pane_ids);
    json!({
        "id": screen.id,
        "short_id": short_ids.get(&screen.id).cloned().unwrap_or_default(),
        "name": screen.name,
        "active": active,
        "active_pane": screen.active_pane,
        "layout": node_json(&screen.root),
        "panes": pane_ids.iter().map(|id| pane_json(state, *id, short_ids)).collect::<Vec<_>>(),
    })
}

fn workspaces_json(state: &State) -> Value {
    let ids = state
        .workspaces
        .iter()
        .flat_map(|ws| {
            let mut ids = vec![ws.id];
            for screen in &ws.screens {
                ids.push(screen.id);
                screen.root.pane_ids(&mut ids);
            }
            ids
        })
        .chain(state.surfaces.keys().copied());
    let short_ids = assign_short_ids(ids);
    json!({
        "workspaces": state.workspaces.iter().enumerate().map(|(i, ws)| {
            json!({
                "id": ws.id,
                "short_id": short_ids.get(&ws.id).cloned().unwrap_or_default(),
                "name": ws.name,
                "color": ws.color.map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
                "icon": ws.icon.as_ref().map(|icon| icon.as_str()),
                "active": i == state.active_workspace,
                "screens": ws.screens.iter().enumerate().map(|(s, screen)| {
                    screen_json(state, screen, s == ws.active_screen, &short_ids)
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    })
}

/// Shell-aware input sanitisation for `send` (issue #35).
///
/// Some shells (fish especially) interpret a leading `$`, `!` or an
/// unterminated quote in a pasted input buffer, so a `mtyx send --text
/// '$ pwd\n'` can corrupt a pane. When a known shell is selected
/// (explicitly or via `auto`) we prefix a single `\n` to reset the
/// line editor's buffer when the text could be mis-parsed. `raw` (the
/// default) writes bytes verbatim — unchanged from pre-#35 behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShellMode {
    Auto,
    Fish,
    Bash,
    Zsh,
    Sh,
    Nu,
    Raw,
}

impl ShellMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "fish" => Some(Self::Fish),
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "sh" => Some(Self::Sh),
            "nu" => Some(Self::Nu),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }
}

/// Resolve the `shell` request field to a concrete mode. Unknown values
/// are a protocol error; `auto` resolves against the pane's child pid.
fn resolve_shell_mode(shell: Option<&str>, child_pid: Option<u32>) -> anyhow::Result<ShellMode> {
    match shell {
        None => Ok(ShellMode::Raw),
        Some(name) => match ShellMode::parse(name) {
            Some(ShellMode::Auto) => Ok(detect_shell_from_child(child_pid)),
            Some(mode) => Ok(mode),
            None => {
                anyhow::bail!("bad shell {name:?} (want auto, fish, bash, zsh, sh, nu, or raw)")
            }
        },
    }
}

/// Detect the pane's shell from its PTY child process.
///
/// Linux: reads `/proc/<pid>/cmdline` and matches the argv[0] basename
/// (minus a leading `-` for login shells). Windows: matches the
/// child's exe image name via a Toolhelp32 snapshot (no argv is
/// visible), falling back to `$COMSPEC`. Falls back to `raw` on any
/// lookup failure, so `--shell auto` never errors.
fn detect_shell_from_child(child_pid: Option<u32>) -> ShellMode {
    #[cfg(target_os = "linux")]
    {
        let Some(pid) = child_pid else { return ShellMode::Raw };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return ShellMode::Raw;
        };
        // argv[0] is the first NUL-terminated element (cmdline is
        // NUL-separated); a login shell may be invoked with a leading `-`.
        let argv0 = cmdline
            .split(|&b| b == 0)
            .next()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        let name = argv0.trim_start_matches('-').rsplit('/').next().unwrap_or("");
        match name {
            "fish" => ShellMode::Fish,
            "bash" => ShellMode::Bash,
            "zsh" => ShellMode::Zsh,
            "sh" | "dash" => ShellMode::Sh,
            "nu" | "nushell" => ShellMode::Nu,
            _ => ShellMode::Raw,
        }
    }
    #[cfg(windows)]
    {
        // No /proc: match the child's exe image name from a Toolhelp32
        // snapshot, falling back to $COMSPEC's basename — the closest
        // analogue to the unix "what shell did the pane spawn" probe.
        // PowerShell maps to Raw on purpose: its quoting rules differ
        // from every mode in the issue #35 table, so no transformation
        // is the safe default. Anything else is Raw too, keeping the
        // never-error contract of the linux path's fallback.
        let Some(pid) = child_pid else { return ShellMode::Raw };
        let name = crate::win::process_image_name(pid)
            .or_else(|| {
                std::env::var_os("COMSPEC").map(|c| {
                    let c = c.to_string_lossy().into_owned();
                    c.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
                })
            })
            .unwrap_or_default();
        let name = name.trim_end_matches(".exe");
        match name {
            "fish" => ShellMode::Fish,
            "bash" => ShellMode::Bash,
            "zsh" => ShellMode::Zsh,
            "sh" | "dash" => ShellMode::Sh,
            "nu" | "nushell" => ShellMode::Nu,
            _ => ShellMode::Raw,
        }
    }
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    {
        let _ = child_pid;
        ShellMode::Raw
    }
}

/// True when `text` has an unbalanced single or double quote, which a
/// shell line editor would keep waiting on (holding the input buffer).
fn has_unclosed_quote(text: &str) -> bool {
    let mut single = false;
    let mut double = false;
    for c in text.chars() {
        match c {
            '\'' => single = !single,
            '"' => double = !double,
            _ => {}
        }
    }
    single || double
}

/// Issue #35's table: a leading shell metacharacter (`$`, `!`, a quote,
/// a bracket, `~`, `#`) or an unclosed quote needs a buffer reset.
/// `sh` is not in the table (no transformation); `nu` only resets for
/// unclosed quotes.
fn needs_buffer_reset(mode: ShellMode, text: &str) -> bool {
    match mode {
        ShellMode::Raw | ShellMode::Sh | ShellMode::Auto => false,
        ShellMode::Nu => has_unclosed_quote(text),
        ShellMode::Fish | ShellMode::Bash | ShellMode::Zsh => {
            has_unclosed_quote(text)
                || text.starts_with('$')
                || text.starts_with('!')
                || text.starts_with('\'')
                || text.starts_with('"')
                || text.starts_with('(')
                || text.starts_with('[')
                || text.starts_with('{')
                || text.starts_with('~')
                || text.starts_with('#')
        }
    }
}

/// Apply issue #35's sanitisation: prefix a single `\n` when the text
/// could be mis-parsed by the selected shell. `raw` passes through.
fn sanitise_text(mode: ShellMode, text: &str) -> String {
    if needs_buffer_reset(mode, text) {
        let mut out = String::with_capacity(text.len() + 1);
        out.push('\n');
        out.push_str(text);
        out
    } else {
        text.to_string()
    }
}

/// Issue #76: the socket `command`/`env` spawn fields → `SpawnOverrides`
/// (agent start). `None` when neither is present keeps the legacy spawn.
fn spawn_overrides(
    command: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
) -> Option<crate::SpawnOverrides> {
    if command.is_none() && env.is_none() {
        return None;
    }
    Some(crate::SpawnOverrides {
        command,
        extra_env: env.map(|m| m.into_iter().collect()).unwrap_or_default(),
        cwd: None,
    })
}

fn get_surface(mux: &Mux, id: SurfaceId) -> anyhow::Result<Arc<crate::Surface>> {
    mux.surface(id).ok_or_else(|| anyhow::anyhow!("unknown surface {id}"))
}

/// Last `n` lines of `text`, ignoring trailing blank rows the VT plain
/// formatter can leave below the cursor (issue #75's `agent-read --lines`).
fn tail_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut lines: Vec<&str> = text.lines().collect();
    while matches!(lines.last(), Some(last) if last.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    lines.join("\n")
}

/// Upper bound for `wait-agent-status` timeouts (10 minutes): the
/// server parks one connection thread per waiter, so an unbounded
/// timeout would let leaked waiters accumulate forever (plan §5.4).
const MAX_AGENT_WAIT_MS: u64 = 600_000;

/// Issue #85: default `wait-ready` timeout when the request omits
/// `timeout_ms`. 5 s is long enough for a login shell/first prompt on a
/// cold pane and short enough that a caller that forgot the flag does
/// not hang; explicit callers should pass their own budget.
pub const DEFAULT_WAIT_READY_MS: u64 = 5_000;

/// Issue #85: resolve + cap a `wait-ready` timeout. Absent falls back to
/// [`DEFAULT_WAIT_READY_MS`]; `0` is a legal single immediate check
/// (mirrors `wait-agent-status`), anything over [`MAX_AGENT_WAIT_MS`]
/// is rejected. Extracted so a table test can pin it.
fn validate_wait_ready_timeout(timeout_ms: Option<u64>) -> anyhow::Result<u64> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_WAIT_READY_MS);
    if timeout_ms > MAX_AGENT_WAIT_MS {
        anyhow::bail!("timeout {timeout_ms}ms exceeds the {MAX_AGENT_WAIT_MS}ms cap");
    }
    Ok(timeout_ms)
}

/// Issue #93: resolve + cap a `send --wait` observed-activity timeout. `0`
/// is a legal single immediate check (mirrors `wait-agent-status`); the
/// absent case never reaches here (the field being present is what turns
/// the wait on), so `0` here means exactly one observation attempt.
/// Anything over [`MAX_AGENT_WAIT_MS`] is rejected before any wait, so a
/// leaked waiter cannot park a connection thread forever.
fn validate_wait_activity_timeout(timeout_ms: u64) -> anyhow::Result<u64> {
    if timeout_ms > MAX_AGENT_WAIT_MS {
        anyhow::bail!("timeout {timeout_ms}ms exceeds the {MAX_AGENT_WAIT_MS}ms cap");
    }
    Ok(timeout_ms)
}

/// Issue #93: block until the surface's agent is observed to transition
/// (its `state_seq` strictly exceeds `started_seq`) into `working` or
/// `blocked`, or `timeout` elapses.
///
/// Returns the observed state on success, or `None` on timeout. A cached
/// report whose sequence predates `started_seq` (e.g. the agent was
/// already `working` before the caller's send) does NOT satisfy this —
/// success requires an *observed* transition at/after the call's start,
/// which is the whole point of `--wait`.
///
/// A pane whose surface exited mid-wait errors immediately (it can never
/// produce the transition), mirroring `wait-agent-status`'s review F2.
fn wait_for_agent_activity(
    mux: &Arc<Mux>,
    surface: &Arc<crate::Surface>,
    started_seq: u64,
    timeout: std::time::Duration,
) -> anyhow::Result<Option<crate::AgentState>> {
    let surface_id = surface.id;
    // Subscribe BEFORE the immediate check so a report landing between
    // the two is still observed (the channel is unbounded).
    let events = mux.subscribe();
    let immediate = surface.agent_report().filter(|report| {
        report.state_seq > started_seq
            && matches!(report.state, crate::AgentState::Working | crate::AgentState::Blocked)
    });
    if let Some(report) = immediate {
        return Ok(Some(report.state));
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match events.recv_timeout(remaining) {
            Ok(MuxEvent::AgentStateChanged { surface: s, report, .. })
                if s == surface_id
                    && report.state_seq > started_seq
                    && matches!(
                        report.state,
                        crate::AgentState::Working | crate::AgentState::Blocked
                    ) =>
            {
                return Ok(Some(report.state));
            }
            Ok(MuxEvent::SurfaceExited(s)) if s == surface_id => {
                anyhow::bail!("surface {surface_id} exited while waiting for agent activity");
            }
            Ok(_) => continue,
            Err(_) => return Ok(None),
        }
    }
}

/// Issue #85 response payload, sent both when ready and on timeout
/// (`ready` distinguishes them). `child` is `null` until a process-tree
/// child is observed; `prompt_seen` reports the screen half on its own
/// so a caller can tell "shell up, command not yet" from "nothing yet".
fn wait_ready_json(
    surface: SurfaceId,
    readiness: &crate::mux::SurfaceReadiness,
    elapsed_ms: u64,
) -> Value {
    json!({
        "ready": readiness.is_ready(),
        "surface": surface,
        "prompt_seen": readiness.prompt_seen,
        "child": readiness.child.as_ref().map(|child| json!({
            "pid": child.pid,
            "comm": child.comm,
        })),
        "elapsed_ms": elapsed_ms,
    })
}

/// Shared validation for `wait-agent-status` (issue #75): the state
/// string and the timeout cap. Extracted from the handler so the table
/// test can pin both rejections.
fn validate_wait_request(state: &str, timeout_ms: u64) -> anyhow::Result<crate::AgentState> {
    let wanted = crate::AgentState::parse(state).ok_or_else(|| {
        anyhow::anyhow!("bad state {state:?} (want idle, working, blocked, done, or unknown)")
    })?;
    if timeout_ms > MAX_AGENT_WAIT_MS {
        anyhow::bail!("timeout {timeout_ms}ms exceeds the {MAX_AGENT_WAIT_MS}ms cap");
    }
    Ok(wanted)
}

/// Issue #88: resolve + validate a confirmed send's `timeout_ms`.
/// Absent means [`DEFAULT_INPUT_ACK_TIMEOUT_MS`]; `0` and anything over
/// [`MAX_INPUT_ACK_TIMEOUT_MS`] are rejected before any bytes are written
/// (extracted from the handler so the table test can pin it).
fn validate_input_ack_timeout(timeout_ms: Option<u64>) -> anyhow::Result<u64> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_INPUT_ACK_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_INPUT_ACK_TIMEOUT_MS {
        anyhow::bail!("timeout {timeout_ms}ms must be between 1 and {MAX_INPUT_ACK_TIMEOUT_MS}ms");
    }
    Ok(timeout_ms)
}

/// Success payload for `wait-agent-status`: the matched report plus the
/// surface's current plain-text snapshot (the "read payload").
fn wait_agent_status_json(
    surface: &crate::Surface,
    surface_id: SurfaceId,
    report: &crate::AgentReport,
    elapsed_ms: u64,
) -> Value {
    // The read payload is auxiliary to a MATCHED wait: `agent-read`
    // propagates terminal-read errors (`??`), but failing an already-
    // matched wait because a best-effort text snapshot failed would be
    // worse, so an empty payload is the deliberate fallback here.
    let text =
        surface.try_with_terminal(|t| t.plain_text()).ok().and_then(|r| r.ok()).unwrap_or_default();
    json!({
        "matched": true,
        "surface": surface_id,
        "state": report.state.as_str(),
        "agent": report.agent,
        "message": report.message,
        "updated_at_ms": report.updated_at_ms,
        "elapsed_ms": elapsed_ms,
        "text": text,
    })
}

fn agent_report_json(surface: SurfaceId, report: &crate::AgentReport) -> Value {
    json!({
        "surface": surface,
        "state": report.state.as_str(),
        "source": report.source.as_str(),
        "session": report.session,
        "agent": report.agent,
        "message": report.message,
        "updated_at_ms": report.updated_at_ms,
        // Issue #93: monotonic per-surface state-change sequence.
        "state_seq": report.state_seq,
    })
}

/// Issue #78 AC1 response: agent name + confidence + the evidence line
/// that triggered the match. Issue #96 adds `state`: the screen-derived
/// lifecycle classification (informational; the `Detected`-tier report
/// it may publish is what actually changes `agent_status`).
fn detection_json(surface: SurfaceId, detection: &crate::agent_detect::Detection) -> Value {
    json!({
        "surface": surface,
        "agent": detection.agent,
        "confidence": detection.confidence.map(|c| c.as_str()),
        "evidence": detection.evidence,
        "state": detection.screen_state.as_str(),
    })
}

fn agent_pattern_json(pattern: &crate::agent_detect::AgentPattern) -> Value {
    json!({
        "name": pattern.name,
        "kind": pattern.kind.as_str(),
        "pattern": pattern.pattern,
        "confidence": pattern.confidence.as_str(),
        "case_insensitive": pattern.case_insensitive,
    })
}

/// One surviving worktree child in `close-workspace` JSON (issue #100).
fn worktree_child_json(child: &crate::mux::WorktreeChild) -> Value {
    json!({
        "workspace": child.workspace,
        "name": child.name,
        "worktree_path": child.worktree_path,
        "worktree_branch": child.worktree_branch,
        "running_agent": child.running_agent,
    })
}

/// One pane worktree record in wire/`list-workspaces` JSON (issue #77).
fn worktree_record_json(record: &crate::worktree::WorktreeRecord) -> Value {
    json!({
        "branch": record.branch,
        "path": record.path,
        "label": record.label,
        "created_at_ms": record.created_at_ms,
    })
}

fn require_pty(surface: &crate::Surface) -> anyhow::Result<()> {
    if surface.kind() == SurfaceKind::Pty {
        Ok(())
    } else {
        anyhow::bail!("browser surface does not support PTY/VT socket commands")
    }
}

fn require_browser(surface: &crate::Surface) -> anyhow::Result<()> {
    if surface.kind() == SurfaceKind::Browser {
        Ok(())
    } else {
        anyhow::bail!("PTY surface is not a browser surface")
    }
}

pub(crate) fn parse_hex_color(value: &str) -> anyhow::Result<Rgb> {
    let bytes = value.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' {
        anyhow::bail!("bad color {value:?} (want \"#rrggbb\")");
    }
    let nibble = |b: u8| -> anyhow::Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => anyhow::bail!("bad color {value:?} (want \"#rrggbb\")"),
        }
    };
    let hex = |idx: usize| -> anyhow::Result<u8> {
        Ok((nibble(bytes[idx])? << 4) | nibble(bytes[idx + 1])?)
    };
    Ok(Rgb { r: hex(1)?, g: hex(3)?, b: hex(5)? })
}

pub fn parse_workspace_color(value: &str) -> anyhow::Result<Rgb> {
    let hex = match value.to_ascii_lowercase().as_str() {
        "red" => "#ff0000",
        "orange" => "#ff8800",
        "yellow" => "#ffff00",
        "green" => "#00ff00",
        "blue" => "#0000ff",
        "purple" => "#800080",
        "pink" => "#ff00ff",
        "cyan" => "#00ffff",
        "grey" | "gray" => "#808080",
        _ => value,
    };
    parse_hex_color(hex).map_err(|_| {
        anyhow::anyhow!("bad workspace color {value:?} (want \"#rrggbb\" or a named preset)")
    })
}

pub fn parse_workspace_icon(value: &str) -> anyhow::Result<IconName> {
    let glyph = match value.to_ascii_lowercase().as_str() {
        "folder" => "📁".to_string(),
        "robot" => "🤖".to_string(),
        "eye" => "👁".to_string(),
        "gear" => "⚙".to_string(),
        "search" | "magnifier" => "🔍".to_string(),
        "lock" => "🔒".to_string(),
        "check" => "✓".to_string(),
        _ if value.starts_with("\\u{") && value.ends_with('}') => {
            let hex = &value[3..value.len() - 1];
            let code = u32::from_str_radix(hex, 16).ok();
            code.and_then(char::from_u32).map(|c| c.to_string()).ok_or_else(|| {
                anyhow::anyhow!("unknown workspace icon {value:?}")
            })?
        }
        _ if value.chars().count() == 1 => value.to_string(),
        _ => anyhow::bail!(
            "unknown workspace icon {value:?}; expected folder, robot, eye, gear, search, magnifier, lock, check, or one Unicode character"
        ),
    };
    Ok(IconName::new(glyph))
}

fn browser_state_json(
    surface: SurfaceId,
    state: &crate::BrowserAttachState,
    include_frame: bool,
) -> Value {
    let mut value = json!({
        "event": "browser-state",
        "surface": surface,
        "cols": state.cols,
        "rows": state.rows,
        "url": state.url,
        "title": state.title,
        "status": state.status.as_str(),
        "error": state.status.error(),
        "frames_stalled": state.frames_stalled,
    });
    if include_frame {
        value["frame"] = match state.frame.as_ref() {
            Some(frame) => json!({
                "seq": frame.seq,
                "width": frame.css_width,
                "height": frame.css_height,
                "data": frame.data_b64,
            }),
            None => Value::Null,
        };
    }
    value
}

/// Resolve a `layout-export` workspace selector: exact name first, then
/// numeric workspace id (ids are session-local; names are the stable
/// fleet identity — issue #76 builder decision D2). `None` selects the
/// active workspace.
fn resolve_workspace_index(mux: &Mux, selector: Option<&str>) -> anyhow::Result<usize> {
    mux.with_state(|s| {
        if s.workspaces.is_empty() {
            anyhow::bail!("no workspaces in this session");
        }
        match selector {
            None => Ok(s.active_workspace),
            Some(sel) => s
                .workspaces
                .iter()
                .position(|ws| ws.name == sel)
                .or_else(|| {
                    sel.parse::<u64>()
                        .ok()
                        .and_then(|id| s.workspaces.iter().position(|ws| ws.id == id))
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown workspace {sel:?} (matched neither a name nor a numeric id)"
                    )
                }),
        }
    })
}

fn handle_command(mux: &Arc<Mux>, cmd: Command, writer: &LineWriter) -> anyhow::Result<Value> {
    match cmd {
        Command::Identify => Ok(json!({
            "app": "mtyx",
            "version": crate::VERSION,
            "protocol": PROTOCOL_VERSION,
            // Issue #88 capability record: a client wanting confirmed
            // (receipted) send gates on this before sending `confirm:true`
            // (see require_input_ack_capability).
            "capabilities": { CAP_INPUT_ACK: true },
            "session": mux.session_name(),
            "pid": std::process::id(),
        })),
        Command::ListWorkspaces => Ok(mux.with_state(workspaces_json)),
        Command::GetResolvedConfig => Ok(mux.resolved_chrome().unwrap_or_else(|| json!({}))),
        Command::Send {
            surface,
            text,
            bytes,
            send_cr,
            shell,
            confirm,
            timeout_ms,
            force,
            wait_activity_ms,
        } => {
            let surface = get_surface(mux, surface)?;
            require_pty(&surface)?;
            // Issue #93: blocked-send gate. Refuse to write ANY bytes into
            // a pane whose effective agent state is `Blocked` (any source
            // tier — see `Mux::effective_agent_state`), so an orchestrator
            // cannot blow through an approval dialog. `force` restores the
            // pre-#93 raw behaviour. `Unknown` is deliberately NOT gated:
            // a plain shell pane has no report and classifies `Unknown`,
            // and gating it would break every existing `send` caller (the
            // plan's "unknown" gating is not workable against the live
            // code — see the issue report).
            //
            // Spawn-readiness (issue #85) is the sibling half of this
            // lifecycle contract: `wait-ready` (see `Command::WaitReady`)
            // is the post-spawn health signal an orchestrator gates on
            // BEFORE its first send, so the gate below is only ever
            // reached once the pane is known to be up.
            let started_seq = surface.agent_state_seq();
            if !force.unwrap_or(false) {
                let (state, source, _) = mux.effective_agent_state(&surface);
                if state == crate::AgentState::Blocked {
                    return Err(ServerError::new(
                        "agent_blocked",
                        format!(
                            "surface {} is blocked by an agent approval/dialog \
                             (effective state via {source}); no input was sent. \
                             Resolve the prompt in the pane, or pass --force to send anyway.",
                            surface.id
                        ),
                    )
                    .into());
                }
            }
            // Issue #35: shell-aware sanitisation of `text` (raw bytes
            // via `bytes` are always written verbatim). `raw` (the
            // default) keeps the pre-#35 passthrough behaviour.
            let mode = resolve_shell_mode(shell.as_deref(), surface.child_pid())?;
            // Issue #88: build the whole payload up front (text first,
            // then bytes — the pre-#88 wire order, each with its optional
            // trailing CR) so a confirmed send writes ONE ordered, sized
            // unit under the surface's input-ACK FIFO, and so a base64
            // decode error can no longer land AFTER the text half was
            // already applied (a latent pre-#88 wart: the bytes half was
            // decoded after the text half was written).
            let mut payload = Vec::new();
            if let Some(text) = text {
                let mut text_bytes = sanitise_text(mode, &text).into_bytes();
                if send_cr.unwrap_or(false) {
                    text_bytes.push(b'\r');
                }
                payload.extend_from_slice(&text_bytes);
            }
            if let Some(b64) = bytes {
                let mut raw = base64::engine::general_purpose::STANDARD.decode(b64)?;
                if send_cr.unwrap_or(false) {
                    raw.push(b'\r');
                }
                payload.extend_from_slice(&raw);
            }
            if confirm.unwrap_or(false) {
                // Bounded input: reject before writing rather than hold
                // receipt backpressure over an unbounded payload.
                if payload.len() > MAX_CONFIRMED_SEND_BYTES {
                    return Err(ServerError::new(
                        "oversized_input",
                        format!(
                            "confirmed send payload is {} bytes; the cap is \
                             {MAX_CONFIRMED_SEND_BYTES} bytes (MAX_CONFIRMED_SEND_BYTES)",
                            payload.len()
                        ),
                    )
                    .into());
                }
                let timeout =
                    std::time::Duration::from_millis(validate_input_ack_timeout(timeout_ms)?);
                match surface.write_bytes_confirmed(&payload, timeout) {
                    Ok(()) => {}
                    Err(crate::ConfirmedSendError::AckTimeout { waited_ms }) => {
                        return Err(ServerError::new(
                            "input_ack_timeout",
                            format!(
                                "no input receipt (surface output or child-drain) within \
                                 {waited_ms}ms; bytes were written to the pty but consumption \
                                 was not observed"
                            ),
                        )
                        .into())
                    }
                    Err(crate::ConfirmedSendError::QueueTimeout { waited_ms }) => {
                        return Err(ServerError::new(
                            "input_ack_timeout",
                            format!(
                                "confirmed-input queue for this surface did not reach this \
                                 send within {waited_ms}ms; nothing was written (earlier \
                                 confirmed sends hold the queue)"
                            ),
                        )
                        .into())
                    }
                    Err(crate::ConfirmedSendError::Io(err)) => return Err(err.into()),
                };
            } else {
                // Unconfirmed (pre-#88) path: same byte sequence to the
                // pty, no receipt wait, no size cap.
                surface.write_bytes(&payload)?;
            }
            // Issue #93: observed-transition success. Only report success
            // after the target agent is seen to enter working/blocked at
            // or after this send — a pre-existing cached working/blocked
            // state (sequenced before `started_seq`) does NOT satisfy the
            // wait.
            match wait_activity_ms {
                Some(ms) => {
                    let timeout = validate_wait_activity_timeout(ms)?;
                    let observed = wait_for_agent_activity(
                        mux,
                        &surface,
                        started_seq,
                        std::time::Duration::from_millis(timeout),
                    )?;
                    Ok(json!({
                        "confirmed": confirm.unwrap_or(false),
                        "activity_observed": observed.is_some(),
                        "state": observed.map(|state| state.as_str()),
                    }))
                }
                None if confirm.unwrap_or(false) => Ok(json!({ "confirmed": true })),
                None => Ok(json!({})),
            }
        }
        Command::ReadScreen { surface } => {
            let surface = get_surface(mux, surface)?;
            require_pty(&surface)?;
            let text = surface.try_with_terminal(|t| t.plain_text())??;
            Ok(json!({ "text": text }))
        }
        Command::VtState { surface } => {
            let surface = get_surface(mux, surface)?;
            require_pty(&surface)?;
            let (cols, rows, replay) = surface.try_with_terminal(|t| {
                t.vt_replay().map(|replay| (t.cols(), t.rows(), replay))
            })??;
            Ok(json!({
                "cols": cols,
                "rows": rows,
                "data": base64::engine::general_purpose::STANDARD.encode(replay),
            }))
        }
        Command::NewTab { pane, cwd, cols, rows, command, env, branch, label } => {
            // Issue #77 AC4: when `branch` is set, create the worktree
            // BEFORE spawning so the pane starts inside the branch by
            // construction. Issue #76 overrides layer on top.
            let surface = if let Some(branch) = branch {
                mux.new_tab_with_worktree(pane, cwd, cols.zip(rows), &branch, label)?.0
            } else {
                let overrides = spawn_overrides(command, env);
                mux.new_tab_with_overrides(pane, cwd, cols.zip(rows), overrides.as_ref(), None)?
            };
            Ok(json!({ "surface": surface.id }))
        }
        Command::NewBrowserTab { url, pane, cols, rows } => {
            let surface = mux.new_browser_tab(url, pane, cols.zip(rows))?;
            Ok(json!({ "surface": surface.id }))
        }
        Command::SetCellPixels { width_px, height_px } => {
            mux.set_cell_pixel_size(width_px, height_px);
            Ok(json!({}))
        }
        Command::BrowserMouse { surface, kind, x_px, y_px, button, click_count } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            let event_type = match kind.as_str() {
                "down" => "mousePressed",
                "up" => "mouseReleased",
                "move" => "mouseMoved",
                other => anyhow::bail!("bad browser mouse kind {other:?}"),
            };
            surface.browser_mouse_event(event_type, x_px, y_px, button.as_deref(), click_count)?;
            Ok(json!({}))
        }
        Command::BrowserWheel { surface, x_px, y_px, delta_y_px } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_wheel(x_px, y_px, delta_y_px)?;
            Ok(json!({}))
        }
        Command::BrowserKey {
            surface,
            kind,
            key,
            code,
            windows_virtual_key_code,
            modifiers,
            text,
        } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            let event_type = match kind.as_str() {
                "down" => "keyDown",
                "up" => "keyUp",
                other => anyhow::bail!("bad browser key kind {other:?}"),
            };
            surface.browser_key_event(
                event_type,
                &key,
                &code,
                windows_virtual_key_code,
                modifiers,
                text.as_deref(),
            )?;
            Ok(json!({}))
        }
        Command::BrowserInsertText { surface, text } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_insert_text(&text)?;
            Ok(json!({}))
        }
        Command::BrowserNavigate { surface, url } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_navigate(&url)?;
            Ok(json!({}))
        }
        Command::BrowserBack { surface } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_back()?;
            Ok(json!({}))
        }
        Command::BrowserForward { surface } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_forward()?;
            Ok(json!({}))
        }
        Command::BrowserReload { surface } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_reload()?;
            Ok(json!({}))
        }
        Command::BrowserActivate { surface } => {
            let surface = get_surface(mux, surface)?;
            require_browser(&surface)?;
            surface.browser_activate()?;
            Ok(json!({}))
        }
        Command::NewWorkspace { name, cols, rows } => {
            let surface = mux.new_workspace(name, cols.zip(rows))?;
            Ok(json!({ "surface": surface.id }))
        }
        Command::NewRemoteWorkspace {
            host,
            slot,
            session_id,
            local_binary_path,
            name,
            cols,
            rows,
        } => {
            let spec = crate::remote_pty::RemoteSpec {
                host,
                slot,
                session_id,
                local_binary_path: local_binary_path.into(),
            };
            let surface = mux.new_remote_workspace(spec, name, cols.zip(rows))?;
            Ok(json!({ "surface": surface.id }))
        }
        Command::NewScreen { workspace, cols, rows } => {
            let surface = mux.new_screen(workspace, cols.zip(rows))?;
            Ok(json!({ "surface": surface.id }))
        }
        Command::Split { pane, dir, cols, rows, command, env, branch, label } => {
            let dir = match dir.as_str() {
                "right" => SplitDir::Right,
                "down" => SplitDir::Down,
                other => anyhow::bail!("bad dir {other:?} (want \"right\" or \"down\")"),
            };
            let surface = if let Some(branch) = branch {
                mux.split_with_worktree(pane, dir, cols.zip(rows), &branch, label)?.0
            } else {
                let overrides = spawn_overrides(command, env);
                mux.split_with_overrides(pane, dir, cols.zip(rows), overrides.as_ref(), None)?
            };
            Ok(json!({ "surface": surface.id }))
        }
        Command::SetRatio { pane, dir, ratio } => {
            let dir = match dir.as_str() {
                "right" => SplitDir::Right,
                "down" => SplitDir::Down,
                other => anyhow::bail!("bad dir {other:?} (want \"right\" or \"down\")"),
            };
            if !mux.set_ratio(pane, dir, ratio) {
                anyhow::bail!("unknown pane/split {pane}");
            }
            Ok(json!({}))
        }
        Command::MoveTab { surface, pane, index } => {
            let valid = mux.with_state(|state| {
                state.surfaces.contains_key(&surface)
                    && state.panes.contains_key(&pane)
                    && state.pane_of(surface).is_some()
            });
            if !valid {
                anyhow::bail!("unknown surface/pane");
            }
            mux.move_tab(surface, pane, index);
            Ok(json!({}))
        }
        Command::MoveWorkspace { workspace, index } => {
            if !mux.with_state(|state| state.workspaces.iter().any(|ws| ws.id == workspace)) {
                anyhow::bail!("unknown workspace");
            }
            mux.move_workspace(workspace, index);
            Ok(json!({}))
        }
        Command::SetDefaultColors { fg, bg } => {
            let current = mux.default_colors();
            let colors = DefaultColors {
                fg: match fg {
                    Some(value) => Some(parse_hex_color(&value)?),
                    None => current.fg,
                },
                bg: match bg {
                    Some(value) => Some(parse_hex_color(&value)?),
                    None => current.bg,
                },
            };
            mux.set_default_colors(colors);
            Ok(json!({}))
        }
        Command::CloseSurface { surface } => {
            get_surface(mux, surface)?;
            mux.close_surface(surface);
            Ok(json!({}))
        }
        Command::ClosePane { pane } => {
            if !mux.with_state(|s| s.panes.contains_key(&pane)) {
                anyhow::bail!("unknown pane {pane}");
            }
            mux.close_pane(pane);
            Ok(json!({}))
        }
        Command::CloseScreen { screen } => {
            if !mux.close_screen(screen) {
                anyhow::bail!("unknown screen {screen}");
            }
            Ok(json!({}))
        }
        Command::CloseWorkspace { workspace, group } => {
            let Some(report) = mux.close_workspace_reported(workspace, group) else {
                anyhow::bail!("unknown workspace {workspace}");
            };
            Ok(json!({
                "closed": report
                    .closed
                    .iter()
                    .map(|(id, name)| json!({ "id": id, "name": name }))
                    .collect::<Vec<_>>(),
                "survivors": report
                    .survivors
                    .iter()
                    .map(worktree_child_json)
                    .collect::<Vec<_>>(),
            }))
        }
        Command::RenamePane { pane, name } => {
            if !mux.rename_pane(pane, name) {
                anyhow::bail!("unknown pane {pane}");
            }
            Ok(json!({}))
        }
        Command::RenameSurface { surface, name } => {
            if !mux.rename_surface(surface, name) {
                anyhow::bail!("unknown surface {surface}");
            }
            Ok(json!({}))
        }
        Command::RenameScreen { screen, name } => {
            if !mux.rename_screen(screen, name) {
                anyhow::bail!("unknown screen {screen}");
            }
            Ok(json!({}))
        }
        Command::RenameWorkspace { workspace, name } => {
            if !mux.rename_workspace(workspace, name) {
                anyhow::bail!("unknown workspace {workspace}");
            }
            Ok(json!({}))
        }
        Command::SetWorkspaceColor { workspace, colour } => {
            let color = match colour {
                Some(value) => Some(parse_workspace_color(&value)?),
                None => None,
            };
            if !mux.set_workspace_color(workspace, color) {
                anyhow::bail!("unknown workspace {workspace}");
            }
            Ok(json!({}))
        }
        Command::SetStatus { workspace, icon } => {
            let workspace = workspace.or_else(|| {
                mux.with_state(|state| state.workspaces.get(state.active_workspace).map(|ws| ws.id))
            });
            let workspace = workspace.ok_or_else(|| anyhow::anyhow!("no active workspace"))?;
            let icon = parse_workspace_icon(&icon)?;
            if !mux.set_workspace_icon(workspace, Some(icon)) {
                anyhow::bail!("unknown workspace {workspace}");
            }
            Ok(json!({}))
        }
        Command::WorkspaceColor { name, color } => {
            let color = parse_workspace_color(&color)?;
            let workspace = mux.with_state(|state| {
                state.workspaces.iter().find(|ws| ws.name == name).map(|ws| ws.id)
            });
            let workspace = match workspace {
                Some(id) => id,
                None => {
                    mux.new_workspace(Some(name), None)?;
                    mux.with_state(|state| state.workspaces.last().unwrap().id)
                }
            };
            mux.set_workspace_color(workspace, Some(color));
            Ok(json!({}))
        }
        Command::TriggerFlash { workspace, surface } => {
            if !mux.trigger_flash(workspace, surface) {
                anyhow::bail!("unknown workspace {workspace}");
            }
            Ok(json!({}))
        }
        Command::ResizeSurface { surface, cols, rows } => {
            mux.resize_surface(surface, cols, rows)?;
            Ok(json!({}))
        }
        Command::FocusPane { pane } => {
            if !mux.focus_pane(pane) {
                anyhow::bail!("unknown pane {pane}");
            }
            Ok(json!({}))
        }
        Command::SelectTab { pane, index, delta } => {
            mux.select_tab(pane, index, delta);
            Ok(json!({}))
        }
        Command::SelectScreen { index, delta } => {
            mux.select_screen(index, delta);
            Ok(json!({}))
        }
        Command::SelectWorkspace { index, delta } => {
            mux.select_workspace(index, delta);
            Ok(json!({}))
        }
        Command::ScrollSurface { surface, delta } => {
            let surface = get_surface(mux, surface)?;
            require_pty(&surface)?;
            surface.try_with_terminal(|t| t.scroll_delta(delta))?;
            Ok(json!({}))
        }
        Command::ReportAgent { surface, state, source, session, agent, message } => {
            get_surface(mux, surface)?;
            let state = crate::AgentState::parse(&state)
                .ok_or_else(|| anyhow::anyhow!("bad state {state:?}"))?;
            let source = crate::AgentStateSource::parse(source.as_deref().unwrap_or("socket"))
                .ok_or_else(|| anyhow::anyhow!("bad source {source:?}"))?;
            let report = mux
                .report_agent(surface, state, source, session, agent, message)
                .ok_or_else(|| anyhow::anyhow!("surface {surface} does not support agent state"))?;
            Ok(agent_report_json(surface, &report))
        }
        Command::ListAgents { surface, state } => {
            let state = state
                .map(|s| {
                    crate::AgentState::parse(&s).ok_or_else(|| anyhow::anyhow!("bad state {s:?}"))
                })
                .transpose()?;
            let agents = mux
                .list_agents(surface, state)
                .iter()
                .map(|(id, report)| agent_report_json(*id, report))
                .collect::<Vec<_>>();
            Ok(json!({ "agents": agents }))
        }
        Command::DetectAgent { surface } => {
            let detection = mux.detect_agent(surface)?;
            Ok(detection_json(surface, &detection))
        }
        Command::DetectAgents => {
            let detections = mux.detect_all_agents()?;
            let agents: serde_json::Map<String, Value> = detections
                .into_iter()
                .map(|(id, detection)| (id.to_string(), Value::String(detection.agent)))
                .collect();
            Ok(json!({ "agents": agents }))
        }
        Command::AgentPatternAdd { name, pattern, kind, confidence, case_insensitive } => {
            let kind = match kind.as_deref() {
                None | Some("screen") => crate::agent_detect::PatternKind::Screen,
                Some("process") => crate::agent_detect::PatternKind::Process,
                Some(other) => anyhow::bail!("bad kind {other:?} (want \"process\" or \"screen\")"),
            };
            let confidence = match confidence.as_deref() {
                None | Some("medium") => crate::agent_detect::Confidence::Medium,
                Some(other) => crate::agent_detect::Confidence::parse(other).ok_or_else(|| {
                    anyhow::anyhow!(
                        "bad confidence {other:?} (want \"high\", \"medium\", or \"low\")"
                    )
                })?,
            };
            let pattern = crate::agent_detect::AgentPattern {
                name,
                kind,
                pattern,
                confidence,
                case_insensitive: case_insensitive.unwrap_or(false),
            };
            mux.agent_pattern_add(pattern.clone())?;
            Ok(agent_pattern_json(&pattern))
        }
        Command::AgentPatternList => {
            let patterns =
                mux.agent_pattern_list()?.iter().map(agent_pattern_json).collect::<Vec<_>>();
            Ok(json!({ "patterns": patterns }))
        }
        Command::AgentPatternRemove { name } => {
            mux.agent_pattern_remove(&name)?;
            Ok(json!({}))
        }
        Command::PaneWorktreeCreate { pane, branch, label } => {
            let record = mux.pane_worktree_create(pane, &branch, label)?;
            Ok(json!({ "pane": pane, "branch": record.branch, "path": record.path }))
        }
        Command::PaneWorktreeList { pane } => {
            let worktrees =
                mux.pane_worktree_list(pane)?.iter().map(worktree_record_json).collect::<Vec<_>>();
            Ok(json!({ "worktrees": worktrees }))
        }
        Command::PaneWorktreeRemove { pane, branch } => {
            mux.pane_worktree_remove(pane, &branch)?;
            Ok(json!({}))
        }
        Command::AgentRead { target, source, lines } => {
            let surface_id = mux.resolve_agent_target(&target)?;
            let surface = get_surface(mux, surface_id)?;
            require_pty(&surface)?;
            let lines = lines.unwrap_or(40);
            // Herdr's visibility ladder (review F1): `visible` reads the
            // viewport rows only; `recent`/`recent-unwrapped` read a
            // bottom-anchored window that includes SCROLLBACK (the
            // unwrapped variant re-joins soft-wrapped rows).
            let text = match source.as_deref().unwrap_or("visible") {
                "visible" => surface.try_with_terminal(|t| t.plain_text_viewport(false))??,
                "recent" => surface.try_with_terminal(|t| t.plain_text_recent(lines, false))??,
                "recent-unwrapped" => {
                    surface.try_with_terminal(|t| t.plain_text_recent(lines, true))??
                }
                other => anyhow::bail!(
                    "bad source {other:?} (want \"visible\", \"recent\", or \"recent-unwrapped\")"
                ),
            };
            Ok(json!({ "surface": surface_id, "text": tail_lines(&text, lines) }))
        }
        Command::AgentSend { target, text, shell } => {
            let surface_id = mux.resolve_agent_target(&target)?;
            let surface = get_surface(mux, surface_id)?;
            require_pty(&surface)?;
            // Same write path as `send`, minus the CR: agent-send types
            // the text and leaves submitting to the caller (AC4).
            let mode = resolve_shell_mode(shell.as_deref(), surface.child_pid())?;
            let bytes = sanitise_text(mode, &text).into_bytes();
            surface.write_bytes(&bytes)?;
            Ok(json!({ "surface": surface_id }))
        }
        Command::WaitAgentStatus { target, state, timeout_ms, require_transition } => {
            let wanted = validate_wait_request(&state, timeout_ms)?;
            let surface_id = mux.resolve_agent_target(&target)?;
            let surface = get_surface(mux, surface_id)?;
            require_pty(&surface)?;
            // Issue #93: when `require_transition` is set, the wait must
            // observe a strictly-newer report (a state change at/after
            // this call), not merely find the cached state already equal
            // to the target. Snapshot the sequence BEFORE subscribing so
            // a report racing in is still counted.
            let require_transition = require_transition.unwrap_or(false);
            let started_seq = surface.agent_state_seq();
            let accepts = |report: &crate::AgentReport| {
                report.state == wanted && (!require_transition || report.state_seq > started_seq)
            };
            // Subscribe BEFORE the immediate check so a report landing
            // between the two is still observed by the loop below (the
            // channel is unbounded, so `emit` never blocks the reporter).
            let events = mux.subscribe();
            let started = std::time::Instant::now();
            if let Some(report) = surface.agent_report() {
                if accepts(&report) {
                    return Ok(wait_agent_status_json(
                        &surface,
                        surface_id,
                        &report,
                        started.elapsed().as_millis() as u64,
                    ));
                }
            }
            if timeout_ms == 0 {
                anyhow::bail!("timeout waiting for agent status {state}");
            }
            let deadline = started + std::time::Duration::from_millis(timeout_ms);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    anyhow::bail!("timeout waiting for agent status {state}");
                }
                match events.recv_timeout(remaining) {
                    Ok(MuxEvent::AgentStateChanged { surface: s, report, .. })
                        if s == surface_id && accepts(&report) =>
                    {
                        return Ok(wait_agent_status_json(
                            &surface,
                            surface_id,
                            &report,
                            started.elapsed().as_millis() as u64,
                        ));
                    }
                    // Review F2: the pane died mid-wait (agent crashed /
                    // surface closed). It can never reach the target
                    // state, so error now instead of parking until the
                    // deadline — the K3-orchestrator "wait for the agent
                    // to finish" case wants the fast failure.
                    Ok(MuxEvent::SurfaceExited(s)) if s == surface_id => {
                        anyhow::bail!(
                            "surface {surface_id} exited while waiting for agent status {state}"
                        );
                    }
                    Ok(_) => continue,
                    Err(_) => anyhow::bail!("timeout waiting for agent status {state}"),
                }
            }
        }
        Command::WaitReady { surface, timeout_ms } => {
            // Issue #85: read-only readiness poll. Everything here only
            // observes (terminal lock + /proc); no writes, no state
            // mutation, so it is safe alongside a live orchestrator.
            let timeout_ms = validate_wait_ready_timeout(timeout_ms)?;
            // A vanished surface (child exited and the pane was reaped) is
            // reported as `ready:false`, not an error: a health verb must
            // give an orchestrator a uniform "not ready" answer rather
            // than forcing it to distinguish an exit from a timeout. An
            // id that never existed reads the same way — acceptable for a
            // probe, and the caller owns the id it just spawned.
            let started = std::time::Instant::now();
            let mut last = crate::mux::SurfaceReadiness::default();
            loop {
                if let Some(surface_arc) = mux.surface(surface) {
                    require_pty(&surface_arc)?;
                    last = mux.surface_readiness(&surface_arc);
                    if last.is_ready() {
                        return Ok(wait_ready_json(
                            surface,
                            &last,
                            started.elapsed().as_millis() as u64,
                        ));
                    }
                } else {
                    // Surface removed: no prompt, no child.
                    last = crate::mux::SurfaceReadiness::default();
                }
                if std::time::Instant::now()
                    >= started + std::time::Duration::from_millis(timeout_ms)
                {
                    // Timeout is a successful RPC with `ready:false` (see
                    // the Command doc); the CLI turns that into exit 1.
                    return Ok(wait_ready_json(
                        surface,
                        &last,
                        started.elapsed().as_millis() as u64,
                    ));
                }
                // Poll cadence: fine-grained enough that a prompt landing
                // just after a check is seen promptly, coarse enough not
                // to spin a core re-reading /proc + the terminal.
                let remaining = (started + std::time::Duration::from_millis(timeout_ms))
                    .saturating_duration_since(std::time::Instant::now());
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(25)));
            }
        }
        Command::RenameSession { new_name } => {
            // Issue #63. Scout-plan Q4 ordering: the socket rename is the
            // commit point (only it changes reachability), so the pid moves
            // FIRST — if that fails we bail before touching the socket and
            // nothing is committed. Partial failure is self-healing (Q4).
            //
            // Q4.1: validate (server is the security authority; the CLI also
            // pre-validates for defence in depth).
            validate_session_name(&new_name)?;

            let old_name = mux.session_name();
            let old_sock = mux.socket_path().ok_or_else(|| {
                anyhow::anyhow!("rename-session issued before the socket was bound")
            })?;
            let parent = old_sock
                .parent()
                .ok_or_else(|| anyhow::anyhow!("socket path has no parent directory"))?;
            let new_sock = parent.join(format!("{new_name}.sock"));
            let old_pid = pid_path(&old_sock);
            let new_pid = pid_path(&new_sock);

            // Q4.2: resolve + clear target. Refuse a LIVE target; clobber a
            // stale one (mirrors serve()'s stale-clear policy).
            if is_session_socket_live(&new_sock) {
                anyhow::bail!("session {new_name:?} already exists");
            }
            if new_sock.exists() {
                let _ = std::fs::remove_file(&new_sock);
            }
            if new_pid.exists() {
                let _ = std::fs::remove_file(&new_pid);
            }

            // Q4.3: rename the pid file first. If this fails, bail before the
            // socket rename commits — old state stays fully intact.
            std::fs::rename(&old_pid, &new_pid)
                .map_err(|e| anyhow::anyhow!("rename failed: {e}"))?;
            // Q4.4: rename the socket — the COMMIT point. From here the daemon
            // is reachable only at new_sock (the listener, bound to the inode,
            // keeps accepting there: see unix_socket_survives_rename). If this
            // rename fails (near-impossible: same FS, adjacent syscalls) we
            // undo the pid move above so the pre-rename fs state is exactly
            // restored (old.sock bound, old.pid present, no `new.*` artefacts).
            if let Err(e) = std::fs::rename(&old_sock, &new_sock) {
                let _ = std::fs::rename(&new_pid, &old_pid);
                return Err(anyhow::anyhow!("rename failed: {e}"));
            }

            // Q4.5: update state (logical name + canonical socket path).
            mux.set_session_name(new_name.clone());
            mux.set_socket_path(new_sock.clone());

            // Q4.6: best-effort reparent of the persisted snapshot. If we only
            // flipped Mux.session, the next write_snapshot would target the
            // new path while the old file orphaned, and restore_session on a
            // fresh `bar` start would find nothing (silent data loss across
            // rename+restart). Benign race with the debounced persist writer
            // (it reads Mux.session post-update, so at worst rewrites the new
            // file with the same tree).
            let old_snap = platform::session_snapshot_path(&old_name);
            let new_snap = platform::session_snapshot_path(&new_name);
            if old_snap.exists() {
                let _ = std::fs::remove_file(&new_snap);
                let _ = std::fs::rename(&old_snap, &new_snap);
            }

            // Q4.7/Q2: response. `pid` proves the same daemon keeps serving.
            Ok(json!({
                "session": new_name,
                "socket_path": new_sock,
                "pid": std::process::id(),
            }))
        }
        Command::LayoutExport { workspace } => {
            let index = resolve_workspace_index(mux, workspace.as_deref())?;
            let doc = mux.with_state(|s| crate::layout_doc::capture_workspace(s, index))?;
            Ok(serde_json::to_value(&doc)?)
        }
        Command::LayoutExportAll => {
            let files = mux.with_state(|s| -> anyhow::Result<Vec<Value>> {
                (0..s.workspaces.len())
                    .map(|i| {
                        let doc = crate::layout_doc::capture_workspace(s, i)?;
                        let filename = format!(
                            "{}.json",
                            crate::layout_doc::sanitize_filename(&s.workspaces[i].name)
                        );
                        Ok(json!({ "filename": filename, "document": doc }))
                    })
                    .collect()
            })?;
            Ok(json!({ "files": files }))
        }
        Command::LayoutApply { workspace, document } => {
            document.validate()?;
            let summary = mux.apply_layout(&workspace, &document)?;
            Ok(json!({
                "workspace": workspace,
                "workspace_id": summary.workspace_id,
                "panes": summary.panes,
                "surfaces": summary.surfaces,
            }))
        }
        Command::Subscribe => {
            let events = mux.subscribe();
            let writer = writer.clone();
            std::thread::Builder::new().name("mux-events-out".into()).spawn(move || {
                while let Ok(event) = events.recv() {
                    let value = match &event {
                        MuxEvent::SurfaceOutput(id) => {
                            json!({"event": "surface-output", "surface": id})
                        }
                        MuxEvent::SurfaceResized { surface, cols, rows } => {
                            json!({
                                "event": "surface-resized",
                                "surface": surface,
                                "cols": cols,
                                "rows": rows,
                            })
                        }
                        MuxEvent::SurfaceExited(id) => {
                            json!({"event": "surface-exited", "surface": id})
                        }
                        MuxEvent::TitleChanged(id) => {
                            json!({"event": "title-changed", "surface": id})
                        }
                        MuxEvent::Bell(id) => json!({"event": "bell", "surface": id}),
                        MuxEvent::Flash { workspace, surface } => json!({
                            "event": "flash",
                            "workspace": workspace,
                            "surface": surface,
                        }),
                        MuxEvent::Status(message) => {
                            json!({"event": "status", "message": message})
                        }
                        MuxEvent::TreeChanged => json!({"event": "tree-changed"}),
                        MuxEvent::Empty => json!({"event": "empty"}),
                        MuxEvent::AgentStateChanged { surface, previous, report } => json!({
                            "event": "agent-state-changed",
                            "surface": surface,
                            "previous": previous.map(|s| s.as_str()),
                            "state": report.state.as_str(),
                            "source": report.source.as_str(),
                            "session": report.session,
                            "agent": report.agent,
                            "message": report.message,
                            "updated_at_ms": report.updated_at_ms,
                            "state_seq": report.state_seq,
                        }),
                        MuxEvent::OscNotification { surface, title, body } => json!({
                            "event": "osc-notification",
                            "surface": surface,
                            "title": title,
                            "body": body,
                        }),
                    };
                    if writer.send(&value).is_err() {
                        break;
                    }
                }
            })?;
            Ok(json!({}))
        }
        Command::AttachSurface { surface: surface_id } => {
            let surface = get_surface(mux, surface_id)?;
            if surface.kind() == SurfaceKind::Browser {
                let (state, frames) = surface.attach_frames()?;
                writer.send(&browser_state_json(surface_id, &state, true))?;
                let writer = writer.clone();
                std::thread::Builder::new().name("mux-attach-out".into()).spawn(move || {
                    while frames.notify.recv().is_ok() {
                        let update = std::mem::take(&mut *frames.slot.lock().unwrap());
                        if let Some(state) = update.state {
                            if writer.send(&browser_state_json(surface_id, &state, false)).is_err()
                            {
                                break;
                            }
                        }
                        if let Some(frame) = update.frame {
                            let value = json!({
                                "event": "frame",
                                "surface": surface_id,
                                "seq": frame.seq,
                                "width": frame.css_width,
                                "height": frame.css_height,
                                "data": frame.data_b64,
                            });
                            if writer.send(&value).is_err() {
                                break;
                            }
                        }
                    }
                    let _ = writer.send(&json!({"event": "detached", "surface": surface_id}));
                })?;
                return Ok(json!({}));
            }
            let attach = surface.attach_stream()?;
            writer.send(&json!({
                "event": "vt-state",
                "surface": surface_id,
                "cols": attach.cols,
                "rows": attach.rows,
                "data": base64::engine::general_purpose::STANDARD.encode(attach.replay),
            }))?;
            let writer = writer.clone();
            std::thread::Builder::new().name("mux-attach-out".into()).spawn(move || {
                while let Ok(frame) = attach.stream.recv() {
                    let value = match frame {
                        AttachFrame::Output(chunk) => json!({
                            "event": "output",
                            "surface": surface_id,
                            "data": base64::engine::general_purpose::STANDARD.encode(chunk),
                        }),
                        AttachFrame::Resized { cols, rows, replay } => json!({
                            "event": "resized",
                            "surface": surface_id,
                            "cols": cols,
                            "rows": rows,
                            "data": base64::engine::general_purpose::STANDARD.encode(replay),
                        }),
                    };
                    if writer.send(&value).is_err() {
                        break;
                    }
                }
                // Surface gone (or reader stopped): signal end of stream.
                let _ = writer.send(&json!({"event": "detached", "surface": surface_id}));
            })?;
            Ok(json!({}))
        }
    }
}

/// Remove the socket file and pid file (call on clean shutdown).
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(pid_path(path));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Foundation pin for `mtyx rename-session` (issue #63 L2, scout-plan
    /// Q1). On a bound `AF_UNIX` `SOCK_STREAM` listener, `rename(2)`
    /// reparents the dirent while the kernel keeps the listener bound to
    /// the inode. The listener therefore keeps accepting at the NEW path
    /// and the OLD path ceases to be connectable. The rename-session
    /// daemon mechanism relies on this — it never rebinds, it just
    /// `rename(2)`s the `.sock`. This test pins the kernel property so a
    /// future platform or libc quirk can't silently regress the whole
    /// feature.
    #[test]
    #[cfg(unix)] // pins rename(2) semantics on an AF_UNIX socket
    fn unix_socket_survives_rename() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir =
            std::env::temp_dir().join(format!("mtyx-t0-rename-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.sock");
        let new = dir.join("new.sock");

        let listener = UnixListener::bind(&old).unwrap();
        std::fs::rename(&old, &new).unwrap();

        // The old dirent is gone -> connecting there must fail.
        assert!(
            UnixStream::connect(&old).is_err(),
            "old socket path should not be connectable after rename"
        );
        // The new dirent resolves to the same bound inode -> connectable.
        let client =
            UnixStream::connect(&new).expect("new socket path should be connectable after rename");
        // The listener (bound to the inode, not the dirent) still accepts
        // the connection that arrived at the new path.
        let (_accepted, _addr) =
            listener.accept().expect("listener must accept a connection after the rename");

        drop(client);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_session_name_table() {
        // Issue #63 L2 (scout-plan Q6/T12): names become filesystem paths
        // (`<name>.sock`/`<name>.pid`/`<name>.json`), so `/`, `\0`, control
        // chars, `.`, `..`, leading/trailing whitespace and overlong names
        // must be rejected; ordinary names (incl. unicode) accepted.
        for good in ["main", "foo-bar", "a_b", "café", "session.number"] {
            assert!(validate_session_name(good).is_ok(), "{good:?} should be a valid session name");
        }
        let overlong = "a".repeat(256);
        for bad in ["", "a/b", "a\\b", "..", ".", " foo", "foo ", "\0", "a\u{1}b", "\t", &overlong]
        {
            assert!(
                validate_session_name(bad).is_err(),
                "{bad:?} should be rejected as a session name"
            );
        }
    }

    #[test]
    fn workspace_color_accepts_hex_and_named_presets() {
        assert_eq!(parse_workspace_color("#1234ab").unwrap(), Rgb { r: 0x12, g: 0x34, b: 0xab });
        assert_eq!(parse_workspace_color("blue").unwrap(), Rgb { r: 0, g: 0, b: 255 });
        assert!(parse_workspace_color("ultraviolet").is_err());
    }

    #[test]
    fn wait_agent_status_validates_state_and_timeout_table() {
        // Issue #75: bad state strings and over-cap timeouts are
        // rejected before any blocking; 0 and the cap itself are legal.
        for good in ["idle", "working", "blocked", "done", "unknown"] {
            assert!(validate_wait_request(good, 1000).is_ok(), "{good} should be valid");
        }
        assert!(validate_wait_request("nonsense", 1000).is_err());
        assert!(validate_wait_request("idle", MAX_AGENT_WAIT_MS + 1).is_err());
        assert!(validate_wait_request("idle", MAX_AGENT_WAIT_MS).is_ok());
        assert!(validate_wait_request("idle", 0).is_ok());
    }

    #[test]
    fn peer_auth_denial_json_pins_the_exact_wire_shape() {
        // Issue #86: a foreign uid is denied with a structured response
        // that names the uid, carries `id` as null (unlike Response), and
        // is `ok:false`.
        let foreign = platform::PeerAuthDecision::RejectForeign { uid: 4242 };
        let denial = peer_auth_denial_json(&foreign).expect("foreign uid must deny");
        assert_eq!(denial["ok"], json!(false));
        assert!(denial["id"].is_null(), "id must be present and null");
        assert_eq!(denial["error"], json!("peer uid 4242 rejected"));

        // A lookup error also denies, but names no uid.
        let lookup = peer_auth_denial_json(&platform::PeerAuthDecision::RejectLookupError)
            .expect("lookup error must deny");
        assert_eq!(lookup["ok"], json!(false));
        assert_eq!(lookup["error"], json!("peer authentication failed"));

        // Accept and Unsupported write NO denial (Unsupported falls back
        // to the filesystem-permissions boundary).
        assert!(peer_auth_denial_json(&platform::PeerAuthDecision::Accept).is_none());
        assert!(peer_auth_denial_json(&platform::PeerAuthDecision::Unsupported).is_none());
    }

    #[test]
    fn wait_ready_validates_timeout_table() {
        // Issue #85: absent defaults, 0 and the cap are legal, over-cap
        // is rejected. Extracted so this is pinned without a live PTY.
        assert_eq!(validate_wait_ready_timeout(None).unwrap(), DEFAULT_WAIT_READY_MS);
        assert_eq!(validate_wait_ready_timeout(Some(0)).unwrap(), 0);
        assert_eq!(validate_wait_ready_timeout(Some(5000)).unwrap(), 5000);
        assert!(validate_wait_ready_timeout(Some(MAX_AGENT_WAIT_MS + 1)).is_err());
        assert_eq!(
            validate_wait_ready_timeout(Some(MAX_AGENT_WAIT_MS)).unwrap(),
            MAX_AGENT_WAIT_MS
        );
    }

    #[test]
    fn tail_lines_drops_trailing_blank_rows_and_tails() {
        // Issue #75 agent-read --lines: trailing blank rows (the VT plain
        // formatter can leave empty rows below the cursor) don't consume
        // the tail budget, and 0 yields empty.
        let screen = "cmd\nrow-a\nrow-b\nrow-c\n\n \n";
        assert_eq!(tail_lines(screen, 0), "");
        assert_eq!(tail_lines(screen, 1), "row-c");
        assert_eq!(tail_lines(screen, 2), "row-b\nrow-c");
        assert_eq!(tail_lines(screen, 10), "cmd\nrow-a\nrow-b\nrow-c");
        assert_eq!(tail_lines("", 5), "");
    }

    /// Issue #88: confirmed-send receipt timeout validation. Absent →
    /// the default; 0 and over-cap are rejected; 1 and the cap are legal.
    #[test]
    fn input_ack_timeout_validation_table() {
        assert_eq!(validate_input_ack_timeout(None).unwrap(), DEFAULT_INPUT_ACK_TIMEOUT_MS);
        assert_eq!(validate_input_ack_timeout(Some(1)).unwrap(), 1);
        assert_eq!(
            validate_input_ack_timeout(Some(MAX_INPUT_ACK_TIMEOUT_MS)).unwrap(),
            MAX_INPUT_ACK_TIMEOUT_MS
        );
        assert!(validate_input_ack_timeout(Some(0)).is_err());
        assert!(validate_input_ack_timeout(Some(MAX_INPUT_ACK_TIMEOUT_MS + 1)).is_err());
    }

    /// Issue #88: the CLIENT-side capability gate. A protocol-6 identify
    /// (no `capabilities` record) and even a protocol-7 identify without
    /// the record are refused with the structured
    /// `legacy_host_receipt_rejected` code — never a silent downgrade to
    /// fire-and-forget. A daemon advertising `input-ack` passes. (The
    /// end-to-end identify shape is pinned in tests/input_ack.rs.)
    #[test]
    fn legacy_host_receipt_rejected_by_capability_gate() {
        let legacy = json!({
            "app": "mtyx", "version": "0.0.0", "protocol": 6,
            "session": "main", "pid": 1,
        });
        let err = require_input_ack_capability(&legacy).unwrap_err();
        assert_eq!(err.code, "legacy_host_receipt_rejected");
        let message = err.to_string();
        assert!(
            message.starts_with("legacy_host_receipt_rejected: daemon (protocol 6)"),
            "{message}"
        );
        assert!(message.contains("input-ACK capability"), "{message}");
        assert!(message.contains("--no-confirm"), "{message}");
        // The gate keys on the capability record, not the number: a v7
        // daemon that (hypothetically) omitted the record is still
        // refused, and a reply WITH the record passes.
        let no_record = json!({ "app": "mtyx", "protocol": 7 });
        assert_eq!(
            require_input_ack_capability(&no_record).unwrap_err().code,
            "legacy_host_receipt_rejected"
        );
        let capable =
            json!({ "app": "mtyx", "protocol": 7, "capabilities": { "input-ack": true } });
        assert!(require_input_ack_capability(&capable).is_ok());
        assert!(identify_has_input_ack(&capable));
    }

    #[test]
    fn send_shell_sanitisation_table() {
        // Issue #35: known shells reset the input buffer (leading \n) for
        // metacharacter-leading or quote-unbalanced text; raw passes through.
        assert_eq!(sanitise_text(ShellMode::Fish, "$ pwd\n"), "\n$ pwd\n");
        assert_eq!(sanitise_text(ShellMode::Bash, "$ pwd\n"), "\n$ pwd\n");
        assert_eq!(sanitise_text(ShellMode::Zsh, "! foo\n"), "\n! foo\n");
        assert_eq!(sanitise_text(ShellMode::Raw, "$ pwd\n"), "$ pwd\n");
        // `sh` is not in the issue's table: no transformation.
        assert_eq!(sanitise_text(ShellMode::Sh, "$ pwd\n"), "$ pwd\n");
        // nu: no special handling for a leading `$`, but unclosed quotes reset.
        assert_eq!(sanitise_text(ShellMode::Nu, "$ pwd\n"), "$ pwd\n");
        assert_eq!(sanitise_text(ShellMode::Nu, "echo 'oops\n"), "\necho 'oops\n");
        // Unclosed quotes reset for fish/bash/zsh too.
        assert_eq!(sanitise_text(ShellMode::Fish, "echo 'oops\n"), "\necho 'oops\n");
        // Balanced quotes / plain commands need no reset.
        assert_eq!(sanitise_text(ShellMode::Fish, "echo 'hi'\n"), "echo 'hi'\n");
        assert_eq!(sanitise_text(ShellMode::Fish, "ls -la\n"), "ls -la\n");
    }

    #[test]
    fn send_shell_auto_falls_back_to_raw_on_lookup_failure() {
        // No flag, a missing/bogus pid, and unmatched cmdlines all resolve
        // to raw (never an error); unknown names are a protocol error.
        assert_eq!(resolve_shell_mode(None, None).unwrap(), ShellMode::Raw);
        assert_eq!(resolve_shell_mode(Some("raw"), None).unwrap(), ShellMode::Raw);
        assert_eq!(resolve_shell_mode(Some("auto"), None).unwrap(), ShellMode::Raw);
        assert_eq!(detect_shell_from_child(None), ShellMode::Raw);
        assert_eq!(detect_shell_from_child(Some(u32::MAX)), ShellMode::Raw);
        assert!(resolve_shell_mode(Some("tcsh"), None).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn send_shell_auto_detects_shell_from_proc_cmdline() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        // `sh -c 'while :; do sleep 1; done'` keeps sh as the direct child,
        // so /proc/<pid>/cmdline's argv[0] is the shell we should detect.
        let mut child =
            Command::new("/bin/sh").arg("-c").arg("while :; do sleep 1; done").spawn().unwrap();
        let pid = child.id();
        // Retry briefly in case we read /proc before the child's exec
        // lands (mirrors the wait-for-child pattern in process.rs tests).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut detected = ShellMode::Raw;
        while Instant::now() < deadline {
            detected = detect_shell_from_child(Some(pid));
            if detected == ShellMode::Sh {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(detected, ShellMode::Sh);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn workspace_icon_validates_names_and_unicode() {
        assert_eq!(parse_workspace_icon("robot").unwrap().as_str(), "🤖");
        assert_eq!(parse_workspace_icon("\\u{1f50d}").unwrap().as_str(), "🔍");
        assert!(parse_workspace_icon("bogus")
            .unwrap_err()
            .to_string()
            .contains("unknown workspace icon"));
    }

    // --- Per-pane git worktrees over the wire (issue #77) ---

    /// One request → one response over a fresh connection, skipping
    /// any pushed events (the shape `mtyx` CLI verbs speak).
    fn rpc(socket: &Path, request: Value) -> Value {
        let mut stream = transport::connect(socket).unwrap();
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        loop {
            buf.clear();
            reader.read_line(&mut buf).unwrap();
            let value: Value = serde_json::from_str(&buf).unwrap();
            if value.get("event").is_some() {
                continue;
            }
            return value;
        }
    }

    /// A temp git repo with one commit (worktree ops need a HEAD).
    fn temp_git_repo(name: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let dir = std::env::temp_dir().join(format!(
            "mtyx-srv-wt-{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::process::Command::new("git").arg("init").arg(&dir).output().unwrap();
        assert!(out.status.success(), "git init failed");
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=mtyx@test", "-c", "user.name=mtyx"])
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git commit failed");
        dir
    }

    #[test]
    fn pane_worktree_commands_round_trip_over_socket() {
        use crate::{Mux, SurfaceOptions};
        use std::sync::OnceLock;

        // One shared daemon: SurfaceOptions spawns /bin/cat panes so no
        // user shell is involved; OSC 7 never fires, so the pane cwd is
        // the spawn cwd — exactly what the worktree resolution uses.
        static MUX: OnceLock<Arc<Mux>> = OnceLock::new();
        let mux = MUX.get_or_init(|| {
            Mux::new(
                "wt-wire",
                SurfaceOptions {
                    command: Some(vec!["/bin/cat".to_string()]),
                    ..Default::default()
                },
            )
        });
        let dir = temp_git_repo("wire");
        let sock = dir.join("wt.sock");
        serve(mux.clone(), Some(sock.clone())).unwrap();

        // Workspace + a tab parked in the repo so the pane has a cwd.
        let ws = rpc(&sock, json!({"cmd": "new-workspace", "id": 1}));
        assert_eq!(ws["ok"], json!(true), "new-workspace failed: {ws}");
        let surface = ws["data"]["surface"].as_u64().unwrap();
        let tree = rpc(&sock, json!({"cmd": "list-workspaces", "id": 2}));
        let pane = tree["data"]["workspaces"][0]["screens"][0]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                p["tabs"]
                    .as_array()
                    .is_some_and(|tabs| tabs.iter().any(|t| t["surface"].as_u64() == Some(surface)))
            })
            .expect("pane holding the workspace surface")
            .get("id")
            .and_then(Value::as_u64)
            .unwrap();
        let parked = rpc(
            &sock,
            json!({"cmd": "new-tab", "id": 3, "pane": pane, "cwd": dir.to_string_lossy()}),
        );
        assert_eq!(parked["ok"], json!(true), "new-tab failed: {parked}");

        // Create: AC1 — the worktree path comes back on JSON stdout.
        let created = rpc(
            &sock,
            json!({"cmd": "pane-worktree-create", "id": 4, "pane": pane, "branch": "feat-auth", "label": "auth"}),
        );
        assert_eq!(created["ok"], json!(true), "create failed: {created}");
        assert_eq!(created["data"]["pane"], json!(pane));
        assert_eq!(created["data"]["branch"], json!("feat-auth"));
        let path = created["data"]["path"].as_str().unwrap().to_string();
        assert!(Path::new(&path).is_dir(), "worktree {path} should exist");

        // List: AC2 — the record shape round-trips.
        let listed = rpc(&sock, json!({"cmd": "pane-worktree-list", "id": 5, "pane": pane}));
        assert_eq!(listed["ok"], json!(true), "list failed: {listed}");
        let worktrees = listed["data"]["worktrees"].as_array().unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0]["branch"], json!("feat-auth"));
        assert_eq!(worktrees[0]["path"], json!(path));
        assert_eq!(worktrees[0]["label"], json!("auth"));
        assert!(worktrees[0]["created_at_ms"].as_u64().unwrap() > 0);

        // A create failure maps to ok:false with git's message (AC7).
        let failed = rpc(
            &sock,
            json!({"cmd": "pane-worktree-create", "id": 6, "pane": pane, "branch": "bad..name"}),
        );
        assert_eq!(failed["ok"], json!(false), "expected ok:false, got {failed}");
        assert!(
            failed["error"].as_str().unwrap().contains("not a valid branch name"),
            "git error should propagate: {failed}"
        );

        // Remove: AC3 — teardown drops the dir and the record.
        let removed = rpc(
            &sock,
            json!({"cmd": "pane-worktree-remove", "id": 7, "pane": pane, "branch": "feat-auth"}),
        );
        assert_eq!(removed["ok"], json!(true), "remove failed: {removed}");
        assert!(!Path::new(&path).exists(), "worktree dir should be gone");
        let listed = rpc(&sock, json!({"cmd": "pane-worktree-list", "id": 8, "pane": pane}));
        assert_eq!(listed["data"]["worktrees"].as_array().unwrap().len(), 0);

        // Unknown pane is ok:false, not a silent empty list.
        let unknown = rpc(&sock, json!({"cmd": "pane-worktree-list", "id": 9, "pane": 9999}));
        assert_eq!(unknown["ok"], json!(false));
        assert!(unknown["error"].as_str().unwrap().contains("unknown pane"));

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(pid_path(&sock));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
