//! Surface runtime: one tab inside a pane.
//!
//! A surface is either a PTY backed by libghostty-vt state or a local CDP
//! browser surface. PTY-only methods stay available for existing callers;
//! browser-aware frontends should branch on [`SurfaceKind`] before using
//! VT operations.

use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use ghostty_vt::{Callbacks, RenderState, Rgb, Terminal};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use anyhow::Context;

use crate::platform;
use crate::{Mux, MuxEvent, SurfaceId};

use crate::browser::BrowserSurface;
pub use crate::browser::{
    BrowserAttachState, BrowserFrame, BrowserFrameStream, BrowserSource, BrowserStatus,
};

/// How to spawn surface children.
#[derive(Debug, Clone)]
pub struct SurfaceOptions {
    /// Command argv. When unset, the platform shell is spawned as a
    /// login shell (argv0 `-<basename>`, e.g. `-bash`) — the same
    /// convention as gnome-terminal / wezterm / tmux. A bare `/bin/bash`
    /// on a freshly opened PTY is CrowdStrike Falcon's GenReverseShell
    /// signature.
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    /// TERM value for children. xterm-256color is the compatible default;
    /// set xterm-ghostty when the ghostty terminfo is installed.
    pub term: String,
    /// Initial VT geometry (cols, rows). When a surface is spawned with
    /// no client attached and no explicit size, this is the headless
    /// default — 120x40, overridable via `MTYX_MUX_VT_SIZE=COLSxROWS`
    /// (issue #99; see [`SurfaceOptions::default`]).
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    /// Extra environment for children (e.g. MTYX_MUX_SOCKET).
    pub extra_env: Vec<(String, String)>,
    /// Optional Chrome/Chromium binary for browser surfaces.
    pub chrome_binary: Option<String>,
    /// Optional existing Chrome CDP endpoint, as ws://... or http://host:port.
    pub cdp_url: Option<String>,
    /// Whether browser panes should probe local debuggable Chrome ports.
    pub browser_discover: bool,
    /// Local ports to probe for /json/version when discovery is enabled.
    pub browser_discover_ports: Vec<u16>,
    /// Optional Chrome user data directory for launched browser runtime.
    pub browser_user_data_dir: Option<String>,
    /// Session component for the default launched Chrome profile path.
    pub browser_session_name: String,
    /// Use a temporary launched Chrome profile and delete it on shutdown.
    pub browser_ephemeral: bool,
    /// Maximum browser capture size before downscaling, in megapixels.
    pub browser_max_capture_megapixels: f64,
    /// Optional fixed browser capture scale, where 1.0 captures at pane pixels.
    pub browser_capture_scale: Option<f64>,
    /// When set, this PTY is backed by `cmuxd-remote` over SSH instead of
    /// a local child process (see `remote_pty.rs`). Per-spawn, not part
    /// of a `Mux`'s default template.
    pub remote: Option<crate::remote_pty::RemoteSpec>,
}

/// Default headless VT geometry (issue #99): the size a surface spawns
/// at when no client is attached and no explicit size is given.
/// Attaching a client still resizes to its real geometry as before.
const DEFAULT_VT_COLS: u16 = 120;
const DEFAULT_VT_ROWS: u16 = 40;

/// Parse a `COLSxROWS` geometry string (e.g. `"100x30"`; an uppercase
/// `X` separator and surrounding whitespace are tolerated). Both
/// dimensions must be >= 1. Shared by the `MTYX_MUX_VT_SIZE` env
/// override and mux-tui's `headless.vt_size` config key (issue #99).
pub fn parse_vt_size(value: &str) -> Option<(u16, u16)> {
    let value = value.trim();
    let (cols, rows) = value.split_once('x').or_else(|| value.split_once('X'))?;
    let cols: u16 = cols.trim().parse().ok()?;
    let rows: u16 = rows.trim().parse().ok()?;
    (cols >= 1 && rows >= 1).then_some((cols, rows))
}

/// Headless VT geometry for [`SurfaceOptions::default`]: the
/// `MTYX_MUX_VT_SIZE` env var when it parses as `COLSxROWS`, else the
/// 120x40 default (issue #99).
fn vt_size_from_env() -> (u16, u16) {
    if let Ok(value) = std::env::var("MTYX_MUX_VT_SIZE") {
        match parse_vt_size(&value) {
            Some(size) => size,
            None => {
                eprintln!(
                    "mtyx: ignoring MTYX_MUX_VT_SIZE={value:?}; expected COLSxROWS, e.g. \"100x30\""
                );
                (DEFAULT_VT_COLS, DEFAULT_VT_ROWS)
            }
        }
    } else {
        (DEFAULT_VT_COLS, DEFAULT_VT_ROWS)
    }
}

