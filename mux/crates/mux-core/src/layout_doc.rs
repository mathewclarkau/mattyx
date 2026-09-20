//! Layout export/apply document (issue #76): the user-addressable JSON
//! snapshot of one workspace's tab + pane + agent-argv topology, and its
//! replay. The schema is versioned (`schema_version: 1`); see
//! `spec/layout-schema.md`.
//!
//! This is the *user-visible* counterpart of `persist.rs`'s internal
//! session snapshot: same BSP-by-pane-index shape, but it additionally
//! records each pty tab's spawn argv and env (the "boot this fleet
//! tomorrow" primitive), browser tab URLs, and remote specs. Capture
//! reads the provenance recorded at spawn time (`PtySurface::spawn_*`,
//! set from `SurfaceOptions` in `Surface::spawn`) — agents typed into a
//! shell by hand have no recoverable argv, so their tabs export with
//! `command: null` and apply restores a shell in the recorded cwd.

use std::collections::BTreeMap;

use anyhow::bail;
use serde::{Deserialize, Serialize};

use crate::model::{Node, Screen, State};
use crate::{SurfaceId, SurfaceKind, WorkspaceId};

/// The only schema version this build reads or writes. `validate`
/// hard-fails anything else so a future file fails loudly instead of
/// being misparsed (issue #76 AC4/AC7).
pub const LAYOUT_SCHEMA_VERSION: u32 = 1;

