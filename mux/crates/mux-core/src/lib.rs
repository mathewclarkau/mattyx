//! Terminal multiplexer core.
//!
//! Owns the workspace → screen → pane → tab tree and each tab's runtime
//! (a PTY child whose output feeds a libghostty-vt terminal). A workspace
//! holds screens; each screen is a binary split tree of panes; each pane
//! holds one or more tabs, and each tab is a [`Surface`]. Frontends (the
//! bundled TUI, or the mtyx app over the control socket) subscribe to
//! [`MuxEvent`]s and read surface state; they never own terminal state
//! themselves, which is what makes the backend attachable.

mod browser;
mod layout_doc;
mod model;
mod mux;
mod notify;
mod persist;
pub mod process;
pub mod remote_pty;
mod short_id;
mod surface;
pub mod worktree;

pub mod agent_detect;
pub mod agent_state_classify;
pub mod layout;
pub mod platform;
pub mod server;

/// Windows parity layer (job objects, console APIs, Toolhelp
/// snapshots). Empty on non-Windows targets.
#[cfg(windows)]
pub mod win;

/// Windows: hard-terminate one pid via `TerminateProcess` (the
/// `kill(pid, SIGKILL)` analogue). cfg-gated re-export so mux-tui's
/// kill-session path needs no direct windows-sys usage.
#[cfg(windows)]
pub fn win_terminate_pid(pid: u32) -> bool {
    win::terminate_pid(pid)
}

/// The mtyx version, resolved at build time by `build.rs` and baked
/// into the binary — it does not depend on git, a manifest, or anything
/// else being present at run time. Prefer this over
/// `env!("CARGO_PKG_VERSION")` anywhere a version is reported to a user
/// or a peer: the manifest value is a fallback floor, not the version of
/// the release this binary came from (issue #71).
///
/// Shape is `0.17.2` for a release build, `0.17.2-14-gabc1234` (with an
/// optional `-dirty`) for a build off a tag.
pub const VERSION: &str = env!("MTYX_VERSION");

pub use browser::normalize_url;
pub use layout::{
    directional_neighbor, layout_screen, split_for_pane_edge, split_sides, LayoutResult, Rect,
    SplitEdge, SplitResize,
};
pub use layout_doc::{
    capture_workspace, sanitize_filename, ApplySummary, LayoutDir, LayoutDocument, LayoutNode,
    LayoutPane, LayoutScreen, LayoutTab, LayoutWorkspace, LAYOUT_SCHEMA_VERSION,
};
pub use model::{IconName, Node, Pane, Screen, State, Workspace};
pub use mux::{CloseWorkspaceReport, Mux, MuxEvent, WorktreeChild};
pub use remote_pty::RemoteSpec;
pub use short_id::assign_short_ids;
pub use surface::{
    parse_vt_size, AgentReport, AgentState, AgentStateSource, AttachFrame, AttachStream,
    BrowserAttachState, BrowserFrame, BrowserFrameStream, BrowserSource, BrowserStatus,
    ConfirmedSendError, DefaultColors, SpawnOverrides, Surface, SurfaceKind, SurfaceOptions,
};
pub use worktree::WorktreeRecord;

pub use ghostty_vt::Rgb;

pub type SurfaceId = u64;
pub type PaneId = u64;
pub type ScreenId = u64;
pub type WorkspaceId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Split into left/right columns.
    Right,
    /// Split into top/bottom rows.
    Down,
}