impl Default for SurfaceOptions {
    fn default() -> Self {
        // Issue #99: headless VT geometry — 120x40 unless overridden with
        // `MTYX_MUX_VT_SIZE=COLSxROWS` (e.g. "100x30"). The TUI's
        // mux.json `headless.vt_size` key (applied in mux-tui's
        // `run_server`) is the config-file form of the same knob; the
        // env var wins because it is read here, first.
        let (cols, rows) = vt_size_from_env();
        SurfaceOptions {
            command: None,
            cwd: None,
            term: std::env::var("MTYX_MUX_TERM").unwrap_or_else(|_| "xterm-256color".into()),
            cols,
            rows,
            scrollback: 10_000,
            extra_env: Vec::new(),
            chrome_binary: None,
            cdp_url: None,
            browser_discover: false,
            browser_discover_ports: vec![9222],
            browser_user_data_dir: None,
            browser_session_name: "default".to_string(),
            browser_ephemeral: false,
            browser_max_capture_megapixels: 2.0,
            browser_capture_scale: None,
            remote: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DefaultColors {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
}

/// Per-spawn overrides layered onto a [`Mux`](crate::Mux)'s default
/// [`SurfaceOptions`] (issue #76): the recorded argv/env/cwd a layout
/// `apply` or a `new-tab --exec` replays. `None`/empty fields keep the
/// mux default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnOverrides {
    /// Explicit child argv (e.g. an agent command). `None` = the default
    /// login shell.
    pub command: Option<Vec<String>>,
    /// Extra env entries layered on top of the template (a repeated key
    /// replaces the template's entry).
    pub extra_env: Vec<(String, String)>,
    /// Explicit working directory for the child.
    pub cwd: Option<String>,
}

/// Coding-agent lifecycle state for a surface, as reported by `report-agent`
/// (see `spec/commands.md`). Not detected automatically; a frontend or a
/// hook script is the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Blocked,
    Idle,
    Done,
    Unknown,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Idle => "idle",
            AgentState::Done => "done",
            AgentState::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "idle" => AgentState::Idle,
            "done" => AgentState::Done,
            "unknown" => AgentState::Unknown,
            _ => return None,
        })
    }
}

/// Authority of an agent-state report, lowest to highest (derived `Ord`
/// follows declaration order): a hook report always applies; a socket
/// report is rejected while a hook report is the current source; a
/// detected report (from watching the surface's own output, e.g. an OSC 9
/// notification) is rejected while either an explicit socket or hook
/// report is current (see `spec/commands.md`'s `report-agent` authority
/// rules, which name "detected" as the lowest tier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentStateSource {
    Detected,
    Socket,
    Hook,
}

impl AgentStateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStateSource::Detected => "detected",
            AgentStateSource::Socket => "socket",
            AgentStateSource::Hook => "hook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "detected" => AgentStateSource::Detected,
            "socket" => AgentStateSource::Socket,
            "hook" => AgentStateSource::Hook,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    pub state: AgentState,
    pub source: AgentStateSource,
    pub session: Option<String>,
    /// Agent name (issue #75): the `--agent` label on a report, used by
    /// the name-addressed verbs (`agent-read`, `agent-send`,
    /// `wait-agent-status`). Preserved across later reports that omit
    /// it, so interim hook/socket reports don't break name addressing;
    /// replaced when a report carries a new name.
    pub agent: Option<String>,
    /// Last message (issue #75): free-text context from the reporting
    /// agent ("compiling", "waiting on user", …). Reflects the latest
    /// applied report; absent means cleared.
    pub message: Option<String>,
    pub updated_at_ms: u64,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Everything an attaching frontend needs to adopt a PTY surface: its
/// size, a VT replay of the current state, and a live stream of every pty
/// byte applied after the replay snapshot.
pub struct AttachStream {
    pub cols: u16,
    pub rows: u16,
    pub replay: Vec<u8>,
    pub stream: std::sync::mpsc::Receiver<AttachFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachFrame {
    Output(Vec<u8>),
    Resized { cols: u16, rows: u16, replay: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Pty,
    Browser,
}

impl SurfaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SurfaceKind::Pty => "pty",
            SurfaceKind::Browser => "browser",
        }
    }
}

pub struct SurfaceMeta {
    pub id: SurfaceId,
    /// User-assigned tab name (rename tab); shared by every surface kind.
    pub(crate) name: Mutex<Option<String>>,
    /// Last ambient agent-detection result (issue #78), cached for
    /// `list-workspaces`/sidebar rendering. Refreshed only by explicit
    /// `detect-agent` calls (v1: no background polling).
    pub(crate) detected_agent: Mutex<Option<crate::agent_detect::Detection>>,
}

/// A pane tab runtime.
pub enum Surface {
    Pty(PtySurface),
    Browser(BrowserSurface),
}

impl Deref for Surface {
    type Target = SurfaceMeta;

    fn deref(&self) -> &Self::Target {
        match self {
            Surface::Pty(surface) => &surface.meta,
            Surface::Browser(surface) => &surface.meta,
        }
    }
}