/// Env keys mtyx auto-injects (or dual-writes) into every spawn. They
/// are re-derived from the *applying* daemon's live socket path, so they
/// must never round-trip through an exported file (a stale
/// `MTYX_MUX_SOCKET` would detach the restored fleet).
const AUTO_ENV_KEYS: &[&str] = &["MTYX_MUX_SOCKET", "MTYX_SOCKET_PATH"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutDocument {
    pub schema_version: u32,
    /// The mtyx build that produced the file (`mux_core::VERSION`, never
    /// `CARGO_PKG_VERSION` — see issue #71). Informational; not gated.
    pub cmux_version: String,
    pub workspace: LayoutWorkspace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutWorkspace {
    pub name: String,
    /// `#rrggbb` or a named preset, as accepted by
    /// [`crate::server::parse_workspace_color`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub active_screen: usize,
    pub screens: Vec<LayoutScreen>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutScreen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Index into [`LayoutScreen::panes`].
    #[serde(default)]
    pub active_pane: usize,
    /// BSP tree with pane-*index* leaves (the `persist.rs` pattern), in
    /// the same `node_json` shape the socket already speaks.
    pub layout: LayoutNode,
    pub panes: Vec<LayoutPane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayoutNode {
    Leaf { pane: usize },
    Split { dir: LayoutDir, ratio: f32, a: Box<LayoutNode>, b: Box<LayoutNode> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutDir {
    Right,
    Down,
}

impl From<crate::SplitDir> for LayoutDir {
    fn from(dir: crate::SplitDir) -> Self {
        match dir {
            crate::SplitDir::Right => LayoutDir::Right,
            crate::SplitDir::Down => LayoutDir::Down,
        }
    }
}

impl From<LayoutDir> for crate::SplitDir {
    fn from(dir: LayoutDir) -> Self {
        match dir {
            LayoutDir::Right => crate::SplitDir::Right,
            LayoutDir::Down => crate::SplitDir::Down,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutPane {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub active_tab: usize,
    pub tabs: Vec<LayoutTab>,
}

/// One recorded tab. `pty` is the agent-bearing kind: `command` is the
/// exact argv the tab was spawned with (recorded at spawn time; `None`
/// for a default login shell), `env` the injected variables minus mtyx's
/// auto-injected socket keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LayoutTab {
    Pty {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Browser {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        url: String,
    },
    Remote {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        host: String,
        slot: String,
        session_id: String,
        local_binary_path: String,
    },
}

impl LayoutDocument {
    /// Strict JSON entry point: deserialize (propagating parse errors —
    /// never silently defaulting) then [`Self::validate`].
    pub fn from_json_str(json: &str) -> anyhow::Result<Self> {
        let doc: LayoutDocument = serde_json::from_str(json)
            .map_err(|e| anyhow::anyhow!("parsing layout document: {e}"))?;
        doc.validate()?;
        Ok(doc)
    }

    /// Structural + semantic gate applied before any replay. Hard-fails
    /// (issue #76 AC7): unknown `schema_version`, layout leaves outside
    /// the pane table (or duplicated / unreferenced pane entries), split
    /// ratios outside (0,1), out-of-range selections, empty tab lists,
    /// and unparseable workspace color/icon values.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != LAYOUT_SCHEMA_VERSION {
            bail!(
                "unsupported layout schema_version {} (this mtyx writes {})",
                self.schema_version,
                LAYOUT_SCHEMA_VERSION
            );
        }
        let ws = &self.workspace;
        if ws.screens.is_empty() {
            bail!("layout document has no screens");
        }
        if ws.active_screen >= ws.screens.len() {
            bail!("active_screen {} out of range ({} screens)", ws.active_screen, ws.screens.len());
        }
        if let Some(color) = &ws.color {
            crate::server::parse_workspace_color(color)
                .map_err(|e| anyhow::anyhow!("workspace color: {e}"))?;
        }
        if let Some(icon) = &ws.icon {
            crate::server::parse_workspace_icon(icon)
                .map_err(|e| anyhow::anyhow!("workspace icon: {e}"))?;
        }
        for (si, screen) in ws.screens.iter().enumerate() {
            let count = screen.panes.len();
            if count == 0 {
                bail!("screen {si} has no panes");
            }
            if screen.active_pane >= count {
                bail!(
                    "screen {si}: active_pane {} out of range ({count} panes)",
                    screen.active_pane
                );
            }
            let mut seen = vec![false; count];
            Self::validate_node(&screen.layout, si, count, &mut seen)?;
            for (pi, unreferenced) in seen.iter().enumerate() {
                if !unreferenced {
                    bail!("screen {si}: pane {pi} is not referenced by the layout tree");
                }
            }
            for (pi, pane) in screen.panes.iter().enumerate() {
                if pane.tabs.is_empty() {
                    bail!("screen {si} pane {pi} has no tabs");
                }
                if pane.active_tab >= pane.tabs.len() {
                    bail!(
                        "screen {si} pane {pi}: active_tab {} out of range ({} tabs)",
                        pane.active_tab,
                        pane.tabs.len()
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_node(
        node: &LayoutNode,
        screen: usize,
        pane_count: usize,
        seen: &mut Vec<bool>,
    ) -> anyhow::Result<()> {
        match node {
            LayoutNode::Leaf { pane } => {
                if *pane >= pane_count {
                    bail!(
                        "screen {screen}: layout leaf pane index {pane} out of range ({pane_count} panes)"
                    );
                }
                if seen[*pane] {
                    bail!("screen {screen}: pane index {pane} appears more than once in the layout tree");
                }
                seen[*pane] = true;
            }
            LayoutNode::Split { ratio, a, b, .. } => {
                if !(*ratio > 0.0 && *ratio < 1.0) {
                    bail!("screen {screen}: split ratio {ratio} out of range (0,1)");
                }
                Self::validate_node(a, screen, pane_count, seen)?;
                Self::validate_node(b, screen, pane_count, seen)?;
            }
        }
        Ok(())
    }
}

/// Result of a successful replay: identity + size of what was created,
/// for the `layout-apply` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplySummary {
    pub workspace_id: WorkspaceId,
    pub panes: usize,
    pub surfaces: usize,
}

/// Capture `state.workspaces[ws_idx]` as a standalone document.
pub fn capture_workspace(state: &State, ws_idx: usize) -> anyhow::Result<LayoutDocument> {
    let ws = state
        .workspaces
        .get(ws_idx)
        .ok_or_else(|| anyhow::anyhow!("no workspace at index {ws_idx}"))?;
    Ok(LayoutDocument {
        schema_version: LAYOUT_SCHEMA_VERSION,
        cmux_version: crate::VERSION.to_string(),
        workspace: LayoutWorkspace {
            name: ws.name.clone(),
            color: ws.color.map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
            icon: ws.icon.as_ref().map(|icon| icon.as_str().to_string()),
            active_screen: ws.active_screen,
            screens: ws.screens.iter().map(|screen| capture_screen(state, screen)).collect(),
        },
    })
}

fn capture_screen(state: &State, screen: &Screen) -> LayoutScreen {
    let mut pane_ids = Vec::new();
    screen.root.pane_ids(&mut pane_ids);
    let index_of = |id: crate::PaneId| pane_ids.iter().position(|&p| p == id).unwrap_or(usize::MAX);

    LayoutScreen {
        name: screen.name.clone(),
        active_pane: index_of(screen.active_pane),
        layout: capture_node(&screen.root, &index_of),
        panes: pane_ids
            .iter()
            .map(|pid| {
                let pane = &state.panes[pid];
                LayoutPane {
                    name: pane.name.clone(),
                    active_tab: pane.active_tab,
                    tabs: pane.tabs.iter().map(|sid| capture_tab(state, *sid)).collect(),
                }
            })
            .collect(),
    }
}

fn capture_node(node: &Node, index_of: &impl Fn(crate::PaneId) -> usize) -> LayoutNode {
    match node {
        Node::Leaf(id) => LayoutNode::Leaf { pane: index_of(*id) },
        Node::Split { dir, ratio, a, b } => LayoutNode::Split {
            dir: (*dir).into(),
            ratio: *ratio,
            a: Box::new(capture_node(a, index_of)),
            b: Box::new(capture_node(b, index_of)),
        },
    }
}

fn capture_tab(state: &State, sid: SurfaceId) -> LayoutTab {
    let surface = state.surfaces.get(&sid);
    let name = surface.and_then(|s| s.name());
    match surface.map(|s| s.kind()) {
        Some(SurfaceKind::Browser) => LayoutTab::Browser {
            name,
            url: surface.and_then(|s| s.browser_url()).unwrap_or_default(),
        },
        _ => {
            if let Some(spec) = surface.and_then(|s| s.remote_spec()) {
                LayoutTab::Remote {
                    name,
                    host: spec.host,
                    slot: spec.slot,
                    session_id: spec.session_id,
                    local_binary_path: spec.local_binary_path.display().to_string(),
                }
            } else {
                LayoutTab::Pty {
                    name,
                    cwd: surface.and_then(|s| s.cwd()),
                    command: surface.and_then(|s| s.spawn_command()),
                    env: surface
                        .map(|s| {
                            s.spawn_env()
                                .into_iter()
                                .filter(|(k, _)| !AUTO_ENV_KEYS.contains(&k.as_str()))
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            }
        }
    }
}

/// A workspace name turned into a safe single path component for
/// `layout-export-all` (names are user-chosen and never validated as
/// filenames at creation time).
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::{Mux, PaneId, SpawnOverrides, SurfaceId, SurfaceOptions};

    /// A mux whose default spawn is a quiet, long-lived child (mirrors
    /// `mux::tests::test_mux`), with mtyx's auto-injected socket env keys
    /// present so capture's exclusion list is exercised.
    fn test_mux() -> Arc<Mux> {
        let opts = SurfaceOptions {
            command: Some(vec!["/bin/cat".to_string()]),
            extra_env: vec![
                ("MTYX_MUX_SOCKET".into(), "/tmp/mtyx-layout-test.sock".into()),
                ("MTYX_SOCKET_PATH".into(), "/tmp/mtyx-layout-test.sock".into()),
                ("FLEET_TIER".into(), "A".into()),
            ],
            ..Default::default()
        };
        Mux::new("layout-doc-test", opts)
    }

    fn pane_of(mux: &Mux, surface: SurfaceId) -> PaneId {
        mux.with_state(|s| s.pane_of(surface).unwrap())
    }

    // 1. `layout_doc_round_trips_through_json`
    #[test]
    fn layout_doc_round_trips_through_json() {
        let mux = test_mux();
        let s1 = mux.new_workspace(Some("fleet".into()), None).unwrap();
        let p1 = pane_of(&mux, s1.id);
        mux.split(p1, crate::SplitDir::Right, None).unwrap();
        mux.rename_pane(p1, "build".into());

        let doc = mux.with_state(|s| capture_workspace(s, 0)).unwrap();
        assert_eq!(doc.schema_version, LAYOUT_SCHEMA_VERSION);
        assert_eq!(doc.cmux_version, crate::VERSION);

        let json = serde_json::to_string_pretty(&doc).unwrap();
        assert!(json.contains("\"schema_version\": 1"), "json was: {json}");
        let restored = LayoutDocument::from_json_str(&json).unwrap();
        assert_eq!(restored, doc);
    }

    // 2. `layout_doc_rejects_unknown_schema_version`
    #[test]
    fn layout_doc_rejects_unknown_schema_version() {
        let mux = test_mux();
        mux.new_workspace(Some("fleet".into()), None).unwrap();
        let mut doc = mux.with_state(|s| capture_workspace(s, 0)).unwrap();
        doc.schema_version = 2;

        let err = doc.validate().unwrap_err().to_string();
        assert!(err.contains("schema_version"), "error was: {err}");
        assert!(err.contains('2'), "error should name the bad version: {err}");

        // The strict JSON entry point applies the same gate.
        doc.schema_version = LAYOUT_SCHEMA_VERSION;
        let json = serde_json::to_string(&doc)
            .unwrap()
            .replace("\"schema_version\":1", "\"schema_version\":7");
        let err = LayoutDocument::from_json_str(&json).unwrap_err().to_string();
        assert!(err.contains("schema_version"), "error was: {err}");
    }

    // 3. `layout_doc_rejects_leaf_index_out_of_range`
    #[test]
    fn layout_doc_rejects_leaf_index_out_of_range() {
        let doc = LayoutDocument {
            schema_version: LAYOUT_SCHEMA_VERSION,
            cmux_version: crate::VERSION.to_string(),
            workspace: LayoutWorkspace {
                name: "bad".into(),
                color: None,
                icon: None,
                active_screen: 0,
                screens: vec![LayoutScreen {
                    name: None,
                    active_pane: 0,
                    layout: LayoutNode::Leaf { pane: 3 },
                    panes: vec![
                        LayoutPane { name: None, active_tab: 0, tabs: vec![sample_tab()] },
                        LayoutPane { name: None, active_tab: 0, tabs: vec![sample_tab()] },
                    ],
                }],
            },
        };
        let err = doc.validate().unwrap_err().to_string();
        assert!(err.contains('3'), "error should name the bad index: {err}");
        assert!(err.contains("out of range"), "error was: {err}");
    }

    // 4. `layout_doc_rejects_unknown_tab_kind`
    #[test]
    fn layout_doc_rejects_unknown_tab_kind() {
        let json = r#"{
            "schema_version": 1,
            "cmux_version": "test",
            "workspace": {
                "name": "w",
                "active_screen": 0,
                "screens": [{
                    "active_pane": 0,
                    "layout": {"type": "leaf", "pane": 0},
                    "panes": [{"tabs": [{"kind": "matrix"}]}]
                }]
            }
        }"#;
        let err = LayoutDocument::from_json_str(json).unwrap_err().to_string();
        assert!(err.contains("matrix"), "error should name the unknown kind: {err}");
    }

    // 5. `layout_doc_capture_includes_split_geometry_names_and_selections`
    #[test]
    fn layout_doc_capture_includes_split_geometry_names_and_selections() {
        let mux = test_mux();
        let s1 = mux.new_workspace(Some("fleet".into()), None).unwrap();
        let p1 = pane_of(&mux, s1.id);
        mux.rename_pane(p1, "build".into());
        mux.rename_surface(s1.id, "api".into());
        mux.split(p1, crate::SplitDir::Right, None).unwrap();
        mux.set_ratio(p1, crate::SplitDir::Right, 0.7);
        mux.focus_pane(p1);
        // A second screen, left active: capture must record the selection.
        mux.new_screen(None, None).unwrap();

        let doc = mux.with_state(|s| capture_workspace(s, 0)).unwrap();
        let ws = &doc.workspace;
        assert_eq!(ws.name, "fleet");
        assert_eq!(ws.screens.len(), 2);
        assert_eq!(ws.active_screen, 1, "capture should record the active screen");

        let sc = &ws.screens[0];
        let LayoutNode::Split { dir, ratio, a, b } = &sc.layout else {
            panic!("expected a split root, got {:?}", sc.layout);
        };
        assert_eq!(*dir, LayoutDir::Right);
        assert!((*ratio - 0.7).abs() < 1e-6, "ratio was {ratio}");
        assert_eq!(**a, LayoutNode::Leaf { pane: 0 });
        assert_eq!(**b, LayoutNode::Leaf { pane: 1 });
        assert_eq!(sc.panes[0].name.as_deref(), Some("build"));
        assert_eq!(sc.active_pane, 0, "focused pane is pane 0");
        match &sc.panes[0].tabs[0] {
            LayoutTab::Pty { name, .. } => assert_eq!(name.as_deref(), Some("api")),
            other => panic!("expected a pty tab, got {other:?}"),
        }
    }

    // 6. `layout_doc_capture_records_command_and_env_dropping_cmux_auto_vars`
    #[test]
    fn layout_doc_capture_records_command_and_env_dropping_cmux_auto_vars() {
        let mux = test_mux();
        let s1 = mux.new_workspace(Some("fleet".into()), None).unwrap();
        let pane = pane_of(&mux, s1.id);
        let s2 = mux
            .new_tab_with_overrides(
                Some(pane),
                None,
                None,
                Some(&SpawnOverrides {
                    command: Some(vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        "sleep 30".to_string(),
                    ]),
                    extra_env: vec![("FLEET_WORKER".into(), "9".into())],
                    cwd: Some("/tmp".into()),
                }),
                None,
            )
            .unwrap();
        assert!(s2.id > 0);

        let doc = mux.with_state(|s| capture_workspace(s, 0)).unwrap();
        let tabs = &doc.workspace.screens[0].panes[0].tabs;
        assert_eq!(tabs.len(), 2, "pane should have the workspace tab + the override tab");
        match &tabs[1] {
            LayoutTab::Pty { command, env, cwd, .. } => {
                assert_eq!(
                    command.as_deref(),
                    Some(
                        ["/bin/sh".to_string(), "-c".to_string(), "sleep 30".to_string()]
                            .as_slice()
                    ),
                    "recorded argv should round-trip"
                );
                assert_eq!(env.get("FLEET_TIER").map(String::as_str), Some("A"));
                assert_eq!(env.get("FLEET_WORKER").map(String::as_str), Some("9"));
                assert!(
                    !env.contains_key("MTYX_MUX_SOCKET"),
                    "auto socket env must not be exported"
                );
                assert!(
                    !env.contains_key("MTYX_SOCKET_PATH"),
                    "auto socket env must not be exported"
                );
                assert_eq!(cwd.as_deref(), Some("/tmp"));
            }
            other => panic!("expected a pty tab, got {other:?}"),
        }
    }

    // 7. `layout_doc_rejects_bad_ratio`
    #[test]
    fn layout_doc_rejects_bad_ratio() {
        for bad in [1.5f32, 0.0, -0.25, 1.0] {
            let mut doc = sample_doc();
            doc.workspace.screens[0].layout = LayoutNode::Split {
                dir: LayoutDir::Right,
                ratio: bad,
                a: Box::new(LayoutNode::Leaf { pane: 0 }),
                b: Box::new(LayoutNode::Leaf { pane: 1 }),
            };
            doc.workspace.screens[0].panes.push(LayoutPane {
                name: None,
                active_tab: 0,
                tabs: vec![sample_tab()],
            });
            let err = doc.validate().unwrap_err().to_string();
            assert!(err.contains("ratio"), "ratio {bad} should be rejected, got: {err}");
        }
    }

    // -- fixtures ---------------------------------------------------------

    fn sample_tab() -> LayoutTab {
        LayoutTab::Pty { name: None, cwd: None, command: None, env: BTreeMap::new() }
    }

    fn sample_doc() -> LayoutDocument {
        LayoutDocument {
            schema_version: LAYOUT_SCHEMA_VERSION,
            cmux_version: crate::VERSION.to_string(),
            workspace: LayoutWorkspace {
                name: "sample".into(),
                color: None,
                icon: None,
                active_screen: 0,
                screens: vec![LayoutScreen {
                    name: None,
                    active_pane: 0,
                    layout: LayoutNode::Leaf { pane: 0 },
                    panes: vec![LayoutPane { name: None, active_tab: 0, tabs: vec![sample_tab()] }],
                }],
            },
        }
    }
}
