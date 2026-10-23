//! Read-only tree snapshots shared by the renderer and input handling,
//! plus the JSON parser for the remote `list-workspaces` shape.

use mux_core::{
    assign_short_ids, AgentState, AgentStateSource, BrowserSource, Node, PaneId, ScreenId,
    SplitDir, State, SurfaceId, SurfaceKind, WorkspaceId,
};
use ratatui::style::Color;
use serde_json::Value;

use crate::config::parse_color;

#[derive(Clone, Default)]
pub struct TreeView {
    pub workspaces: Vec<WorkspaceView>,
    pub active_workspace: usize,
}

#[derive(Clone)]
pub struct WorkspaceView {
    pub id: WorkspaceId,
    pub short_id: String,
    pub name: String,
    /// User-assigned sidebar rail color, if any.
    pub color: Option<Color>,
    /// User-assigned status icon glyph, if any.
    pub icon: Option<String>,
    pub screens: Vec<ScreenView>,
    pub active_screen: usize,
}

#[derive(Clone)]
pub struct ScreenView {
    pub id: ScreenId,
    #[allow(dead_code)]
    pub short_id: String,
    /// User-assigned name, if any (display falls back to the number).
    pub name: Option<String>,
    pub layout: Node,
    pub active_pane: PaneId,
    pub panes: Vec<PaneView>,
    pub zoomed: Option<PaneId>,
}

#[derive(Clone)]
pub struct PaneView {
    pub id: PaneId,
    pub short_id: String,
    /// User-assigned name, if any (display falls back to the active
    /// tab's title).
    pub name: Option<String>,
    pub tabs: Vec<TabView>,
    pub active_tab: usize,
    /// Git worktrees attached to the pane over its lifetime, in
    /// creation order (issue #77). Mirrors `mux_core::WorktreeRecord`
    /// as JSON (`list-workspaces` / `pane-worktree-list`).
    pub worktrees: Vec<PaneWorktreeView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneWorktreeView {
    pub branch: String,
    pub path: String,
    pub label: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Clone)]
pub struct TabView {
    pub surface: SurfaceId,
    pub short_id: String,
    pub name: Option<String>,
    pub title: String,
    pub cwd: Option<String>,
    pub agent_state: Option<AgentState>,
    /// The applied report's source tier (issue #140 AC6, protocol 8):
    /// heuristic / detected / socket / hook. `None` when no report or an
    /// older server omits `agent_source`, so the sidebar can render
    /// heuristic-grade state distinctly (hollow chip) from hook-grade.
    pub agent_source: Option<AgentStateSource>,
    /// The agent session id reported on this surface (the same string
    /// `list-agents` reports as `session`), if any. Surfaced here so the
    /// fuzzy finder can index it for type-ahead search.
    pub agent_session: Option<String>,
    /// The agent name from the last ambient detection (issue #78),
    /// cached by `detect-agent`. `None` when never detected or detected
    /// as `unknown` (no badge).
    pub agent_name: Option<String>,
    /// Last agent message (issue #75): the `--message` free-text from
    /// the surface's latest applied agent report. The status string
    /// itself is derivable (`agent_state` or "unknown").
    #[allow(dead_code)]
    pub agent_message: Option<String>,
    pub kind: SurfaceKind,
    pub browser_source: Option<BrowserSource>,
    pub browser_frames_stalled: bool,
}

impl TreeView {
    pub fn active_workspace(&self) -> Option<&WorkspaceView> {
        self.workspaces.get(self.active_workspace)
    }

    /// The active screen of the active workspace.
    pub fn active_screen(&self) -> Option<&ScreenView> {
        self.active_workspace()?.active_screen_ref()
    }

