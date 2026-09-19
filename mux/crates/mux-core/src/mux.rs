//! The multiplexer: owns the session [`State`] and every surface runtime,
//! and broadcasts [`MuxEvent`]s to subscribed frontends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::agent_detect::{AgentPattern, Detection, DetectionSettings};
use crate::browser::{self, BrowserBootstrap, BrowserRuntime};
use crate::model::{IconName, Node, Pane, Screen, State, Workspace};
use crate::surface::{
    AgentReport, AgentState, AgentStateSource, DefaultColors, SpawnOverrides, Surface,
    SurfaceOptions,
};
use crate::{PaneId, Rgb, ScreenId, SplitDir, SurfaceId, WorkspaceId};

/// Events pushed to subscribed frontends.
#[derive(Debug, Clone)]
pub enum MuxEvent {
    /// New output arrived in a surface (coalesced; cleared when rendered).
    SurfaceOutput(SurfaceId),
    /// A surface's runtime changed size.
    SurfaceResized {
        surface: SurfaceId,
        cols: u16,
        rows: u16,
    },
    /// A surface's child exited. The mux has already reaped it from the
    /// tree (a tree-changed follows) by the time this arrives.
    SurfaceExited(SurfaceId),
    TitleChanged(SurfaceId),
    Bell(SurfaceId),
    Status(String),
    /// The workspace/screen/pane/tab tree changed (from any frontend or
    /// the control socket).
    TreeChanged,
    /// Every workspace is gone.
    Empty,
    /// A surface's reported agent state changed (see `report-agent`).
    AgentStateChanged {
        surface: SurfaceId,
        previous: Option<AgentState>,
        report: AgentReport,
    },
    /// A surface's pty output contained an OSC 9 / OSC 777 / kitty desktop
    /// notification. Distinct from the mux's own (unimplemented) notify
    /// inbox in `spec/commands.md`/`events.md` - this is a raw, ephemeral
    /// signal from the pane's own output, not a stored, dismissible
    /// notification record.
    OscNotification {
        surface: SurfaceId,
        title: String,
        body: String,
    },
    /// A manual flash was triggered for a workspace (e.g. via the
    /// `trigger-flash` command), for frontends to render a transient
    /// visual pulse. `surface` is advisory context about which surface
    /// triggered it, if any — not validated.
    Flash {
        workspace: WorkspaceId,
        surface: Option<SurfaceId>,
    },
}

/// One surviving worktree child of a workspace close (issue #100): a
/// workspace whose panes live under a worktree the closed workspace's
/// panes created/own (see [`Mux::worktree_children`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeChild {
    pub workspace: WorkspaceId,
    pub name: String,
    /// Path of the parent-pane worktree the child lives under.
    pub worktree_path: String,
    /// Branch of that worktree (from the owning pane's record).
    pub worktree_branch: Option<String>,
    /// Some pane in the child workspace has a live agent report
    /// (`working`/`blocked`). Best-effort, purely informational - agent
    /// state is whatever hooks/sockets last reported (issue #100).
    pub running_agent: bool,
}

/// What a guarded workspace close did (issue #100).
#[derive(Debug, Clone, Default)]
pub struct CloseWorkspaceReport {
    /// Workspaces closed by the call: the target first, then any
    /// worktree children a `--group` close took with it.
    pub closed: Vec<(WorkspaceId, String)>,
    /// Worktree children that survived because `group` was not set.
    pub survivors: Vec<WorktreeChild>,
}

/// The multiplexer. Shared by frontends and the control socket server.
pub struct Mux {
    state: Mutex<State>,
    subscribers: Mutex<Vec<Sender<MuxEvent>>>,
    next_id: AtomicU64,
    next_active_at: AtomicU64,
    surface_options: SurfaceOptions,
    browser_runtime: Mutex<Option<Arc<BrowserRuntime>>>,
    cell_pixels: Mutex<(u16, u16)>,
    default_colors: Mutex<DefaultColors>,
    /// Resolved presentation chrome (theme/tabs/sidebar/keys) the server
    /// process loaded, exposed to thin-client attaches via the
    /// `get-resolved-config` verb so a local `Overlay` can layer on top
    /// of the *server* config rather than the laptop's own config. Set
    /// from `mux-tui`'s `run_server` after `config::load()`. `None` until
    /// the server registers it (e.g. a `mux-core`-only host with no TUI).
    resolved_chrome: Mutex<Option<serde_json::Value>>,
    /// Set only by `enable_persistence()`. Gates every snapshot write,
    /// including the one on `shutdown()` — without this, every ephemeral
    /// `Mux` a test or one-shot CLI invocation creates would write a
    /// session file to the real `$XDG_STATE_HOME`.
    persistence_enabled: std::sync::atomic::AtomicBool,
    /// Logical session name. Interior-mutable because a running daemon is
    /// shared across the accept/conn/persist threads (`Arc<Mux>`) and can
    /// be renamed in place by the `rename-session` command (issue #63).
    /// Use [`session_name`](Self::session_name) to read it.
    session: Mutex<String>,
    /// The daemon's currently-bound control-socket path: the single source
    /// of truth read at shutdown (`cleanup`) and at every surface spawn
    /// (so panes spawned after a rename inherit the new `MTYX_MUX_SOCKET`).
    /// `None` until `server::serve` calls [`set_socket_path`](Self::set_socket_path).
    socket_path: Mutex<Option<PathBuf>>,
    /// Ambient agent-detection settings (issue #78 AC7), pushed from the
    /// TUI host after `config::load()`.
    agent_detection: Mutex<DetectionSettings>,
    /// User-added agent patterns layered on top of the bundled registry
    /// (issue #78 AC4). Session-lifetime only in v1 — not persisted
    /// across daemon restarts.
    custom_agent_patterns: Mutex<Vec<AgentPattern>>,
    /// Operator-configured worktree path pattern (issue #77 AC6), set
    /// from `mux-tui`'s `run_server` after `config::load()`.
    /// `None` means the default `<repo>/../<repo>.<branch>/`.
    worktree_pattern: Mutex<Option<String>>,
}

impl Mux {
    pub fn new(session: impl Into<String>, surface_options: SurfaceOptions) -> Arc<Self> {
        let session = session.into();
        let mut surface_options = surface_options;
        surface_options.browser_session_name = session.clone();
        Arc::new(Mux {
            state: Mutex::new(State {
                workspaces: Vec::new(),
                active_workspace: 0,
                panes: HashMap::new(),
                surfaces: HashMap::new(),
            }),
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            next_active_at: AtomicU64::new(1),
            surface_options,
            browser_runtime: Mutex::new(None),
            cell_pixels: Mutex::new((8, 16)),
            default_colors: Mutex::new(DefaultColors::default()),
            resolved_chrome: Mutex::new(None),
            persistence_enabled: std::sync::atomic::AtomicBool::new(false),
            session: Mutex::new(session),
            socket_path: Mutex::new(None),
            agent_detection: Mutex::new(DetectionSettings::default()),
            custom_agent_patterns: Mutex::new(Vec::new()),
            worktree_pattern: Mutex::new(None),
        })
    }

    /// The logical session name (cloned; cheap vs. a JSON encode). Renamed
    /// in place by `rename-session` (issue #63).
    pub fn session_name(&self) -> String {
        self.session.lock().unwrap().clone()
    }

    /// The daemon's currently-bound control-socket path, or `None` before
    /// `server::serve` has bound. The single source of truth for the live
    /// socket location (moves on rename).
    pub fn socket_path(&self) -> Option<PathBuf> {
        self.socket_path.lock().unwrap().clone()
    }

    /// Record the bound socket path. Called by `server::serve` immediately
    /// after a successful `transport::listen` (before the accept thread
    /// spawns, so the value is set before any client can connect).
    pub(crate) fn set_socket_path(&self, path: PathBuf) {
        *self.socket_path.lock().unwrap() = Some(path);
    }

    /// Update the logical session name. Called by the `rename-session`
    /// handler after the socket/pid files have moved (issue #63).
    pub(crate) fn set_session_name(&self, name: String) {
        *self.session.lock().unwrap() = name;
    }

    /// Update the `MTYX_MUX_SOCKET` entry in a cloned `SurfaceOptions` so a
    /// newly-spawned pane inherits the daemon's *current* live socket path
    /// (not the stale startup path). Existing panes keep whatever they
    /// inherited at spawn — this is the AC4 lifetime guarantee (issue #63).
    fn refresh_socket_env(&self, opts: &mut SurfaceOptions) {
        if let Some(p) = self.socket_path() {
            opts.extra_env.retain(|(k, _)| k != "MTYX_MUX_SOCKET");
            opts.extra_env.push(("MTYX_MUX_SOCKET".into(), p.display().to_string()));
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn next_active_at(&self) -> u64 {
        self.next_active_at.fetch_add(1, Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> Receiver<MuxEvent> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn emit(&self, event: MuxEvent) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.send(event.clone()).is_ok());
    }

    fn spawn_surface(
        self: &Arc<Self>,
        cwd: Option<String>,
        size: Option<(u16, u16)>,
        overrides: Option<&SpawnOverrides>,
    ) -> anyhow::Result<Arc<Surface>> {
        let id = self.next_id();
        let mut opts = self.surface_options.clone();
        // New panes inherit the daemon's *current* live socket path (issue #63
        // AC4): existing panes keep what they got at spawn; panes spawned
        // after a rename get the new path.
        self.refresh_socket_env(&mut opts);
        // Issue #76: layer explicit spawn overrides (recorded agent argv /
        // env / cwd from a layout apply or `new-tab --exec`) on top of the
        // refreshed template. Precedence: override > cwd param > template.
        if let Some(overrides) = overrides {
            if let Some(command) = &overrides.command {
                opts.command = Some(command.clone());
            }
            for (key, value) in &overrides.extra_env {
                opts.extra_env.retain(|(k, _)| k != key);
                opts.extra_env.push((key.clone(), value.clone()));
            }
            if overrides.cwd.is_some() {
                opts.cwd = overrides.cwd.clone();
            }
        }
        if cwd.is_some() && opts.cwd.is_none() {
            opts.cwd = cwd;
        }
        // Spawn at the final size when the frontend knows it: starting at
        // the headless default 120x40 (issue #99) and resizing a frame
        // later makes shells emit artifacts (e.g. zsh's reverse-video %%
        // partial-line marker).
        if let Some((cols, rows)) = size {
            opts.cols = cols.max(1);
            opts.rows = rows.max(1);
        }
        let surface = Surface::spawn(id, opts, Arc::downgrade(self))?;
        self.state.lock().unwrap().surfaces.insert(id, surface.clone());
        Ok(surface)
    }

    fn spawn_remote_surface(
        self: &Arc<Self>,
        remote: crate::remote_pty::RemoteSpec,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        let id = self.next_id();
        let mut opts = self.surface_options.clone();
        self.refresh_socket_env(&mut opts);
        opts.remote = Some(remote);
        if let Some((cols, rows)) = size {
            opts.cols = cols.max(1);
            opts.rows = rows.max(1);
        }
        let surface = Surface::spawn(id, opts, Arc::downgrade(self))?;
        self.state.lock().unwrap().surfaces.insert(id, surface.clone());
        Ok(surface)
    }

    fn spawn_browser_surface(
        self: &Arc<Self>,
        url: String,
        size: Option<(u16, u16)>,
    ) -> Arc<Surface> {
        let id = self.next_id();
        let mut opts = self.surface_options.clone();
        self.refresh_socket_env(&mut opts);
        let size = size.unwrap_or((opts.cols, opts.rows));
        let cell_pixels = *self.cell_pixels.lock().unwrap();
        let surface = browser::new_surface(id, url.clone(), size, cell_pixels, &opts);
        self.state.lock().unwrap().surfaces.insert(id, surface.clone());
        self.start_browser_bootstrap(surface.clone(), BrowserBootstrap::Create { url }, None);
        surface
    }

    fn browser_runtime(&self) -> anyhow::Result<Arc<BrowserRuntime>> {
        let mut runtime = self.browser_runtime.lock().unwrap();
        if let Some(existing) = runtime.as_ref().filter(|existing| !existing.is_closed()) {
            return Ok(existing.clone());
        }
        let created = BrowserRuntime::connect(&self.surface_options)?;
        *runtime = Some(created.clone());
        Ok(created)
    }

    fn start_browser_bootstrap(
        self: &Arc<Self>,
        surface: Arc<Surface>,
        bootstrap: BrowserBootstrap,
        runtime: Option<Arc<BrowserRuntime>>,
    ) {
        let mux = self.clone();
        let id = surface.id;
        let _ = std::thread::Builder::new().name(format!("browser-surface-{id}-bootstrap")).spawn(
            move || {
                let result = (|| -> anyhow::Result<()> {
                    let runtime = match runtime {
                        Some(runtime) => runtime,
                        None => mux.browser_runtime()?,
                    };
                    runtime.bootstrap_surface_sync(surface.clone(), bootstrap, Arc::downgrade(&mux))
                })();
                if let Err(err) = result {
                    if let Surface::Browser(browser) = surface.as_ref() {
                        browser.mark_failed(err.to_string());
                    }
                    mux.emit(MuxEvent::Status(format!("browser failed: {err}")));
                    mux.emit(MuxEvent::TitleChanged(id));
                    mux.emit(MuxEvent::SurfaceOutput(id));
                }
            },
        );
    }

    /// A fresh single-tab pane wrapping `surface`.
    fn make_pane(&self, surface: SurfaceId) -> (PaneId, Pane) {
        let id = self.next_id();
        (
            id,
            Pane {
                id,
                name: None,
                tabs: vec![surface],
                active_tab: 0,
                active_at: self.next_active_at(),
                worktrees: Vec::new(),
            },
        )
    }

    pub fn surface(&self, id: SurfaceId) -> Option<Arc<Surface>> {
        self.state.lock().unwrap().surfaces.get(&id).cloned()
    }

    /// Run `f` with the session state.
    ///
    /// The state lock is held for the duration of `f`; do not call back
    /// into `Mux` methods that take it (`surface()`, `close_pane()`, ...).
    pub fn with_state<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock().unwrap())
    }

    pub fn surface_count(&self) -> usize {
        self.state.lock().unwrap().surfaces.len()
    }

    pub fn shutdown(&self) {
        if self.persistence_enabled.load(Ordering::Acquire) {
            self.write_snapshot();
        }
        let surfaces = self.state.lock().unwrap().surfaces.values().cloned().collect::<Vec<_>>();
        for surface in surfaces {
            surface.kill();
        }
        if let Some(runtime) = self.browser_runtime.lock().unwrap().take() {
            runtime.shutdown();
        }
        // Note: do NOT call process::kill_remaining_children() here.
        // Mux is also constructed in-process by tests that run in parallel;
        // a global child sweep would kill sibling tests' PTY children.
        // The daemon path in mux-tui calls kill_remaining_children after
        // shutdown so subreaper-reparented orphans are still cleaned up
        // in production (issue #28).
    }

    fn snapshot_path(&self) -> std::path::PathBuf {
        crate::platform::session_snapshot_path(&self.session_name())
    }

