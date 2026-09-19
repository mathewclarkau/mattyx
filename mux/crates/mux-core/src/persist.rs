//! Session tree snapshots: capturing enough of `State` to reconstruct
//! workspace/screen/pane layout (and each tab's cwd) after a restart, and
//! replaying that reconstruction against a fresh [`crate::Mux`].
//!
//! Scope, deliberately: layout (split tree shape + ratios), names, cwd,
//! and active selections restore exactly. A tab's *command* does not —
//! every restored tab is the user's default shell, `cd`'d into its
//! recorded cwd (see [`crate::Mux::restore`]). Threading an arbitrary
//! command through `new_workspace`/`new_screen`/`split`'s spawn calls
//! would be a reasonable follow-up; this keeps the first version to what
//! the existing spawn APIs already support cleanly.
//!
//! Remote (`cmuxd-remote`) tabs are a partial exception: a workspace's
//! very first pane's first tab reattaches to the *same remote session*
//! (via `Mux::new_remote_workspace`, not the local-shell path at all) —
//! see `Mux::restore_workspace`. A remote tab anywhere else in the tree
//! (a second tab in a pane, or any pane created by a split) has no such
//! path today - `new_tab`/`split` only ever spawn local shells - so it's
//! captured faithfully but restored as an ordinary local tab, with a
//! `MuxEvent::Status` noting the downgrade rather than failing silently.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Node, Screen, State};
use crate::{PaneId, Rgb, SplitDir};

/// Issue #95: how many rename-aside recovery copies to retain per session.
/// Older ones are pruned on each new backup, so a session can never
/// accumulate unbounded copies of a snapshot that keeps failing to parse.
pub const BACKUP_KEEP: usize = 5;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum DirSnapshot {
    Right,
    Down,
}

impl From<SplitDir> for DirSnapshot {
    fn from(dir: SplitDir) -> Self {
        match dir {
            SplitDir::Right => DirSnapshot::Right,
            SplitDir::Down => DirSnapshot::Down,
        }
    }
}