/// A single terminal surface: PTY child plus ghostty VT state.
///
/// The terminal is behind a mutex; the pty reader thread holds it only
/// while feeding bytes, renderers hold it only while snapshotting into a
/// [`RenderState`].
pub struct PtySurface {
    pub(crate) meta: SurfaceMeta,
    term: Mutex<Terminal>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send>>,
    /// Direct PTY child PID. Used to walk/kill the process tree on
    /// surface kill (issue #28); grandchildren that double-forked would
    /// otherwise outlive a single-pid SIGHUP from ChildKiller.
    child_pid: Option<u32>,
    dead: AtomicBool,
    /// Set when output arrived since the last render; cleared by the
    /// frontend when it draws.
    dirty: AtomicBool,
    /// Monotonic count of output chunks the reader thread has applied to
    /// the VT (issue #88). A confirmed send snapshots this BEFORE writing
    /// its input and treats any later bump — the shell echoing the input,
    /// a program advancing the screen — as the practical receipt that the
    /// child consumed the bytes. This is a heuristic receipt (see
    /// [`Surface::write_bytes_confirmed`]), not a byte-exact consumption
    /// proof.
    output_epoch: AtomicU64,
    /// Confirmed-input FIFO state (issue #88): tickets are handed out in
    /// submission order and a confirmed send only writes while holding its
    /// turn, so concurrent confirmed sends to one surface resolve in
    /// submission order. Plain `write_bytes` bypasses the gate entirely.
    ack_gate: Mutex<AckGateState>,
    /// Wakes gate waiters when the queue advances past a ticket.
    ack_turn: Condvar,
    title: Mutex<String>,
    pwd: Mutex<Option<String>>,
    /// Working directory the child was spawned in. Fixed at spawn time;
    /// [`pwd`](PtySurface::pwd) supersedes it once the shell reports OSC 7,
    /// but this stays as a fallback for shells that never do.
    initial_cwd: Option<String>,
    /// Set only for a surface spawned via `SurfaceOptions.remote`. Lets
    /// `persist.rs` capture enough to reattach the same remote session on
    /// restore, instead of recreating this tab as a local shell.
    remote: Option<crate::remote_pty::RemoteSpec>,
    /// The argv this surface was spawned with (`None` = the default
    /// login shell), recorded at spawn time so `layout export` (issue
    /// #76) can replay the agent command. Reading it back from
    /// `/proc/<child>` at export time is insufficient: agents typed into
    /// a shell via `mtyx send` are grandchildren of the pty child.
    spawn_command: Option<Vec<String>>,
    /// The `extra_env` entries in force at spawn time (post
    /// socket-env refresh), for the same reason. mtyx's auto-injected
    /// `MTYX_MUX_SOCKET`/`MTYX_SOCKET_PATH` are filtered out again at
    /// capture time (`layout_doc::capture_tab`).
    spawn_env: Vec<(String, String)>,
    agent: Mutex<Option<AgentReport>>,
    size: Mutex<(u16, u16)>,
    /// Live output subscribers (attach streams). Guarded by the terminal
    /// lock ordering: the reader thread broadcasts while holding the
    /// terminal lock, and [`Surface::attach_stream`] registers taps under
    /// the same lock, so a subscriber sees exactly the bytes applied
    /// after its replay snapshot — no gap, no duplication.
    taps: Mutex<Vec<std::sync::mpsc::Sender<AttachFrame>>>,
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface").field("id", &self.id).field("kind", &self.kind()).finish()
    }
}

/// Confirmed-input FIFO state for one PTY surface (issue #88). Guarded by
/// [`PtySurface::ack_gate`]; `current` names the ticket currently allowed
/// to write.
#[derive(Default)]
struct AckGateState {
    next_ticket: u64,
    current: u64,
    /// Tickets abandoned by senders whose wait-for-turn deadline elapsed
    /// (nothing was written for them). Skipped over when the queue
    /// advances so an abandoned head cannot stall later senders.
    abandoned: Vec<u64>,
}

impl AckGateState {
    fn take_ticket(&mut self) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        ticket
    }

    /// Release `ticket`'s turn and advance the queue past it and any
    /// tickets abandoned behind it.
    fn advance_past(&mut self, ticket: u64) {
        debug_assert_eq!(self.current, ticket);
        self.current = self.current.max(ticket) + 1;
        while let Some(pos) = self.abandoned.iter().position(|&t| t == self.current) {
            self.abandoned.swap_remove(pos);
            self.current += 1;
        }
    }
}

/// Why a confirmed (receipted) input write failed (issue #88).
#[derive(Debug)]
pub enum ConfirmedSendError {
    /// The input WAS written to the PTY, but no receipt — surface output
    /// (echo/screen advance) or child exit — was observed within the
    /// timeout. Delivery is unproven, not failed.
    AckTimeout {
        waited_ms: u64,
    },
    /// The send never reached its turn on the surface's confirmed-input
    /// FIFO before the deadline (queue pressure from earlier confirmed
    /// sends); nothing was written for this request.
    QueueTimeout {
        waited_ms: u64,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for ConfirmedSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfirmedSendError::AckTimeout { waited_ms } => {
                write!(f, "no input receipt (surface output or child-drain) within {waited_ms}ms")
            }
            ConfirmedSendError::QueueTimeout { waited_ms } => {
                write!(f, "confirmed-input queue did not reach this send within {waited_ms}ms")
            }
            ConfirmedSendError::Io(err) => write!(f, "pty write failed: {err}"),
        }
    }
}

impl std::error::Error for ConfirmedSendError {}

impl From<std::io::Error> for ConfirmedSendError {
    fn from(err: std::io::Error) -> Self {
        ConfirmedSendError::Io(err)
    }
}