    fn write_snapshot(&self) {
        let snapshot = self.with_state(crate::persist::capture);
        let path = self.snapshot_path();
        let result = if snapshot.is_empty() {
            // An intentionally-emptied session shouldn't resurrect old
            // panes next time it starts.
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else {
            snapshot.save(&path)
        };
        if let Err(e) = result {
            self.emit(MuxEvent::Status(format!("session snapshot write failed: {e}")));
        }
    }

    /// Restores workspace/screen/pane layout and each tab's cwd from a
    /// prior [`Self::enable_persistence`] snapshot for this session, if
    /// one exists. Every restored tab is the default shell, `cd`'d into
    /// its recorded cwd (see `persist.rs` for why commands aren't
    /// restored). Call once, right after [`Self::new`], before any other
    /// mutation — it assumes an empty tree.
    pub fn restore_session(self: &Arc<Self>) {
        let Some(snapshot) = crate::persist::SessionSnapshot::load(&self.snapshot_path()) else {
            return;
        };
        let (workspaces, active_workspace) = crate::persist::workspaces(&snapshot);
        for ws in &workspaces {
            if let Err(e) = self.restore_workspace(ws) {
                let message = format!("failed to restore workspace {:?}: {e}", ws.name);
                // restore_session runs before the control socket is even
                // listening, so no subscriber could possibly see this
                // Status event live - eprintln! is the only way a headless
                // daemon's restore failures are visible anywhere.
                eprintln!("mtyx: {message}");
                self.emit(MuxEvent::Status(message));
            }
        }
        self.select_workspace(Some(active_workspace), None);
    }

    fn restore_workspace(
        self: &Arc<Self>,
        ws: &crate::persist::RestoreWorkspace<'_>,
    ) -> anyhow::Result<()> {
        // A workspace's very first tab is the one case restore can
        // recreate as a genuine remote reattach (via new_remote_workspace)
        // rather than a local shell - see the persist module doc for why
        // that's the only one.
        let first_tab_remote = ws.screens[0].panes[0].tabs[0].remote.clone();
        let first_surface = match first_tab_remote {
            Some(spec) => self.new_remote_workspace(spec, Some(ws.name.to_string()), None)?,
            None => self.new_workspace(Some(ws.name.to_string()), None)?,
        };
        let ws_id = self.with_state(|s| s.workspaces.last().unwrap().id);
        if let Some(color) = ws.color {
            self.set_workspace_color(ws_id, Some(color));
        }
        if let Some(icon) = ws.icon.clone() {
            self.set_workspace_icon(ws_id, Some(icon));
        }
        let (screen_id, pane_id) = self.with_state(|s| {
            let pane_id = s.pane_of(first_surface.id).unwrap();
            let (wi, si) = s.screen_of(pane_id).unwrap();
            (s.workspaces[wi].screens[si].id, pane_id)
        });
        self.restore_screen(&ws.screens[0], screen_id, pane_id)?;

        for screen in &ws.screens[1..] {
            let surface = self.new_screen(Some(ws_id), None)?;
            let (screen_id, pane_id) = self.with_state(|s| {
                let pane_id = s.pane_of(surface.id).unwrap();
                let (wi, si) = s.screen_of(pane_id).unwrap();
                (s.workspaces[wi].screens[si].id, pane_id)
            });
            self.restore_screen(screen, screen_id, pane_id)?;
        }

        self.select_screen(Some(ws.active_screen), None);
        Ok(())
    }

    fn restore_screen(
        self: &Arc<Self>,
        screen: &crate::persist::RestoreScreen<'_>,
        screen_id: ScreenId,
        initial_pane: PaneId,
    ) -> anyhow::Result<()> {
        // `initial_pane` always ends up mapped to snapshot index 0: both
        // capture (`Node::pane_ids`) and `replay_layout` walk the tree
        // "a" before "b", and `initial_pane` is `replay_layout`'s root,
        // which only ever recurses down the "a" side of itself.
        let pane_ids = self.replay_layout(&screen.layout, initial_pane)?;

        for (index, pane) in screen.panes.iter().enumerate() {
            let pane_id = pane_ids[&index];
            if let Some(name) = pane.name {
                self.rename_pane(pane_id, name.to_string());
            }
            match &pane.tabs[0].remote {
                // pane_id == initial_pane: restore_workspace already
                // recreated this exact tab via new_remote_workspace;
                // nothing left to do. Any other pane can't be remote -
                // split()/new_tab always spawn a local shell - so it's
                // already a (wrong) local shell; say so instead of
                // silently pretending the reconnect worked.
                Some(remote) if pane_id != initial_pane => {
                    self.emit(MuxEvent::Status(format!(
                        "restored a local shell instead of reattaching to remote session {} on {} (only a workspace's first pane can do that)",
                        remote.session_id, remote.host
                    )));
                }
                Some(_) => {}
                None => {
                    // Tab 0 is always the pane's auto-spawned tab (from
                    // new_workspace, new_screen, or split) - a live `cd`,
                    // since none of those spawn calls take a command/cwd
                    // override for it (see the persist module doc).
                    let tab0_surface = self.with_state(|s| s.panes[&pane_id].tabs[0]);
                    if let Some(surface) = self.surface(tab0_surface) {
                        self.apply_tab(&surface, &pane.tabs[0]);
                    }
                }
            }
            for tab in &pane.tabs[1..] {
                if let Some(remote) = &tab.remote {
                    self.emit(MuxEvent::Status(format!(
                        "restored a local shell instead of reattaching to remote session {} on {} (only a workspace's first pane can do that)",
                        remote.session_id, remote.host
                    )));
                }
                let surface = self.new_tab(Some(pane_id), tab.cwd.map(str::to_string), None)?;
                if let Some(name) = tab.name {
                    self.rename_surface(surface.id, name.to_string());
                }
            }
            self.select_tab(Some(pane_id), Some(pane.active_tab_index), None);
        }

        if let Some(name) = screen.name {
            self.rename_screen(screen_id, name.to_string());
        }
        self.focus_pane(pane_ids[&screen.active_pane_index]);
        Ok(())
    }

    /// Replays a [`crate::persist::RestoreLayout`] tree as `split()` calls
    /// starting from `root`, which represents the whole screen (a single
    /// unsplit pane) going in. Returns every leaf's pane id, keyed by its
    /// snapshot index.
    fn replay_layout(
        self: &Arc<Self>,
        layout: &crate::persist::RestoreLayout,
        root: PaneId,
    ) -> anyhow::Result<HashMap<usize, PaneId>> {
        match layout {
            crate::persist::RestoreLayout::Leaf(index) => Ok(HashMap::from([(*index, root)])),
            crate::persist::RestoreLayout::Split { dir, ratio, a, b } => {
                // `split()` keeps `root` as the "a" side and puts a new
                // pane on the "b" side - see `Node::split_leaf`.
                let new_surface = self.split(root, *dir, None)?;
                let new_pane = self.with_state(|s| s.pane_of(new_surface.id).unwrap());
                self.set_ratio(root, *dir, *ratio);
                let mut map = self.replay_layout(a, root)?;
                map.extend(self.replay_layout(b, new_pane)?);
                Ok(map)
            }
        }
    }

    /// Replay a validated layout document into this session under
    /// workspace name `name`, creating the workspace if it doesn't exist
    /// (issue #76 AC2). `name` — not the document's embedded name — is
    /// the workspace identity, so one fleet file can be booted under many
    /// names.
    ///
    /// Fails loudly (AC7): a pane whose tabs can't be recreated aborts
    /// the apply with `pane <index> (pane-id <id>): <error>`; a pane that
    /// couldn't even be created names its index. Already-created panes
    /// are left in place — fail-loud, not transactional (a `--replace`
    /// rollback is follow-up scope).
    pub fn apply_layout(
        self: &Arc<Self>,
        name: &str,
        doc: &crate::layout_doc::LayoutDocument,
    ) -> anyhow::Result<crate::layout_doc::ApplySummary> {
        doc.validate()?;
        if let Some(existing) =
            self.with_state(|s| s.workspaces.iter().find(|ws| ws.name == name).map(|ws| ws.id))
        {
            anyhow::bail!(
                "workspace {name:?} already exists (id {existing}); \
close it first or apply under a new name"
            );
        }
        let ws = &doc.workspace;
        // The workspace's very first tab is the only one apply can
        // recreate as a genuine remote reattach (via new_remote_workspace)
        // — the same inherited limitation as restore_session; every other
        // remote tab downgrades to a local shell with a loud Status.
        let first_tab = &ws.screens[0].panes[0].tabs[0];
        let first_surface = match first_tab {
            crate::layout_doc::LayoutTab::Remote {
                host,
                slot,
                session_id,
                local_binary_path,
                ..
            } => {
                let spec = crate::remote_pty::RemoteSpec {
                    host: host.clone(),
                    slot: slot.clone(),
                    session_id: session_id.clone(),
                    local_binary_path: local_binary_path.clone().into(),
                };
                self.new_remote_workspace(spec, Some(name.to_string()), None)?
            }
            tab => {
                let overrides = layout_tab_overrides(tab);
                self.new_workspace_with_overrides(Some(name.to_string()), None, overrides.as_ref())?
            }
        };
        let ws_id = self.with_state(|s| s.workspaces.last().unwrap().id);
        if let Some(color) = &ws.color {
            let rgb = crate::server::parse_workspace_color(color)
                .map_err(|e| anyhow::anyhow!("workspace {name:?}: {e}"))?;
            self.set_workspace_color(ws_id, Some(rgb));
        }
        if let Some(icon) = &ws.icon {
            let icon = crate::server::parse_workspace_icon(icon)
                .map_err(|e| anyhow::anyhow!("workspace {name:?}: {e}"))?;
            self.set_workspace_icon(ws_id, Some(icon));
        }

        // Screen 0 already exists around `first_surface`.
        let bootstrap_pane = self.with_state(|s| s.pane_of(first_surface.id).unwrap());
        let screen0_id = self.with_state(|s| {
            s.screen_of(bootstrap_pane).map(|(wi, si)| s.workspaces[wi].screens[si].id).unwrap()
        });
        self.apply_layout_screen(&ws.screens[0], screen0_id, bootstrap_pane, Some(bootstrap_pane))?;

        for screen in &ws.screens[1..] {
            // The new screen's first pane auto-spawns its tab 0: replay
            // the recorded argv/env when that tab is a pty.
            let first = &screen.panes[leftmost_layout_index(&screen.layout)].tabs[0];
            let overrides = layout_tab_overrides(first);
            let surface = self.new_screen_with_overrides(Some(ws_id), None, overrides.as_ref())?;
            let (screen_id, pane_id) = self.with_state(|s| {
                let pane_id = s.pane_of(surface.id).unwrap();
                let (wi, si) = s.screen_of(pane_id).unwrap();
                (s.workspaces[wi].screens[si].id, pane_id)
            });
            self.apply_layout_screen(screen, screen_id, pane_id, None)?;
        }
        self.select_screen(Some(ws.active_screen.min(ws.screens.len() - 1)), None);

        let (panes, surfaces) = self.with_state(|s| {
            let ws = s.workspaces.iter().find(|w| w.id == ws_id).unwrap();
            let panes: usize = ws
                .screens
                .iter()
                .map(|sc| {
                    let mut ids = Vec::new();
                    sc.root.pane_ids(&mut ids);
                    ids.len()
                })
                .sum();
            let surfaces = ws.screens.iter().map(|sc| screen_tabs(s, sc).len()).sum();
            (panes, surfaces)
        });
        Ok(crate::layout_doc::ApplySummary { workspace_id: ws_id, panes, surfaces })
    }

    /// Replay one screen of a layout document. `initial_pane` is the
    /// pane the screen is built around (the workspace bootstrap pane for
    /// screen 0, the new screen's first pane otherwise); `bootstrap_pane`
    /// marks the one pane whose tab 0 was created by `apply_layout`'s
    /// remote-first-tab handling.
    fn apply_layout_screen(
        self: &Arc<Self>,
        screen: &crate::layout_doc::LayoutScreen,
        screen_id: ScreenId,
        initial_pane: PaneId,
        bootstrap_pane: Option<PaneId>,
    ) -> anyhow::Result<()> {
        let pane_ids = self.replay_layout_doc(&screen.layout, screen, initial_pane)?;
        for (index, pane) in screen.panes.iter().enumerate() {
            let pane_id = pane_ids[&index];
            if let Some(name) = &pane.name {
                self.rename_pane(pane_id, name.clone());
            }
            // Tab 0 was auto-spawned while the tree structure was built:
            // pty tabs already run the recorded argv/env/cwd; a browser
            // tab replaces its placeholder shell; remote tabs downgrade
            // loudly everywhere except the workspace bootstrap pane.
            match &pane.tabs[0] {
                crate::layout_doc::LayoutTab::Pty { name, .. } => {
                    let tab0 = self.with_state(|s| s.panes[&pane_id].tabs[0]);
                    if let Some(surface) = self.surface(tab0) {
                        if let Some(name) = name {
                            self.rename_surface(surface.id, name.clone());
                        }
                    }
                }
                crate::layout_doc::LayoutTab::Browser { name, url } => {
                    let shell = self.with_state(|s| s.panes[&pane_id].tabs[0]);
                    let surface = self
                        .new_browser_tab(url.clone(), Some(pane_id), None)
                        .map_err(|e| anyhow::anyhow!("pane {index} (pane-id {pane_id}): {e}"))?;
                    if let Some(name) = name {
                        self.rename_surface(surface.id, name.clone());
                    }
                    self.close_surface(shell);
                }
                crate::layout_doc::LayoutTab::Remote { host, session_id, name, .. } => {
                    if Some(pane_id) != bootstrap_pane {
                        self.emit(MuxEvent::Status(format!(
                            "layout apply restored a local shell instead of \
reattaching to remote session {session_id} on {host} \
(only a workspace's first pane can do that)"
                        )));
                    }
                    let tab0 = self.with_state(|s| s.panes[&pane_id].tabs[0]);
                    if let Some(surface) = self.surface(tab0) {
                        if let Some(name) = name {
                            self.rename_surface(surface.id, name.clone());
                        }
                    }
                }
            }
            for tab in &pane.tabs[1..] {
                match tab {
                    tab @ crate::layout_doc::LayoutTab::Pty { name, .. } => {
                        let overrides = layout_tab_overrides(tab)
                            .expect("a pty layout tab always maps to spawn overrides");
                        let surface = self
                            .new_tab_with_overrides(
                                Some(pane_id),
                                None,
                                None,
                                Some(&overrides),
                                None,
                            )
                            .map_err(|e| {
                                anyhow::anyhow!("pane {index} (pane-id {pane_id}): {e}")
                            })?;
                        if let Some(name) = name {
                            self.rename_surface(surface.id, name.clone());
                        }
                    }
                    crate::layout_doc::LayoutTab::Browser { name, url } => {
                        let surface =
                            self.new_browser_tab(url.clone(), Some(pane_id), None).map_err(
                                |e| anyhow::anyhow!("pane {index} (pane-id {pane_id}): {e}"),
                            )?;
                        if let Some(name) = name {
                            self.rename_surface(surface.id, name.clone());
                        }
                    }
                    crate::layout_doc::LayoutTab::Remote { host, session_id, name, .. } => {
                        self.emit(MuxEvent::Status(format!(
                            "layout apply restored a local shell instead of \
reattaching to remote session {session_id} on {host} \
(only a workspace's first pane can do that)"
                        )));
                        let surface = self.new_tab(Some(pane_id), None, None).map_err(|e| {
                            anyhow::anyhow!("pane {index} (pane-id {pane_id}): {e}")
                        })?;
                        if let Some(name) = name {
                            self.rename_surface(surface.id, name.clone());
                        }
                    }
                }
            }
            self.select_tab(Some(pane_id), Some(pane.active_tab), None);
        }
        if let Some(name) = &screen.name {
            self.rename_screen(screen_id, name.clone());
        }
        self.focus_pane(pane_ids[&screen.active_pane]);
        Ok(())
    }

    /// The layout-doc counterpart of [`Self::replay_layout`]: replays the
    /// pane-index BSP as `split()` calls, spawning each new pane's tab 0
    /// with the recorded argv/env/cwd when it's a pty tab. Returns every
    /// leaf's pane id keyed by its document index.
    fn replay_layout_doc(
        self: &Arc<Self>,
        node: &crate::layout_doc::LayoutNode,
        screen: &crate::layout_doc::LayoutScreen,
        root: PaneId,
    ) -> anyhow::Result<HashMap<usize, PaneId>> {
        match node {
            crate::layout_doc::LayoutNode::Leaf { pane } => Ok(HashMap::from([(*pane, root)])),
            crate::layout_doc::LayoutNode::Split { dir, ratio, a, b } => {
                // `split()` keeps `root` on the "a" side and auto-spawns
                // the new pane's tab 0 (the agent-start primitive when
                // the recorded tab is a pty).
                let new_index = leftmost_layout_index(b);
                let tab0 = &screen.panes[new_index].tabs[0];
                let overrides = layout_tab_overrides(tab0);
                let new_surface = self
                    .split_with_overrides(root, (*dir).into(), None, overrides.as_ref(), None)
                    .map_err(|e| anyhow::anyhow!("pane {new_index}: {e} (pane not created)"))?;
                let new_pane = self.with_state(|s| s.pane_of(new_surface.id).unwrap());
                self.set_ratio(root, (*dir).into(), *ratio);
                let mut map = self.replay_layout_doc(a, screen, root)?;
                map.extend(self.replay_layout_doc(b, screen, new_pane)?);
                Ok(map)
            }
        }
    }

    /// Applies a restored tab's name and cwd to an already-spawned
    /// surface (see the module doc on why this is a live `cd`, not a
    /// spawn-time option).
    fn apply_tab(&self, surface: &Arc<Surface>, tab: &crate::persist::RestoreTab<'_>) {
        if let Some(name) = tab.name {
            self.rename_surface(surface.id, name.to_string());
        }
        if let Some(cwd) = tab.cwd {
            let _ = surface.write_bytes(
                format!("cd {} && clear\n", crate::persist::shell_quote(cwd)).as_bytes(),
            );
        }
    }

    /// Enables background session persistence: an internal subscriber
    /// debounces `TreeChanged` bursts and writes a snapshot to
    /// `platform::session_snapshot_path(session)`, which
    /// [`Self::restore_session`] reads back on next start. Opt-in (tests
    /// and one-shot CLI invocations should not write session files).
    pub fn enable_persistence(self: &Arc<Self>) {
        self.persistence_enabled.store(true, Ordering::Release);
        let events = self.subscribe();
        let mux = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("mux-persist".into()).spawn(move || loop {
            match events.recv() {
                Ok(MuxEvent::TreeChanged) => {}
                Ok(_) => continue,
                Err(_) => break,
            }
            // Debounce: let a burst of structural changes (e.g. setting
            // up several splits) settle before writing.
            std::thread::sleep(std::time::Duration::from_millis(300));
            while events.try_recv().is_ok() {}
            let Some(mux) = mux.upgrade() else { break };
            mux.write_snapshot();
        });
    }

    pub fn set_cell_pixel_size(&self, width_px: u16, height_px: u16) {
        let next = (width_px.max(1), height_px.max(1));
        {
            let mut cell = self.cell_pixels.lock().unwrap();
            if *cell == next {
                return;
            }
            *cell = next;
        }
        let surfaces = self.state.lock().unwrap().surfaces.values().cloned().collect::<Vec<_>>();
        for surface in surfaces {
            surface.set_cell_pixel_size(next.0, next.1);
        }
    }

    pub fn default_colors(&self) -> DefaultColors {
        *self.default_colors.lock().unwrap()
    }

    pub fn set_default_colors(&self, colors: DefaultColors) {
        *self.default_colors.lock().unwrap() = colors;
        let surfaces = self.state.lock().unwrap().surfaces.values().cloned().collect::<Vec<_>>();
        for surface in surfaces {
            surface.set_default_colors(colors);
            self.emit(MuxEvent::SurfaceOutput(surface.id));
        }
    }

    /// The resolved presentation chrome this server process loaded, if it
    /// has registered one. Returned to thin-client attaches by the
    /// `get-resolved-config` control-socket verb so a local `Overlay` can
    /// layer on top of the *server* config.
    pub fn resolved_chrome(&self) -> Option<serde_json::Value> {
        self.resolved_chrome.lock().unwrap().clone()
    }

    /// Register the resolved presentation chrome (theme/tabs/sidebar/keys)
    /// for this server, exposed via `get-resolved-config`. Called from the
    /// TUI's server entry point after `config::load()`.
    pub fn set_resolved_chrome(&self, value: serde_json::Value) {
        *self.resolved_chrome.lock().unwrap() = Some(value);
    }

    /// Resize a surface and broadcast the final clamped size when it
    /// actually changes.
    pub fn resize_surface(&self, id: SurfaceId, cols: u16, rows: u16) -> anyhow::Result<bool> {
        let Some(surface) = self.surface(id) else {
            anyhow::bail!("unknown surface {id}");
        };
        if !surface.resize(cols, rows) {
            return Ok(false);
        }
        let (cols, rows) = surface.size();
        self.emit(MuxEvent::SurfaceResized { surface: id, cols, rows });
        Ok(true)
    }

    /// Create a workspace with one screen holding one pane with one tab.
    /// Returns the tab's surface. `size` is the expected content size in
    /// cells, when the caller knows it (spawning at the final size avoids
    /// shell redraw artifacts).
    pub fn new_workspace(
        self: &Arc<Self>,
        name: Option<String>,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        self.new_workspace_with_overrides(name, size, None)
    }

    /// Issue #76: [`Self::new_workspace`] with explicit spawn overrides
    /// (the recorded first-tab argv/env/cwd of a layout apply, or a
    /// socket `new-workspace` carrying `command`/`env`).
    pub fn new_workspace_with_overrides(
        self: &Arc<Self>,
        name: Option<String>,
        size: Option<(u16, u16)>,
        overrides: Option<&SpawnOverrides>,
    ) -> anyhow::Result<Arc<Surface>> {
        let surface = self.spawn_surface(None, size, overrides)?;
        Ok(self.attach_new_workspace(surface, name))
    }

    /// Like [`Self::new_workspace`], but the tab is a `cmuxd-remote`
    /// session over SSH instead of a local shell (see `remote_pty.rs`).
    /// `remote.session_id` decides whether this creates a fresh remote
    /// shell or reattaches to one from a prior run - callers that want
    /// reconnect-after-restart pass back a session_id they've persisted
    /// (see `persist.rs`'s `TabSnapshot`); a fresh one starts a new shell.
    pub fn new_remote_workspace(
        self: &Arc<Self>,
        remote: crate::remote_pty::RemoteSpec,
        name: Option<String>,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        let surface = self.spawn_remote_surface(remote, size)?;
        Ok(self.attach_new_workspace(surface, name))
    }

    /// Wraps an already-spawned surface in a brand new workspace/screen/
    /// pane and makes it active. Shared by [`Self::new_workspace`] and
    /// [`Self::new_remote_workspace`], which only differ in how the
    /// surface itself gets spawned.
    fn attach_new_workspace(
        self: &Arc<Self>,
        surface: Arc<Surface>,
        name: Option<String>,
    ) -> Arc<Surface> {
        let (pane_id, pane) = self.make_pane(surface.id);
        let screen_id = self.next_id();
        let ws_id = self.next_id();
        {
            let mut state = self.state.lock().unwrap();
            let name = name.unwrap_or_else(|| format!("{}", state.workspaces.len() + 1));
            state.panes.insert(pane_id, pane);
            state.workspaces.push(Workspace {
                id: ws_id,
                name,
                screens: vec![Screen {
                    id: screen_id,
                    name: None,
                    root: Node::Leaf(pane_id),
                    active_pane: pane_id,
                }],
                active_screen: 0,
                color: None,
                icon: None,
            });
            state.active_workspace = state.workspaces.len() - 1;
        }
        self.emit(MuxEvent::TreeChanged);
        self.reap_if_dead(&surface);
        surface
    }

    /// Create a screen in a workspace (default: the active one) with one
    /// pane/tab, and make it active. Returns the tab's surface.
    pub fn new_screen(
        self: &Arc<Self>,
        workspace: Option<WorkspaceId>,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        self.new_screen_with_overrides(workspace, size, None)
    }

    /// Issue #76: [`Self::new_screen`] with explicit spawn overrides for
    /// the new screen's first tab.
    pub fn new_screen_with_overrides(
        self: &Arc<Self>,
        workspace: Option<WorkspaceId>,
        size: Option<(u16, u16)>,
        overrides: Option<&SpawnOverrides>,
    ) -> anyhow::Result<Arc<Surface>> {
        // Validate the target before spawning a child.
        {
            let state = self.state.lock().unwrap();
            match workspace {
                Some(id) if !state.workspaces.iter().any(|w| w.id == id) => {
                    anyhow::bail!("unknown workspace {id}")
                }
                None if state.workspaces.is_empty() => {
                    drop(state);
                    return self.new_workspace_with_overrides(None, size, overrides);
                }
                _ => {}
            }
        }
        let surface = self.spawn_surface(None, size, overrides)?;
        let (pane_id, pane) = self.make_pane(surface.id);
        let screen_id = self.next_id();
        let attached = {
            let mut state = self.state.lock().unwrap();
            let active = state.active_workspace;
            let ws = match workspace {
                Some(id) => state.workspaces.iter_mut().find(|w| w.id == id),
                None => state.workspaces.get_mut(active),
            };
            match ws {
                Some(ws) => {
                    ws.screens.push(Screen {
                        id: screen_id,
                        name: None,
                        root: Node::Leaf(pane_id),
                        active_pane: pane_id,
                    });
                    ws.active_screen = ws.screens.len() - 1;
                    state.panes.insert(pane_id, pane);
                    true
                }
                None => {
                    state.surfaces.remove(&surface.id);
                    false
                }
            }
        };
        if !attached {
            surface.kill();
            anyhow::bail!("workspace disappeared while creating screen");
        }
        self.emit(MuxEvent::TreeChanged);
        self.reap_if_dead(&surface);
        Ok(surface)
    }

    /// Create a tab in a pane (default: the active pane of the active
    /// screen). When the session has no workspaces yet (headless before
    /// any command), a workspace is created around the new tab.
    pub fn new_tab(
        self: &Arc<Self>,
        pane: Option<PaneId>,
        cwd: Option<String>,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        // Issue #76 override path, no worktree.
        self.new_tab_with_overrides(pane, cwd, size, None, None)
    }

    /// Issue #76: [`Self::new_tab`] with explicit spawn overrides — the
    /// agent-start primitive (`mtyx new-tab --exec -- <argv>` on the CLI,
    /// `command`/`env` fields on the socket command). The optional
    /// `worktree` is appended to the pane's `Pane.worktrees` registry on
    /// attach (issue #77 AC4); passing both `overrides` and a `worktree`
    /// is the `new-tab --exec --branch` composition.
    pub fn new_tab_with_overrides(
        self: &Arc<Self>,
        pane: Option<PaneId>,
        cwd: Option<String>,
        size: Option<(u16, u16)>,
        overrides: Option<&SpawnOverrides>,
        worktree: Option<&crate::worktree::WorktreeRecord>,
    ) -> anyhow::Result<Arc<Surface>> {
        // Resolve and validate the target before spawning a child.
        let target = {
            let state = self.state.lock().unwrap();
            match pane {
                Some(id) => {
                    if !state.panes.contains_key(&id) {
                        anyhow::bail!("unknown pane {id}");
                    }
                    Some(id)
                }
                None => state.active_pane(),
            }
        };
        let Some(target) = target else {
            // Empty session: still spawn inside the worktree if one was
            // requested, then attach the record to the new pane.
            if let Some(record) = worktree {
                let surface = self.spawn_surface(Some(record.path.clone()), size, overrides)?;
                let attached = self.attach_new_workspace(surface, None);
                let pane = self.with_state(|s| s.pane_of(attached.id).unwrap());
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(p) = state.panes.get_mut(&pane) {
                        p.worktrees.push(record.clone());
                    }
                }
                self.emit(MuxEvent::TreeChanged);
                self.reap_if_dead(&attached);
                return Ok(attached);
            }
            return self.new_workspace_with_overrides(None, size, overrides);
        };

        let cwd = cwd.or_else(|| self.pane_cwd(target));
        // A sibling tab renders at the size the pane already has.
        let size = size.or_else(|| self.pane_size(target));
        let surface = self.spawn_surface(cwd, size, overrides)?;
        let active_at = self.next_active_at();
        let attached = {
            let mut state = self.state.lock().unwrap();
            match state.panes.get_mut(&target) {
                Some(pane) => {
                    pane.tabs.push(surface.id);
                    pane.active_tab = pane.tabs.len() - 1;
                    pane.active_at = active_at;
                    if let Some(record) = worktree {
                        pane.worktrees.push(record.clone());
                    }
                    true
                }
                None => {
                    // Pane disappeared between validation and attach.
                    state.surfaces.remove(&surface.id);
                    false
                }
            }
        };
        if !attached {
            surface.kill();
            anyhow::bail!("pane disappeared while creating tab");
        }
        self.emit(MuxEvent::TreeChanged);
        self.reap_if_dead(&surface);
        Ok(surface)
    }