impl From<DirSnapshot> for SplitDir {
    fn from(dir: DirSnapshot) -> Self {
        match dir {
            DirSnapshot::Right => SplitDir::Right,
            DirSnapshot::Down => SplitDir::Down,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum LayoutSnapshot {
    /// Index into the owning [`ScreenSnapshot::panes`].
    Leaf(usize),
    Split {
        dir: DirSnapshot,
        ratio: f32,
        a: Box<LayoutSnapshot>,
        b: Box<LayoutSnapshot>,
    },
}

/// Enough of a `RemoteSpec` to reattach the same `cmuxd-remote` session on
/// restore. `local_binary_path` is persisted too even though it's really
/// a cache artifact from `ssh_bootstrap` (a frontend concern, which
/// `persist.rs`/`Mux::restore_session` otherwise know nothing about) -
/// restore happens inside `mux-core`, with no Go toolchain access of its
/// own, so reusing the same cached path is the only option; if that file
/// is gone or stale, `open_remote_pty`'s upload step fails cleanly and
/// the user reconnects manually via `mtyx ssh <host>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteTabSnapshot {
    host: String,
    slot: String,
    session_id: String,
    local_binary_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TabSnapshot {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    remote: Option<RemoteTabSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PaneSnapshot {
    #[serde(default)]
    name: Option<String>,
    tabs: Vec<TabSnapshot>,
    active_tab_index: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ScreenSnapshot {
    #[serde(default)]
    name: Option<String>,
    layout: LayoutSnapshot,
    panes: Vec<PaneSnapshot>,
    active_pane_index: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceSnapshot {
    name: String,
    screens: Vec<ScreenSnapshot>,
    active_screen: usize,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    icon: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    workspaces: Vec<WorkspaceSnapshot>,
    active_workspace: usize,
}

impl SessionSnapshot {
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// Issue #95: an unloadable snapshot is never overwritten in place.
    /// If the file EXISTS but cannot be read or parsed (truncated/torn
    /// write, schema drift), it is renamed aside into
    /// `session-backups/<session>.<unix-ts>.json` (keeping the last
    /// [`BACKUP_KEEP`]) and `None` is returned so the caller starts fresh.
    /// A missing file is the ordinary first-run case: nothing to do, and
    /// nothing on disk is touched.
    pub fn load(path: &Path) -> Option<Self> {
        // Issue #87: mirror the runtime-dir discipline on the sessions
        // dir at load time too, so a pre-existing (or concurrently
        // recreated) dir is tightened before we read a snapshot out of
        // it. Best-effort: an unwritable dir should not make an existing
        // snapshot unreadable.
        if let Some(dir) = path.parent() {
            let _ = crate::platform::restrict_directory(dir);
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // Absent: the ordinary first-run case. Leave the filesystem
            // alone and report nothing to restore.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            // Present but unreadable (e.g. a torn write left invalid
            // UTF-8): preserve it rather than let the next save clobber
            // it, then start fresh.
            Err(_) => {
                backup_unloadable_snapshot(path);
                return None;
            }
        };
        match serde_json::from_str(&contents) {
            Ok(snapshot) => Some(snapshot),
            Err(_) => {
                backup_unloadable_snapshot(path);
                None
            }
        }
    }

    /// Writes atomically (write-to-temp then rename) so a crash or a
    /// concurrent read never observes a truncated file.
    ///
    /// Issue #87: snapshots name cwds, workspaces and remote SSH hosts,
    /// so the `sessions/` dir is `restrict_directory`d (0700) and the
    /// temp file `restrict_file`d (0600) before the rename lands it at
    /// the canonical name — the same discipline the runtime dir already
    /// uses. The rename preserves the temp file's mode, so the final
    /// snapshot is 0600 too.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            crate::platform::restrict_directory(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        crate::platform::restrict_file(&tmp)?;
        std::fs::rename(&tmp, path)
    }
}

/// Issue #95: the name of a session's snapshot without its extension
/// (the `<session>` half of `<session>.<ts>.json`). `None` for a path with
/// no UTF-8 file stem.
fn session_stem(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()
}

/// Issue #95: parse the unix-second timestamp out of a backup filename
/// (`<session>.<ts>.json`, or `<session>.<ts>-<n>.json` for a same-second
/// collision). Returns `None` for anything that is not this session's
/// backup, so pruning can never touch an unrelated file.
fn backup_timestamp(file_name: &str, session: &str) -> Option<u64> {
    let rest = file_name.strip_prefix(session)?.strip_prefix('.')?.strip_suffix(".json")?;
    let ts = rest.split('-').next()?;
    ts.parse().ok()
}

/// Issue #95: move an unloadable snapshot into the backup dir, never
/// deleting it, then prune that session's older copies. Returns the backup
/// path on success.
///
/// Best-effort by design: if the rename fails (e.g. a read-only state dir)
/// we return `None` and leave the original in place — the shutdown guard in
/// `Mux::write_snapshot_to` is what stops a later save from clobbering it.
pub fn backup_unloadable_snapshot(path: &Path) -> Option<PathBuf> {
    let session = session_stem(path)?;
    let dir = crate::platform::session_backup_dir_for(path);
    std::fs::create_dir_all(&dir).ok()?;
    // Issue #87 discipline, extended to the backup dir: it holds the same
    // cwds/hosts as the snapshot, so it is 0700 and each copy 0600.
    let _ = crate::platform::restrict_directory(&dir);

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // A same-second collision (two corrupt snapshots read back to back)
    // must not clobber the first backup; suffix a counter.
    let mut dest = dir.join(format!("{session}.{ts}.json"));
    let mut n = 1u32;
    while dest.exists() {
        dest = dir.join(format!("{session}.{ts}-{n}.json"));
        n += 1;
    }
    std::fs::rename(path, &dest).ok()?;
    let _ = crate::platform::restrict_file(&dest);
    prune_backups(&dir, session, BACKUP_KEEP);
    Some(dest)
}

/// Issue #95: retain the newest `keep` backups for `session`, removing
/// older ones. Only files that parse as this session's backups are ever
/// considered for removal.
pub fn prune_backups(backup_dir: &Path, session: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(backup_dir) else { return };
    let mut backups: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(ts) = backup_timestamp(name, session) {
            backups.push((ts, entry.path()));
        }
    }
    // Newest first; the path breaks ties deterministically for
    // same-second copies (`...-1`, `...-2`).
    backups.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
    for (_, stale) in backups.into_iter().skip(keep) {
        let _ = std::fs::remove_file(stale);
    }
}

pub fn capture(state: &State) -> SessionSnapshot {
    SessionSnapshot {
        active_workspace: state.active_workspace,
        workspaces: state
            .workspaces
            .iter()
            .map(|ws| WorkspaceSnapshot {
                name: ws.name.clone(),
                active_screen: ws.active_screen,
                screens: ws.screens.iter().map(|screen| capture_screen(state, screen)).collect(),
                color: ws.color.map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
                icon: ws.icon.as_ref().map(|icon| icon.as_str().to_string()),
            })
            .collect(),
    }
}

fn capture_screen(state: &State, screen: &Screen) -> ScreenSnapshot {
    let mut pane_ids = Vec::new();
    screen.root.pane_ids(&mut pane_ids);
    let index_of = |id: PaneId| pane_ids.iter().position(|&p| p == id).unwrap();

    let panes = pane_ids
        .iter()
        .map(|pid| {
            let pane = &state.panes[pid];
            PaneSnapshot {
                name: pane.name.clone(),
                active_tab_index: pane.active_tab,
                tabs: pane
                    .tabs
                    .iter()
                    .map(|sid| {
                        let surface = state.surfaces.get(sid);
                        let remote =
                            surface.and_then(|s| s.remote_spec()).map(|spec| RemoteTabSnapshot {
                                host: spec.host,
                                slot: spec.slot,
                                session_id: spec.session_id,
                                local_binary_path: spec
                                    .local_binary_path
                                    .to_string_lossy()
                                    .into_owned(),
                            });
                        TabSnapshot {
                            name: surface.and_then(|s| s.name()),
                            cwd: surface.and_then(|s| s.cwd()),
                            remote,
                        }
                    })
                    .collect(),
            }
        })
        .collect();

    ScreenSnapshot {
        name: screen.name.clone(),
        active_pane_index: index_of(screen.active_pane),
        layout: capture_node(&screen.root, &index_of),
        panes,
    }
}

fn capture_node(node: &Node, index_of: &impl Fn(PaneId) -> usize) -> LayoutSnapshot {
    match node {
        Node::Leaf(id) => LayoutSnapshot::Leaf(index_of(*id)),
        Node::Split { dir, ratio, a, b } => LayoutSnapshot::Split {
            dir: (*dir).into(),
            ratio: *ratio,
            a: Box::new(capture_node(a, index_of)),
            b: Box::new(capture_node(b, index_of)),
        },
    }
}

/// Quotes a path as a single POSIX shell word.
pub(crate) fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

pub(crate) struct RestoreWorkspace<'a> {
    pub name: &'a str,
    pub screens: Vec<RestoreScreen<'a>>,
    pub active_screen: usize,
    pub color: Option<Rgb>,
    pub icon: Option<crate::IconName>,
}

pub(crate) struct RestoreScreen<'a> {
    pub name: Option<&'a str>,
    pub layout: RestoreLayout,
    pub panes: Vec<RestorePane<'a>>,
    pub active_pane_index: usize,
}

pub(crate) enum RestoreLayout {
    Leaf(usize),
    Split { dir: SplitDir, ratio: f32, a: Box<RestoreLayout>, b: Box<RestoreLayout> },
}

pub(crate) struct RestorePane<'a> {
    pub name: Option<&'a str>,
    pub tabs: Vec<RestoreTab<'a>>,
    pub active_tab_index: usize,
}

pub(crate) struct RestoreTab<'a> {
    pub name: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub remote: Option<crate::remote_pty::RemoteSpec>,
}