impl Surface {
    pub(crate) fn spawn(
        id: SurfaceId,
        opts: SurfaceOptions,
        mux: Weak<Mux>,
    ) -> anyhow::Result<Arc<Surface>> {
        let size = PtySize { rows: opts.rows, cols: opts.cols, pixel_width: 0, pixel_height: 0 };
        let pty = match &opts.remote {
            Some(spec) => crate::remote_pty::open_remote_pty(spec, size)
                .with_context(|| format!("opening remote pty for {spec:?}"))?,
            None => native_pty_system()
                .openpty(size)
                .context("opening local pty (check /dev/ptmx and devpts mount)")?,
        };

        // Default surfaces are a login shell (argv0 "-bash"), matching
        // gnome-terminal / wezterm / tmux. A bare `/bin/bash` attached to
        // a PTY opened by an unsigned parent is CrowdStrike Falcon IOA
        // GenReverseShell's textbook signature and gets the child SIGKILLed
        // on protected hosts (the pane then looks "Killed" mid-test).
        // Explicit `command` argv is left untouched so tests can still
        // spawn `/bin/cat`, `/bin/sh -c …`, etc.
        let (mut cmd, spawn_label) = match opts.command.as_ref().filter(|argv| !argv.is_empty()) {
            Some(argv) => {
                let mut cmd = CommandBuilder::new(&argv[0]);
                cmd.args(&argv[1..]);
                (cmd, argv.join(" "))
            }
            None => {
                let mut cmd = CommandBuilder::new_default_prog();
                cmd.env("SHELL", platform::default_shell());
                (cmd, format!("login-shell({})", platform::default_shell()))
            }
        };
        cmd.env("TERM", &opts.term);
        // Lets a hook script (e.g. a Claude Code hook) invoked from inside
        // this pty call back into `mtyx report-agent --surface
        // $MTYX_MUX_SURFACE ...` without needing to know its own surface id.
        cmd.env("MTYX_MUX_SURFACE", id.to_string());
        // Grok Build's multiplexer detector (and macOS mtyx) look for these
        // names. Dual-write so an unpatched grok still classifies the pane
        // as MultiplexerKind::Cmux.
        cmd.env("MTYX_PANEL_ID", id.to_string());
        for (k, v) in &opts.extra_env {
            cmd.env(k, v);
            if k == "MTYX_MUX_SOCKET" {
                cmd.env("MTYX_SOCKET_PATH", v);
            }
        }
        // The local-home-dir fallback only makes sense for a local child;
        // for a remote surface, an unset cwd should mean "let the remote
        // shell start wherever it normally would," not "assume it starts
        // in this machine's $HOME."
        let initial_cwd = opts.cwd.clone().or_else(|| {
            opts.remote
                .is_none()
                .then(platform::home_dir)
                .flatten()
                .map(|p| p.display().to_string())
        });
        if let Some(cwd) = initial_cwd.as_deref() {
            cmd.cwd(cwd);
        }

        let mut child = pty
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning pty child: {spawn_label}"))?;
        drop(pty.slave);
        let child_pid = child.process_id();
        // Windows parity: put the pane child into a kill-on-close job
        // object right after spawn — the PR_SET_PDEATHSIG analogue. The
        // brief window between CreateProcess (inside portable-pty) and
        // this assignment is a documented race: grandchildren spawned in
        // it escape the job, exactly like fork-vs-prctl on unix.
        #[cfg(windows)]
        if let Some(pid) = child_pid {
            crate::win::assign_pid_to_kill_on_close_job(pid);
        }
        let killer = child.clone_killer();
        let mut reader = pty.master.try_clone_reader().context("cloning pty master reader")?;
        let writer = pty.master.take_writer().context("taking pty master writer")?;

        // Query responses generated while parsing pty output are queued
        // here and flushed to the pty after each vt_write (the callback
        // runs under the terminal lock; writing to the pty from inside it
        // is fine, but keeping it queued makes the locking obvious).
        let pending_responses: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let title_changed = Arc::new(AtomicBool::new(false));

        let callbacks = Callbacks {
            on_pty_write: Some(Box::new({
                let pending = pending_responses.clone();
                move |bytes| pending.lock().unwrap().extend_from_slice(bytes)
            })),
            on_title_changed: Some(Box::new({
                let flag = title_changed.clone();
                move || flag.store(true, Ordering::Relaxed)
            })),
            on_bell: Some(Box::new({
                let mux = mux.clone();
                move || {
                    if let Some(mux) = mux.upgrade() {
                        mux.emit(MuxEvent::Bell(id));
                    }
                }
            })),
        };

        let mut term = Terminal::new(opts.cols, opts.rows, opts.scrollback, callbacks)?;
        if let Some(mux) = mux.upgrade() {
            let colors = mux.default_colors();
            term.set_default_colors(colors.fg, colors.bg);
        }
        let surface = Arc::new(Surface::Pty(PtySurface {
            meta: SurfaceMeta { id, name: Mutex::new(None), detected_agent: Mutex::new(None) },
            term: Mutex::new(term),
            writer: Mutex::new(writer),
            master: Mutex::new(pty.master),
            killer: Mutex::new(killer),
            child_pid,
            dead: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            output_epoch: AtomicU64::new(0),
            ack_gate: Mutex::new(AckGateState::default()),
            ack_turn: Condvar::new(),
            title: Mutex::new(String::new()),
            pwd: Mutex::new(None),
            initial_cwd,
            remote: opts.remote.clone(),
            spawn_command: opts.command.clone().filter(|argv| !argv.is_empty()),
            spawn_env: opts.extra_env.clone(),
            agent: Mutex::new(None),
            size: Mutex::new((opts.cols, opts.rows)),
            taps: Mutex::new(Vec::new()),
        }));

        // PTY reader: pty bytes -> terminal state -> SurfaceOutput events.
        std::thread::Builder::new().name(format!("surface-{id}-reader")).spawn({
            let surface = surface.clone();
            let mux = mux.clone();
            move || {
                let mut buf = [0u8; 64 * 1024];
                // Best-effort: if the OSC parser can't even allocate, skip
                // notification detection for this surface rather than
                // failing the whole pty.
                let mut osc_watcher = crate::notify::OscWatcher::new().ok();
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let pty = surface.as_pty().expect("surface reader got non-pty surface");
                    {
                        let mut term = pty.term.lock().unwrap();
                        term.vt_write(&buf[..n]);
                        // Issue #88 receipt signal: every chunk the reader
                        // thread applies advances the output epoch, so a
                        // confirmed send waiting on `output_epoch` sees
                        // echoes/screen advances promptly.
                        pty.output_epoch.fetch_add(1, Ordering::Release);
                        {
                            let mut taps = pty.taps.lock().unwrap();
                            if !taps.is_empty() {
                                let frame = AttachFrame::Output(buf[..n].to_vec());
                                taps.retain(|tap| tap.send(frame.clone()).is_ok());
                            }
                        }
                        if title_changed.swap(false, Ordering::Relaxed) {
                            let title = term.title().unwrap_or_default();
                            *pty.title.lock().unwrap() = title;
                            if let Some(mux) = mux.upgrade() {
                                mux.emit(MuxEvent::TitleChanged(surface.id));
                            }
                        }
                        if let Some(pwd) = term.pwd() {
                            *pty.pwd.lock().unwrap() = Some(pwd);
                        }
                    }
                    if let Some(watcher) = osc_watcher.as_mut() {
                        for (title, body) in watcher.feed(&buf[..n]) {
                            if let Some(mux) = mux.upgrade() {
                                mux.report_agent(
                                    surface.id,
                                    crate::AgentState::Blocked,
                                    crate::AgentStateSource::Detected,
                                    None,
                                    None,
                                    None,
                                );
                                mux.emit(MuxEvent::OscNotification {
                                    surface: surface.id,
                                    title,
                                    body,
                                });
                            }
                        }
                    }
                    let responses = std::mem::take(&mut *pending_responses.lock().unwrap());
                    if !responses.is_empty() {
                        let _ = surface.write_bytes(&responses);
                    }
                    if !pty.dirty.swap(true, Ordering::AcqRel) {
                        if let Some(mux) = mux.upgrade() {
                            mux.emit(MuxEvent::SurfaceOutput(surface.id));
                        }
                    }
                }
                if let Some(pty) = surface.as_pty() {
                    pty.dead.store(true, Ordering::Release);
                }
                if let Some(mux) = mux.upgrade() {
                    mux.surface_exited(surface.id);
                }
            }
        })?;