    /// `new-tab --branch <name>` (issue #77 AC4): create the worktree
    /// BEFORE the surface spawns and pass its path as the spawn cwd, so
    /// the pane starts inside the worktree — the agent launched into the
    /// pane by the existing send flow lands on the branch by
    /// construction. The record attaches to the pane owning the new tab.
    pub fn new_tab_with_worktree(
        self: &Arc<Self>,
        pane: Option<PaneId>,
        cwd: Option<String>,
        size: Option<(u16, u16)>,
        branch: &str,
        label: Option<String>,
    ) -> anyhow::Result<(Arc<Surface>, crate::worktree::WorktreeRecord)> {
        // Repository to branch from: an explicit cwd wins, else the
        // target pane's working directory (the active pane, matching
        // new_tab's None semantics). With neither there is nothing to
        // resolve a repository from.
        let target = {
            let state = self.state.lock().unwrap();
            match pane {
                Some(id) => {
                    if !state.panes.contains_key(&id) {
                        anyhow::bail!("unknown pane {id}");
                    }
                    Some(id)
                }
                None => state.active_pane(),
            }
        };
        let start = cwd.clone().or_else(|| target.and_then(|t| self.pane_surface_cwd(t)));
        let Some(start) = start else {
            anyhow::bail!("cannot resolve a repository: no --cwd and no pane working directory");
        };
        let record = self.create_worktree(&start, branch, label)?;
        // Force the spawn cwd to the worktree path. Future callers can
        // build their own overrides for new-tab --exec --branch
        // composition; this wrapper keeps the simple --branch case to
        // the canonical AC4 layout.
        let mut overrides = SpawnOverrides::default();
        overrides.cwd = Some(record.path.clone());
        let surface =
            self.new_tab_with_overrides(pane, cwd, size, Some(&overrides), Some(&record))?;
        Ok((surface, record))
    }

    /// Create a browser tab in a pane (default: the active pane). When
    /// the session has no workspaces yet, a workspace is created around
    /// the browser tab.
    pub fn new_browser_tab(
        self: &Arc<Self>,
        url: String,
        pane: Option<PaneId>,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        let target = {
            let state = self.state.lock().unwrap();
            match pane {
                Some(id) => {
                    if !state.panes.contains_key(&id) {
                        anyhow::bail!("unknown pane {id}");
                    }
                    Some(id)
                }
                None => state.active_pane(),
            }
        };
        let Some(target) = target else {
            let surface = self.spawn_browser_surface(url, size);
            let (pane_id, pane) = self.make_pane(surface.id);
            let screen_id = self.next_id();
            let ws_id = self.next_id();
            {
                let mut state = self.state.lock().unwrap();
                let name = format!("{}", state.workspaces.len() + 1);
                state.panes.insert(pane_id, pane);
                state.workspaces.push(Workspace {
                    id: ws_id,
                    name,
                    screens: vec![Screen {
                        id: screen_id,
                        name: None,
                        root: Node::Leaf(pane_id),
                        active_pane: pane_id,
                    }],
                    active_screen: 0,
                    color: None,
                    icon: None,
                });
                state.active_workspace = state.workspaces.len() - 1;
            }
            self.emit(MuxEvent::TreeChanged);
            self.reap_if_dead(&surface);
            return Ok(surface);
        };

        let size = size.or_else(|| self.pane_size(target));
        let surface = self.spawn_browser_surface(url, size);
        let active_at = self.next_active_at();
        let attached = {
            let mut state = self.state.lock().unwrap();
            match state.panes.get_mut(&target) {
                Some(pane) => {
                    pane.tabs.push(surface.id);
                    pane.active_tab = pane.tabs.len() - 1;
                    pane.active_at = active_at;
                    true
                }
                None => {
                    state.surfaces.remove(&surface.id);
                    false
                }
            }
        };
        if !attached {
            surface.kill();
            anyhow::bail!("pane disappeared while creating browser tab");
        }
        self.emit(MuxEvent::TreeChanged);
        self.reap_if_dead(&surface);
        Ok(surface)
    }

    pub fn adopt_browser_target(
        self: &Arc<Self>,
        opener_surface: SurfaceId,
        target_id: String,
        url: String,
        runtime: Arc<BrowserRuntime>,
    ) -> bool {
        let (pane_id, size) = {
            let state = self.state.lock().unwrap();
            let Some(pane_id) = state.pane_of(opener_surface) else {
                return false;
            };
            let size = state.surfaces.get(&opener_surface).map(|surface| surface.size());
            (pane_id, size)
        };
        let id = self.next_id();
        let mut opts = self.surface_options.clone();
        self.refresh_socket_env(&mut opts);
        let size = size.unwrap_or((opts.cols, opts.rows));
        let cell_pixels = *self.cell_pixels.lock().unwrap();
        let surface = browser::new_surface(id, url.clone(), size, cell_pixels, &opts);
        let active_at = self.next_active_at();
        let attached = {
            let mut state = self.state.lock().unwrap();
            let Some(pane) = state.panes.get_mut(&pane_id) else {
                return false;
            };
            pane.tabs.push(surface.id);
            pane.active_tab = pane.tabs.len() - 1;
            pane.active_at = active_at;
            state.surfaces.insert(surface.id, surface.clone());
            true
        };
        if !attached {
            surface.kill();
            return false;
        }
        self.emit(MuxEvent::TreeChanged);
        self.start_browser_bootstrap(
            surface,
            BrowserBootstrap::ExistingTarget { target_id, url },
            Some(runtime),
        );
        true
    }

    /// Working directory of a pane's active surface, if reported.
    fn pane_cwd(&self, pane: PaneId) -> Option<String> {
        let surface = {
            let state = self.state.lock().unwrap();
            let active = state.panes.get(&pane)?.active_surface()?;
            state.surfaces.get(&active).cloned()
        };
        surface.and_then(|s| s.pwd())
    }

    /// Best-known working directory of a pane's active tab: the shell's
    /// live OSC 7 report when available, otherwise the directory the
    /// surface was spawned in. Unlike [`Self::pane_cwd`] (OSC 7 only),
    /// this is what worktree resolution needs — a shell that never
    /// reports OSC 7 (a bare `cat`, a fresh non-reporting shell) still
    /// resolves its spawn repository (issue #77).
    fn pane_surface_cwd(&self, pane: PaneId) -> Option<String> {
        let surface = {
            let state = self.state.lock().unwrap();
            let active = state.panes.get(&pane)?.active_surface()?;
            state.surfaces.get(&active).cloned()
        };
        surface.and_then(|s| s.cwd())
    }

    /// Current cell size of a pane's active surface.
    fn pane_size(&self, pane: PaneId) -> Option<(u16, u16)> {
        let state = self.state.lock().unwrap();
        let active = state.panes.get(&pane)?.active_surface()?;
        state.surfaces.get(&active).map(|s| s.size())
    }