    pub fn pane(&self, id: PaneId) -> Option<&PaneView> {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.screens.iter())
            .flat_map(|screen| screen.panes.iter())
            .find(|p| p.id == id)
    }

    /// The active surface of the active pane of the active screen.
    pub fn active_surface(&self) -> Option<SurfaceId> {
        let screen = self.active_screen()?;
        screen.pane(screen.active_pane)?.active_surface()
    }

    pub fn surface_kind(&self, id: SurfaceId) -> SurfaceKind {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.screens.iter())
            .flat_map(|screen| screen.panes.iter())
            .flat_map(|pane| pane.tabs.iter())
            .find(|tab| tab.surface == id)
            .map(|tab| tab.kind)
            .unwrap_or(SurfaceKind::Pty)
    }

    /// A human label for a surface, for contexts (like desktop
    /// notifications) that need to identify a pane outside the sidebar's
    /// own rendering: `"<workspace> · <tab title or agent name>"`.
    pub fn tab_label(&self, id: SurfaceId) -> Option<String> {
        self.workspaces.iter().find_map(|ws| {
            let tab = ws
                .screens
                .iter()
                .flat_map(|screen| screen.panes.iter())
                .flat_map(|pane| pane.tabs.iter())
                .find(|tab| tab.surface == id)?;
            let title = tab.name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&tab.title);
            Some(if title.is_empty() { ws.name.clone() } else { format!("{} · {title}", ws.name) })
        })
    }
}

impl WorkspaceView {
    pub fn active_screen_ref(&self) -> Option<&ScreenView> {
        self.screens.get(self.active_screen)
    }
}

impl ScreenView {
    pub fn pane(&self, id: PaneId) -> Option<&PaneView> {
        self.panes.iter().find(|p| p.id == id)
    }

    /// Display name: the user-assigned name, else "screen N" by position.
    pub fn display_name(&self, index: usize) -> String {
        match self.name.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => format!("{}", index + 1),
        }
    }
}

impl PaneView {
    pub fn active_surface(&self) -> Option<SurfaceId> {
        self.tabs.get(self.active_tab).map(|t| t.surface)
    }

    /// Display name: the user-assigned name, else the active tab's
    /// process title, else "shell".
    pub fn display_name(&self) -> &str {
        if let Some(name) = self.name.as_deref() {
            if !name.is_empty() {
                return name;
            }
        }
        self.tabs
            .get(self.active_tab)
            .map(|t| if t.title.is_empty() { "shell" } else { t.title.as_str() })
            .unwrap_or("shell")
    }

    /// Working directory of the active tab, if known.
    pub fn active_cwd(&self) -> Option<&str> {
        self.tabs.get(self.active_tab)?.cwd.as_deref()
    }

    /// Reported agent state of the active tab, if any.
    pub fn active_agent_state(&self) -> Option<AgentState> {
        self.tabs.get(self.active_tab)?.agent_state
    }

    /// Source tier of the active tab's applied agent report, if any
    /// (issue #140 AC6).
    pub fn active_agent_source(&self) -> Option<AgentStateSource> {
        self.tabs.get(self.active_tab)?.agent_source
    }

    /// Detected agent name of the active tab, if any (issue #78 AC6).
    pub fn active_agent_name(&self) -> Option<&str> {
        self.tabs.get(self.active_tab)?.agent_name.as_deref()
    }

    /// The pane's most recently created worktree record (issue #77):
    /// the one the sidebar badge shows (`feat-auth: abc1234`).
    pub fn active_worktree(&self) -> Option<&PaneWorktreeView> {
        self.worktrees.last()
    }
}