        // Child reaper: avoid zombies; the reader thread handles EOF.
        std::thread::Builder::new().name(format!("surface-{id}-wait")).spawn(move || {
            let _ = child.wait();
        })?;

        Ok(surface)
    }

    fn as_pty(&self) -> Option<&PtySurface> {
        match self {
            Surface::Pty(surface) => Some(surface),
            Surface::Browser(_) => None,
        }
    }

    pub(crate) fn as_browser(&self) -> Option<&BrowserSurface> {
        match self {
            Surface::Pty(_) => None,
            Surface::Browser(surface) => Some(surface),
        }
    }

    pub fn kind(&self) -> SurfaceKind {
        match self {
            Surface::Pty(_) => SurfaceKind::Pty,
            Surface::Browser(_) => SurfaceKind::Browser,
        }
    }

    /// Write input bytes to the PTY child.
    pub fn write_bytes(&self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(pty) = self.as_pty() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "browser surface does not accept PTY bytes",
            ));
        };
        let mut writer = pty.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Confirmed (receipted) input write (issue #88). Writes `bytes` to
    /// the PTY and returns success only after observing a practical
    /// receipt that the child consumed them, within `timeout`:
    ///
    /// - the surface produced output after the write (the reader thread
    ///   applied a chunk: the shell echoed the input, or a program
    ///   advanced the screen), **or**
    /// - the child exited (it cannot exit without draining its input
    ///   queue first for anything it was going to act on).
    ///
    /// This is deliberately a HEURISTIC receipt, not a byte-exact
    /// consumption proof: the epoch is snapshotted immediately before
    /// the write and any later output counts, so output merely coincident
    /// with the send (a busy pane) can also satisfy it. The alternative —
    /// proving the tty input queue drained — has no portable kernel
    /// interface and is not attempted.
    ///
    /// Ordering: the write happens under this surface's confirmed-input
    /// FIFO ticket, so concurrent confirmed sends to one surface resolve
    /// in submission (ticket) order. Unconfirmed [`Surface::write_bytes`]
    /// bypasses the gate and is unchanged from pre-#88 behavior. The
    /// overall `timeout` budget covers BOTH the queue wait and the
    /// receipt wait; on a queue-timeout nothing is written.
    pub fn write_bytes_confirmed(
        &self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<(), ConfirmedSendError> {
        let Some(pty) = self.as_pty() else {
            return Err(ConfirmedSendError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "browser surface does not accept PTY bytes",
            )));
        };
        let start = Instant::now();
        let deadline = start + timeout;

        // 1. Take a ticket (submission order) and wait for our turn.
        let ticket = {
            let mut state = pty.ack_gate.lock().unwrap();
            state.take_ticket()
        };
        {
            let mut state = pty.ack_gate.lock().unwrap();
            while state.current < ticket {
                let now = Instant::now();
                if now >= deadline {
                    state.abandoned.push(ticket);
                    drop(state);
                    pty.ack_turn.notify_all();
                    return Err(ConfirmedSendError::QueueTimeout {
                        waited_ms: start.elapsed().as_millis() as u64,
                    });
                }
                let (guard, _) =
                    pty.ack_turn.wait_timeout(state, deadline - now).map_err(|_| {
                        ConfirmedSendError::Io(std::io::Error::other("ack gate poisoned"))
                    })?;
                state = guard;
            }
        }

        // 2. We hold the turn: snapshot the epoch, write, await receipt.
        //    Baseline BEFORE the write so an echo that races back while
        //    the write returns still counts (a baseline taken after could
        //    already include the echo and then never advance again).
        let result = (|| {
            let baseline = pty.output_epoch.load(Ordering::Acquire);
            {
                let mut writer = pty.writer.lock().unwrap();
                writer.write_all(bytes)?;
                writer.flush()?;
            }
            loop {
                if pty.dead.load(Ordering::Acquire) {
                    return Ok(());
                }
                if pty.output_epoch.load(Ordering::Acquire) > baseline {
                    return Ok(());
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(ConfirmedSendError::AckTimeout {
                        waited_ms: start.elapsed().as_millis() as u64,
                    });
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
            }
        })();

        // 3. Release the turn whether or not the receipt arrived (the
        //    bytes are already written; a timeout must not wedge the
        //    queue for later senders).
        {
            let mut state = pty.ack_gate.lock().unwrap();
            state.advance_past(ticket);
        }
        pty.ack_turn.notify_all();
        result
    }

    /// Direct PTY child PID (the pane's shell process). Used by
    /// `send --shell auto` to resolve the pane's shell from
    /// `/proc/<pid>/cmdline` on Linux (issue #35).
    pub fn child_pid(&self) -> Option<u32> {
        self.as_pty().and_then(|pty| pty.child_pid)
    }

    /// Run `f` with exclusive access to the terminal state.
    ///
    /// Browser-aware code should call [`Surface::kind`] first. This
    /// method is kept for existing PTY call sites.
    pub fn with_terminal<R>(&self, f: impl FnOnce(&mut Terminal) -> R) -> Option<R> {
        let pty = self.as_pty()?;
        Some(f(&mut pty.term.lock().unwrap()))
    }

    pub fn try_with_terminal<R>(&self, f: impl FnOnce(&mut Terminal) -> R) -> anyhow::Result<R> {
        let Some(pty) = self.as_pty() else {
            anyhow::bail!("browser surface does not have a VT terminal");
        };
        Ok(f(&mut pty.term.lock().unwrap()))
    }

    pub fn set_default_colors(&self, colors: DefaultColors) {
        if let Some(pty) = self.as_pty() {
            pty.term.lock().unwrap().set_default_colors(colors.fg, colors.bg);
            pty.dirty.store(true, Ordering::Release);
        }
    }

    pub fn set_name(&self, name: Option<String>) {
        *self.name.lock().unwrap() = name;
    }

    pub fn name(&self) -> Option<String> {
        self.name.lock().unwrap().clone()
    }

    /// Cache the latest ambient agent-detection result (issue #78).
    /// Informational only — it never touches `AgentReport` state.
    pub fn set_detected_agent(&self, detection: crate::agent_detect::Detection) {
        *self.detected_agent.lock().unwrap() = Some(detection);
    }

    /// The last ambient agent-detection result, if `detect-agent` ever
    /// ran on this surface. `None` until then (distinct from a cached
    /// `unknown` detection, which is `Some`).
    pub fn detected_agent(&self) -> Option<crate::agent_detect::Detection> {
        self.detected_agent.lock().unwrap().clone()
    }

    /// Snapshot the terminal into `rs` (holds the terminal lock only for
    /// the duration of the update).
    pub fn snapshot(&self, rs: &mut RenderState) -> ghostty_vt::Result<()> {
        let Some(pty) = self.as_pty() else {
            return Err(ghostty_vt::Error::InvalidValue);
        };
        rs.update(&mut pty.term.lock().unwrap())
    }

    /// Resize this surface. PTYs receive cell dimensions; browsers also
    /// use the last configured cell pixel size for CDP device metrics.
    /// Returns whether the final clamped size actually changed.
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        match self {
            Surface::Pty(pty) => pty.resize(cols, rows),
            Surface::Browser(browser) => {
                let before = browser.size();
                browser.resize(cols, rows);
                browser.size() != before
            }
        }
    }

    pub fn set_cell_pixel_size(&self, width_px: u16, height_px: u16) {
        if let Some(browser) = self.as_browser() {
            browser.set_cell_pixel_size(width_px, height_px);
        }
    }

    pub fn size(&self) -> (u16, u16) {
        match self {
            Surface::Pty(pty) => *pty.size.lock().unwrap(),
            Surface::Browser(browser) => browser.size(),
        }
    }

    pub fn title(&self) -> String {
        match self {
            Surface::Pty(pty) => pty.title.lock().unwrap().clone(),
            Surface::Browser(browser) => browser.title(),
        }
    }

    pub fn pwd(&self) -> Option<String> {
        self.as_pty().and_then(|pty| pty.pwd.lock().unwrap().clone())
    }

    /// Best-known working directory: the shell's live OSC 7 report when
    /// available, otherwise the directory the surface was spawned in.
    pub fn cwd(&self) -> Option<String> {
        self.as_pty()
            .and_then(|pty| pty.pwd.lock().unwrap().clone().or_else(|| pty.initial_cwd.clone()))
    }

    /// The `RemoteSpec` this surface was spawned with, if it's a
    /// `cmuxd-remote` session rather than a local shell.
    pub fn remote_spec(&self) -> Option<crate::remote_pty::RemoteSpec> {
        self.as_pty().and_then(|pty| pty.remote.clone())
    }

    /// The argv this surface was spawned with (`None` for the default
    /// login shell or a browser surface) — recorded at spawn time for
    /// layout export (issue #76).
    pub fn spawn_command(&self) -> Option<Vec<String>> {
        self.as_pty().and_then(|pty| pty.spawn_command.clone())
    }

    /// The extra env entries injected at spawn time (`[]` for browser
    /// surfaces). Includes mtyx's auto-injected socket keys; layout
    /// capture filters those back out.
    pub fn spawn_env(&self) -> Vec<(String, String)> {
        self.as_pty().map(|pty| pty.spawn_env.clone()).unwrap_or_default()
    }

    pub fn agent_report(&self) -> Option<AgentReport> {
        self.as_pty().and_then(|pty| pty.agent.lock().unwrap().clone())
    }

    /// Applies a new agent-state report under the authority rules from
    /// `spec/commands.md`: a hook report always applies; a socket report
    /// is rejected while the current source is a hook report. Returns the
    /// report now in effect and whether this call is the one that applied
    /// it (`false` when rejected by the authority rule, in which case the
    /// report is the unchanged prior one), or `None` for a non-PTY surface.
    ///
    /// Name semantics (issue #75): an incoming report that omits `agent`
    /// keeps the pane's established name (names live as long as the pane
    /// has a report); `message` — like `state` — always reflects the
    /// latest applied report.
    pub fn set_agent_report(
        &self,
        state: AgentState,
        source: AgentStateSource,
        session: Option<String>,
        agent: Option<String>,
        message: Option<String>,
    ) -> Option<(AgentReport, bool)> {
        let pty = self.as_pty()?;
        let mut current = pty.agent.lock().unwrap();
        let accept = current.as_ref().map_or(true, |existing| source >= existing.source);
        if accept {
            let agent = agent.or_else(|| current.as_ref().and_then(|r| r.agent.clone()));
            let report =
                AgentReport { state, source, session, agent, message, updated_at_ms: now_ms() };
            *current = Some(report.clone());
            Some((report, true))
        } else {
            current.clone().map(|report| (report, false))
        }
    }

    pub fn is_dead(&self) -> bool {
        match self {
            Surface::Pty(pty) => pty.dead.load(Ordering::Acquire),
            Surface::Browser(browser) => browser.is_dead(),
        }
    }

    /// Clear the coalesced output flag; returns whether output was pending.
    pub fn take_dirty(&self) -> bool {
        match self {
            Surface::Pty(pty) => pty.dirty.swap(false, Ordering::AcqRel),
            Surface::Browser(browser) => browser.take_dirty(),
        }
    }

    /// Attach to a PTY surface: a VT replay plus a live byte stream.
    pub fn attach_stream(&self) -> ghostty_vt::Result<AttachStream> {
        let Some(pty) = self.as_pty() else {
            return Err(ghostty_vt::Error::InvalidValue);
        };
        let mut term = pty.term.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // Snapshot and tap registration under the same terminal lock:
        // the reader thread cannot apply bytes between the two.
        let replay = term.vt_replay()?;
        let (cols, rows) = (term.cols(), term.rows());
        pty.taps.lock().unwrap().push(tx);
        Ok(AttachStream { cols, rows, replay, stream: rx })
    }

    pub fn kill(&self) {
        match self {
            Surface::Pty(pty) => {
                // Issue #28: kill the whole process tree rooted at the PTY
                // child, not just the direct child. portable_pty's
                // ChildKiller only SIGHUPs a single pid, so backgrounded
                // / double-forked grandchildren would otherwise leak.
                #[cfg(target_os = "linux")]
                {
                    if let Ok(master) = pty.master.lock() {
                        if let Some(pgid) = master.process_group_leader() {
                            if pgid > 1 {
                                // Negative pgid = signal the whole process group.
                                let _ = unsafe { libc::kill(-pgid, libc::SIGTERM) };
                            }
                        }
                    }
                    if let Some(pid) = pty.child_pid {
                        crate::process::kill_process_tree(pid);
                    }
                }
                // Windows parity: the kill(-pgid) analogue — terminate the
                // per-surface job object assigned at spawn (see win.rs).
                // No graceful phase exists cross-console; documented
                // limitation in win.rs.
                #[cfg(windows)]
                if let Some(pid) = pty.child_pid {
                    crate::win::terminate_pid_tree(pid);
                }
                let _ = pty.killer.lock().unwrap().kill();
            }
            Surface::Browser(browser) => browser.kill(),
        }
    }

    pub fn browser_frame(&self) -> Option<BrowserFrame> {
        self.as_browser().and_then(BrowserSurface::latest_frame)
    }

    pub fn browser_url(&self) -> Option<String> {
        self.as_browser().map(BrowserSurface::url)
    }

    pub fn browser_source(&self) -> Option<BrowserSource> {
        self.as_browser().and_then(BrowserSurface::source)
    }

    pub fn browser_status(&self) -> Option<BrowserStatus> {
        self.as_browser().map(BrowserSurface::status)
    }

    pub fn browser_frames_stalled(&self) -> Option<bool> {
        self.as_browser().map(BrowserSurface::frames_stalled)
    }

    pub fn attach_frames(&self) -> anyhow::Result<(BrowserAttachState, BrowserFrameStream)> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        Ok(browser.attach_frames())
    }

    pub fn browser_insert_text(&self, text: &str) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.insert_text(text)
    }

    pub fn browser_key_event(
        &self,
        event_type: &str,
        key: &str,
        code: &str,
        windows_virtual_key_code: u32,
        modifiers: u32,
        text: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.key_event(event_type, key, code, windows_virtual_key_code, modifiers, text)
    }

    pub fn browser_mouse_event(
        &self,
        event_type: &str,
        x: f64,
        y: f64,
        button: Option<&str>,
        click_count: Option<u32>,
    ) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.mouse_event(event_type, x, y, button, click_count)
    }

    pub fn browser_wheel(&self, x: f64, y: f64, delta_y: f64) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.wheel(x, y, delta_y)
    }

    pub fn browser_navigate(&self, url: &str) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.navigate(url)
    }

    pub fn browser_back(&self) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.back()
    }

    pub fn browser_forward(&self) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.forward()
    }

    pub fn browser_reload(&self) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.reload()
    }

    pub fn browser_activate(&self) -> anyhow::Result<()> {
        let Some(browser) = self.as_browser() else {
            anyhow::bail!("PTY surface is not a browser surface");
        };
        browser.activate()
    }
}