    /// Split the screen containing `target`, putting a new single-tab
    /// pane after it. Returns the new pane's surface. `size` is the
    /// expected content size of the new pane, when the caller knows it.
    pub fn split(
        self: &Arc<Self>,
        target: PaneId,
        dir: SplitDir,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<Arc<Surface>> {
        // Issue #76 override path, no worktree.
        self.split_with_overrides(target, dir, size, None, None)
    }

    /// Issue #76: [`Self::split`] with explicit spawn overrides for the
    /// new pane's first tab. The optional `worktree` is recorded on the
    /// new pane so a subsequent `pane worktree list` sees it
    /// (issue #77 AC4, the `split --exec --branch` composition).
    pub fn split_with_overrides(
        self: &Arc<Self>,
        target: PaneId,
        dir: SplitDir,
        size: Option<(u16, u16)>,
        overrides: Option<&SpawnOverrides>,
        worktree: Option<&crate::worktree::WorktreeRecord>,
    ) -> anyhow::Result<Arc<Surface>> {
        let cwd = self.pane_cwd(target);
        // Halve the split axis as a fallback estimate; the frontend sends
        // the exact size on its next layout pass.
        let size = size.or_else(|| {
            self.pane_size(target).map(|(cols, rows)| match dir {
                SplitDir::Right => ((cols.saturating_sub(1) / 2).max(1), rows),
                SplitDir::Down => (cols, (rows.saturating_sub(1) / 2).max(1)),
            })
        });
        let surface = self.spawn_surface(cwd, size, overrides)?;
        let pane_id = self.next_id();
        let active_at = self.next_active_at();
        let mut done = false;
        {
            let mut state = self.state.lock().unwrap();
            'outer: for ws in state.workspaces.iter_mut() {
                for screen in ws.screens.iter_mut() {
                    if screen.root.split_leaf(target, dir, pane_id) {
                        screen.active_pane = pane_id;
                        done = true;
                        break 'outer;
                    }
                }
            }
            if done {
                let mut pane = Pane {
                    id: pane_id,
                    name: None,
                    tabs: vec![surface.id],
                    active_tab: 0,
                    active_at,
                    worktrees: Vec::new(),
                };
                if let Some(record) = worktree {
                    pane.worktrees.push(record.clone());
                }
                state.panes.insert(pane_id, pane);
            } else {
                state.surfaces.remove(&surface.id);
            }
        }
        if !done {
            surface.kill();
            anyhow::bail!("pane {target} not found");
        }
        self.emit(MuxEvent::TreeChanged);
        self.reap_if_dead(&surface);
        Ok(surface)
    }

    /// `split --branch <name>` (issue #77 AC4): create the worktree
    /// before spawning, so the NEW pane starts inside it and owns the
    /// record.
    pub fn split_with_worktree(
        self: &Arc<Self>,
        target: PaneId,
        dir: SplitDir,
        size: Option<(u16, u16)>,
        branch: &str,
        label: Option<String>,
    ) -> anyhow::Result<(Arc<Surface>, crate::worktree::WorktreeRecord)> {
        let start = self.pane_surface_cwd(target).ok_or_else(|| {
            anyhow::anyhow!("pane {target} has no working directory to resolve a repository from")
        })?;
        let record = self.create_worktree(&start, branch, label)?;
        let mut overrides = SpawnOverrides::default();
        overrides.cwd = Some(record.path.clone());
        let surface =
            self.split_with_overrides(target, dir, size, Some(&overrides), Some(&record))?;
        Ok((surface, record))
    }