/// Snapshot a local mux state into a TreeView.
pub fn tree_from_state(state: &State) -> TreeView {
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
    let pane_view = |id: &PaneId| {
        state.panes.get(id).map(|pane| PaneView {
            id: pane.id,
            short_id: short_ids.get(&pane.id).cloned().unwrap_or_default(),
            name: pane.name.clone(),
            active_tab: pane.active_tab,
            worktrees: pane
                .worktrees
                .iter()
                .map(|record| PaneWorktreeView {
                    branch: record.branch.clone(),
                    path: record.path.clone(),
                    label: record.label.clone(),
                    created_at_ms: record.created_at_ms,
                })
                .collect(),
            tabs: pane
                .tabs
                .iter()
                .map(|sid| TabView {
                    surface: *sid,
                    short_id: short_ids.get(sid).cloned().unwrap_or_default(),
                    name: state.surfaces.get(sid).and_then(|s| s.name()),
                    title: state.surfaces.get(sid).map(|s| s.title()).unwrap_or_default(),
                    cwd: state.surfaces.get(sid).and_then(|s| s.cwd()),
                    agent_state: state
                        .surfaces
                        .get(sid)
                        .and_then(|s| s.agent_report())
                        .map(|r| r.state),
                    agent_source: state
                        .surfaces
                        .get(sid)
                        .and_then(|s| s.agent_report())
                        .map(|r| r.source),
                    agent_session: state
                        .surfaces
                        .get(sid)
                        .and_then(|s| s.agent_report())
                        .and_then(|r| r.session.clone()),
                    // Issue #78: the cached ambient detection; a cached
                    // `unknown` reads as no badge.
                    agent_name: state
                        .surfaces
                        .get(sid)
                        .and_then(|s| s.detected_agent())
                        .filter(|d| !d.is_unknown())
                        .map(|d| d.agent),
                    agent_message: state
                        .surfaces
                        .get(sid)
                        .and_then(|s| s.agent_report())
                        .and_then(|r| r.message.clone()),
                    kind: state.surfaces.get(sid).map(|s| s.kind()).unwrap_or(SurfaceKind::Pty),
                    browser_source: state.surfaces.get(sid).and_then(|s| s.browser_source()),
                    browser_frames_stalled: state
                        .surfaces
                        .get(sid)
                        .and_then(|s| s.browser_frames_stalled())
                        .unwrap_or(false),
                })
                .collect(),
        })
    };
    TreeView {
        active_workspace: state.active_workspace,
        workspaces: state
            .workspaces
            .iter()
            .map(|ws| WorkspaceView {
                id: ws.id,
                short_id: short_ids.get(&ws.id).cloned().unwrap_or_default(),
                name: ws.name.clone(),
                color: ws.color.map(|c| Color::Rgb(c.r, c.g, c.b)),
                icon: ws.icon.as_ref().map(|icon| icon.as_str().to_string()),
                active_screen: ws.active_screen,
                screens: ws
                    .screens
                    .iter()
                    .map(|screen| {
                        let mut pane_ids = Vec::new();
                        screen.root.pane_ids(&mut pane_ids);
                        ScreenView {
                            id: screen.id,
                            short_id: short_ids.get(&screen.id).cloned().unwrap_or_default(),
                            name: screen.name.clone(),
                            layout: screen.root.clone(),
                            active_pane: screen.active_pane,
                            panes: pane_ids.iter().filter_map(pane_view).collect(),
                            zoomed: screen.zoomed,
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn parse_layout(value: &Value) -> Option<Node> {
    match value.get("type")?.as_str()? {
        "leaf" => Some(Node::Leaf(value.get("pane")?.as_u64()?)),
        "split" => {
            let dir = match value.get("dir")?.as_str()? {
                "right" => SplitDir::Right,
                "down" => SplitDir::Down,
                _ => return None,
            };
            Some(Node::Split {
                dir,
                ratio: value.get("ratio")?.as_f64()? as f32,
                a: Box::new(parse_layout(value.get("a")?)?),
                b: Box::new(parse_layout(value.get("b")?)?),
            })
        }
        _ => None,
    }
}

fn parse_pane(value: &Value) -> Option<PaneView> {
    Some(PaneView {
        id: value.get("id")?.as_u64()?,
        short_id: value.get("short_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        name: value.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
        active_tab: value.get("active_tab").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        worktrees: value
            .get("worktrees")
            .and_then(|v| v.as_array())
            .map(|worktrees| {
                worktrees
                    .iter()
                    .filter_map(|worktree| {
                        Some(PaneWorktreeView {
                            branch: worktree.get("branch")?.as_str()?.to_string(),
                            path: worktree.get("path")?.as_str()?.to_string(),
                            label: worktree
                                .get("label")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            created_at_ms: worktree
                                .get("created_at_ms")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        tabs: value
            .get("tabs")
            .and_then(|v| v.as_array())
            .map(|tabs| {
                tabs.iter()
                    .filter_map(|tab| {
                        Some(TabView {
                            surface: tab.get("surface")?.as_u64()?,
                            short_id: tab
                                .get("short_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            name: tab.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                            title: tab
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            cwd: tab.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string()),
                            agent_state: tab
                                .get("agent_state")
                                .and_then(|v| v.as_str())
                                .and_then(AgentState::parse),
                            agent_source: tab
                                .get("agent_source")
                                .and_then(|v| v.as_str())
                                .and_then(AgentStateSource::parse),
                            agent_session: tab
                                .get("agent_session")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            agent_name: tab
                                .get("agent_name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            agent_message: tab
                                .get("agent_message")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            kind: match tab.get("kind").and_then(|v| v.as_str()) {
                                Some("browser") => SurfaceKind::Browser,
                                _ => SurfaceKind::Pty,
                            },
                            browser_source: match tab.get("browser_source").and_then(|v| v.as_str())
                            {
                                Some("external") => Some(BrowserSource::External),
                                Some("launched") => Some(BrowserSource::Launched),
                                _ => None,
                            },
                            browser_frames_stalled: tab
                                .get("browser_frames_stalled")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn parse_screen(value: &Value) -> Option<ScreenView> {
    Some(ScreenView {
        id: value.get("id")?.as_u64()?,
        short_id: value.get("short_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        name: value.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
        layout: value.get("layout").and_then(parse_layout)?,
        active_pane: value.get("active_pane").and_then(|v| v.as_u64()).unwrap_or(0),
        zoomed: value.get("zoomed").and_then(|v| v.as_u64()),
        panes: value
            .get("panes")
            .and_then(|v| v.as_array())
            .map(|panes| panes.iter().filter_map(parse_pane).collect())
            .unwrap_or_default(),
    })
}

/// Parse the remote `list-workspaces` response.
pub fn parse_tree(data: &Value) -> TreeView {
    let mut tree = TreeView::default();
    let Some(workspaces) = data.get("workspaces").and_then(|v| v.as_array()) else {
        return tree;
    };
    for (i, ws) in workspaces.iter().enumerate() {
        if ws.get("active").and_then(|v| v.as_bool()) == Some(true) {
            tree.active_workspace = i;
        }
        let mut view = WorkspaceView {
            id: ws.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
            short_id: ws.get("short_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            name: ws.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            color: ws.get("color").and_then(|v| v.as_str()).and_then(parse_color),
            icon: ws.get("icon").and_then(|v| v.as_str()).map(str::to_string),
            screens: Vec::new(),
            active_screen: 0,
        };
        if let Some(screens) = ws.get("screens").and_then(|v| v.as_array()) {
            for (s, screen) in screens.iter().enumerate() {
                if screen.get("active").and_then(|v| v.as_bool()) == Some(true) {
                    view.active_screen = s;
                }
                if let Some(parsed) = parse_screen(screen) {
                    view.screens.push(parsed);
                }
            }
        }
        tree.workspaces.push(view);
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_core::AgentState;
    use serde_json::json;

    #[test]
    fn parse_tree_reads_workspace_color_as_rgb() {
        let data = json!({
            "workspaces": [
                {"id": 1, "name": "red", "color": "#ff0000", "active": true, "screens": []},
                {"id": 2, "name": "none", "color": null, "active": false, "screens": []},
            ]
        });
        let tree = parse_tree(&data);
        assert_eq!(tree.workspaces[0].color, Some(Color::Rgb(0xff, 0x00, 0x00)));
        assert_eq!(tree.workspaces[0].icon, None);
        assert_eq!(tree.workspaces[1].color, None);
    }

    #[test]
    fn parse_tree_reads_workspace_icon() {
        let tree = parse_tree(&json!({
            "workspaces": [{"id": 1, "name": "bot", "icon": "🤖", "screens": []}]
        }));
        assert_eq!(tree.workspaces[0].icon.as_deref(), Some("🤖"));
    }

    #[test]
    fn parse_tree_treats_missing_color_key_as_none() {
        let data = json!({
            "workspaces": [
                {"id": 1, "name": "no-color-key", "active": true, "screens": []},
            ]
        });
        let tree = parse_tree(&data);
        assert_eq!(tree.workspaces[0].color, None);
    }

    /// Issue #140 AC6: the pane snapshot's `agent_source` (protocol 8)
    /// round-trips through `parse_tree`, so a remote-attached TUI can
    /// render heuristic-grade state distinctly from hook-grade. Absent
    /// on older servers → `None`.
    #[test]
    fn parse_tree_reads_tab_agent_source() {
        let tree = parse_tree(&json!({
            "workspaces": [{
                "id": 1, "name": "ag", "active": true,
                "screens": [{
                    "id": 2, "name": null, "active": true,
                    "layout": {"type": "leaf", "pane": 3},
                    "active_pane": 3,
                    "panes": [{
                        "id": 3, "short_id": "3", "name": null, "active_tab": 0,
                        "tabs": [
                            {"surface": 4, "title": "pi", "agent_state": "idle",
                             "agent_source": "heuristic"},
                            {"surface": 5, "title": "claude", "agent_state": "working",
                             "agent_source": "hook"},
                            {"surface": 6, "title": "old", "agent_state": "blocked"},
                        ],
                        "worktrees": []
                    }]
                }]
            }]
        }));
        let pane = &tree.workspaces[0].screens[0].panes[0];
        assert_eq!(pane.active_agent_source(), Some(mux_core::AgentStateSource::Heuristic));
        assert_eq!(pane.tabs[1].agent_source, Some(mux_core::AgentStateSource::Hook));
        assert_eq!(pane.tabs[2].agent_source, None, "absent key stays None (older server)");
    }

    #[test]
    fn parse_tree_reads_pane_worktrees() {
        // The server's `pane_json` serialises `worktrees` per pane
        // (issue #77); a remote-attach client must read them back so
        // the sidebar badge survives a thin attach. The last record is
        // the "active" one the badge shows.
        let tree = parse_tree(&json!({
            "workspaces": [{
                "id": 1, "name": "wt", "active": true,
                "screens": [{
                    "id": 2, "name": null, "active": true,
                    "layout": {"type": "leaf", "pane": 3},
                    "active_pane": 3,
                    "panes": [{
                        "id": 3, "short_id": "3", "name": null, "active_tab": 0,
                        "tabs": [],
                        "worktrees": [
                            {"branch": "feat-auth", "path": "/tmp/proj.feat-auth", "label": "auth", "created_at_ms": 42},
                            {"branch": "feat-docs", "path": "/tmp/proj.feat-docs", "label": null, "created_at_ms": 43},
                        ]
                    }]
                }]
            }]
        }));
        let pane = &tree.workspaces[0].screens[0].panes[0];
        assert_eq!(pane.worktrees.len(), 2);
        assert_eq!(pane.worktrees[0].branch, "feat-auth");
        assert_eq!(pane.worktrees[0].label.as_deref(), Some("auth"));
        assert_eq!(pane.worktrees[1].created_at_ms, 43);
        let active = pane.active_worktree().unwrap();
        assert_eq!(active.branch, "feat-docs", "the latest record is the active worktree");

        // Absent key (older server) reads as an empty list, not an error.
        let tree = parse_tree(&json!({
            "workspaces": [{
                "id": 1, "name": "old", "active": true,
                "screens": [{
                    "id": 2, "name": null, "active": true,
                    "layout": {"type": "leaf", "pane": 3},
                    "active_pane": 3,
                    "panes": [{"id": 3, "short_id": "3", "name": null, "active_tab": 0, "tabs": []}]
                }]
            }]
        }));
        assert!(tree.workspaces[0].screens[0].panes[0].worktrees.is_empty());
        assert!(tree.workspaces[0].screens[0].panes[0].active_worktree().is_none());
    }

    #[test]
    fn parse_tree_reads_agent_session_and_state_per_tab() {
        // Mirrors the `list-workspaces` JSON shape the server serialises
        // (server.rs `pane_json`): each tab carries `agent_state` AND
        // `agent_session` alongside the chrome fields. The client's
        // tree-parsing must read the session id back so a remote-attach
        // client can index it for the fuzzy finder (AC2).
        let data = json!({
            "workspaces": [{
                "id": 1, "name": "agents", "active": true,
                "screens": [{
                    "id": 2, "short_id": "2", "name": null, "active": true,
                    "layout": {"type": "leaf", "pane": 3},
                    "active_pane": 3,
                    "panes": [{
                        "id": 3, "short_id": "3", "name": null, "active_tab": 0,
                        "tabs": [{
                            "surface": 4, "short_id": "4", "name": null,
                            "title": "shell", "cwd": "/tmp",
                            "agent_state": "working",
                            "agent_session": "sess-abc-123",
                            "kind": "pty", "dead": false
                        }, {
                            "surface": 5, "short_id": "5", "name": null,
                            "title": "editor", "cwd": "/tmp",
                            "agent_state": null,
                            "kind": "pty", "dead": false
                        }]
                    }]
                }]
            }]
        });
        let tree = parse_tree(&data);
        let tabs = &tree.workspaces[0].screens[0].panes[0].tabs;
        assert_eq!(tabs[0].surface, 4);
        assert_eq!(tabs[0].agent_state, Some(AgentState::Working));
        assert_eq!(tabs[0].agent_session.as_deref(), Some("sess-abc-123"));
        // A tab with no agent session reported reads back as None.
        assert_eq!(tabs[1].agent_state, None);
        assert_eq!(tabs[1].agent_session, None);
    }

    /// Issue #78 AC6: `pane_json` carries the cached detection as
    /// `agent_name` per tab; the tree parser must read it back so the
    /// sidebar badge also works on a remote attach. `PaneView::
    /// active_agent_name` surfaces the active tab's badge name.
    #[test]
    fn parse_tree_reads_agent_name_per_tab() {
        let data = json!({
            "workspaces": [{
                "id": 1, "name": "agents", "active": true,
                "screens": [{
                    "id": 2, "short_id": "2", "name": null, "active": true,
                    "layout": {"type": "leaf", "pane": 3},
                    "active_pane": 3,
                    "panes": [{
                        "id": 3, "short_id": "3", "name": null, "active_tab": 0,
                        "tabs": [{
                            "surface": 4, "short_id": "4", "name": null,
                            "title": "shell", "cwd": "/tmp",
                            "agent_name": "claude",
                            "kind": "pty", "dead": false
                        }, {
                            "surface": 5, "short_id": "5", "name": null,
                            "title": "editor", "cwd": "/tmp",
                            "kind": "pty", "dead": false
                        }]
                    }]
                }]
            }]
        });
        let tree = parse_tree(&data);
        let pane = &tree.workspaces[0].screens[0].panes[0];
        assert_eq!(pane.tabs[0].agent_name.as_deref(), Some("claude"));
        assert_eq!(pane.active_agent_name(), Some("claude"));
        // A tab that was never detected reads back as None (no badge).
        assert_eq!(pane.tabs[1].agent_name, None);
    }

    #[test]
    fn parse_tree_reads_agent_status_fields() {
        // Issue #75 AC6: `pane_json` additionally emits `agent_status`
        // (always a string, default "unknown"), `agent_message`, and
        // `agent_updated_at_ms` per tab. The client keeps deriving the
        // state from `agent_state` but parses `agent_message` back so a
        // remote-attach client can surface the last agent message.
        let data = json!({
            "workspaces": [{
                "id": 1, "name": "agents", "active": true,
                "screens": [{
                    "id": 2, "short_id": "2", "name": null, "active": true,
                    "layout": {"type": "leaf", "pane": 3},
                    "active_pane": 3,
                    "panes": [{
                        "id": 3, "short_id": "3", "name": null, "active_tab": 0,
                        "tabs": [{
                            "surface": 4, "short_id": "4", "name": null,
                            "title": "builder", "cwd": "/tmp",
                            "agent_state": "blocked",
                            "agent_status": "blocked",
                            "agent_message": "needs input",
                            "agent_updated_at_ms": 1710000000000u64,
                            "kind": "pty", "dead": false
                        }, {
                            "surface": 5, "short_id": "5", "name": null,
                            "title": "shell", "cwd": "/tmp",
                            "agent_state": null,
                            "agent_status": "unknown",
                            "agent_message": null,
                            "agent_updated_at_ms": null,
                            "kind": "pty", "dead": false
                        }]
                    }]
                }]
            }]
        });
        let tree = parse_tree(&data);
        let tabs = &tree.workspaces[0].screens[0].panes[0].tabs;
        assert_eq!(tabs[0].agent_state, Some(AgentState::Blocked));
        assert_eq!(tabs[0].agent_message.as_deref(), Some("needs input"));
        // Bare tab: no state, no message.
        assert_eq!(tabs[1].agent_state, None);
        assert_eq!(tabs[1].agent_message, None);
    }
}