impl PtySurface {
    /// Resize both the PTY and the terminal state. Returns whether the
    /// final clamped size actually changed.
    fn resize(&self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.max(1), rows.max(1));
        {
            let mut size = self.size.lock().unwrap();
            if *size == (cols, rows) {
                return false;
            }
            *size = (cols, rows);
        }
        // Hold the terminal lock while resizing and while sending the
        // attach marker, so attach mirrors observe bytes and resizes in
        // the exact order the server terminal applied them.
        let mut term = self.term.lock().unwrap();
        let _ = self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        // Nominal cell metrics; only pixel size reports observe these.
        let _ = term.resize(cols, rows, 8, 16);
        let replay = term.vt_replay().unwrap_or_default();
        let mut taps = self.taps.lock().unwrap();
        if !taps.is_empty() {
            taps.retain(|tap| {
                tap.send(AttachFrame::Resized { cols, rows, replay: replay.clone() }).is_ok()
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vt_size_accepts_cols_by_rows() {
        assert_eq!(parse_vt_size("100x30"), Some((100, 30)));
        assert_eq!(parse_vt_size(" 120x40 "), Some((120, 40)));
        assert_eq!(parse_vt_size("100X7"), Some((100, 7)));
        assert_eq!(parse_vt_size("100"), None);
        assert_eq!(parse_vt_size(""), None);
        assert_eq!(parse_vt_size("axb"), None);
        assert_eq!(parse_vt_size("0x30"), None);
        assert_eq!(parse_vt_size("100x0"), None);
        assert_eq!(parse_vt_size("70000x30"), None);
    }

    /// Issue #99: the no-client default geometry is 120x40;
    /// `MTYX_MUX_VT_SIZE=COLSxROWS` overrides it when parseable. The
    /// assertion adapts to an ambient override so the test stays green
    /// on machines that export the env var.
    #[test]
    fn default_geometry_is_120x40_unless_env_overrides() {
        let expected = std::env::var("MTYX_MUX_VT_SIZE")
            .ok()
            .and_then(|value| parse_vt_size(&value))
            .unwrap_or((120, 40));
        let opts = SurfaceOptions::default();
        assert_eq!((opts.cols, opts.rows), expected);
    }

    /// Issue #88: the confirmed-input FIFO hands out tickets in submission
    /// order and `advance_past` walks exactly one live ticket at a time,
    /// skipping tickets abandoned by queue-timeouts so an abandoned head
    /// cannot stall later senders. This is the mechanism behind
    /// `input_ack_ordering` (tests/input_ack.rs).
    #[test]
    fn ack_gate_advances_in_ticket_order_and_skips_abandoned() {
        let mut gate = AckGateState::default();
        let t0 = gate.take_ticket();
        let t1 = gate.take_ticket();
        let t2 = gate.take_ticket();
        assert_eq!((t0, t1, t2), (0, 1, 2));
        assert_eq!(gate.current, 0, "first ticket holds the turn");

        // Ticket 1 gives up while 0 still holds the turn (queue-timeout).
        gate.abandoned.push(t1);
        gate.advance_past(t0);
        assert_eq!(gate.current, t2, "abandoned ticket 1 is skipped, 2 is next");
        assert!(gate.abandoned.is_empty());

        // The queue keeps advancing one ticket at a time.
        gate.advance_past(t2);
        assert_eq!(gate.current, 3);
        let t3 = gate.take_ticket();
        assert_eq!(t3, 3);
    }
}