    fn split_impl(
        self: &Arc<Self>,
        target: PaneId,
        dir: SplitDir,
        size: Option<(u16, u16)>,
        spawn_cwd: Option<String>,
        overrides: Option<&SpawnOverrides>,
        worktree: Option<&crate::worktree::WorktreeRecord>,
    ) -> anyhow::Result<(Arc<Surface>, Option<crate::worktree::WorktreeRecord>)> {
        let cwd = spawn_cwd.or_else(|| self.pane_cwd(target));
        // Halve the split axis as a fallback estimate; the frontend sends
        // the exact size on its next layout pass.
        let size = size.or_else(|| {
            self.pane_size(target).map(|(cols, rows)| match dir {
                SplitDir::Right => ((cols.saturating_sub(1) / 2).max(1), rows),
                SplitDir::Down => (cols, (rows.saturating_sub(1) / 2).max(1)),
            })
        });
        let surface = self.spawn_surface(cwd, size, overrides)?;
        let pane_id = self.next_id();
        let active_at = self.next_active_at();
        let mut done = false;
        {
            let mut state = self.state.lock().unwrap();
            'outer: for ws in state.workspaces.iter_mut() {
                for screen in ws.screens.iter_mut() {
                    if screen.root.split_leaf(target, dir, pane_id) {
                        screen.active_pane = pane_id;
                        done = true;
                        break 'outer;
                    }
                }
            }
            if done {
                let mut pane = Pane {
                    id: pane_id,
                    name: None,
                    tabs: vec![surface.id],
                    active_tab: 0,
                    active_at,
                    worktrees: Vec::new(),
                };
                if let Some(record) = worktree {
                    pane.worktrees.push(record.clone());
                }
                state.panes.insert(pane_id, pane);
            } else {
                state.surfaces.remove(&surface.id);
            }
        }
        if !done {
            surface.kill();
            anyhow::bail!("pane {target} not found");
        }
        self.emit(MuxEvent::TreeChanged);
        self.reap_if_dead(&surface);
        Ok((surface, worktree.cloned()))
    }

    /// Close one tab. When it was the pane's last tab, the pane collapses
    /// out of its split tree (and emptied screens/workspaces are removed).
    pub fn close_surface(&self, target: SurfaceId) {
        let (removed, empty) = {
            let mut state = self.state.lock().unwrap();
            (remove_surface(&mut state, target), state.workspaces.is_empty())
        };
        if let Some(surface) = removed {
            surface.kill();
            self.emit(MuxEvent::TreeChanged);
        }
        if empty {
            self.emit(MuxEvent::Empty);
        }
    }

    /// Close every surface in `tabs` (helper for pane/screen/workspace
    /// close). Emits events outside the lock.
    fn close_surfaces(&self, tabs: Vec<SurfaceId>) {
        let (removed, empty) = {
            let mut state = self.state.lock().unwrap();
            let mut removed = Vec::new();
            for surface in tabs {
                if let Some(surface) = remove_surface(&mut state, surface) {
                    removed.push(surface);
                }
            }
            (removed, state.workspaces.is_empty())
        };
        if !removed.is_empty() {
            for surface in removed {
                surface.kill();
            }
            self.emit(MuxEvent::TreeChanged);
        }
        if empty {
            self.emit(MuxEvent::Empty);
        }
    }

    /// Close a pane and every tab in it.
    pub fn close_pane(&self, target: PaneId) {
        let tabs = {
            let state = self.state.lock().unwrap();
            match state.panes.get(&target) {
                Some(pane) => pane.tabs.clone(),
                None => return,
            }
        };
        self.close_surfaces(tabs);
    }

    /// Close a screen and every pane/tab in it.
    pub fn close_screen(&self, target: ScreenId) -> bool {
        let tabs = {
            let state = self.state.lock().unwrap();
            let Some(screen) =
                state.workspaces.iter().flat_map(|ws| ws.screens.iter()).find(|s| s.id == target)
            else {
                return false;
            };
            screen_tabs(&state, screen)
        };
        self.close_surfaces(tabs);
        true
    }

    /// Close a workspace and every screen/pane/tab in it.
    pub fn close_workspace(&self, target: WorkspaceId) -> bool {
        let tabs = {
            let state = self.state.lock().unwrap();
            let Some(ws) = state.workspaces.iter().find(|ws| ws.id == target) else {
                return false;
            };
            ws.screens.iter().flat_map(|screen| screen_tabs(&state, screen)).collect::<Vec<_>>()
        };
        self.close_surfaces(tabs);
        true
    }

    /// Worktree-child workspaces of `target` (issue #100): every other
    /// workspace holding a pane that lives under a worktree recorded by
    /// one of `target`'s panes. A pane counts as living in the worktree
    /// when its own `Pane::worktrees` record or one of its tabs'
    /// best-known cwd (the shell's OSC 7 report, else the spawn cwd)
    /// resolves under an owned worktree path. The heuristic is
    /// deliberately simple: detection only sees worktrees this session
    /// created via the `pane-worktree-*` / `--branch` flows, and paths
    /// compare lexically with no filesystem access - a same-named
    /// sibling directory (`repo.feat-x2`) never matches `repo.feat-x`.
    pub fn worktree_children(&self, target: WorkspaceId) -> Vec<WorktreeChild> {
        let state = self.state.lock().unwrap();
        let Some(parent) = state.workspaces.iter().find(|ws| ws.id == target) else {
            return Vec::new();
        };
        // Worktrees the parent's panes created/own (absolute, normalised).
        let owned: Vec<&crate::worktree::WorktreeRecord> = workspace_pane_ids(parent)
            .iter()
            .filter_map(|id| state.panes.get(id))
            .flat_map(|pane| pane.worktrees.iter())
            .collect();
        if owned.is_empty() {
            return Vec::new();
        }
        let mut children = Vec::new();
        for ws in state.workspaces.iter().filter(|ws| ws.id != target) {
            let mut matched: Option<&crate::worktree::WorktreeRecord> = None;
            let mut running_agent = false;
            for pane_id in workspace_pane_ids(ws) {
                let Some(pane) = state.panes.get(&pane_id) else { continue };
                // Recorded association: one of the pane's own worktrees
                // is a parent-pane worktree.
                if matched.is_none() {
                    matched = pane.worktrees.iter().find_map(|record| {
                        owned.iter().copied().find(|owner| path_is_under(&record.path, &owner.path))
                    });
                }
                // cwd heuristic: a tab's best-known working directory
                // lives inside a parent-pane worktree.
                for tab in &pane.tabs {
                    let Some(surface) = state.surfaces.get(tab) else { continue };
                    if matched.is_none() {
                        if let Some(cwd) = surface.cwd() {
                            matched = owned
                                .iter()
                                .copied()
                                .find(|owner| path_is_under(&cwd, &owner.path));
                        }
                    }
                    if !running_agent {
                        running_agent = surface.agent_report().is_some_and(|report| {
                            matches!(report.state, AgentState::Working | AgentState::Blocked)
                        });
                    }
                }
            }
            if let Some(owner) = matched {
                children.push(WorktreeChild {
                    workspace: ws.id,
                    name: ws.name.clone(),
                    worktree_path: owner.path.clone(),
                    worktree_branch: Some(owner.branch.clone()),
                    running_agent,
                });
            }
        }
        children
    }

    /// Close `target` with the worktree-child guard (issue #100):
    /// without `group`, the close proceeds but worktree-child
    /// workspaces survive and are reported, so they are never silently
    /// orphaned; with `group`, they close together with the parent.
    /// Returns `None` when `target` does not exist.
    pub fn close_workspace_reported(
        &self,
        target: WorkspaceId,
        group: bool,
    ) -> Option<CloseWorkspaceReport> {
        // Detect survivors BEFORE closing: detection walks the parent's
        // own panes, which the close is about to remove.
        let survivors = self.worktree_children(target);
        let target_name = self.with_state(|s| {
            s.workspaces.iter().find(|ws| ws.id == target).map(|ws| ws.name.clone())
        })?;
        if !self.close_workspace(target) {
            return None;
        }
        let mut report = CloseWorkspaceReport { closed: vec![(target, target_name)], survivors };
        if group {
            for child in std::mem::take(&mut report.survivors) {
                let name = child.name.clone();
                if self.close_workspace(child.workspace) {
                    report.closed.push((child.workspace, name));
                }
                // A child that vanished concurrently (another client
                // closed it first) is neither closed by us nor a
                // survivor; it is simply gone.
            }
        }
        Some(report)
    }

    pub fn rename_workspace(&self, target: WorkspaceId, name: String) -> bool {
        let renamed = {
            let mut state = self.state.lock().unwrap();
            match state.workspaces.iter_mut().find(|ws| ws.id == target) {
                Some(ws) => {
                    ws.name = name;
                    true
                }
                None => false,
            }
        };
        if renamed {
            self.emit(MuxEvent::TreeChanged);
        }
        renamed
    }

    pub fn set_workspace_color(&self, target: WorkspaceId, color: Option<Rgb>) -> bool {
        let changed = {
            let mut state = self.state.lock().unwrap();
            match state.workspaces.iter_mut().find(|ws| ws.id == target) {
                Some(ws) => {
                    ws.color = color;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.emit(MuxEvent::TreeChanged);
        }
        changed
    }

    pub fn set_workspace_icon(&self, target: WorkspaceId, icon: Option<IconName>) -> bool {
        let changed = {
            let mut state = self.state.lock().unwrap();
            match state.workspaces.iter_mut().find(|ws| ws.id == target) {
                Some(ws) => {
                    ws.icon = icon;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.emit(MuxEvent::TreeChanged);
        }
        changed
    }

    /// Emits `MuxEvent::Flash` for a workspace, for frontends to render a
    /// transient visual pulse (e.g. a manual "look here" signal). Doesn't
    /// mutate state, so no `TreeChanged` follows.
    pub fn trigger_flash(&self, workspace: WorkspaceId, surface: Option<SurfaceId>) -> bool {
        let exists = {
            let state = self.state.lock().unwrap();
            state.workspaces.iter().any(|ws| ws.id == workspace)
        };
        if exists {
            self.emit(MuxEvent::Flash { workspace, surface });
        }
        exists
    }

    /// Set a pane's user-visible name. An empty name clears it (the pane
    /// falls back to its active tab's title).
    pub fn rename_pane(&self, target: PaneId, name: String) -> bool {
        let renamed = {
            let mut state = self.state.lock().unwrap();
            match state.panes.get_mut(&target) {
                Some(pane) => {
                    pane.name = (!name.is_empty()).then_some(name);
                    true
                }
                None => false,
            }
        };
        if renamed {
            self.emit(MuxEvent::TreeChanged);
        }
        renamed
    }

    /// Set a tab's user-visible name. An empty name clears it (the tab
    /// falls back to its process title/number label).
    pub fn rename_surface(&self, target: SurfaceId, name: String) -> bool {
        let surface = self.state.lock().unwrap().surfaces.get(&target).cloned();
        let Some(surface) = surface else { return false };
        surface.set_name((!name.is_empty()).then_some(name));
        self.emit(MuxEvent::TreeChanged);
        true
    }

    /// Reports agent state for a surface (see `spec/commands.md`). Returns
    /// the report now in effect, which may be unchanged from before if a
    /// lower-authority source tried to override a hook report.
    pub fn report_agent(
        &self,
        target: SurfaceId,
        state: AgentState,
        source: AgentStateSource,
        session: Option<String>,
        agent: Option<String>,
        message: Option<String>,
    ) -> Option<AgentReport> {
        let surface = self.state.lock().unwrap().surfaces.get(&target).cloned()?;
        let previous = surface.agent_report().map(|r| r.state);
        let (report, applied) = surface.set_agent_report(state, source, session, agent, message)?;
        if applied {
            self.emit(MuxEvent::AgentStateChanged {
                surface: target,
                previous,
                report: report.clone(),
            });
        }
        Some(report)
    }

    /// Resolve an agent-verb target (issue #75): an exact match on a
    /// surface's reported agent name, else a numeric surface id, else an
    /// error. Duplicate names are ambiguous and rejected (disambiguate
    /// with surface ids). Names live as long as the pane has a report.
    pub fn resolve_agent_target(&self, target: &str) -> anyhow::Result<SurfaceId> {
        let state = self.state.lock().unwrap();
        let named: Vec<SurfaceId> = state
            .surfaces
            .iter()
            .filter_map(|(id, s)| {
                let name = s.agent_report().and_then(|r| r.agent)?;
                (name == target).then_some(*id)
            })
            .collect();
        match named.as_slice() {
            [id] => Ok(*id),
            _ if named.len() > 1 => anyhow::bail!(
                "ambiguous agent name {target:?} (surfaces {}); address one by surface id",
                named.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ")
            ),
            _ => match target.parse::<SurfaceId>() {
                Ok(id) if state.surfaces.contains_key(&id) => Ok(id),
                Ok(id) => anyhow::bail!("unknown agent {target:?} (surface {id} does not exist)"),
                Err(_) => anyhow::bail!("unknown agent {target:?}"),
            },
        }
    }

    /// Known agent-status records, optionally filtered by surface or state.
    pub fn list_agents(
        &self,
        surface: Option<SurfaceId>,
        state: Option<AgentState>,
    ) -> Vec<(SurfaceId, AgentReport)> {
        self.state
            .lock()
            .unwrap()
            .surfaces
            .iter()
            .filter(|(id, _)| surface.map_or(true, |filter| filter == **id))
            .filter_map(|(id, s)| s.agent_report().map(|report| (*id, report)))
            .filter(|(_, report)| state.map_or(true, |filter| filter == report.state))
            .collect()
    }

    /// Ambient agent-detection settings (issue #78 AC7).
    pub fn set_agent_detection(&self, settings: DetectionSettings) {
        *self.agent_detection.lock().unwrap() = settings;
    }

    pub fn agent_detection(&self) -> DetectionSettings {
        *self.agent_detection.lock().unwrap()
    }

    /// Add a user pattern on top of the bundled registry. Validates the
    /// pattern and rejects exact duplicates.
    pub fn agent_pattern_add(&self, pattern: AgentPattern) -> anyhow::Result<()> {
        pattern.validate().map_err(anyhow::Error::msg)?;
        let mut custom = self.custom_agent_patterns.lock().unwrap();
        if custom.iter().any(|p| p == &pattern) {
            anyhow::bail!(
                "pattern {:?} for agent {:?} is already registered",
                pattern.pattern,
                pattern.name
            );
        }
        custom.push(pattern);
        Ok(())
    }

    /// Remove every user-added pattern named `name`. Bundled patterns
    /// cannot be removed.
    pub fn agent_pattern_remove(&self, name: &str) -> anyhow::Result<()> {
        let mut custom = self.custom_agent_patterns.lock().unwrap();
        let before = custom.len();
        custom.retain(|p| p.name != name);
        if custom.len() == before {
            anyhow::bail!(
                "no user pattern for agent {name:?} (bundled patterns cannot be removed)"
            );
        }
        Ok(())
    }

    /// The effective pattern registry: bundled patterns plus user adds.
    pub fn agent_pattern_list(&self) -> anyhow::Result<Vec<AgentPattern>> {
        let mut all = crate::agent_detect::bundled_patterns()?;
        all.extend(self.custom_agent_patterns.lock().unwrap().iter().cloned());
        Ok(all)
    }

    /// Run ambient detection on one surface (issue #78 AC1): collect
    /// process + screen evidence, score it against the registry, cache
    /// the result on the surface, and emit `TreeChanged` so frontends
    /// re-snapshot (`pane_json` exposes `agent_name`).
    pub fn detect_agent(&self, surface: SurfaceId) -> anyhow::Result<Detection> {
        let settings = self.agent_detection();
        if !settings.enabled {
            anyhow::bail!("agent detection disabled by configuration");
        }
        let surface =
            self.surface(surface).ok_or_else(|| anyhow::anyhow!("unknown surface {surface}"))?;
        let detection = self.detect_on_surface(&surface)?;
        // Issue #96: publish a screen-derived (`Detected`-tier) state only
        // when the classifier found an unambiguous `blocked`/`working`
        // marker. The authority rules in `Surface::set_agent_report`
        // already reject a `Detected` report while a `Socket`/`Hook`
        // report is current, so this can never override a self-report;
        // and the `blocked`/`working`-only gate keeps it from
        // downgrading one to `idle`/`unknown` either.
        if matches!(detection.screen_state, crate::AgentState::Blocked | crate::AgentState::Working)
        {
            self.report_agent(
                surface.id,
                detection.screen_state,
                crate::AgentStateSource::Detected,
                None,
                None,
                None,
            );
        }
        surface.set_detected_agent(detection.clone());
        self.emit(MuxEvent::TreeChanged);
        Ok(detection)
    }

    /// Run detection on every live surface (issue #78 AC2).
    pub fn detect_all_agents(&self) -> anyhow::Result<Vec<(SurfaceId, Detection)>> {
        if !self.agent_detection().enabled {
            anyhow::bail!("agent detection disabled by configuration");
        }
        let ids: Vec<SurfaceId> = self.with_state(|s| s.surfaces.keys().copied().collect());
        ids.into_iter().map(|id| Ok((id, self.detect_agent(id)?))).collect()
    }

    /// Evidence collection + scoring for one surface, shared by
    /// `detect-agent` and `detect-agents`.
    fn detect_on_surface(&self, surface: &Arc<Surface>) -> anyhow::Result<Detection> {
        let settings = self.agent_detection();
        let patterns = self.agent_pattern_list()?;
        if surface.kind() != crate::SurfaceKind::Pty {
            return Ok(Detection::unknown(
                "browser surface: no PTY process tree or screen to scan",
            ));
        }
        // A cmuxd-remote surface's local child is the ssh transport, not
        // the pane's real processes — skip process evidence there; the
        // screen half (the VT is local) may still match.
        let remote = surface.remote_spec().is_some();
        let process = if remote {
            Vec::new()
        } else {
            crate::agent_detect::collect_process_evidence(surface.child_pid())
        };
        let screen = surface.try_with_terminal(|t| t.plain_text())??;
        let mut detection =
            crate::agent_detect::detect(&process, &screen, &patterns, settings.min_confidence);
        // Issue #96: classify the screen text into a lifecycle state,
        // using the agent detection just resolved (or `"unknown"` /
        // `"generic"` when nothing matched). `screen_state` is
        // informational for the caller; `detect_agent` publishes it as a
        // `Detected`-tier report when it is `blocked`/`working`.
        detection.screen_state =
            crate::agent_state_classify::classify_agent_state(&detection.agent, &screen);
        if remote && detection.is_unknown() {
            detection.evidence =
                "remote surface: process tree not local; no screen marker matched".to_string();
        }
        Ok(detection)
    }

    /// Set the operator-configured worktree path pattern (issue #77
    /// AC6). `None` (or never called) keeps the default
    /// `<repo>/../<repo>.<branch>/`. Called from `mux-tui`'s `run_server`
    /// after `config::load()`, mirroring `set_resolved_chrome`.
    pub fn set_worktree_pattern(&self, pattern: Option<String>) {
        *self.worktree_pattern.lock().unwrap() = pattern;
    }

    fn worktree_pattern(&self) -> String {
        self.worktree_pattern
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| crate::worktree::DEFAULT_PATTERN.to_string())
    }

    /// Create the git worktree for `branch` rooted at the repository
    /// containing `start_cwd`. Runs git OUTSIDE the state lock; on any
    /// failure nothing is recorded and the caller's cwd is untouched
    /// (issue #77 AC7).
    fn create_worktree(
        &self,
        start_cwd: &str,
        branch: &str,
        label: Option<String>,
    ) -> anyhow::Result<crate::worktree::WorktreeRecord> {
        use crate::worktree;

        let repo_root = worktree::find_repo_root(Path::new(start_cwd))
            .ok_or_else(|| anyhow::anyhow!("{start_cwd:?} is not inside a git repository"))?;
        let path = worktree::resolve_worktree_path(&repo_root, &self.worktree_pattern(), branch)?;
        worktree::git_worktree_add(&repo_root, branch, &path)?;
        Ok(crate::worktree::WorktreeRecord {
            branch: branch.to_string(),
            path: path.to_string_lossy().into_owned(),
            label,
            created_at_ms: crate::surface::now_ms(),
        })
    }

    /// `pane worktree create` (issue #77 AC1): create the worktree for
    /// `branch`, record it on the pane, and `cd` the pane's active tab
    /// into it. On failure (dirty tree, bad branch, no repository) the
    /// error propagates and the pane is left untouched (AC7).
    pub fn pane_worktree_create(
        self: &Arc<Self>,
        pane: PaneId,
        branch: &str,
        label: Option<String>,
    ) -> anyhow::Result<crate::worktree::WorktreeRecord> {
        let surface = {
            let state = self.state.lock().unwrap();
            let Some(p) = state.panes.get(&pane) else { anyhow::bail!("unknown pane {pane}") };
            let active = p
                .active_surface()
                .ok_or_else(|| anyhow::anyhow!("pane {pane} has no active tab"))?;
            state.surfaces.get(&active).cloned()
        };
        let Some(surface) = surface else { anyhow::bail!("pane {pane} has no active surface") };
        if surface.kind() != crate::SurfaceKind::Pty {
            anyhow::bail!("pane {pane}'s active tab is not a pty surface");
        }
        let cwd = surface
            .cwd()
            .ok_or_else(|| anyhow::anyhow!("pane {pane} has no working directory yet"))?;
        let record = self.create_worktree(&cwd, branch, label)?;
        {
            let mut state = self.state.lock().unwrap();
            let Some(p) = state.panes.get_mut(&pane) else { anyhow::bail!("unknown pane {pane}") };
            p.worktrees.push(record.clone());
        }
        // Same live `cd` the persist path uses (restore_tab): the pane
        // keeps its shell, it just moves into the worktree. Best-effort:
        // a dead pty keeps the record, which list/remove still manage.
        let _ = surface.write_bytes(
            format!("cd {} && clear\n", crate::persist::shell_quote(&record.path)).as_bytes(),
        );
        self.emit(MuxEvent::TreeChanged);
        Ok(record)
    }

    /// `pane worktree list` (issue #77 AC2): every worktree attached to
    /// the pane over its lifetime, in creation order.
    pub fn pane_worktree_list(
        &self,
        pane: PaneId,
    ) -> anyhow::Result<Vec<crate::worktree::WorktreeRecord>> {
        let state = self.state.lock().unwrap();
        let Some(p) = state.panes.get(&pane) else { anyhow::bail!("unknown pane {pane}") };
        Ok(p.worktrees.clone())
    }

    /// `pane worktree remove` (issue #77 AC3): `git worktree remove` +
    /// `prune`, then drop the record. Refuses (error, no force flag)
    /// while the pane's working directory sits inside the target
    /// worktree; a dirty worktree fails via git's own error.
    pub fn pane_worktree_remove(
        self: &Arc<Self>,
        pane: PaneId,
        branch: &str,
    ) -> anyhow::Result<()> {
        use crate::worktree;

        let (record, cwd) = {
            let state = self.state.lock().unwrap();
            let Some(p) = state.panes.get(&pane) else { anyhow::bail!("unknown pane {pane}") };
            let Some(record) = p.worktrees.iter().find(|w| w.branch == branch) else {
                anyhow::bail!("no worktree for branch {branch:?} on pane {pane}")
            };
            let record = record.clone();
            let cwd = p.active_surface().and_then(|s| state.surfaces.get(&s)).and_then(|s| s.cwd());
            (record, cwd)
        };
        if cwd.as_deref().is_some_and(|cwd| Path::new(cwd).starts_with(record.path.as_str())) {
            anyhow::bail!("pane {pane} is inside {}; cd elsewhere before removing it", record.path);
        }
        // Resolve the MAIN repository through the worktree's own `.git`
        // pointer so remove/prune run from a stable repo context even if
        // the pane has since cd'd away.
        let root = worktree::main_repo_root(Path::new(&record.path)).ok_or_else(|| {
            anyhow::anyhow!("could not resolve the main repository for worktree {}", record.path)
        })?;
        worktree::git_worktree_remove(&root, Path::new(&record.path))?;
        worktree::git_worktree_prune(&root)?;
        {
            let mut state = self.state.lock().unwrap();
            if let Some(p) = state.panes.get_mut(&pane) {
                p.worktrees.retain(|w| w.branch != branch);
            }
        }
        self.emit(MuxEvent::TreeChanged);
        Ok(())
    }

    /// Set a screen's user-visible name. An empty name clears it (the
    /// screen falls back to its number).
    pub fn rename_screen(&self, target: ScreenId, name: String) -> bool {
        let renamed = {
            let mut state = self.state.lock().unwrap();
            match state
                .workspaces
                .iter_mut()
                .flat_map(|ws| ws.screens.iter_mut())
                .find(|s| s.id == target)
            {
                Some(screen) => {
                    screen.name = (!name.is_empty()).then_some(name);
                    true
                }
                None => false,
            }
        };
        if renamed {
            self.emit(MuxEvent::TreeChanged);
        }
        renamed
    }

    /// Reap a surface whose child exited before its tree insert completed.
    /// The exit handler sets the dead flag before calling `surface_exited`,
    /// whose `close_surface` finds nothing to remove in that window; the
    /// creator re-checks after the insert (a harmless no-op otherwise).
    fn reap_if_dead(&self, surface: &Arc<Surface>) {
        if surface.is_dead() {
            self.close_surface(surface.id);
        }
    }

    /// Called by a surface's reader thread when its child exits. The mux
    /// reaps the surface out of the tree itself, so frontends only need to
    /// drop their render state.
    pub fn surface_exited(&self, id: SurfaceId) {
        self.close_surface(id);
        self.emit(MuxEvent::SurfaceExited(id));
    }

    /// Make `pane` the active pane of its screen (and that screen and
    /// workspace active).
    pub fn focus_pane(&self, pane: PaneId) -> bool {
        let active_at = self.next_active_at();
        let found = {
            let mut state = self.state.lock().unwrap();
            match state.screen_of(pane) {
                Some((wi, si)) => {
                    state.active_workspace = wi;
                    let ws = &mut state.workspaces[wi];
                    ws.active_screen = si;
                    ws.screens[si].active_pane = pane;
                    stamp_pane(&mut state, pane, active_at);
                    true
                }
                None => false,
            }
        };
        if found {
            self.emit(MuxEvent::TreeChanged);
        }
        found
    }

    /// Set the deepest split ratio in `dir` on the path to `pane`.
    pub fn set_ratio(&self, pane: PaneId, dir: SplitDir, ratio: f32) -> bool {
        let ratio = ratio.clamp(0.05, 0.95);
        let found = {
            let mut state = self.state.lock().unwrap();
            state
                .workspaces
                .iter_mut()
                .flat_map(|ws| ws.screens.iter_mut())
                .any(|screen| screen.root.set_deepest_ratio(pane, dir, ratio))
        };
        if found {
            self.emit(MuxEvent::TreeChanged);
        }
        found
    }

    /// Move an existing tab to `index` in `pane`. The surface is kept
    /// alive; if moving it empties the source pane, that pane collapses
    /// out of its split tree.
    pub fn move_tab(&self, surface: SurfaceId, pane: PaneId, index: usize) -> bool {
        let active_at = self.next_active_at();
        let moved = {
            let mut state = self.state.lock().unwrap();
            let moved = move_tab_in_state(&mut state, surface, pane, index);
            if moved {
                stamp_pane(&mut state, pane, active_at);
            }
            moved
        };
        if moved {
            self.emit(MuxEvent::TreeChanged);
        }
        moved
    }

    /// Reorder a workspace. The active workspace follows the moved entry.
    pub fn move_workspace(&self, workspace: WorkspaceId, index: usize) -> bool {
        let moved = {
            let mut state = self.state.lock().unwrap();
            let Some(old_idx) = state.workspaces.iter().position(|ws| ws.id == workspace) else {
                return false;
            };
            let new_idx = if index > old_idx { index.saturating_sub(1) } else { index };
            let new_idx = new_idx.min(state.workspaces.len().saturating_sub(1));
            if new_idx == old_idx {
                return false;
            }
            let active_id = state.workspaces.get(state.active_workspace).map(|ws| ws.id);
            let ws = state.workspaces.remove(old_idx);
            state.workspaces.insert(new_idx, ws);
            state.active_workspace = active_id
                .and_then(|id| state.workspaces.iter().position(|ws| ws.id == id))
                .unwrap_or_else(|| state.workspaces.len().saturating_sub(1));
            true
        };
        if moved {
            self.emit(MuxEvent::TreeChanged);
        }
        moved
    }

    /// Select a tab within a pane (default: the active pane) by index or
    /// relative delta.
    pub fn select_tab(&self, pane: Option<PaneId>, index: Option<usize>, delta: Option<isize>) {
        let active_at = self.next_active_at();
        {
            let mut state = self.state.lock().unwrap();
            let Some(target) = pane.or_else(|| state.active_pane()) else { return };
            let Some(pane) = state.panes.get_mut(&target) else { return };
            let len = pane.tabs.len();
            if len == 0 {
                return;
            }
            if let Some(index) = index {
                if index < len {
                    pane.active_tab = index;
                }
            } else if let Some(delta) = delta {
                pane.active_tab =
                    ((pane.active_tab as isize + delta).rem_euclid(len as isize)) as usize;
            }
            stamp_pane(&mut state, target, active_at);
        }
        self.emit(MuxEvent::TreeChanged);
    }

    /// Select a screen in the active workspace by index or relative delta.
    pub fn select_screen(&self, index: Option<usize>, delta: Option<isize>) {
        let active_at = self.next_active_at();
        {
            let mut state = self.state.lock().unwrap();
            let active = state.active_workspace;
            let Some(ws) = state.workspaces.get_mut(active) else { return };
            let len = ws.screens.len();
            if len == 0 {
                return;
            }
            if let Some(index) = index {
                if index < len {
                    ws.active_screen = index;
                }
            } else if let Some(delta) = delta {
                ws.active_screen =
                    ((ws.active_screen as isize + delta).rem_euclid(len as isize)) as usize;
            }
            if let Some(pane) = ws.active_screen_ref().map(|screen| screen.active_pane) {
                stamp_pane(&mut state, pane, active_at);
            }
        }
        self.emit(MuxEvent::TreeChanged);
    }

    /// Select a workspace by index or relative delta.
    pub fn select_workspace(&self, index: Option<usize>, delta: Option<isize>) {
        let active_at = self.next_active_at();
        {
            let mut state = self.state.lock().unwrap();
            let len = state.workspaces.len();
            if len == 0 {
                return;
            }
            if let Some(index) = index {
                if index < len {
                    state.active_workspace = index;
                }
            } else if let Some(delta) = delta {
                state.active_workspace =
                    ((state.active_workspace as isize + delta).rem_euclid(len as isize)) as usize;
            }
            if let Some(pane) = state
                .workspaces
                .get(state.active_workspace)
                .and_then(|ws| ws.active_screen_ref().map(|screen| screen.active_pane))
            {
                stamp_pane(&mut state, pane, active_at);
            }
        }
        self.emit(MuxEvent::TreeChanged);
    }
}

/// Spawn overrides for a layout tab's recorded argv/env/cwd. Browser and
/// remote tabs have no pty overrides — structure spawns a default shell
/// and `apply_layout_screen` fixes the tab up (or downgrades it loudly).
fn layout_tab_overrides(tab: &crate::layout_doc::LayoutTab) -> Option<SpawnOverrides> {
    match tab {
        crate::layout_doc::LayoutTab::Pty { command, env, cwd, .. } => Some(SpawnOverrides {
            command: command.clone(),
            extra_env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            cwd: cwd.clone(),
        }),
        _ => None,
    }
}

/// The pane index `split()` auto-creates for a subtree: its leftmost
/// leaf (replay only ever recurses down the "a" side of its root).
fn leftmost_layout_index(node: &crate::layout_doc::LayoutNode) -> usize {
    match node {
        crate::layout_doc::LayoutNode::Leaf { pane } => *pane,
        crate::layout_doc::LayoutNode::Split { a, .. } => leftmost_layout_index(a),
    }
}

/// Every pane id in a workspace (all screens, in tree order).
fn workspace_pane_ids(ws: &Workspace) -> Vec<PaneId> {
    let mut panes = Vec::new();
    for screen in &ws.screens {
        screen.root.pane_ids(&mut panes);
    }
    panes
}

/// Lexical "path is inside `ancestor`" (or equals it): both sides are
/// absolute, already-normalised paths, so a component-wise prefix
/// check suffices - no filesystem access, no symlink resolution. A
/// sibling like `/repo.feat-x2` is NOT under `/repo.feat-x` (issue #100).
fn path_is_under(path: &str, ancestor: &str) -> bool {
    Path::new(path).starts_with(Path::new(ancestor))
}

/// Every surface in a screen (all panes, all tabs).
fn screen_tabs(state: &State, screen: &Screen) -> Vec<SurfaceId> {
    let mut pane_ids = Vec::new();
    screen.root.pane_ids(&mut pane_ids);
    pane_ids
        .iter()
        .filter_map(|id| state.panes.get(id))
        .flat_map(|pane| pane.tabs.iter().copied())
        .collect()
}

fn stamp_pane(state: &mut State, pane: PaneId, active_at: u64) {
    if let Some(pane) = state.panes.get_mut(&pane) {
        pane.active_at = active_at;
    }
}

fn most_recent_pane(state: &State, panes: &[PaneId]) -> Option<PaneId> {
    panes
        .iter()
        .filter_map(|id| state.panes.get(id).map(|pane| (*id, pane.active_at)))
        .max_by_key(|(_, active_at)| *active_at)
        .map(|(id, _)| id)
}

/// Remove one surface from the state: detach it from its
/// pane, and collapse emptied panes/screens/workspaces. Returns whether
/// anything was removed. Runs under the state lock.
fn remove_surface(state: &mut State, target: SurfaceId) -> Option<Arc<Surface>> {
    let removed = state.surfaces.remove(&target);
    let Some(pane_id) = state.pane_of(target) else {
        return removed;
    };
    let pane = state.panes.get_mut(&pane_id).expect("pane_of returned live id");
    let idx = pane.tabs.iter().position(|id| *id == target).expect("tab in pane");
    pane.tabs.remove(idx);
    if !pane.tabs.is_empty() {
        if pane.active_tab >= idx && pane.active_tab > 0 {
            pane.active_tab -= 1;
        }
        return removed;
    }

    // Last tab gone: the pane collapses out of its screen.
    state.panes.remove(&pane_id);
    let Some((wi, si)) = state.screen_of(pane_id) else {
        return removed;
    };
    let (was_active, root) = {
        let screen = &mut state.workspaces[wi].screens[si];
        let was_active = screen.active_pane == pane_id;
        let root = std::mem::replace(&mut screen.root, Node::Leaf(0));
        (was_active, root)
    };
    match root.remove_leaf(pane_id) {
        Some(root) => {
            let next_active = if was_active {
                let mut ids = Vec::new();
                root.pane_ids(&mut ids);
                most_recent_pane(state, &ids)
            } else {
                None
            };
            let screen = &mut state.workspaces[wi].screens[si];
            screen.root = root;
            if let Some(next) = next_active {
                screen.active_pane = next;
            }
            return removed;
        }
        None => {
            // Screen emptied: drop it from the workspace.
            let ws = &mut state.workspaces[wi];
            ws.screens.remove(si);
            ws.active_screen = ws.active_screen.min(ws.screens.len().saturating_sub(1));
            if !ws.screens.is_empty() {
                return removed;
            }
        }
    }

    // Workspace emptied too: drop it, keeping the active selection stable.
    let active_id = state.workspaces.get(state.active_workspace).map(|w| w.id);
    state.workspaces.remove(wi);
    state.active_workspace = active_id
        .and_then(|id| state.workspaces.iter().position(|w| w.id == id))
        .unwrap_or_else(|| state.workspaces.len().saturating_sub(1));
    removed
}

fn collapse_empty_pane(state: &mut State, pane_id: PaneId) {
    state.panes.remove(&pane_id);
    let Some((wi, si)) = state.screen_of(pane_id) else {
        return;
    };
    let (was_active, root) = {
        let screen = &mut state.workspaces[wi].screens[si];
        let was_active = screen.active_pane == pane_id;
        let root = std::mem::replace(&mut screen.root, Node::Leaf(0));
        (was_active, root)
    };
    match root.remove_leaf(pane_id) {
        Some(root) => {
            let next_active = if was_active {
                let mut ids = Vec::new();
                root.pane_ids(&mut ids);
                most_recent_pane(state, &ids)
            } else {
                None
            };
            let screen = &mut state.workspaces[wi].screens[si];
            screen.root = root;
            if let Some(next) = next_active {
                screen.active_pane = next;
            }
        }
        None => {
            let ws = &mut state.workspaces[wi];
            ws.screens.remove(si);
            ws.active_screen = ws.active_screen.min(ws.screens.len().saturating_sub(1));
            if !ws.screens.is_empty() {
                return;
            }
            let active_id = state.workspaces.get(state.active_workspace).map(|w| w.id);
            state.workspaces.remove(wi);
            state.active_workspace = active_id
                .and_then(|id| state.workspaces.iter().position(|w| w.id == id))
                .unwrap_or_else(|| state.workspaces.len().saturating_sub(1));
        }
    }
}

fn move_tab_in_state(
    state: &mut State,
    surface: SurfaceId,
    target_pane: PaneId,
    index: usize,
) -> bool {
    if !state.surfaces.contains_key(&surface) || !state.panes.contains_key(&target_pane) {
        return false;
    }
    let Some(source_pane) = state.pane_of(surface) else { return false };
    if source_pane == target_pane {
        let Some(pane) = state.panes.get_mut(&target_pane) else {
            return false;
        };
        let Some(old_idx) = pane.tabs.iter().position(|id| *id == surface) else {
            return false;
        };
        let new_idx = if index > old_idx { index.saturating_sub(1) } else { index };
        let new_idx = new_idx.min(pane.tabs.len().saturating_sub(1));
        if new_idx == old_idx {
            return false;
        }
        let tab = pane.tabs.remove(old_idx);
        pane.tabs.insert(new_idx, tab);
        pane.active_tab = new_idx;
        return true;
    }

    {
        let Some(source) = state.panes.get_mut(&source_pane) else {
            return false;
        };
        let Some(old_idx) = source.tabs.iter().position(|id| *id == surface) else {
            return false;
        };
        source.tabs.remove(old_idx);
        if !source.tabs.is_empty() && source.active_tab >= old_idx && source.active_tab > 0 {
            source.active_tab -= 1;
        }
    }

    if state.panes.get(&source_pane).is_some_and(|pane| pane.tabs.is_empty()) {
        collapse_empty_pane(state, source_pane);
    }

    let Some(target) = state.panes.get_mut(&target_pane) else {
        return false;
    };
    let new_idx = index.min(target.tabs.len());
    target.tabs.insert(new_idx, surface);
    target.active_tab = new_idx;
    if let Some((wi, si)) = state.screen_of(target_pane) {
        state.active_workspace = wi;
        let ws = &mut state.workspaces[wi];
        ws.active_screen = si;
        ws.screens[si].active_pane = target_pane;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_mux() -> Arc<Mux> {
        // A child that stays alive without doing anything.
        let opts =
            SurfaceOptions { command: Some(vec!["/bin/cat".to_string()]), ..Default::default() };
        Mux::new("test", opts)
    }

    fn seed_split_ratio_tree(mux: &Mux) -> (PaneId, PaneId, PaneId) {
        let (p1, p2, p3) = (1, 2, 3);
        *mux.state.lock().unwrap() = State {
            workspaces: vec![Workspace {
                id: 1,
                name: "1".into(),
                screens: vec![Screen {
                    id: 1,
                    name: None,
                    root: Node::Split {
                        dir: SplitDir::Right,
                        ratio: 0.5,
                        a: Box::new(Node::Split {
                            dir: SplitDir::Right,
                            ratio: 0.5,
                            a: Box::new(Node::Leaf(p1)),
                            b: Box::new(Node::Leaf(p3)),
                        }),
                        b: Box::new(Node::Leaf(p2)),
                    },
                    active_pane: p3,
                }],
                active_screen: 0,
                color: None,
                icon: None,
            }],
            active_workspace: 0,
            panes: HashMap::from([
                (
                    p1,
                    Pane {
                        id: p1,
                        name: None,
                        tabs: vec![1],
                        active_tab: 0,
                        active_at: 1,
                        worktrees: Vec::new(),
                    },
                ),
                (
                    p2,
                    Pane {
                        id: p2,
                        name: None,
                        tabs: vec![2],
                        active_tab: 0,
                        active_at: 2,
                        worktrees: Vec::new(),
                    },
                ),
                (
                    p3,
                    Pane {
                        id: p3,
                        name: None,
                        tabs: vec![3],
                        active_tab: 0,
                        active_at: 3,
                        worktrees: Vec::new(),
                    },
                ),
            ]),
            surfaces: HashMap::new(),
        };
        (p1, p2, p3)
    }

    #[test]
    fn split_and_close_collapses_tree() {
        let mux = test_mux();
        let s1 = mux.new_workspace(None, None).unwrap();
        let p1 = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        let s2 = mux.split(p1, SplitDir::Right, None).unwrap();
        let p2 = mux.with_state(|s| s.pane_of(s2.id).unwrap());
        let s3 = mux.split(p2, SplitDir::Down, None).unwrap();
        let p3 = mux.with_state(|s| s.pane_of(s3.id).unwrap());

        mux.with_state(|s| {
            let mut ids = Vec::new();
            s.workspaces[0].screens[0].root.pane_ids(&mut ids);
            assert_eq!(ids, vec![p1, p2, p3]);
        });

        mux.close_pane(p2);
        mux.with_state(|s| {
            let mut ids = Vec::new();
            s.workspaces[0].screens[0].root.pane_ids(&mut ids);
            assert_eq!(ids, vec![p1, p3]);
        });

        mux.close_pane(p1);
        mux.close_pane(p3);
        assert_eq!(mux.surface_count(), 0);
        mux.with_state(|s| assert!(s.workspaces.is_empty()));
    }

    #[test]
    fn closing_active_pane_focuses_most_recent_remaining_pane() {
        let mux = test_mux();
        let s1 = mux.new_workspace(None, None).unwrap();
        let p1 = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        let s2 = mux.split(p1, SplitDir::Right, None).unwrap();
        let p2 = mux.with_state(|s| s.pane_of(s2.id).unwrap());
        let s3 = mux.split(p2, SplitDir::Down, None).unwrap();
        let p3 = mux.with_state(|s| s.pane_of(s3.id).unwrap());

        assert!(mux.focus_pane(p1));
        assert!(mux.focus_pane(p3));
        mux.close_pane(p3);

        mux.with_state(|s| {
            assert_eq!(s.workspaces[0].screens[0].active_pane, p1);
            assert!(s.panes.contains_key(&p2));
        });
    }

    #[test]
    fn tabs_within_pane() {
        let mux = test_mux();
        let s1 = mux.new_workspace(None, None).unwrap();
        let pane = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        let s2 = mux.new_tab(Some(pane), None, None).unwrap();

        mux.with_state(|s| {
            let p = &s.panes[&pane];
            assert_eq!(p.tabs, vec![s1.id, s2.id]);
            assert_eq!(p.active_tab, 1);
        });

        // Closing the active tab activates the previous one; the pane stays.
        mux.close_surface(s2.id);
        mux.with_state(|s| {
            let p = &s.panes[&pane];
            assert_eq!(p.tabs, vec![s1.id]);
            assert_eq!(p.active_tab, 0);
            assert_eq!(s.workspaces.len(), 1);
        });

        // Closing the last tab collapses the pane, screen, and workspace.
        mux.close_surface(s1.id);
        mux.with_state(|s| assert!(s.workspaces.is_empty()));
    }

    #[test]
    fn move_tab_within_pane_clamps_and_tracks_active_tab() {
        let mux = test_mux();
        let s1 = mux.new_workspace(None, None).unwrap();
        let pane = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        let s2 = mux.new_tab(Some(pane), None, None).unwrap();
        let s3 = mux.new_tab(Some(pane), None, None).unwrap();

        assert!(mux.move_tab(s3.id, pane, 0));
        mux.with_state(|s| {
            let pane = &s.panes[&pane];
            assert_eq!(pane.tabs, vec![s3.id, s1.id, s2.id]);
            assert_eq!(pane.active_tab, 0);
        });

        assert!(mux.move_tab(s3.id, pane, 99));
        mux.with_state(|s| {
            let pane = &s.panes[&pane];
            assert_eq!(pane.tabs, vec![s1.id, s2.id, s3.id]);
            assert_eq!(pane.active_tab, 2);
        });
    }

    #[test]
    fn move_tab_same_position_preserves_active_tab_and_emits_no_event() {
        let mux = test_mux();
        let s1 = mux.new_workspace(None, None).unwrap();
        let pane = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        let s2 = mux.new_tab(Some(pane), None, None).unwrap();
        let s3 = mux.new_tab(Some(pane), None, None).unwrap();
        mux.select_tab(Some(pane), Some(0), None);
        let events = mux.subscribe();

        assert!(!mux.move_tab(s2.id, pane, 1));
        mux.with_state(|s| {
            let pane = &s.panes[&pane];
            assert_eq!(pane.tabs, vec![s1.id, s2.id, s3.id]);
            assert_eq!(pane.active_tab, 0);
        });
        assert!(events.try_iter().all(|event| !matches!(event, MuxEvent::TreeChanged)));
    }

    #[test]
    fn move_tab_across_panes_collapses_empty_source_and_preserves_surface() {
        let mux = test_mux();
        let s1 = mux.new_workspace(None, None).unwrap();
        let p1 = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        let s2 = mux.split(p1, SplitDir::Right, None).unwrap();
        let p2 = mux.with_state(|s| s.pane_of(s2.id).unwrap());
        let original_count = mux.surface_count();

        assert!(mux.move_tab(s1.id, p2, 0));
        mux.with_state(|s| {
            assert!(!s.panes.contains_key(&p1));
            let target = &s.panes[&p2];
            assert_eq!(target.tabs, vec![s1.id, s2.id]);
            assert_eq!(target.active_tab, 0);
            assert!(s.surfaces.contains_key(&s1.id));
            let mut ids = Vec::new();
            s.workspaces[0].screens[0].root.pane_ids(&mut ids);
            assert_eq!(ids, vec![p2]);
        });
        assert_eq!(mux.surface_count(), original_count);
    }

    #[test]
    fn set_ratio_updates_deepest_split_and_clamps() {
        let mux = test_mux();
        let (p1, p2, p3) = seed_split_ratio_tree(&mux);

        assert!(mux.set_ratio(p1, SplitDir::Right, 0.8));
        mux.with_state(|s| {
            let root = &s.workspaces[0].screens[0].root;
            let Node::Split { ratio: root_ratio, a, .. } = root else {
                panic!("root should be split");
            };
            assert_eq!(*root_ratio, 0.5);
            let Node::Split { ratio: inner_ratio, .. } = a.as_ref() else {
                panic!("first child should be split");
            };
            assert_eq!(*inner_ratio, 0.8);
        });

        assert!(mux.set_ratio(p2, SplitDir::Right, -1.0));
        mux.with_state(|s| {
            let Node::Split { ratio, .. } = &s.workspaces[0].screens[0].root else {
                panic!("root should be split");
            };
            assert_eq!(*ratio, 0.05);
        });

        assert!(mux.set_ratio(p3, SplitDir::Right, 2.0));
        mux.with_state(|s| {
            let Node::Split { a, .. } = &s.workspaces[0].screens[0].root else {
                panic!("root should be split");
            };
            let Node::Split { ratio, .. } = a.as_ref() else {
                panic!("first child should be split");
            };
            assert_eq!(*ratio, 0.95);
        });

        assert!(!mux.set_ratio(9999, SplitDir::Right, 0.4));
    }

    #[test]
    fn screens_within_workspace() {
        let mux = test_mux();
        mux.new_workspace(None, None).unwrap();
        let s2 = mux.new_screen(None, None).unwrap();

        let (screen1, screen2) = mux.with_state(|s| {
            let ws = &s.workspaces[0];
            assert_eq!(ws.screens.len(), 2);
            assert_eq!(ws.active_screen, 1);
            (ws.screens[0].id, ws.screens[1].id)
        });

        // Select back to screen 1; screen 2 keeps running.
        mux.select_screen(Some(0), None);
        mux.with_state(|s| assert_eq!(s.workspaces[0].active_screen, 0));

        // Renaming a screen sticks; clearing falls back.
        assert!(mux.rename_screen(screen2, "logs".into()));
        mux.with_state(|s| {
            assert_eq!(s.workspaces[0].screens[1].name.as_deref(), Some("logs"));
        });

        // Focusing a pane in screen 2 activates that screen.
        let p2 = mux.with_state(|s| s.pane_of(s2.id).unwrap());
        assert!(mux.focus_pane(p2));
        mux.with_state(|s| assert_eq!(s.workspaces[0].active_screen, 1));

        // Closing screen 2 keeps the workspace with screen 1.
        assert!(mux.close_screen(screen2));
        mux.with_state(|s| {
            let ws = &s.workspaces[0];
            assert_eq!(ws.screens.len(), 1);
            assert_eq!(ws.screens[0].id, screen1);
            assert_eq!(ws.active_screen, 0);
        });
    }

    #[test]
    fn workspaces_and_renames() {
        let mux = test_mux();
        let events = mux.subscribe();
        mux.new_workspace(None, None).unwrap();
        mux.new_workspace(Some("dev".into()), None).unwrap();

        let (ws0, ws1, pane1, surface1) = mux.with_state(|s| {
            assert_eq!(s.workspaces.len(), 2);
            assert_eq!(s.workspaces[1].name, "dev");
            assert_eq!(s.active_workspace, 1);
            let pane = s.workspaces[1].screens[0].active_pane;
            let surface = s.panes[&pane].tabs[0];
            (s.workspaces[0].id, s.workspaces[1].id, pane, surface)
        });

        assert!(mux.rename_workspace(ws0, "ops".into()));
        assert!(mux.rename_pane(pane1, "logs".into()));
        assert!(mux.rename_surface(surface1, "api".into()));
        mux.with_state(|s| {
            assert_eq!(s.workspaces[0].name, "ops");
            assert_eq!(s.panes[&pane1].name.as_deref(), Some("logs"));
            assert_eq!(s.surfaces[&surface1].name().as_deref(), Some("api"));
        });
        // Clearing the names falls back to the generated labels.
        assert!(mux.rename_pane(pane1, String::new()));
        assert!(mux.rename_surface(surface1, String::new()));
        mux.with_state(|s| {
            assert_eq!(s.panes[&pane1].name, None);
            assert_eq!(s.surfaces[&surface1].name(), None);
        });

        assert!(mux.close_workspace(ws1));
        mux.with_state(|s| {
            assert_eq!(s.workspaces.len(), 1);
            assert_eq!(s.workspaces[0].id, ws0);
            assert_eq!(s.active_workspace, 0);
        });
        assert!(events.try_iter().count() > 0);
    }

    /// Issue #100 fixture: a temp "worktree" directory that exists on
    /// disk (the child pane's tab is really spawned inside it — its cwd
    /// is what detection reads).
    fn worktree_dir_fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "mtyx-wtchild-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        (base.join("proj.feat-auth"), base.join("proj.feat-auth2"))
    }

    #[test]
    fn close_workspace_reports_surviving_worktree_children() {
        let (worktree, sibling) = worktree_dir_fixture("report");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let worktree = worktree.to_string_lossy().to_string();
        let sibling = sibling.to_string_lossy().to_string();

        let mux = test_mux();
        let parent = mux.new_workspace(Some("parent".into()), None).unwrap();
        let child = mux.new_workspace(Some("child".into()), None).unwrap();
        let elsewhere = mux.new_workspace(Some("elsewhere".into()), None).unwrap();
        let (parent_id, child_id) = mux.with_state(|s| {
            (
                s.workspaces.iter().find(|ws| ws.name == "parent").unwrap().id,
                s.workspaces.iter().find(|ws| ws.name == "child").unwrap().id,
            )
        });
        let parent_pane = mux.with_state(|s| s.pane_of(parent.id).unwrap());
        let child_pane = mux.with_state(|s| s.pane_of(child.id).unwrap());
        let elsewhere_pane = mux.with_state(|s| s.pane_of(elsewhere.id).unwrap());

        // The parent's pane owns a worktree record (the pane-worktree-*
        // flow would have created it for real). `with_state` is &State,
        // so seed the registry under the lock directly.
        {
            let mut state = mux.state.lock().unwrap();
            state.panes.get_mut(&parent_pane).unwrap().worktrees.push(
                crate::worktree::WorktreeRecord {
                    branch: "feat-auth".into(),
                    path: worktree.clone(),
                    label: None,
                    created_at_ms: 1,
                },
            );
        }
        // The child pane lives inside the worktree; `elsewhere` sits in
        // a sibling-named directory that must NOT match.
        let inside = mux.new_tab(Some(child_pane), Some(worktree.clone()), None).unwrap();
        mux.new_tab(Some(elsewhere_pane), Some(sibling.clone()), None).unwrap();
        // A running agent in the child is flagged in the report.
        mux.report_agent(
            inside.id,
            AgentState::Working,
            AgentStateSource::Socket,
            None,
            None,
            None,
        )
        .unwrap();

        // No owned worktrees on the child → no children of its own.
        assert!(mux.worktree_children(child_id).is_empty());

        let children = mux.worktree_children(parent_id);
        assert_eq!(children.len(), 1, "only the cwd-matched child, not `elsewhere`");
        assert_eq!(children[0].workspace, child_id);
        assert_eq!(children[0].name, "child");
        assert_eq!(children[0].worktree_path, worktree);
        assert_eq!(children[0].worktree_branch.as_deref(), Some("feat-auth"));
        assert!(children[0].running_agent, "a working agent pane must be flagged");

        // Default close: parent only; the child survives and is reported.
        let report = mux.close_workspace_reported(parent_id, false).unwrap();
        assert_eq!(report.closed, vec![(parent_id, "parent".to_string())]);
        assert_eq!(report.survivors, children);
        mux.with_state(|s| {
            let names: Vec<&str> = s.workspaces.iter().map(|ws| ws.name.as_str()).collect();
            assert_eq!(names, vec!["child", "elsewhere"]);
            // No silent orphan: the child's panes and tabs are alive.
            assert!(s.panes.contains_key(&child_pane));
            assert!(s.panes[&child_pane].tabs.contains(&inside.id));
        });

        // Closing the child (no children of its own) reports no survivors.
        let report = mux.close_workspace_reported(child_id, false).unwrap();
        assert_eq!(report.closed, vec![(child_id, "child".to_string())]);
        assert!(report.survivors.is_empty());

        // Unknown workspace: no report, `close_workspace` behaviour.
        assert!(mux.close_workspace_reported(9999, false).is_none());

        std::fs::remove_dir_all(std::path::Path::new(&worktree).parent().unwrap()).unwrap();
    }

    #[test]
    fn close_workspace_group_closes_worktree_children_together() {
        let (worktree, _) = worktree_dir_fixture("group");
        std::fs::create_dir_all(&worktree).unwrap();
        let worktree = worktree.to_string_lossy().to_string();
        let base = std::path::Path::new(&worktree).parent().unwrap().to_path_buf();

        let mux = test_mux();
        let parent = mux.new_workspace(Some("parent".into()), None).unwrap();
        let child = mux.new_workspace(Some("child".into()), None).unwrap();
        let (parent_id, child_id) = mux.with_state(|s| {
            (
                s.workspaces.iter().find(|ws| ws.name == "parent").unwrap().id,
                s.workspaces.iter().find(|ws| ws.name == "child").unwrap().id,
            )
        });
        let parent_pane = mux.with_state(|s| s.pane_of(parent.id).unwrap());
        let child_pane = mux.with_state(|s| s.pane_of(child.id).unwrap());
        {
            let mut state = mux.state.lock().unwrap();
            state.panes.get_mut(&parent_pane).unwrap().worktrees.push(
                crate::worktree::WorktreeRecord {
                    branch: "feat-auth".into(),
                    path: worktree.clone(),
                    label: None,
                    created_at_ms: 1,
                },
            );
        }
        mux.new_tab(Some(child_pane), Some(worktree), None).unwrap();

        let report = mux.close_workspace_reported(parent_id, true).unwrap();
        let closed_ids: Vec<WorkspaceId> = report.closed.iter().map(|(id, _)| *id).collect();
        assert_eq!(closed_ids, vec![parent_id, child_id], "group closes both");
        assert!(report.survivors.is_empty());
        mux.with_state(|s| assert!(s.workspaces.is_empty()));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn report_agent_hook_overrides_socket_but_not_vice_versa() {
        let mux = test_mux();
        mux.new_workspace(None, None).unwrap();
        let surface = mux.with_state(|s| {
            let pane = s.workspaces[0].screens[0].active_pane;
            s.panes[&pane].tabs[0]
        });

        assert!(mux.list_agents(None, None).is_empty());

        let report = mux
            .report_agent(surface, AgentState::Working, AgentStateSource::Socket, None, None, None)
            .unwrap();
        assert_eq!(report.state, AgentState::Working);
        assert_eq!(report.source, AgentStateSource::Socket);

        // A hook report overrides a socket report.
        let report = mux
            .report_agent(
                surface,
                AgentState::Blocked,
                AgentStateSource::Hook,
                Some("sess-1".into()),
                None,
                None,
            )
            .unwrap();
        assert_eq!(report.state, AgentState::Blocked);
        assert_eq!(report.source, AgentStateSource::Hook);

        // A socket report cannot override an existing hook report.
        let report = mux
            .report_agent(surface, AgentState::Idle, AgentStateSource::Socket, None, None, None)
            .unwrap();
        assert_eq!(
            report.state,
            AgentState::Blocked,
            "socket report should not downgrade a hook report"
        );
        assert_eq!(report.source, AgentStateSource::Hook);

        // A newer hook report still applies.
        let report = mux
            .report_agent(
                surface,
                AgentState::Idle,
                AgentStateSource::Hook,
                Some("sess-1".into()),
                None,
                None,
            )
            .unwrap();
        assert_eq!(report.state, AgentState::Idle);

        let agents = mux.list_agents(None, None);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].0, surface);

        assert!(mux.list_agents(Some(surface), Some(AgentState::Idle)).len() == 1);
        assert!(mux.list_agents(Some(surface), Some(AgentState::Working)).is_empty());
        assert!(mux.list_agents(Some(surface + 1), None).is_empty());
    }

    /// Issue #96 AC3: the `Detected` tier stays the lowest authority. A
    /// screen-derived report is rejected while a `Socket` or `Hook`
    /// report is current, and a `Detected` report can itself be upgraded
    /// by either.
    #[test]
    fn report_agent_detected_is_lowest_authority_and_never_overrides_reports() {
        let mux = test_mux();
        mux.new_workspace(None, None).unwrap();
        let surface = mux.with_state(|s| {
            let pane = s.workspaces[0].screens[0].active_pane;
            s.panes[&pane].tabs[0]
        });

        // A Detected report applies on a fresh pane…
        let report = mux
            .report_agent(
                surface,
                AgentState::Blocked,
                AgentStateSource::Detected,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(report.state, AgentState::Blocked);
        assert_eq!(report.source, AgentStateSource::Detected);

        // …and a Socket report overrides it.
        let report = mux
            .report_agent(surface, AgentState::Working, AgentStateSource::Socket, None, None, None)
            .unwrap();
        assert_eq!(report.state, AgentState::Working);
        assert_eq!(report.source, AgentStateSource::Socket);

        // A later Detected report cannot downgrade the socket report.
        let report = mux
            .report_agent(surface, AgentState::Idle, AgentStateSource::Detected, None, None, None)
            .unwrap();
        assert_eq!(report.state, AgentState::Working, "detected must not override a socket report");
        assert_eq!(report.source, AgentStateSource::Socket);

        // Same against a hook report.
        mux.report_agent(surface, AgentState::Blocked, AgentStateSource::Hook, None, None, None)
            .unwrap();
        let report = mux
            .report_agent(
                surface,
                AgentState::Working,
                AgentStateSource::Detected,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(report.state, AgentState::Blocked);
        assert_eq!(report.source, AgentStateSource::Hook);
    }

    #[test]
    fn report_agent_emits_event_only_when_applied() {
        let mux = test_mux();
        mux.new_workspace(None, None).unwrap();
        let surface = mux.with_state(|s| {
            let pane = s.workspaces[0].screens[0].active_pane;
            s.panes[&pane].tabs[0]
        });

        mux.report_agent(surface, AgentState::Working, AgentStateSource::Hook, None, None, None)
            .unwrap();
        let events = mux.subscribe();

        // Rejected: socket cannot override the existing hook report, so no
        // event should fire.
        mux.report_agent(surface, AgentState::Idle, AgentStateSource::Socket, None, None, None)
            .unwrap();
        assert!(
            events.try_iter().count() == 0,
            "a rejected report must not emit agent-state-changed"
        );

        mux.report_agent(surface, AgentState::Done, AgentStateSource::Hook, None, None, None)
            .unwrap();
        let fired = events.try_iter().any(|e| {
            matches!(e, MuxEvent::AgentStateChanged { report, .. } if report.state == AgentState::Done)
        });
        assert!(fired, "an applied report must emit agent-state-changed");
    }

    // -- issue #76: layout apply ------------------------------------------

    use crate::layout_doc::{
        LayoutDir, LayoutDocument, LayoutNode, LayoutPane, LayoutScreen, LayoutTab, LayoutWorkspace,
    };
    use crate::LAYOUT_SCHEMA_VERSION;
    use std::collections::BTreeMap;

    fn layout_cat_tab() -> LayoutTab {
        LayoutTab::Pty {
            name: None,
            cwd: None,
            command: Some(vec!["/bin/cat".to_string()]),
            env: BTreeMap::new(),
        }
    }

    fn layout_doc_with(name: &str, layout: LayoutNode, panes: Vec<LayoutPane>) -> LayoutDocument {
        LayoutDocument {
            schema_version: LAYOUT_SCHEMA_VERSION,
            cmux_version: crate::VERSION.to_string(),
            workspace: LayoutWorkspace {
                name: name.to_string(),
                color: Some("#ff0000".into()),
                icon: Some("robot".into()),
                active_screen: 0,
                screens: vec![LayoutScreen {
                    name: Some("main".into()),
                    active_pane: 0,
                    layout,
                    panes,
                }],
            },
        }
    }

    #[test]
    fn apply_layout_creates_missing_workspace_by_name() {
        let mux = test_mux();
        let doc = layout_doc_with(
            "resurrected",
            LayoutNode::Leaf { pane: 0 },
            vec![LayoutPane {
                name: Some("build".into()),
                active_tab: 0,
                tabs: vec![layout_cat_tab()],
            }],
        );
        let summary = mux.apply_layout("resurrected", &doc).unwrap();
        mux.with_state(|s| {
            assert_eq!(s.workspaces.len(), 1);
            let ws = &s.workspaces[0];
            assert_eq!(ws.name, "resurrected");
            assert_eq!(Some(ws.id), Some(summary.workspace_id));
            assert_eq!(ws.color, Some(Rgb { r: 255, g: 0, b: 0 }));
            assert_eq!(ws.icon.as_ref().map(|i| i.as_str()), Some("\u{1f916}"));
            assert_eq!(ws.screens[0].name.as_deref(), Some("main"));
            let pane = s.panes[&ws.screens[0].active_pane].name.clone();
            assert_eq!(pane.as_deref(), Some("build"));
        });
        assert_eq!(summary.panes, 1);
        assert_eq!(summary.surfaces, 1);

        // Applying onto an existing name is refused (non-destructive, v1).
        let err = mux.apply_layout("resurrected", &doc).unwrap_err().to_string();
        assert!(err.contains("already exists"), "error was: {err}");
    }

    #[test]
    fn apply_layout_recreates_split_tree_ratios_and_selections() {
        let mux = test_mux();
        let mut doc = layout_doc_with(
            "splitdoc",
            LayoutNode::Split {
                dir: LayoutDir::Right,
                ratio: 0.3,
                a: Box::new(LayoutNode::Leaf { pane: 0 }),
                b: Box::new(LayoutNode::Split {
                    dir: LayoutDir::Down,
                    ratio: 0.4,
                    a: Box::new(LayoutNode::Leaf { pane: 1 }),
                    b: Box::new(LayoutNode::Leaf { pane: 2 }),
                }),
            },
            vec![
                LayoutPane { name: None, active_tab: 0, tabs: vec![layout_cat_tab()] },
                LayoutPane {
                    name: Some("logs".into()),
                    active_tab: 0,
                    tabs: vec![layout_cat_tab()],
                },
                LayoutPane { name: None, active_tab: 0, tabs: vec![layout_cat_tab()] },
            ],
        );
        doc.workspace.screens[0].active_pane = 2;
        mux.apply_layout("splitdoc", &doc).unwrap();

        mux.with_state(|s| {
            let ws = &s.workspaces[0];
            assert_eq!(ws.name, "splitdoc");
            let mut ids = Vec::new();
            ws.screens[0].root.pane_ids(&mut ids);
            assert_eq!(ids.len(), 3, "all three panes should exist");
            let Node::Split { dir, ratio, a: _, b } = &ws.screens[0].root else {
                panic!("expected split root");
            };
            assert!(matches!(dir, SplitDir::Right));
            assert!((ratio - 0.3).abs() < 1e-6, "root ratio was {ratio}");
            let Node::Split { dir: inner_dir, ratio: inner_ratio, a: inner_a, b: inner_b } =
                b.as_ref()
            else {
                panic!("expected inner split on the b side");
            };
            assert!(matches!(inner_dir, SplitDir::Down));
            assert!((inner_ratio - 0.4).abs() < 1e-6, "inner ratio was {inner_ratio}");
            assert!(matches!(**inner_a, Node::Leaf(id) if id == ids[1]));
            assert!(matches!(**inner_b, Node::Leaf(id) if id == ids[2]));
            assert_eq!(ws.screens[0].active_pane, ids[2], "recorded selection should be focused");
            assert_eq!(s.panes[&ids[1]].name.as_deref(), Some("logs"));
        });
    }

    #[test]
    fn apply_layout_spawns_recorded_argv_and_env() {
        let mux = test_mux();
        let tab = LayoutTab::Pty {
            name: Some("worker-9".into()),
            cwd: None,
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "test \"$MTYX_LAYOUT_FLAG\" = yes && printf MTYX_LAYOUT_ARGV_OK; sleep 30"
                    .to_string(),
            ]),
            env: BTreeMap::from([("MTYX_LAYOUT_FLAG".to_string(), "yes".to_string())]),
        };
        let doc = layout_doc_with(
            "argv",
            LayoutNode::Leaf { pane: 0 },
            vec![LayoutPane { name: None, active_tab: 0, tabs: vec![tab] }],
        );
        mux.apply_layout("argv", &doc).unwrap();

        let sid = mux.with_state(|s| {
            let ws = &s.workspaces[0];
            s.panes[&ws.screens[0].active_pane].tabs[0]
        });
        let surface = mux.surface(sid).unwrap();
        assert_eq!(surface.name().as_deref(), Some("worker-9"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(text)) = surface.try_with_terminal(|t| t.plain_text()) {
                if text.contains("MTYX_LAYOUT_ARGV_OK") {
                    saw = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(saw, "recorded argv + env should have produced the marker");
    }

    #[test]
    fn apply_layout_failure_names_pane_index_and_id() {
        let mux = test_mux();
        let bad_tab = LayoutTab::Pty {
            name: None,
            cwd: None,
            command: Some(vec!["/nonexistent/mtyx-layout-fail".to_string()]),
            env: BTreeMap::new(),
        };
        let doc = layout_doc_with(
            "faildoc",
            LayoutNode::Split {
                dir: LayoutDir::Right,
                ratio: 0.5,
                a: Box::new(LayoutNode::Leaf { pane: 0 }),
                b: Box::new(LayoutNode::Leaf { pane: 1 }),
            },
            vec![
                LayoutPane { name: None, active_tab: 0, tabs: vec![layout_cat_tab()] },
                LayoutPane { name: None, active_tab: 0, tabs: vec![layout_cat_tab(), bad_tab] },
            ],
        );
        let err = mux.apply_layout("faildoc", &doc).unwrap_err().to_string();
        assert!(err.contains("pane 1"), "error should name pane index 1: {err}");
        // The already-created pane id is named too (AC7).
        let pane_id = mux.with_state(|s| {
            let mut ids = Vec::new();
            s.workspaces[0].screens[0].root.pane_ids(&mut ids);
            ids[1]
        });
        assert!(err.contains(&format!("pane-id {pane_id}")), "error was: {err}");
    }

    #[test]
    fn detect_agent_caches_result_and_agent_pattern_add_extends_registry() {
        use crate::agent_detect::{Confidence, PatternKind};
        use std::time::{Duration, Instant};

        let mux = test_mux();
        mux.new_workspace(None, None).unwrap();
        let surface_id = mux.with_state(|s| {
            let pane = s.workspaces[0].screens[0].active_pane;
            s.panes[&pane].tabs[0]
        });
        let surface = mux.surface(surface_id).unwrap();
        assert!(surface.detected_agent().is_none(), "no detection cached yet");

        // A user pattern extends the registry on top of the bundled one.
        let custom = AgentPattern {
            name: "myagent".into(),
            kind: PatternKind::Screen,
            pattern: "MYMARKER>".into(),
            confidence: Confidence::Medium,
            case_insensitive: false,
        };
        mux.agent_pattern_add(custom.clone()).unwrap();
        let listed = mux.agent_pattern_list().unwrap();
        assert!(listed.iter().any(|p| p.name == "myagent" && p.pattern == "MYMARKER>"));
        assert!(listed.iter().any(|p| p.name == "claude"), "bundled patterns stay listed");

        // Duplicate adds are rejected, not duplicated.
        assert!(mux.agent_pattern_add(custom.clone()).is_err());

        // /bin/cat echoes its input on the pty: write the marker and poll
        // until the echo lands on the screen, then detect.
        surface.write_bytes(b"MYMARKER> ").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut detection = Detection::unknown("no run yet");
        while Instant::now() < deadline {
            detection = mux.detect_agent(surface_id).unwrap();
            if detection.agent == "myagent" {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(detection.agent, "myagent");
        assert_eq!(detection.confidence, Some(Confidence::Medium));
        assert!(detection.evidence.contains("MYMARKER>"), "evidence: {:?}", detection.evidence);

        // The result is cached on the surface (pane_json / sidebar read it).
        assert_eq!(surface.detected_agent().unwrap().agent, "myagent");

        // Unknown surfaces error; removal drops only the custom pattern.
        assert!(mux.detect_agent(surface_id + 999).is_err());
        mux.agent_pattern_remove("myagent").unwrap();
        assert!(mux.agent_pattern_remove("myagent").is_err(), "no longer present");
        assert!(!mux.agent_pattern_list().unwrap().iter().any(|p| p.name == "myagent"));
    }

    #[test]
    fn report_agent_stores_message_and_agent_name() {
        // Issue #75: a report carries an optional agent name (for the
        // name-addressed verbs) and an optional free-text message (the
        // "last message" `list-agents` shows). The name outlives interim
        // reports that omit it; the message reflects the latest report.
        let mux = test_mux();
        mux.new_workspace(None, None).unwrap();
        let surface = mux.with_state(|s| {
            let pane = s.workspaces[0].screens[0].active_pane;
            s.panes[&pane].tabs[0]
        });

        let report = mux
            .report_agent(
                surface,
                AgentState::Working,
                AgentStateSource::Socket,
                Some("sess-9".into()),
                Some("worker-1".into()),
                Some("compiling".into()),
            )
            .unwrap();
        assert_eq!(report.agent.as_deref(), Some("worker-1"));
        assert_eq!(report.message.as_deref(), Some("compiling"));

        let agents = mux.list_agents(None, None);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.agent.as_deref(), Some("worker-1"));
        assert_eq!(agents[0].1.message.as_deref(), Some("compiling"));

        // A later report that omits both keeps the pane's name (so
        // name addressing survives interim reports) but clears the
        // message (last report wins).
        let report = mux
            .report_agent(surface, AgentState::Idle, AgentStateSource::Socket, None, None, None)
            .unwrap();
        assert_eq!(report.message, None);
        assert_eq!(report.agent.as_deref(), Some("worker-1"));

        // A report carrying a new name replaces the old one.
        let report = mux
            .report_agent(
                surface,
                AgentState::Done,
                AgentStateSource::Hook,
                None,
                Some("renamed".into()),
                None,
            )
            .unwrap();
        assert_eq!(report.agent.as_deref(), Some("renamed"));
    }

    #[test]
    fn resolve_agent_target_exact_ambiguous_and_id_fallback() {
        let mux = test_mux();
        mux.new_workspace(Some("a".into()), None).unwrap();
        mux.new_workspace(Some("b".into()), None).unwrap();
        let (s1, s2) = mux.with_state(|s| {
            let t1 = s.panes[&s.workspaces[0].screens[0].active_pane].tabs[0];
            let t2 = s.panes[&s.workspaces[1].screens[0].active_pane].tabs[0];
            (t1, t2)
        });

        // No reports yet: a name lookup fails, but numeric ids work.
        assert!(mux.resolve_agent_target("worker").is_err());
        assert_eq!(mux.resolve_agent_target(&s1.to_string()).unwrap(), s1);
        assert!(mux.resolve_agent_target("999999").is_err());

        // One surface named "worker": exact match wins, id still works.
        mux.report_agent(
            s1,
            AgentState::Working,
            AgentStateSource::Socket,
            None,
            Some("worker".into()),
            None,
        )
        .unwrap();
        assert_eq!(mux.resolve_agent_target("worker").unwrap(), s1);
        assert_eq!(mux.resolve_agent_target(&s1.to_string()).unwrap(), s1);

        // Two surfaces named "worker": ambiguous, listing both ids.
        mux.report_agent(
            s2,
            AgentState::Working,
            AgentStateSource::Socket,
            None,
            Some("worker".into()),
            None,
        )
        .unwrap();
        let err = mux.resolve_agent_target("worker").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(err.contains(&s1.to_string()) && err.contains(&s2.to_string()), "got: {err}");
    }

    #[test]
    fn move_workspace_reorders_and_tracks_active_workspace() {
        let mux = test_mux();
        mux.new_workspace(Some("one".into()), None).unwrap();
        mux.new_workspace(Some("two".into()), None).unwrap();
        mux.new_workspace(Some("three".into()), None).unwrap();
        let (ws1, ws2, ws3) =
            mux.with_state(|s| (s.workspaces[0].id, s.workspaces[1].id, s.workspaces[2].id));

        assert!(mux.move_workspace(ws3, 0));
        mux.with_state(|s| {
            assert_eq!(
                s.workspaces.iter().map(|ws| ws.id).collect::<Vec<_>>(),
                vec![ws3, ws1, ws2]
            );
            assert_eq!(s.active_workspace, 0);
        });

        assert!(mux.move_workspace(ws1, 99));
        mux.with_state(|s| {
            assert_eq!(
                s.workspaces.iter().map(|ws| ws.id).collect::<Vec<_>>(),
                vec![ws3, ws2, ws1]
            );
            assert_eq!(s.active_workspace, 0);
        });
    }

    // --- Per-pane git worktrees (issue #77) ---

    /// A temp git repo with one commit, for worktree tests. `git` is
    /// part of the pinned dev environment (AGENTS.md; CI images).
    fn temp_git_repo(name: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let dir = std::env::temp_dir().join(format!(
            "mtyx-mux-wt-{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::process::Command::new("git").arg("init").arg(&dir).output().unwrap();
        assert!(out.status.success(), "git init failed: {}", String::from_utf8_lossy(&out.stderr));
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=mtyx@test", "-c", "user.name=mtyx"])
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir
    }

    /// A workspace whose pane's active tab is parked inside `repo`, so
    /// worktree ops can resolve the repository from the pane's cwd.
    fn pane_parked_in_repo(mux: &Arc<Mux>, repo: &std::path::Path) -> PaneId {
        let s1 = mux.new_workspace(None, None).unwrap();
        let pane = mux.with_state(|s| s.pane_of(s1.id).unwrap());
        mux.new_tab(Some(pane), Some(repo.to_string_lossy().into_owned()), None).unwrap();
        pane
    }

    #[test]
    fn pane_worktree_create_records_and_lists_history() {
        let mux = test_mux();
        let repo = temp_git_repo("create-list");
        let pane = pane_parked_in_repo(&mux, &repo);

        let r1 = mux.pane_worktree_create(pane, "feat-auth", Some("auth".into())).unwrap();
        assert_eq!(r1.branch, "feat-auth");
        assert_eq!(r1.label.as_deref(), Some("auth"));
        // Default pattern: <repo>/../<repo>.<branch>/ (issue #77 AC1/AC6).
        assert!(
            r1.path.ends_with(".feat-auth"),
            "default pattern should be <repo>.<branch>, got {}",
            r1.path
        );
        let wt1 = std::path::PathBuf::from(&r1.path);
        assert!(wt1.is_dir(), "worktree {} should exist on disk", r1.path);
        assert!(wt1.join(".git").is_file(), "linked worktree carries a .git file");
        assert!(r1.created_at_ms > 0);

        // A pane accumulates worktrees over its lifetime (AC2).
        let r2 = mux.pane_worktree_create(pane, "feat-docs", None).unwrap();
        let list = mux.pane_worktree_list(pane).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].branch, "feat-auth");
        assert_eq!(list[1].branch, "feat-docs");
        assert_eq!(list[1].label, None);
        assert!(mux.pane_worktree_list(9999).is_err(), "unknown pane should error");

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&r1.path);
        let _ = std::fs::remove_dir_all(&r2.path);
    }

    #[test]
    fn pane_worktree_remove_prunes_and_drops_record() {
        let mux = test_mux();
        let repo = temp_git_repo("remove");
        let pane = pane_parked_in_repo(&mux, &repo);
        let record = mux.pane_worktree_create(pane, "feat-auth", None).unwrap();
        // The pane's active tab is parked in the REPO (not the
        // worktree), so remove is allowed.

        mux.pane_worktree_remove(pane, "feat-auth").unwrap();
        assert!(mux.pane_worktree_list(pane).unwrap().is_empty());
        assert!(!std::path::PathBuf::from(&record.path).exists(), "worktree dir should be gone");
        // git's own registry no longer knows the branch (remove + prune).
        let out = std::process::Command::new("git")
            .args(["worktree", "list"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listing.contains("feat-auth"),
            "git worktree list should drop feat-auth: {listing}"
        );

        // Removing again finds no record.
        assert!(mux.pane_worktree_remove(pane, "feat-auth").is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn new_tab_with_branch_spawns_inside_worktree() {
        let mux = test_mux();
        let repo = temp_git_repo("new-tab-branch");
        let s1 = mux.new_workspace(None, None).unwrap();
        let pane = mux.with_state(|s| s.pane_of(s1.id).unwrap());

        // AC4: the worktree is created BEFORE the surface spawns, so the
        // pane starts inside it by construction.
        let (surface, record) = mux
            .new_tab_with_worktree(
                Some(pane),
                Some(repo.to_string_lossy().into_owned()),
                None,
                "feat-nt",
                Some("auth".into()),
            )
            .unwrap();
        assert_eq!(surface.cwd().as_deref(), Some(record.path.as_str()));
        assert!(std::path::PathBuf::from(&record.path).is_dir());
        let list = mux.pane_worktree_list(pane).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].branch, "feat-nt");
        assert_eq!(list[0].label.as_deref(), Some("auth"));

        // Split with a branch: the NEW pane starts inside its worktree
        // and owns the record.
        let (s2, rec2) =
            mux.split_with_worktree(pane, SplitDir::Right, None, "feat-split", None).unwrap();
        assert_eq!(s2.cwd().as_deref(), Some(rec2.path.as_str()));
        let p2 = mux.with_state(|s| s.pane_of(s2.id).unwrap());
        assert_eq!(mux.pane_worktree_list(p2).unwrap().len(), 1);
        assert_eq!(
            mux.pane_worktree_list(pane).unwrap().len(),
            1,
            "the split's worktree record belongs to the new pane"
        );

        // Empty session: a branch-aware new tab creates the workspace
        // with its pane already inside the worktree.
        let mux2 = test_mux();
        let (s3, rec3) = mux2
            .new_tab_with_worktree(
                None,
                Some(repo.to_string_lossy().into_owned()),
                None,
                "feat-empty",
                None,
            )
            .unwrap();
        assert_eq!(s3.cwd().as_deref(), Some(rec3.path.as_str()));
        let p3 = mux2.with_state(|s| s.pane_of(s3.id).unwrap());
        assert_eq!(mux2.pane_worktree_list(p3).unwrap().len(), 1);

        // A worktree failure propagates and spawns nothing.
        let before = mux2.with_state(|s| s.surfaces.len());
        assert!(mux2
            .new_tab_with_worktree(
                None,
                Some(repo.to_string_lossy().into_owned()),
                None,
                "bad..name",
                None
            )
            .is_err());
        assert_eq!(mux2.with_state(|s| s.surfaces.len()), before);

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&record.path);
        let _ = std::fs::remove_dir_all(&rec2.path);
        let _ = std::fs::remove_dir_all(&rec3.path);
    }

    #[test]
    fn pane_worktree_create_failure_leaves_no_record() {
        let mux = test_mux();
        let repo = temp_git_repo("create-failure");
        let pane = pane_parked_in_repo(&mux, &repo);

        let err = mux.pane_worktree_create(pane, "bad..name", None).unwrap_err();
        assert!(
            err.to_string().contains("not a valid branch name"),
            "git's error should propagate (AC7), got: {err}"
        );
        assert!(mux.pane_worktree_list(pane).unwrap().is_empty(), "no record on failure (AC7)");
        // The pane's working directory is unchanged.
        let cwd = mux.with_state(|s| {
            let active = s.panes[&pane].active_surface().unwrap();
            s.surfaces[&active].cwd()
        });
        assert_eq!(
            cwd.as_deref(),
            Some(repo.to_string_lossy().as_ref()),
            "pane cwd unchanged (AC7)"
        );

        // Not-in-a-repo also propagates cleanly.
        let bare = std::env::temp_dir().join(format!("mtyx-mux-wt-bare-{}", std::process::id()));
        std::fs::create_dir_all(&bare).unwrap();
        mux.new_tab(Some(pane), Some(bare.to_string_lossy().into_owned()), None).unwrap();
        let err = mux.pane_worktree_create(pane, "feat-x", None).unwrap_err();
        assert!(err.to_string().contains("not inside a git repository"), "got: {err}");
        assert!(mux.pane_worktree_list(pane).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&bare);
    }
}