/// Borrowed, engine-agnostic view of a snapshot for `Mux::restore` to
/// replay — keeps the (de)serialization types above private to this
/// module.
pub(crate) fn workspaces(snapshot: &SessionSnapshot) -> (Vec<RestoreWorkspace<'_>>, usize) {
    let workspaces = snapshot
        .workspaces
        .iter()
        .map(|ws| RestoreWorkspace {
            name: &ws.name,
            active_screen: ws.active_screen,
            color: ws.color.as_deref().and_then(|s| crate::server::parse_hex_color(s).ok()),
            icon: ws.icon.as_deref().and_then(|s| crate::server::parse_workspace_icon(s).ok()),
            screens: ws
                .screens
                .iter()
                .map(|screen| RestoreScreen {
                    name: screen.name.as_deref(),
                    active_pane_index: screen.active_pane_index,
                    layout: restore_layout(&screen.layout),
                    panes: screen
                        .panes
                        .iter()
                        .map(|pane| RestorePane {
                            name: pane.name.as_deref(),
                            active_tab_index: pane.active_tab_index,
                            tabs: pane
                                .tabs
                                .iter()
                                .map(|tab| RestoreTab {
                                    name: tab.name.as_deref(),
                                    cwd: tab.cwd.as_deref(),
                                    remote: tab.remote.as_ref().map(|r| {
                                        crate::remote_pty::RemoteSpec {
                                            host: r.host.clone(),
                                            slot: r.slot.clone(),
                                            session_id: r.session_id.clone(),
                                            local_binary_path: r.local_binary_path.clone().into(),
                                        }
                                    }),
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    (workspaces, snapshot.active_workspace)
}

fn restore_layout(layout: &LayoutSnapshot) -> RestoreLayout {
    match layout {
        LayoutSnapshot::Leaf(i) => RestoreLayout::Leaf(*i),
        LayoutSnapshot::Split { dir, ratio, a, b } => RestoreLayout::Split {
            dir: (*dir).into(),
            ratio: *ratio,
            a: Box::new(restore_layout(a)),
            b: Box::new(restore_layout(b)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir(label: &str) -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mtyx_persist_test_{}_{}_{}",
            std::process::id(),
            n,
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn shell_quote_handles_spaces_and_single_quotes() {
        assert_eq!(
            shell_quote("/home/matc/Projects/mtyx-linux"),
            "'/home/matc/Projects/mtyx-linux'"
        );
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    #[test]
    fn round_trips_through_json() {
        let snapshot = SessionSnapshot {
            active_workspace: 0,
            workspaces: vec![WorkspaceSnapshot {
                name: "work".into(),
                active_screen: 0,
                color: None,
                icon: None,
                screens: vec![ScreenSnapshot {
                    name: None,
                    active_pane_index: 1,
                    layout: LayoutSnapshot::Split {
                        dir: DirSnapshot::Right,
                        ratio: 0.5,
                        a: Box::new(LayoutSnapshot::Leaf(0)),
                        b: Box::new(LayoutSnapshot::Leaf(1)),
                    },
                    panes: vec![
                        PaneSnapshot {
                            name: None,
                            active_tab_index: 0,
                            tabs: vec![TabSnapshot {
                                name: None,
                                cwd: Some("/a".into()),
                                remote: None,
                            }],
                        },
                        PaneSnapshot {
                            name: Some("logs".into()),
                            active_tab_index: 0,
                            tabs: vec![TabSnapshot {
                                name: None,
                                cwd: Some("/b".into()),
                                remote: None,
                            }],
                        },
                    ],
                }],
            }],
        };

        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        let restored: SessionSnapshot = serde_json::from_str(&json).unwrap();
        let (workspaces, active_workspace) = self::workspaces(&restored);
        assert_eq!(active_workspace, 0);
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "work");
        assert_eq!(workspaces[0].screens[0].panes.len(), 2);
        assert_eq!(workspaces[0].screens[0].panes[1].name, Some("logs"));
        assert!(matches!(workspaces[0].screens[0].layout, RestoreLayout::Split { .. }));
    }

    /// Issue #95: an unloadable snapshot is renamed aside, not deleted,
    /// and the caller sees a fresh start.
    #[test]
    fn unloadable_snapshot_is_renamed_aside_not_deleted() {
        let dir = scratch_dir("rename_aside");
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("main.json");
        std::fs::write(&path, b"{\"workspaces\": [ truncated").unwrap();

        assert!(SessionSnapshot::load(&path).is_none(), "corrupt snapshot must not load");
        assert!(!path.exists(), "corrupt snapshot must be moved, not left to be overwritten");

        let backups: Vec<_> = std::fs::read_dir(dir.join("session-backups"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup expected, got {backups:?}");
        assert!(backups[0].starts_with("main."), "named <session>.<ts>.json: {backups:?}");
        assert!(backups[0].ends_with(".json"));
    }

    /// Issue #95: a missing file is the ordinary first-run case — no
    /// backup dir is created and nothing is touched.
    #[test]
    fn missing_snapshot_is_not_backed_up() {
        let dir = scratch_dir("missing");
        let path = dir.join("sessions").join("main.json");
        assert!(SessionSnapshot::load(&path).is_none());
        assert!(!dir.join("session-backups").exists());
    }

    /// Issue #95: pruning keeps only the newest `keep` copies and never
    /// removes a same-named file belonging to a different session.
    #[test]
    fn pruning_retains_newest_and_ignores_other_sessions() {
        let dir = scratch_dir("prune");
        let backups = dir.join("session-backups");
        std::fs::create_dir_all(&backups).unwrap();
        for ts in 1..=8u64 {
            std::fs::write(backups.join(format!("main.{ts}.json")), b"x").unwrap();
        }
        // A sibling session's backup and an unrelated file must survive.
        std::fs::write(backups.join("work.1.json"), b"x").unwrap();
        std::fs::write(backups.join("notes.txt"), b"x").unwrap();

        prune_backups(&backups, "main", 5);

        let mut remaining: Vec<u64> = std::fs::read_dir(&backups)
            .unwrap()
            .flatten()
            .filter_map(|e| backup_timestamp(&e.file_name().to_string_lossy(), "main"))
            .collect();
        remaining.sort_unstable();
        assert_eq!(remaining, vec![4, 5, 6, 7, 8], "newest five retained");
        assert!(backups.join("work.1.json").exists(), "other session untouched");
        assert!(backups.join("notes.txt").exists(), "unrelated file untouched");
    }
}
