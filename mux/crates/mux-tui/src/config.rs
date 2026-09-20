//! TUI configuration: `~/.config/mattyx/mux.json` (override the path with
//! `MTYX_MUX_CONFIG`), with colors seeded from the user's Ghostty config
//! where sensible.
//!
//! ```json
//! {
//!   "theme": {
//!     "selection_background": "#3a3a3a",
//!     "selection_foreground": null,
//!     "sidebar_rail": "#87afd7",
//!     "sidebar_active_bg": 236,
//!     "tab_rail": "#87afd7",
//!     "tab_bg": 236,
//!     "tab_active_bg": null,
//!     "border_active": "#87afd7",
//!     "border_inactive": "#444444"
//!   },
//!   "tabs": {
//!     "min_width": 7,
//!     "solid_background": true,
//!     "show_titles": false,
//!     "agents": ["claude", "codex", "grok", "opencode", "pi"]
//!   },
//!   "sidebar": {
//!     "width": 22,
//!     "max_width": 0
//!   },
//!   "browser": {
//!     "chrome_binary": "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
//!     "cdp_url": "http://127.0.0.1:9222",
//!     "discover": false,
//!     "discover_ports": [9222],
//!     "user_data_dir": "/Users/me/Library/Application Support/mattyx/chrome-profile",
//!     "ephemeral": false,
//!     "max_capture_megapixels": 2.0,
//!     "capture_scale": null
//!   },
//!   "scrollbar": {
//!     "position": "column"
//!   },
//!   "headless": {
//!     "vt_size": "100x30"
//!   },
//!   "keys": {
//!     "prefix": "ctrl+b",
//!     "alt_shortcuts": true,
//!     "new-tab": ["t", "alt+t"],
//!     "next-tab": "tab",
//!     "prev-tab": "backtab",
//!     "browser-edit-url": "u"
//!   }
//! }
//! ```
//!
//! Every key is optional. Colors are `#rrggbb`, `#rgb`, or an xterm-256
//! index (number or numeric string). Resolution order for the selection
//! colors: explicit config value, then the user's Ghostty config
//! (`selection-background`/`selection-foreground`), then the built-in
//! default.
//!
//! `headless.vt_size` is the VT geometry (`"COLSxROWS"`, e.g.
//! `"100x30"`) for surfaces spawned with no client attached and no
//! explicit size (issue #99). The default is 120x40; the
//! `MTYX_MUX_VT_SIZE` env var overrides the config file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mux_core::platform;
use ratatui::style::Color;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

/// For a field typed `Option<Option<T>>`: makes an explicit `null` in the
/// input deserialize to `Some(None)` rather than the `None` an absent key
/// also produces, so callers can tell "not set" from "set to null".
fn deserialize_some<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ThemeValue {
    String(String),
    Table(RawTheme),
}

impl Default for ThemeValue {
    fn default() -> Self {
        ThemeValue::Table(RawTheme::default())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    theme: ThemeValue,
    #[serde(default)]
    tabs: RawTabs,
    #[serde(default)]
    sidebar: RawSidebar,
    #[serde(default)]
    browser: RawBrowser,
    #[serde(default)]
    scrollbar: RawScrollbar,
    /// Headless spawn geometry (issue #99): `vt_size = "100x30"` for
    /// surfaces created with no client attached.
    #[serde(default)]
    headless: RawHeadless,
    #[serde(default)]
    workspaces: Vec<WorkspaceConfig>,
    /// Issue #78 AC7: `[[agent_detection]]` — turn ambient agent detection
    /// on/off and override the default confidence threshold. Entries
    /// apply in order; the last value for each key wins.
    #[serde(default)]
    agent_detection: Vec<RawAgentDetection>,
    /// Worktree path pattern override (issue #77 AC6): array table so
    /// `mux.toml` writes `[[worktree_pattern]]` with a single `pattern`
    /// key. Multiple entries are accepted; the first wins (resolved in
    /// `apply_worktree_pattern`).
    #[serde(default)]
    worktree_pattern: Vec<WorktreePatternConfig>,
    /// Key bindings: `"prefix"` plus one entry per action. Values may be
    /// a chord string, an array of chord strings, `"none"`, or
    /// `"alt_shortcuts": false`.
    #[serde(default)]
    keys: HashMap<String, Value>,
}

/// Raw `[[agent_detection]]` table (issue #78 AC7).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentDetection {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    min_confidence: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    selection_background: Option<ColorValue>,
    /// Distinguishes an absent key (keep the Ghostty-seeded value) from an
    /// explicit `null` (clear it back to "no override"), which `Option`
    /// alone cannot: serde maps both to `None`.
    #[serde(default, deserialize_with = "deserialize_some")]
    selection_foreground: Option<Option<ColorValue>>,
    sidebar_rail: Option<ColorValue>,
    sidebar_active_bg: Option<ColorValue>,
    tab_rail: Option<ColorValue>,
    tab_bg: Option<ColorValue>,
    tab_active_bg: Option<ColorValue>,
    border_active: Option<ColorValue>,
    border_inactive: Option<ColorValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTabs {
    min_width: Option<u16>,
    solid_background: Option<bool>,
    show_titles: Option<bool>,
    agents: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSidebar {
    width: Option<u16>,
    max_width: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBrowser {
    chrome_binary: Option<String>,
    cdp_url: Option<String>,
    discover: Option<bool>,
    discover_ports: Option<Vec<u16>>,
    user_data_dir: Option<String>,
    ephemeral: Option<bool>,
    max_capture_megapixels: Option<f64>,
    capture_scale: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScrollbar {
    position: Option<ScrollbarPosition>,
}

/// Raw `[headless]` table (issue #99): `vt_size = "100x30"`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeadless {
    /// VT geometry for no-client spawns, `"COLSxROWS"`.
    vt_size: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    pub color: Option<String>,
    pub icon: Option<String>,
}

/// One `[[worktree_pattern]]` entry (issue #77 AC6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreePatternConfig {
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollbarPosition {
    Column,
    Border,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollbar {
    pub position: ScrollbarPosition,
}

impl Default for Scrollbar {
    fn default() -> Self {
        Scrollbar { position: ScrollbarPosition::Column }
    }
}

/// Headless spawn settings (issue #99).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Headless {
    /// VT geometry `(cols, rows)` for surfaces spawned with no client
    /// attached and no explicit size, parsed from `headless.vt_size`
    /// (e.g. `"100x30"`). `None` keeps the mux-core default (120x40;
    /// `MTYX_MUX_VT_SIZE` can still override it).
    pub vt_size: Option<(u16, u16)>,
}

/// A color in the config file: "#rrggbb", "#rgb", or an xterm-256 index.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
enum ColorValue {
    Index(u8),
    Text(String),
}

impl ColorValue {
    fn to_color(&self) -> Option<Color> {
        match self {
            ColorValue::Index(i) => Some(Color::Indexed(*i)),
            ColorValue::Text(s) => parse_color(s),
        }
    }
}

/// Resolved presentation colors used by the renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub selection_bg: Color,
    /// None keeps each cell's own foreground under the selection.
    pub selection_fg: Option<Color>,
    pub sidebar_rail: Color,
    pub sidebar_active_bg: Color,
    pub tab_rail: Color,
    pub tab_bg: Color,
    /// None keeps the focused/unfocused active-tab two-tone default.
    pub tab_active_bg: Option<Color>,
    pub border_active: Color,
    pub border_inactive: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            // Dark grey: readable but clearly a selection.
            selection_bg: Color::Rgb(0x3a, 0x3a, 0x3a),
            selection_fg: None,
            sidebar_rail: Color::Indexed(110),
            sidebar_active_bg: Color::Indexed(236),
            tab_rail: Color::Indexed(110),
            tab_bg: Color::Indexed(236),
            tab_active_bg: None,
            border_active: Color::Indexed(110),
            border_inactive: Color::Indexed(238),
        }
    }
}

/// Tab-bar behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct Tabs {
    /// Minimum label width in cells (padded with spaces).
    pub min_width: u16,
    /// Tabs render with a solid background instead of text on the border.
    pub solid_background: bool,
    /// Show the process title after the number for every tab. Off by
    /// default: tabs are just numbers, except recognized agent programs.
    pub show_titles: bool,
    /// Program names worth surfacing in the tab label even when
    /// `show_titles` is off (matched as words in the reported title).
    pub agents: Vec<String>,
}

impl Default for Tabs {
    fn default() -> Self {
        Tabs {
            min_width: 7,
            solid_background: true,
            show_titles: false,
            agents: ["claude", "codex", "grok", "opencode", "pi"].map(String::from).to_vec(),
        }
    }
}

/// Sidebar behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sidebar {
    pub width: u16,
    pub max_width: u16,
}

impl Default for Sidebar {
    fn default() -> Self {
        Sidebar { width: 22, max_width: 0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Browser {
    pub chrome_binary: Option<String>,
    pub cdp_url: Option<String>,
    pub discover: bool,
    pub discover_ports: Vec<u16>,
    pub user_data_dir: Option<String>,
    pub ephemeral: bool,
    pub max_capture_megapixels: f64,
    pub capture_scale: Option<f64>,
}

impl Default for Browser {
    fn default() -> Self {
        Browser {
            chrome_binary: None,
            cdp_url: None,
            discover: false,
            discover_ports: vec![9222],
            user_data_dir: None,
            ephemeral: false,
            max_capture_megapixels: 2.0,
            capture_scale: None,
        }
    }
}

/// Categories used to group key bindings in the help overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HelpCategory {
    Window,
    Workspace,
    Pane,
    Tab,
    Browser,
    Agent,
    Misc,
}

impl HelpCategory {
    /// Categories in the display order used by the help overlay.
    pub const ALL: [Self; 7] = [
        Self::Window,
        Self::Workspace,
        Self::Pane,
        Self::Tab,
        Self::Browser,
        Self::Agent,
        Self::Misc,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Window => "Window",
            Self::Workspace => "Workspace",
            Self::Pane => "Pane",
            Self::Tab => "Tab",
            Self::Browser => "Browser",
            Self::Agent => "Agent",
            Self::Misc => "Misc",
        }
    }
}

/// Every prefix-key action, so bindings are configurable end to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    NewTab,
    NewBrowserTab,
    NewPaneSmart,
    NextTab,
    PrevTab,
    SplitRight,
    SplitDown,
    CloseTab,
    ClosePane,
    RenameTab,
    RenameScreen,
    RenameWorkspace,
    CloseScreen,
    PrevScreen,
    NextScreen,
    NewScreen,
    NextWorkspace,
    NewWorkspace,
    ToggleSidebar,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeGrow,
    ResizeShrink,
    ScrollUp,
    ScrollDown,
    BrowserBack,
    BrowserForward,
    BrowserReload,
    BrowserEditUrl,
    Detach,
    OpenFuzzyFinder,
    OpenSessionManager,
    ShowHelp,
}

impl Action {
    pub fn config_key(&self) -> &'static str {
        match self {
            Action::NewTab => "new-tab",
            Action::NewBrowserTab => "new-browser-tab",
            Action::NewPaneSmart => "new-pane-smart",
            Action::NextTab => "next-tab",
            Action::PrevTab => "prev-tab",
            Action::SplitRight => "split-right",
            Action::SplitDown => "split-down",
            Action::CloseTab => "close-tab",
            Action::ClosePane => "close-pane",
            Action::RenameTab => "rename-tab",
            Action::RenameScreen => "rename-screen",
            Action::RenameWorkspace => "rename-workspace",
            Action::CloseScreen => "close-screen",
            Action::PrevScreen => "prev-screen",
            Action::NextScreen => "next-screen",
            Action::NewScreen => "new-screen",
            Action::NextWorkspace => "next-workspace",
            Action::NewWorkspace => "new-workspace",
            Action::ToggleSidebar => "toggle-sidebar",
            Action::FocusLeft => "focus-left",
            Action::FocusRight => "focus-right",
            Action::FocusUp => "focus-up",
            Action::FocusDown => "focus-down",
            Action::ResizeGrow => "resize-grow",
            Action::ResizeShrink => "resize-shrink",
            Action::ScrollUp => "scroll-up",
            Action::ScrollDown => "scroll-down",
            Action::BrowserBack => "browser-back",
            Action::BrowserForward => "browser-forward",
            Action::BrowserReload => "browser-reload",
            Action::BrowserEditUrl => "browser-edit-url",
            Action::Detach => "detach",
            Action::OpenFuzzyFinder => "open-fuzzy-finder",
            Action::OpenSessionManager => "open-session-manager",
            Action::ShowHelp => "show-help",
        }
    }

    /// Human-readable action name shown in the help overlay.
    pub fn display_name(self) -> &'static str {
        match self {
            Action::NewTab => "New tab",
            Action::NewBrowserTab => "New browser tab",
            Action::NewPaneSmart => "New pane",
            Action::NextTab => "Next tab",
            Action::PrevTab => "Previous tab",
            Action::SplitRight => "Split right",
            Action::SplitDown => "Split down",
            Action::CloseTab => "Close tab",
            Action::ClosePane => "Close pane",
            Action::RenameTab => "Rename tab",
            Action::RenameScreen => "Rename screen",
            Action::RenameWorkspace => "Rename workspace",
            Action::CloseScreen => "Close screen",
            Action::PrevScreen => "Previous screen",
            Action::NextScreen => "Next screen",
            Action::NewScreen => "New screen",
            Action::NextWorkspace => "Next workspace",
            Action::NewWorkspace => "New workspace",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::FocusLeft => "Focus left",
            Action::FocusRight => "Focus right",
            Action::FocusUp => "Focus up",
            Action::FocusDown => "Focus down",
            Action::ResizeGrow => "Grow pane",
            Action::ResizeShrink => "Shrink pane",
            Action::ScrollUp => "Scroll up",
            Action::ScrollDown => "Scroll down",
            Action::BrowserBack => "Browser back",
            Action::BrowserForward => "Browser forward",
            Action::BrowserReload => "Reload browser",
            Action::BrowserEditUrl => "Edit browser URL",
            Action::Detach => "Detach",
            Action::OpenFuzzyFinder => "Open fuzzy finder",
            Action::OpenSessionManager => "Open session manager",
            Action::ShowHelp => "Show help",
        }
    }

    /// One-line explanation of the action for the help overlay.
    pub fn description(self) -> &'static str {
        match self {
            Action::NewTab => "Create a new terminal tab",
            Action::NewBrowserTab => "Open a new browser tab",
            Action::NewPaneSmart => "Create a smartly placed pane",
            Action::NextTab => "Select the next tab",
            Action::PrevTab => "Select the previous tab",
            Action::SplitRight => "Split the pane to the right",
            Action::SplitDown => "Split the pane below",
            Action::CloseTab => "Close the active tab",
            Action::ClosePane => "Close the active pane",
            Action::RenameTab => "Rename the active tab",
            Action::RenameScreen => "Rename the active screen",
            Action::RenameWorkspace => "Rename the active workspace",
            Action::CloseScreen => "Close the active screen",
            Action::PrevScreen => "Select the previous screen",
            Action::NextScreen => "Select the next screen",
            Action::NewScreen => "Create a new screen",
            Action::NextWorkspace => "Select the next workspace",
            Action::NewWorkspace => "Create a new workspace",
            Action::ToggleSidebar => "Show or hide the sidebar",
            Action::FocusLeft => "Focus the pane to the left",
            Action::FocusRight => "Focus the pane to the right",
            Action::FocusUp => "Focus the pane above",
            Action::FocusDown => "Focus the pane below",
            Action::ResizeGrow => "Grow the focused pane",
            Action::ResizeShrink => "Shrink the focused pane",
            Action::ScrollUp => "Scroll the active surface up",
            Action::ScrollDown => "Scroll the active surface down",
            Action::BrowserBack => "Go back in browser history",
            Action::BrowserForward => "Go forward in browser history",
            Action::BrowserReload => "Reload the browser page",
            Action::BrowserEditUrl => "Edit the browser URL",
            Action::Detach => "Detach from the current session",
            Action::OpenFuzzyFinder => "Open the fuzzy finder",
            Action::OpenSessionManager => "Open the session manager overlay",
            Action::ShowHelp => "Show this key binding help",
        }
    }

    pub fn category(self) -> HelpCategory {
        match self {
            Action::RenameScreen
            | Action::CloseScreen
            | Action::PrevScreen
            | Action::NextScreen
            | Action::NewScreen => HelpCategory::Window,
            Action::RenameWorkspace
            | Action::NextWorkspace
            | Action::NewWorkspace
            | Action::ToggleSidebar => HelpCategory::Workspace,
            Action::NewPaneSmart
            | Action::SplitRight
            | Action::SplitDown
            | Action::ClosePane
            | Action::FocusLeft
            | Action::FocusRight
            | Action::FocusUp
            | Action::FocusDown
            | Action::ResizeGrow
            | Action::ResizeShrink => HelpCategory::Pane,
            Action::NewTab
            | Action::NewBrowserTab
            | Action::NextTab
            | Action::PrevTab
            | Action::CloseTab
            | Action::RenameTab => HelpCategory::Tab,
            Action::BrowserBack
            | Action::BrowserForward
            | Action::BrowserReload
            | Action::BrowserEditUrl => HelpCategory::Browser,
            Action::ScrollUp
            | Action::ScrollDown
            | Action::Detach
            | Action::OpenFuzzyFinder
            | Action::OpenSessionManager
            | Action::ShowHelp => HelpCategory::Misc,
        }
    }
}

/// A key chord: code plus required modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    /// Render this chord in the same syntax accepted by the config parser.
    pub fn display(self) -> String {
        chord_to_string(self)
    }

    pub fn matches(&self, key: &KeyEvent) -> bool {
        // Shift is implied by uppercase/symbol chars; compare it only
        // for non-char codes.
        let mods_match = if matches!(self.code, KeyCode::Char(_)) {
            key.modifiers.contains(self.mods & !KeyModifiers::SHIFT)
        } else {
            const TRACKED: KeyModifiers =
                KeyModifiers::CONTROL.union(KeyModifiers::ALT).union(KeyModifiers::SHIFT);
            key.modifiers & TRACKED == self.mods & TRACKED
        };
        self.code == key.code && mods_match
    }
}

/// Resolved key bindings: the prefix chord plus one chord per action.
#[derive(Debug, Clone, PartialEq)]
pub struct Keys {
    pub prefix: Chord,
    bindings: Vec<(Chord, Action)>,
}

impl Default for Keys {
    fn default() -> Self {
        let bind = |code, action| (Chord { code, mods: KeyModifiers::NONE }, action);
        let alt = |code, action| (Chord { code, mods: KeyModifiers::ALT }, action);
        Keys {
            prefix: Chord { code: KeyCode::Char('b'), mods: KeyModifiers::CONTROL },
            bindings: vec![
                bind(KeyCode::Char('t'), Action::NewTab),
                alt(KeyCode::Char('t'), Action::NewTab),
                bind(KeyCode::Char('B'), Action::NewBrowserTab),
                alt(KeyCode::Char('n'), Action::NewPaneSmart),
                bind(KeyCode::Tab, Action::NextTab),
                bind(KeyCode::BackTab, Action::PrevTab),
                bind(KeyCode::Char('%'), Action::SplitRight),
                bind(KeyCode::Char('"'), Action::SplitDown),
                bind(KeyCode::Char('x'), Action::CloseTab),
                bind(KeyCode::Char('X'), Action::ClosePane),
                bind(KeyCode::Char(','), Action::RenameScreen),
                bind(KeyCode::Char('$'), Action::RenameWorkspace),
                bind(KeyCode::Char('&'), Action::CloseScreen),
                bind(KeyCode::Char('p'), Action::PrevScreen),
                alt(KeyCode::Char('['), Action::PrevScreen),
                bind(KeyCode::Char('n'), Action::NextScreen),
                alt(KeyCode::Char(']'), Action::NextScreen),
                bind(KeyCode::Char('c'), Action::NewScreen),
                bind(KeyCode::Char('w'), Action::NextWorkspace),
                bind(KeyCode::Char('W'), Action::NewWorkspace),
                bind(KeyCode::Char('s'), Action::ToggleSidebar),
                bind(KeyCode::Char('h'), Action::FocusLeft),
                bind(KeyCode::Left, Action::FocusLeft),
                alt(KeyCode::Char('h'), Action::FocusLeft),
                alt(KeyCode::Left, Action::FocusLeft),
                bind(KeyCode::Char('l'), Action::FocusRight),
                bind(KeyCode::Right, Action::FocusRight),
                alt(KeyCode::Char('l'), Action::FocusRight),
                alt(KeyCode::Right, Action::FocusRight),
                bind(KeyCode::Char('k'), Action::FocusUp),
                bind(KeyCode::Up, Action::FocusUp),
                alt(KeyCode::Char('k'), Action::FocusUp),
                alt(KeyCode::Up, Action::FocusUp),
                bind(KeyCode::Char('j'), Action::FocusDown),
                bind(KeyCode::Down, Action::FocusDown),
                alt(KeyCode::Char('j'), Action::FocusDown),
                alt(KeyCode::Down, Action::FocusDown),
                alt(KeyCode::Char('='), Action::ResizeGrow),
                alt(KeyCode::Char('-'), Action::ResizeShrink),
                bind(KeyCode::PageUp, Action::ScrollUp),
                bind(KeyCode::PageDown, Action::ScrollDown),
                bind(KeyCode::Char('<'), Action::BrowserBack),
                bind(KeyCode::Char('>'), Action::BrowserForward),
                bind(KeyCode::Char('r'), Action::BrowserReload),
                bind(KeyCode::Char('u'), Action::BrowserEditUrl),
                bind(KeyCode::Char('d'), Action::Detach),
                bind(KeyCode::Char('G'), Action::OpenFuzzyFinder),
                bind(KeyCode::Char('S'), Action::OpenSessionManager),
                bind(KeyCode::Char('?'), Action::ShowHelp),
            ],
        }
    }
}

impl Keys {
    /// Return every resolved binding in registry order.
    pub fn bindings(&self) -> &[(Chord, Action)] {
        &self.bindings
    }

    /// The action bound to a key event (after the prefix).
    pub fn action_for(&self, key: &KeyEvent) -> Option<Action> {
        self.bindings.iter().find(|(chord, _)| chord.matches(key)).map(|(_, a)| *a)
    }

    /// The modeless action bound to a key event. Only Alt-modified
    /// chords are modeless; non-Alt chords remain prefix-only.
    pub fn modeless_action_for(&self, key: &KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(chord, _)| chord.mods.contains(KeyModifiers::ALT) && chord.matches(key))
            .map(|(_, a)| *a)
    }

    /// Apply config overrides: `"prefix"` rebinds the prefix; any action
    /// name rebinds that action (replacing ALL default chords for it).
    fn apply(&mut self, raw: &HashMap<String, Value>) {
        if raw.get("alt_shortcuts").and_then(Value::as_bool) == Some(false) {
            self.bindings.retain(|(chord, _)| !chord.mods.contains(KeyModifiers::ALT));
        }
        for (name, value) in raw {
            if name == "alt_shortcuts" {
                continue;
            }
            if name == "prefix" {
                let Some(value) = value.as_str() else {
                    eprintln!("mtyx: ignoring non-string prefix binding {value:?}");
                    continue;
                };
                let Some(chord) = parse_chord(value) else {
                    eprintln!("mtyx: ignoring unparseable key binding prefix = {value:?}");
                    continue;
                };
                self.prefix = chord;
                continue;
            }
            match all_actions().iter().find(|a| {
                a.config_key() == name
                    || (**a == Action::RenameTab && name == "rename-pane")
                    || (**a == Action::NewBrowserTab && name == "new_browser_tab")
            }) {
                Some(action) => {
                    self.bindings.retain(|(_, a)| a != action);
                    for raw_chord in key_values(value) {
                        if raw_chord.eq_ignore_ascii_case("none") {
                            continue;
                        }
                        let Some(chord) = parse_chord(raw_chord) else {
                            eprintln!(
                                "mtyx: ignoring unparseable key binding {name} = {raw_chord:?}"
                            );
                            continue;
                        };
                        self.bindings.retain(|(existing, _)| existing != &chord);
                        self.bindings.push((chord, *action));
                    }
                }
                None => eprintln!("mtyx: ignoring unknown key action {name:?}"),
            }
        }
    }

    /// Serialise the resolved bindings back into the raw `keys` map shape
    /// (`"prefix"`, `"alt_shortcuts"`, one entry per action config key
    /// mapped to a chord string or array) so that
    /// `Config::resolved_chrome_value` can send the server's keys to a
    /// thin client, which re-applies them via `Keys::apply` and reproduces
    /// the same resolved state (issue #40 blocker 1). Every action that
    /// still has a chord is emitted so the round-trip is whole, not just
    /// the non-default ones.
    pub fn to_raw_map(&self) -> HashMap<String, Value> {
        let mut map: HashMap<String, Value> = HashMap::new();
        let has_alt = self.bindings.iter().any(|(c, _)| c.mods.contains(KeyModifiers::ALT));
        map.insert("alt_shortcuts".to_string(), Value::Bool(has_alt));
        map.insert("prefix".to_string(), Value::String(chord_to_string(self.prefix)));
        let mut by_action: HashMap<&str, Vec<Value>> = HashMap::new();
        for (chord, action) in &self.bindings {
            by_action
                .entry(action.config_key())
                .or_default()
                .push(Value::String(chord_to_string(*chord)));
        }
        for (name, chords) in by_action {
            map.insert(name.to_string(), Value::Array(chords));
        }
        map
    }
}

/// Inverse of `parse_chord`: render a resolved `Chord` back as the chord
/// string (`ctrl+b`, `alt+t`, `tab`, `B`, ...). Shift is implied by an
/// uppercase/symbol char, so for `Char` we emit only ctrl/alt, matching
/// how `parse_chord` interprets single characters (its shift branch only
/// fires for non-char codes).
fn chord_to_string(chord: Chord) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    let is_char = matches!(chord.code, KeyCode::Char(_));
    if chord.mods.contains(KeyModifiers::SHIFT) && !is_char {
        parts.push("shift");
    }
    let code = match chord.code {
        KeyCode::Tab => "tab",
        KeyCode::BackTab => "backtab",
        KeyCode::Enter => "enter",
        KeyCode::Esc => "escape",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::Char(c) => {
            let joined = parts.join("+");
            let mut buf = [0u8; 4];
            let ch = c.encode_utf8(&mut buf);
            return if joined.is_empty() { ch.to_string() } else { format!("{joined}+{ch}") };
        }
        other => return format!("{}{:?}", parts.join("+"), other),
    };
    let mut s = parts.join("+");
    if !s.is_empty() {
        s.push('+');
    }
    s.push_str(code);
    s
}

fn key_values(value: &Value) -> Vec<&str> {
    match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn all_actions() -> &'static [Action] {
    &[
        Action::NewTab,
        Action::NewBrowserTab,
        Action::NewPaneSmart,
        Action::NextTab,
        Action::PrevTab,
        Action::SplitRight,
        Action::SplitDown,
        Action::CloseTab,
        Action::ClosePane,
        Action::RenameTab,
        Action::RenameScreen,
        Action::RenameWorkspace,
        Action::CloseScreen,
        Action::PrevScreen,
        Action::NextScreen,
        Action::NewScreen,
        Action::NextWorkspace,
        Action::NewWorkspace,
        Action::ToggleSidebar,
        Action::FocusLeft,
        Action::FocusRight,
        Action::FocusUp,
        Action::FocusDown,
        Action::ResizeGrow,
        Action::ResizeShrink,
        Action::ScrollUp,
        Action::ScrollDown,
        Action::BrowserBack,
        Action::BrowserForward,
        Action::BrowserReload,
        Action::BrowserEditUrl,
        Action::Detach,
        Action::OpenFuzzyFinder,
        Action::OpenSessionManager,
        Action::ShowHelp,
    ]
}

/// Parse "c", "%", "ctrl+b", "alt+enter", "tab", "pageup", ...
fn parse_chord(s: &str) -> Option<Chord> {
    let mut mods = KeyModifiers::NONE;
    let mut code = None;
    for part in s.split('+') {
        let part = part.trim();
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "tab" => code = Some(KeyCode::Tab),
            "backtab" => code = Some(KeyCode::BackTab),
            "enter" | "return" => code = Some(KeyCode::Enter),
            "esc" | "escape" => code = Some(KeyCode::Esc),
            "space" => code = Some(KeyCode::Char(' ')),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "pageup" => code = Some(KeyCode::PageUp),
            "pagedown" => code = Some(KeyCode::PageDown),
            "home" => code = Some(KeyCode::Home),
            "end" => code = Some(KeyCode::End),
            _ => {
                // Single character, case-sensitive (uppercase = shifted).
                let mut chars = part.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                code = Some(KeyCode::Char(c));
            }
        }
    }
    let mut code = code?;
    if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
        code = KeyCode::BackTab;
        mods.remove(KeyModifiers::SHIFT);
    }
    Some(Chord { code, mods })
}

/// Full resolved configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub theme: Theme,
    pub tabs: Tabs,
    pub sidebar: Sidebar,
    pub browser: Browser,
    pub scrollbar: Scrollbar,
    pub headless: Headless,
    pub keys: Keys,
    pub workspaces: Vec<WorkspaceConfig>,
    pub agent_detection: AgentDetectionConfig,
    /// Resolved worktree path pattern (issue #77 AC6): the first
    /// `[[worktree_pattern]]` entry's `pattern`, or `None` for the
    /// default `<repo>/../<repo>.<branch>/`.
    pub worktree_pattern: Option<String>,
}

/// Resolved `[[agent_detection]]` settings (issue #78 AC7). Detection is
/// on with a `low` threshold by default, so every match surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDetectionConfig {
    pub enabled: bool,
    pub min_confidence: mux_core::agent_detect::Confidence,
}

impl Default for AgentDetectionConfig {
    fn default() -> Self {
        AgentDetectionConfig {
            enabled: true,
            min_confidence: mux_core::agent_detect::Confidence::Low,
        }
    }
}

impl Config {
    /// Serialise the resolved chrome (theme/tabs/sidebar/keys) into the
    /// same raw JSON shape that `RawConfig`/the `apply_*_raw` helpers
    /// consume, so a thin-client attach can round-trip it back through
    /// `Config::from_server_chrome` (issue #40 blocker 1). Browser and
    /// scrollbar are server-side truth and intentionally omitted.
    pub fn resolved_chrome_value(&self) -> Value {
        let theme = json!({
            "selection_background": color_to_value(&self.theme.selection_bg),
            "selection_foreground": self.theme.selection_fg.as_ref().and_then(color_to_value),
            "sidebar_rail": color_to_value(&self.theme.sidebar_rail),
            "sidebar_active_bg": color_to_value(&self.theme.sidebar_active_bg),
            "tab_rail": color_to_value(&self.theme.tab_rail),
            "tab_bg": color_to_value(&self.theme.tab_bg),
            "tab_active_bg": self.theme.tab_active_bg.as_ref().and_then(color_to_value),
            "border_active": color_to_value(&self.theme.border_active),
            "border_inactive": color_to_value(&self.theme.border_inactive),
        });
        let tabs = json!({
            "min_width": self.tabs.min_width,
            "solid_background": self.tabs.solid_background,
            "show_titles": self.tabs.show_titles,
            "agents": self.tabs.agents,
        });
        let sidebar = json!({
            "width": self.sidebar.width,
            "max_width": self.sidebar.max_width,
        });
        let keys = serde_json::to_value(self.keys.to_raw_map()).unwrap_or_else(|_| json!({}));
        json!({ "theme": theme, "tabs": tabs, "sidebar": sidebar, "keys": keys })
    }

    /// Rebuild a `Config` from a server's `resolved_chrome_value` payload:
    /// start from `Config::default()` and re-apply each present chrome
    /// section through the same `apply_*_raw` helpers `load()` uses, so
    /// the round-trip is byte-identical to the server's resolution. Used
    /// as the base for a thin-client attach, then the local `Overlay` is
    /// layered on top. Browser/scrollbar stay at defaults: the server
    /// keeps the truth for them and the attach client does not spawn
    /// browsers locally.
    pub fn from_server_chrome(value: &Value) -> Config {
        let mut config = Config::default();
        if let Some(theme) = value.get("theme").filter(|v| !v.is_null()) {
            if let Ok(raw) = serde_json::from_value::<RawTheme>(theme.clone()) {
                apply_theme_raw(&mut config, &raw);
            }
        }
        if let Some(tabs) = value.get("tabs").filter(|v| !v.is_null()) {
            if let Ok(raw) = serde_json::from_value::<RawTabs>(tabs.clone()) {
                apply_tabs_raw(&mut config, &raw);
            }
        }
        if let Some(sidebar) = value.get("sidebar").filter(|v| !v.is_null()) {
            if let Ok(raw) = serde_json::from_value::<RawSidebar>(sidebar.clone()) {
                apply_sidebar_raw(&mut config, &raw);
            }
        }
        if let Some(keys) = value.get("keys").filter(|v| v.is_object()) {
            if let Ok(raw) = serde_json::from_value::<HashMap<String, Value>>(keys.clone()) {
                config.keys.apply(&raw);
            }
        }
        config
    }
}

/// Serialise a resolved `Color` back into the raw config shape that
/// `ColorValue`/`parse_color` accept: `#rrggbb` for true-colour, a bare
/// number for an xterm-256 index. Named/reset colours have no portable
/// raw form and yield `None` (which renders as `null` and is ignored on
/// the client), matching the overlay's permissive overlay semantics.
fn color_to_value(color: &Color) -> Option<Value> {
    match color {
        Color::Rgb(r, g, b) => Some(Value::String(format!("#{r:02x}{g:02x}{b:02x}"))),
        Color::Indexed(i) => Some(Value::Number((*i).into())),
        _ => None,
    }
}

/// Apply a raw theme table onto a resolved config: every present colour
/// overrides the seeded/default value. Shared by `load()` (server config)
/// and `Overlay::apply` (client overlay), so the two never drift.
fn apply_theme_raw(config: &mut Config, t: &RawTheme) {
    if let Some(c) = t.selection_background.as_ref().and_then(ColorValue::to_color) {
        config.theme.selection_bg = c;
    }
    match t.selection_foreground.as_ref() {
        None => {}
        Some(None) => config.theme.selection_fg = None,
        Some(Some(c)) => {
            if let Some(color) = c.to_color() {
                config.theme.selection_fg = Some(color);
            }
        }
    }
    if let Some(c) = t.sidebar_rail.as_ref().and_then(ColorValue::to_color) {
        config.theme.sidebar_rail = c;
    }
    if let Some(c) = t.sidebar_active_bg.as_ref().and_then(ColorValue::to_color) {
        config.theme.sidebar_active_bg = c;
    }
    if let Some(c) = t.tab_rail.as_ref().and_then(ColorValue::to_color) {
        config.theme.tab_rail = c;
    }
    if let Some(c) = t.tab_bg.as_ref().and_then(ColorValue::to_color) {
        config.theme.tab_bg = c;
    }
    if let Some(c) = t.tab_active_bg.as_ref().and_then(ColorValue::to_color) {
        config.theme.tab_active_bg = Some(c);
    }
    if let Some(c) = t.border_active.as_ref().and_then(ColorValue::to_color) {
        config.theme.border_active = c;
    }
    if let Some(c) = t.border_inactive.as_ref().and_then(ColorValue::to_color) {
        config.theme.border_inactive = c;
    }
}

/// Apply raw tab overrides onto a resolved config.
fn apply_tabs_raw(config: &mut Config, t: &RawTabs) {
    if let Some(w) = t.min_width {
        config.tabs.min_width = w.clamp(3, 40);
    }
    if let Some(b) = t.solid_background {
        config.tabs.solid_background = b;
    }
    if let Some(b) = t.show_titles {
        config.tabs.show_titles = b;
    }
    if let Some(agents) = t.agents.clone() {
        config.tabs.agents = agents.into_iter().map(|a| a.to_lowercase()).collect();
    }
}

/// Apply raw sidebar overrides onto a resolved config.
fn apply_sidebar_raw(config: &mut Config, s: &RawSidebar) {
    if let Some(w) = s.width {
        config.sidebar.width = w.clamp(10, 60);
    }
    if let Some(w) = s.max_width {
        config.sidebar.max_width = w;
    }
}

/// Load the config: defaults, overlaid with the user's Ghostty selection
/// colors, overlaid with `mux.json`.
pub fn load() -> Config {
    let mut config = Config::default();

    if let Some((bg, fg)) = ghostty_selection_colors() {
        if let Some(bg) = bg {
            config.theme.selection_bg = bg;
        }
        config.theme.selection_fg = fg;
    }

    let raw = load_raw_config();
    let raw_theme = match &raw.theme {
        ThemeValue::String(name) => {
            if name == "none" {
                None
            } else {
                load_preset(name)
            }
        }
        ThemeValue::Table(t) => Some(t.clone()),
    };
    if let Some(t) = raw_theme {
        apply_theme_raw(&mut config, &t);
    }
    apply_tabs_raw(&mut config, &raw.tabs);
    apply_sidebar_raw(&mut config, &raw.sidebar);
    config.browser.chrome_binary = raw.browser.chrome_binary.filter(|s| !s.trim().is_empty());
    config.browser.cdp_url = raw.browser.cdp_url.filter(|s| !s.trim().is_empty());
    if let Some(discover) = raw.browser.discover {
        config.browser.discover = discover;
    }
    if let Some(ports) = raw.browser.discover_ports {
        config.browser.discover_ports = ports;
    }
    config.browser.user_data_dir = raw.browser.user_data_dir.filter(|s| !s.trim().is_empty());
    if let Some(ephemeral) = raw.browser.ephemeral {
        config.browser.ephemeral = ephemeral;
    }
    if let Some(megapixels) = raw.browser.max_capture_megapixels {
        if megapixels.is_finite() && megapixels > 0.0 {
            config.browser.max_capture_megapixels = megapixels;
        } else {
            eprintln!("mtyx: ignoring browser.max_capture_megapixels={megapixels:?}; expected > 0");
        }
    }
    if let Some(scale) = raw.browser.capture_scale {
        if scale.is_finite() && scale > 0.0 && scale <= 1.0 {
            config.browser.capture_scale = Some(scale);
        } else {
            eprintln!("mtyx: ignoring browser.capture_scale={scale:?}; expected 0 < scale <= 1");
        }
    }
    if let Some(position) = raw.scrollbar.position {
        config.scrollbar.position = position;
    }
    // Issue #99: `headless.vt_size = "100x30"`; an unparseable value
    // warns and is ignored (config degradation, matching the browser
    // settings).
    if let Some(raw_size) = &raw.headless.vt_size {
        match mux_core::parse_vt_size(raw_size) {
            Some(size) => config.headless.vt_size = Some(size),
            None => eprintln!(
                "mtyx: ignoring headless.vt_size={raw_size:?}; expected COLSxROWS, e.g. \"100x30\""
            ),
        }
    }
    config.keys.apply(&raw.keys);
    apply_worktree_pattern(&mut config, &raw.worktree_pattern);
    config.workspaces = raw.workspaces;
    // Issue #78 AC7: `[[agent_detection]]` entries apply in order; the
    // last value for each key wins. An unknown min_confidence warns and
    // is ignored (config degradation, matching the browser settings).
    for entry in &raw.agent_detection {
        if let Some(enabled) = entry.enabled {
            config.agent_detection.enabled = enabled;
        }
        if let Some(confidence) = &entry.min_confidence {
            match mux_core::agent_detect::Confidence::parse(confidence) {
                Some(parsed) => config.agent_detection.min_confidence = parsed,
                None => eprintln!(
                    "mtyx: ignoring agent_detection.min_confidence={confidence:?}; expected high, medium, or low"
                ),
            }
        }
    }
    config
}

/// Resolve `[[worktree_pattern]]` onto the config: the FIRST entry wins
/// (array tables can repeat; a single entry is the intended use).
fn apply_worktree_pattern(config: &mut Config, raw: &[WorktreePatternConfig]) {
    config.worktree_pattern = raw.first().map(|entry| entry.pattern.clone());
}

// --- Local config overlay (issue #40) ---
//
// The overlay is the *client* side of an attach: a typed subset of the
// server config (theme/tabs/sidebar/keys only) that the laptop layers on
// top of the server-side session so colours and key bindings travel with
// the operator. Browser panes, scrollbar, and the session name are
// server-side truth and must NOT be overridable here, so `RawOverlay`
// carries `#[serde(deny_unknown_fields)]` to reject them at parse time
// (AC7) instead of silently ignoring them.

/// Raw deserialization shape for a local overlay file. Only the chrome
/// fields the client is allowed to override; anything else (browser,
// session, scrollbar) is rejected by `deny_unknown_fields`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)] // overlay: reject server-only chrome fields
struct RawOverlay {
    #[serde(default)]
    theme: Option<ThemeValue>,
    #[serde(default)]
    tabs: Option<RawTabs>,
    #[serde(default)]
    sidebar: Option<RawSidebar>,
    #[serde(default)]
    keys: Option<HashMap<String, Value>>,
}

/// Typed client overlay: a subset of `Config` (theme/tabs/sidebar/keys)
/// applied on top of the server-side config during `mtyx attach`. The
/// browser, scrollbar, and session name stay server-side truth.
#[derive(Debug, Default)]
pub struct Overlay {
    theme: Option<ThemeValue>,
    tabs: Option<RawTabs>,
    sidebar: Option<RawSidebar>,
    keys: Option<HashMap<String, Value>>,
}

impl Overlay {
    fn from_raw(raw: RawOverlay) -> Self {
        Overlay { theme: raw.theme, tabs: raw.tabs, sidebar: raw.sidebar, keys: raw.keys }
    }

    /// Resolve a string/table theme the same way `load()` does, then layer
    /// the present chrome fields onto `config`. Browser and scrollbar are
    /// intentionally untouched: the server keeps the truth for the tree.
    pub fn apply(&self, config: &mut Config) {
        if let Some(tv) = &self.theme {
            let resolved = match tv {
                ThemeValue::String(name) => {
                    if name == "none" {
                        None
                    } else {
                        load_preset(name)
                    }
                }
                ThemeValue::Table(t) => Some(t.clone()),
            };
            if let Some(t) = resolved {
                apply_theme_raw(config, &t);
            }
        }
        if let Some(t) = &self.tabs {
            apply_tabs_raw(config, t);
        }
        if let Some(s) = &self.sidebar {
            apply_sidebar_raw(config, s);
        }
        if let Some(k) = &self.keys {
            config.keys.apply(k);
        }
    }

    /// How many top-level chrome keys this overlay overrides, for the
    /// `mtyx: applying local config from <path> (overrides N keys)` log
    /// line. Counts a section once if it is present at all.
    pub fn override_count(&self) -> usize {
        let mut n = 0;
        if self.theme.is_some() {
            n += 1;
        }
        if self.tabs.is_some() {
            n += 1;
        }
        if self.sidebar.is_some() {
            n += 1;
        }
        if self.keys.is_some() {
            n += 1;
        }
        n
    }
}

/// Resolve which local overlay file would apply for an attach, without
/// reading it. Resolution order (AC2): explicit `--config <path>` ->
/// `$MTYX_LOCAL_CONFIG` -> `~/.config/mattyx/mux.local.toml` ->
/// `~/.config/mattyx/mux.json` -> `None` (server-side config wins). The
/// explicit and env cases are returned as-is even when the file does not
/// exist, so the caller can log the missing path rather than silently
/// falling back to the server config.
pub fn local_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("MTYX_LOCAL_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let dir = platform::config_dir()?;
    let local_toml = dir.join("mux.local.toml");
    if local_toml.exists() {
        return Some(local_toml);
    }
    let json = dir.join("mux.json");
    if json.exists() {
        return Some(json);
    }
    None
}

/// Read and parse a local overlay file into an `Overlay`. Returns `None`
/// (with a stderr note, mirroring `load_raw_config`) when the file is
/// missing or fails to parse, so the attach path degrades to the
/// server-side config instead of taking the TUI down.
pub fn load_overlay_file(path: &Path) -> Option<Overlay> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return None;
    };
    let parsed = if is_toml_path(path) {
        toml::from_str::<RawOverlay>(&text).map_err(|e| e.to_string())
    } else if is_json_path(path) {
        serde_json::from_str::<RawOverlay>(&text).map_err(|e| e.to_string())
    } else if looks_like_json(&text) {
        serde_json::from_str::<RawOverlay>(&text).map_err(|e| e.to_string())
    } else {
        toml::from_str::<RawOverlay>(&text).map_err(|e| e.to_string())
    };
    match parsed {
        Ok(raw) => Some(Overlay::from_raw(raw)),
        Err(e) => {
            eprintln!("mtyx: ignoring invalid local config {}: {e}", path.display());
            None
        }
    }
}

/// Load a preset by name from the bundled themes directory.
/// Returns `None` if the preset is not found or cannot be parsed.
fn load_preset(name: &str) -> Option<RawTheme> {
    let themes_dir = std::env::current_dir().unwrap_or_else(|_| std::path::Path::new(".").into());
    let theme_file = themes_dir.join("themes").join(format!("{name}.toml"));
    let text = std::fs::read_to_string(theme_file).ok()?;
    toml::from_str::<RawTheme>(&text).ok()
}

/// The label for a tab: user name if set, otherwise its 1-based number
/// plus a recognized agent program name (or the full title when
/// `show_titles` is on).
pub fn tab_label(tabs: &Tabs, index: usize, title: &str, name: Option<&str>) -> String {
    if let Some(name) = name {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    let number = index + 1;
    let suffix = if tabs.show_titles {
        (!title.is_empty()).then(|| title.to_string())
    } else {
        agent_in_title(tabs, title)
    };
    match suffix {
        Some(suffix) => format!("{number} {suffix}"),
        None => format!("{number}"),
    }
}

/// The first configured agent program appearing as a word in the title.
fn agent_in_title(tabs: &Tabs, title: &str) -> Option<String> {
    let lower = title.to_lowercase();
    let words: Vec<&str> =
        lower.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_').collect();
    tabs.agents.iter().find(|agent| words.contains(&agent.as_str())).cloned()
}

fn load_raw_config() -> RawConfig {
    let Some(path) = platform::config_path() else { return RawConfig::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return RawConfig::default() };
    let parsed = if is_toml_path(&path) {
        toml::from_str(&text).map_err(|e| e.to_string())
    } else if is_json_path(&path) {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    } else if looks_like_json(&text) {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    } else {
        toml::from_str(&text).map_err(|e| e.to_string())
    };
    match parsed {
        Ok(config) => config,
        Err(e) => {
            // A broken config should not take the TUI down; complain on
            // stderr (visible pre-alternate-screen and in logs).
            eprintln!("mtyx: ignoring invalid config {}: {e}", path.display());
            RawConfig::default()
        }
    }
}

/// True when the path extension (or `MTYX_MUX_CONFIG` value) marks a
/// TOML file.
fn is_toml_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
}

/// True when the path extension marks a JSON file.
fn is_json_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// Content sniff for files with no recognised extension: a leading `{`
/// (after whitespace and Byte-Order-Mark) means JSON; anything else is
/// TOML. TOML never starts with a brace.
fn looks_like_json(text: &str) -> bool {
    let mut chars = text.chars();
    if text.starts_with('\u{feff}') {
        chars.next();
    }
    chars.find(|c| !c.is_whitespace()).is_some_and(|c| c == '{')
}

/// `#rrggbb`, `#rgb`, or an xterm-256 index in a string.
pub(crate) fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return match hex.len() {
            6 => {
                let n = u32::from_str_radix(hex, 16).ok()?;
                Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
            }
            3 => {
                let n = u16::from_str_radix(hex, 16).ok()?;
                let (r, g, b) = ((n >> 8) & 0xf, (n >> 4) & 0xf, n & 0xf);
                Some(Color::Rgb((r * 17) as u8, (g * 17) as u8, (b * 17) as u8))
            }
            _ => None,
        };
    }
    s.parse::<u8>().ok().map(Color::Indexed)
}

/// The user's Ghostty selection colors, if a Ghostty config exists.
/// Returns (background, foreground); either may be absent. Ghostty's
/// config is `key = value` lines; later entries win, matching Ghostty.
fn ghostty_selection_colors() -> Option<(Option<Color>, Option<Color>)> {
    let text =
        platform::ghostty_config_paths().iter().find_map(|p| std::fs::read_to_string(p).ok())?;
    let mut bg = None;
    let mut fg = None;
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "selection-background" => bg = parse_color(value.trim()),
            "selection-foreground" => fg = parse_color(value.trim()),
            _ => {}
        }
    }
    Some((bg, fg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `MTYX_MUX_CONFIG` is process-global state; tests that set it must not
    /// run concurrently with each other.
    static CONFIG_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn workspace_definitions_load_from_toml_and_json() {
        let toml: RawConfig = toml::from_str(
            r##"[[workspaces]]
name = "Build"
color = "blue"
icon = "robot"
"##,
        )
        .unwrap();
        assert_eq!(toml.workspaces[0].name, "Build");
        assert_eq!(toml.workspaces[0].color.as_deref(), Some("blue"));
        assert_eq!(toml.workspaces[0].icon.as_deref(), Some("robot"));

        let json: RawConfig = serde_json::from_str(
            r##"{"workspaces":[{"name":"Docs","color":"#123456","icon":"eye"}]}"##,
        )
        .unwrap();
        assert_eq!(json.workspaces[0].name, "Docs");
        assert_eq!(json.workspaces[0].color.as_deref(), Some("#123456"));
    }

    /// Issue #78 AC7: `[[agent_detection]]` parses from TOML (array of
    /// tables) and JSON (array), and resolves through `load()` — last
    /// entry wins per key.
    #[test]
    fn agent_detection_loads_from_toml_and_json() {
        let toml: RawConfig = toml::from_str(
            r##"[[agent_detection]]
enabled = false
min_confidence = "high"
"##,
        )
        .unwrap();
        assert_eq!(toml.agent_detection.len(), 1);
        assert_eq!(toml.agent_detection[0].enabled, Some(false));
        assert_eq!(toml.agent_detection[0].min_confidence.as_deref(), Some("high"));

        let json: RawConfig = serde_json::from_str(
            r##"{"agent_detection":[{"enabled":false,"min_confidence":"medium"}]}"##,
        )
        .unwrap();
        assert_eq!(json.agent_detection[0].enabled, Some(false));
        assert_eq!(json.agent_detection[0].min_confidence.as_deref(), Some("medium"));

        // Resolution through load(): the table reaches the resolved Config.
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("mux-agent-detect-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.toml");
        std::fs::write(
            &path,
            r##"[[agent_detection]]
enabled = false
min_confidence = "high"
"##,
        )
        .unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        let config = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        assert!(!config.agent_detection.enabled, "detection should resolve to disabled");
        assert_eq!(config.agent_detection.min_confidence, mux_core::agent_detect::Confidence::High);
    }

    /// Review fix F2: typos in `[[agent_detection]]` keys (e.g.
    /// `enable = false`, `min-confidences = "high"`) must be rejected,
    /// not silently ignored — a user who thinks they disabled detection
    /// silently keeps it on otherwise. This matches the discipline of
    /// every sibling sub-table (`deny_unknown_fields`) documented in
    /// `RawOverlay`/`RawConfig`.
    #[test]
    fn agent_detection_rejects_unknown_keys() {
        // TOML: `enable` (missing `d`) and `min-confidences` (plural, hyphen).
        let bad_toml: Result<RawConfig, _> =
            toml::from_str("[[agent_detection]]\nenable = false\nmin_confidences = \"high\"\n");
        assert!(bad_toml.is_err(), "unknown keys in [[agent_detection]] must be rejected");

        // JSON: `minConfidence` (camelCase) and `enable` (missing `d`).
        let bad_json: Result<RawConfig, _> = serde_json::from_str(
            r##"{"agent_detection":[{"enable":false,"minConfidence":"high"}]}"##,
        );
        assert!(bad_json.is_err(), "unknown keys in agent_detection JSON must be rejected");

        // Sanity: well-known keys still parse (this stays valid in both
        // directions; the negative case above is the lock-down).
        let good_toml: RawConfig =
            toml::from_str("[[agent_detection]]\nenabled = false\nmin_confidence = \"high\"\n")
                .unwrap();
        assert_eq!(good_toml.agent_detection[0].enabled, Some(false));
        let good_json: RawConfig = serde_json::from_str(
            r##"{"agent_detection":[{"enabled":false,"min_confidence":"medium"}]}"##,
        )
        .unwrap();
        assert_eq!(good_json.agent_detection[0].enabled, Some(false));
    }

    /// Issue #99: `headless.vt_size` parses from TOML and JSON, resolves
    /// through `load()`, and an unparseable value is ignored (with a
    /// warning) rather than failing the config.
    #[test]
    fn headless_vt_size_loads_from_toml_and_json() {
        let toml: RawConfig = toml::from_str("[headless]\nvt_size = \"100x30\"\n").unwrap();
        assert_eq!(toml.headless.vt_size.as_deref(), Some("100x30"));

        let json: RawConfig =
            serde_json::from_str(r##"{"headless":{"vt_size":"100x30"}}"##).unwrap();
        assert_eq!(json.headless.vt_size.as_deref(), Some("100x30"));

        // Unknown keys in the table are rejected, like every other
        // config table.
        assert!(toml::from_str::<RawConfig>("[headless]\nbogus = true\n").is_err());

        // Resolution through load(): a valid value reaches the resolved
        // config; an unparseable one is dropped, keeping the default.
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("mux-headless-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.toml");
        std::fs::write(&path, "[headless]\nvt_size = \"100x30\"\n").unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        let config = load();
        assert_eq!(config.headless.vt_size, Some((100, 30)));
        std::fs::write(&path, "[headless]\nvt_size = \"huge\"\n").unwrap();
        let config = load();
        assert_eq!(config.headless.vt_size, None, "unparseable vt_size must be ignored");
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn worktree_pattern_parses_toml_table_array() {
        // `[[worktree_pattern]]` mirrors the `[[workspaces]]` array-table
        // shape (issue #77 AC6). Multiple entries are allowed; the first
        // wins when resolved (`load`).
        let toml: RawConfig = toml::from_str(
            r##"[[worktree_pattern]]
pattern = "../wt/<branch>"

[[worktree_pattern]]
pattern = "../other/<branch>"
"##,
        )
        .unwrap();
        assert_eq!(toml.worktree_pattern[0].pattern, "../wt/<branch>");
        assert_eq!(toml.worktree_pattern.len(), 2);

        let json: RawConfig =
            serde_json::from_str(r##"{"worktree_pattern":[{"pattern":"../j/<branch>"}]}"##)
                .unwrap();
        assert_eq!(json.worktree_pattern[0].pattern, "../j/<branch>");

        // Unknown keys in the table are rejected (deny_unknown_fields),
        // consistent with every other config table.
        assert!(toml::from_str::<RawConfig>(
            r##"[[worktree_pattern]]
pattern = "../wt/<branch>"
bogus = true
"##
        )
        .is_err());
    }

    #[test]
    fn worktree_pattern_defaults_when_absent() {
        let mut config = Config::default();
        apply_worktree_pattern(&mut config, &[]);
        assert_eq!(config.worktree_pattern, None, "no [[worktree_pattern]] keeps the default");

        let mut raw = RawConfig::default();
        raw.worktree_pattern = vec![WorktreePatternConfig { pattern: "../wt/<branch>".into() }];
        apply_worktree_pattern(&mut config, &raw.worktree_pattern);
        assert_eq!(config.worktree_pattern.as_deref(), Some("../wt/<branch>"));

        // First entry wins when several are present.
        raw.worktree_pattern.push(WorktreePatternConfig { pattern: "../z/<branch>".into() });
        apply_worktree_pattern(&mut config, &raw.worktree_pattern);
        assert_eq!(config.worktree_pattern.as_deref(), Some("../wt/<branch>"));
    }

    #[test]
    fn parses_hex_and_indexed_colors() {
        assert_eq!(parse_color("#3a3a3a"), Some(Color::Rgb(0x3a, 0x3a, 0x3a)));
        assert_eq!(parse_color("#fff"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_color("110"), Some(Color::Indexed(110)));
        assert_eq!(parse_color("not-a-color"), None);
        assert_eq!(parse_color("#12345"), None);
    }

    #[test]
    fn tab_labels_are_numbers_except_agents() {
        let tabs = Tabs::default();
        assert_eq!(tab_label(&tabs, 0, "", None), "1");
        assert_eq!(tab_label(&tabs, 1, "zsh", None), "2");
        assert_eq!(tab_label(&tabs, 2, "vim src/main.rs", None), "3");
        // Recognized agent programs surface in the label.
        assert_eq!(tab_label(&tabs, 0, "claude", None), "1 claude");
        assert_eq!(tab_label(&tabs, 3, "✳ Codex CLI", None), "4 codex");
        assert_eq!(tab_label(&tabs, 4, "opencode - fix bug", None), "5 opencode");
        // "pi" matches only as a word, not inside other words.
        assert_eq!(tab_label(&tabs, 5, "pick a file", None), "6");
        assert_eq!(tab_label(&tabs, 5, "pi chat", None), "6 pi");
        assert_eq!(tab_label(&tabs, 5, "pi chat", Some("api")), "api");

        let titled = Tabs { show_titles: true, ..Tabs::default() };
        assert_eq!(tab_label(&titled, 1, "zsh", None), "2 zsh");
    }

    #[test]
    fn config_overrides_defaults() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("mux-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.json");
        std::fs::write(
            &path,
            r##"{
                "theme": {
                    "selection_background": "#101010",
                    "sidebar_rail": 42,
                    "sidebar_active_bg": "#202020",
                    "tab_bg": 44
                },
                "tabs": {"min_width": 9, "solid_background": false},
                "sidebar": {"width": 30, "max_width": 38},
                "scrollbar": {"position": "border"},
                "keys": {
                    "alt_shortcuts": false,
                    "rename-pane": "r",
                    "focus-left": ["left", "alt+h"],
                    "next-tab": "none",
                    "browser-edit-url": "u"
                }
            }"##,
        )
        .unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        let config = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_file(&path);
        assert_eq!(config.theme.selection_bg, Color::Rgb(0x10, 0x10, 0x10));
        assert_eq!(config.theme.sidebar_rail, Color::Indexed(42));
        assert_eq!(config.theme.sidebar_active_bg, Color::Rgb(0x20, 0x20, 0x20));
        assert_eq!(config.theme.tab_bg, Color::Indexed(44));
        assert_eq!(config.tabs.min_width, 9);
        assert!(!config.tabs.solid_background);
        assert_eq!(config.sidebar.width, 30);
        assert_eq!(config.sidebar.max_width, 38);
        assert_eq!(config.scrollbar.position, ScrollbarPosition::Border);
        assert_eq!(
            config.keys.action_for(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(Action::RenameTab)
        );
        assert_eq!(config.keys.action_for(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)), None);
        assert_eq!(
            config.keys.action_for(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            Some(Action::BrowserEditUrl)
        );
        assert_eq!(
            config.keys.modeless_action_for(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)),
            None
        );
        assert_eq!(
            config.keys.modeless_action_for(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)),
            Some(Action::FocusLeft)
        );
        // Untouched keys keep their default.
        assert_eq!(config.theme.border_inactive, Theme::default().border_inactive);
    }

    #[test]
    fn default_key_table_has_no_duplicate_chords_or_reserved_alt_words() {
        let keys = Keys::default();
        for (i, (left, _)) in keys.bindings.iter().enumerate() {
            assert!(
                !keys.bindings.iter().skip(i + 1).any(|(right, _)| left == right),
                "duplicate default chord: {left:?}"
            );
        }
        for c in ['b', 'f', 'd', '.'] {
            assert_eq!(
                keys.modeless_action_for(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)),
                None
            );
        }
    }

    #[test]
    fn chord_matches_requires_shift_for_non_char_codes() {
        let shift_left = Chord { code: KeyCode::Left, mods: KeyModifiers::SHIFT };
        assert!(shift_left.matches(&KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)));
        assert!(!shift_left.matches(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));

        let plain_left = Chord { code: KeyCode::Left, mods: KeyModifiers::NONE };
        assert!(plain_left.matches(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        assert!(!plain_left.matches(&KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)));
    }

    #[test]
    fn selection_foreground_absent_vs_null_are_distinct() {
        // Absent key: `Option<Option<_>>` outer is None, meaning "no
        // override" (the Ghostty-seeded value, if any, is kept).
        let absent: RawConfig = serde_json::from_str(r##"{"theme": {}}"##).unwrap();
        let RawTheme { selection_foreground, .. } = match &absent.theme {
            ThemeValue::Table(t) => t,
            _ => panic!("expected table theme"),
        };
        assert!(selection_foreground.is_none());

        // Explicit `null`: outer is `Some(None)`, meaning "clear it".
        let explicit_null: RawConfig =
            serde_json::from_str(r##"{"theme": {"selection_foreground": null}}"##).unwrap();
        let RawTheme { selection_foreground, .. } = match &explicit_null.theme {
            ThemeValue::Table(t) => t,
            _ => panic!("expected table theme"),
        };
        assert!(matches!(selection_foreground, Some(None)));
    }

    #[test]
    fn selection_foreground_null_clears_ghostty_seeded_default() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("mux-config-test-selfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.json");
        std::fs::write(&path, r##"{"theme": {"selection_foreground": null}}"##).unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        // `load()` always seeds `selection_fg` from the Ghostty selection
        // colors (or leaves it `None` if there aren't any) before applying
        // this override, so regardless of the ambient Ghostty config, an
        // explicit `null` here must land back on `None`.
        let config = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_file(&path);
        assert_eq!(config.theme.selection_fg, None);
    }

    #[test]
    fn browser_capture_config_validates_bounds() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir()
            .join(format!("mux-config-test-browser-capture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.json");
        std::fs::write(
            &path,
            r##"{"browser": {"max_capture_megapixels": 3.5, "capture_scale": 0.5}}"##,
        )
        .unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        let config = load();
        assert_eq!(config.browser.max_capture_megapixels, 3.5);
        assert_eq!(config.browser.capture_scale, Some(0.5));

        std::fs::write(
            &path,
            r##"{"browser": {"max_capture_megapixels": 0, "capture_scale": 1.5}}"##,
        )
        .unwrap();
        let config = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            config.browser.max_capture_megapixels,
            Browser::default().max_capture_megapixels
        );
        assert_eq!(config.browser.capture_scale, None);
    }

    // --- TOML config support (issue #37) ---

    /// JSON and TOML configs carrying identical data, used to verify
    /// every key round-trips between the two formats.
    const JSON_EXAMPLE: &str = r##"{
  "theme": {
    "selection_background": "#355c7d",
    "selection_foreground": "#ffffff",
    "sidebar_rail": "#87afd7",
    "sidebar_active_bg": 236,
    "tab_rail": "#87afd7",
    "tab_bg": 236,
    "tab_active_bg": "#87afd7",
    "border_active": "#87afd7",
    "border_inactive": "#444444"
  },
  "tabs": {
    "min_width": 9,
    "solid_background": true,
    "show_titles": false,
    "agents": ["claude", "codex", "grok", "opencode", "pi"]
  },
  "sidebar": { "width": 24, "max_width": 40 },
  "browser": {
    "chrome_binary": "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "cdp_url": "http://127.0.0.1:9222",
    "discover": true,
    "discover_ports": [9222, 9223],
    "user_data_dir": "/Users/me/Library/Application Support/mattyx/chrome-profile",
    "ephemeral": false,
    "max_capture_megapixels": 2.0,
    "capture_scale": 0.5
  },
  "scrollbar": { "position": "column" },
  "keys": {
    "prefix": "ctrl+a",
    "alt_shortcuts": false,
    "new-tab": ["t", "alt+t"],
    "new_browser_tab": "B",
    "new-pane-smart": "alt+n",
    "next-tab": "tab",
    "prev-tab": "backtab",
    "next-screen": ["n", "alt+]"],
    "prev-screen": ["p", "alt+["],
    "rename-tab": "r",
    "rename-screen": ",",
    "focus-left": ["h", "left", "alt+h", "alt+left"],
    "focus-right": ["l", "right", "alt+l", "alt+right"],
    "close-pane": "none",
    "detach": "d"
  }
}
"##;

    const TOML_EXAMPLE: &str = r##"
# mtyx TOML config: the user-facing surface. When both mux.json and
# mux.toml exist, mux.json wins (it is the explicit override).
[theme]
selection_background = "#355c7d"
selection_foreground = "#ffffff"
sidebar_rail = "#87afd7"
sidebar_active_bg = 236
tab_rail = "#87afd7"
tab_bg = 236
tab_active_bg = "#87afd7"
border_active = "#87afd7"
border_inactive = "#444444"

[tabs]
min_width = 9
solid_background = true
show_titles = false
agents = ["claude", "codex", "grok", "opencode", "pi"]

[sidebar]
width = 24
max_width = 40

[browser]
chrome_binary = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
cdp_url = "http://127.0.0.1:9222"
discover = true
discover_ports = [9222, 9223]
user_data_dir = "/Users/me/Library/Application Support/mattyx/chrome-profile"
ephemeral = false
max_capture_megapixels = 2.0
capture_scale = 0.5

[scrollbar]
position = "column"

[keys]
prefix = "ctrl+a"
alt_shortcuts = false
"new-tab" = ["t", "alt+t"]
"new_browser_tab" = "B"
"new-pane-smart" = "alt+n"
"next-tab" = "tab"
"prev-tab" = "backtab"
"next-screen" = ["n", "alt+]"]
"prev-screen" = ["p", "alt+["]
"rename-tab" = "r"
"rename-screen" = ","
"focus-left" = ["h", "left", "alt+h", "alt+left"]
"focus-right" = ["l", "right", "alt+l", "alt+right"]
"close-pane" = "none"
detach = "d"
"##;

    fn unique_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mux-config-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn toml_round_trips_identically_to_json() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = unique_dir("rt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let json_path = dir.join("mux.json");
        let toml_path = dir.join("mux.toml");
        std::fs::write(&json_path, JSON_EXAMPLE).unwrap();
        std::fs::write(&toml_path, TOML_EXAMPLE).unwrap();

        std::env::set_var("MTYX_MUX_CONFIG", &json_path);
        let from_json = load();
        std::env::set_var("MTYX_MUX_CONFIG", &toml_path);
        let from_toml = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);

        // The `keys.bindings` Vec is rebuilt by iterating a HashMap, whose
        // order is randomised per map, so two `load()` calls do not produce
        // a stable Vec order. Compare every typed field plus the bindings as
        // a set so the round trip is about identity of the resolved config,
        // not the storage order of an unordered map.
        assert_eq!(from_json.theme, from_toml.theme);
        assert_eq!(from_json.tabs, from_toml.tabs);
        assert_eq!(from_json.sidebar, from_toml.sidebar);
        assert_eq!(from_json.browser, from_toml.browser);
        assert_eq!(from_json.scrollbar, from_toml.scrollbar);
        assert_eq!(from_json.keys.prefix, from_toml.keys.prefix);
        assert_eq!(from_json.keys.bindings.len(), from_toml.keys.bindings.len());
        for (chord, action) in &from_json.keys.bindings {
            assert!(
                from_toml.keys.bindings.iter().any(|(c, a)| c == chord && a == action),
                "JSON-only binding {chord:?} -> {action:?} absent in TOML round trip"
            );
        }
        // Sanity: the JSON path also produced concrete overrides, not just
        // the defaults, so the equality above is meaningful.
        assert_eq!(from_json.theme.sidebar_rail, Color::Rgb(0x87, 0xaf, 0xd7));
        assert_eq!(from_json.tabs.min_width, 9);
        assert_eq!(from_json.sidebar.width, 24);
        assert_eq!(from_json.browser.discover_ports, vec![9222, 9223]);
        assert_eq!(from_json.scrollbar.position, ScrollbarPosition::Column);
        assert_eq!(from_json.keys.prefix.mods & KeyModifiers::CONTROL, KeyModifiers::CONTROL);
        assert_eq!(
            from_json.keys.action_for(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(Action::RenameTab)
        );
    }

    #[test]
    fn mux_toml_loaded_when_only_toml_present() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let dir = unique_dir("only-toml");
        let _ = std::fs::remove_dir_all(&dir);
        let mtyx_dir = dir.join("mattyx");
        std::fs::create_dir_all(&mtyx_dir).unwrap();
        // A minimal TOML that overrides one distinguishable colour.
        std::fs::write(
            mtyx_dir.join("mux.toml"),
            r##"
[theme]
sidebar_rail = 99
"##,
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let config = load();
        // Capture the path while XDG_CONFIG_HOME still points at the temp
        // dir and the files exist; config_path() does its own existence
        // check, so it must run before cleanup.
        let resolved = platform::config_path();
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(config.theme.sidebar_rail, Color::Indexed(99));
        assert_eq!(resolved, Some(dir.join("mattyx").join("mux.toml")));
    }

    #[test]
    fn mux_json_wins_when_both_present() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let dir = unique_dir("json-wins");
        let _ = std::fs::remove_dir_all(&dir);
        let mtyx_dir = dir.join("mattyx");
        std::fs::create_dir_all(&mtyx_dir).unwrap();
        std::fs::write(mtyx_dir.join("mux.json"), r##"{"theme": {"sidebar_rail": 42}}"##).unwrap();
        std::fs::write(
            mtyx_dir.join("mux.toml"),
            r##"
[theme]
sidebar_rail = 99
"##,
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let config = load();
        let resolved = platform::config_path();
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);

        // JSON is the explicit override and wins over TOML.
        assert_eq!(config.theme.sidebar_rail, Color::Indexed(42));
        assert_eq!(resolved, Some(dir.join("mattyx").join("mux.json")));
    }

    #[test]
    fn mtyx_mux_config_accepts_toml_or_json_by_extension() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = unique_dir("ext");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let toml_path = dir.join("config.toml");
        std::fs::write(
            &toml_path,
            r##"
[theme]
sidebar_rail = 77
"##,
        )
        .unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &toml_path);
        assert_eq!(load().theme.sidebar_rail, Color::Indexed(77));

        let json_path = dir.join("config.json");
        std::fs::write(&json_path, r##"{"theme": {"sidebar_rail": 66}}"##).unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &json_path);
        assert_eq!(load().theme.sidebar_rail, Color::Indexed(66));

        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_g_opens_fuzzy_finder() {
        let keys = Keys::default();
        // Shift+G chord (KeyCode::Char('G') with no modifiers, since the
        // `bind` helper drops shift for char chords) opens the finder. The
        // configurable set contains the binding's action and its key maps
        // back to "open-fuzzy-finder", so users can rebind it.
        assert_eq!(
            keys.action_for(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)),
            Some(Action::OpenFuzzyFinder)
        );
    }

    #[test]
    fn default_question_mark_opens_help() {
        let keys = Keys::default();
        // The question mark is the unmodified suffix after the Ctrl-b prefix.
        assert_eq!(
            keys.action_for(&KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            Some(Action::ShowHelp)
        );
    }

    /// T14 (issue #63 L3): capital `S` (leader prefix + S) opens the
    /// session manager overlay. Mirrors `default_g_opens_fuzzy_finder`.
    /// `S` was previously stale help text ("new screen" is `c` today) and
    /// is otherwise unbound, so binding it here is safe.
    #[test]
    fn default_capital_s_opens_session_manager() {
        let keys = Keys::default();
        assert_eq!(
            keys.action_for(&KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)),
            Some(Action::OpenSessionManager)
        );
    }

    /// T15 (issue #63 L3 / #40): the new action is in `all_actions()` so a
    /// user can rebind it ("open-session-manager" = "..."), and it survives
    /// the server-chrome `to_raw_map` → `apply` round trip a thin client
    /// relies on.
    #[test]
    fn session_manager_action_is_rebindable_and_round_trips() {
        assert!(
            all_actions().contains(&Action::OpenSessionManager),
            "OpenSessionManager must be in all_actions() to be rebindable"
        );

        // Rebind it to ctrl-o and confirm the binding takes.
        let mut keys = Keys::default();
        let mut raw: HashMap<String, Value> = HashMap::new();
        raw.insert("open-session-manager".to_string(), Value::String("ctrl+o".to_string()));
        keys.apply(&raw);
        assert_eq!(
            keys.action_for(&KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Action::OpenSessionManager),
            "rebinding open-session-manager to ctrl+o should take effect"
        );
        // The default `S` chord should be gone after the rebind (apply
        // replaces ALL default chords for the action).
        assert_eq!(
            keys.action_for(&KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)),
            None,
            "default S chord should be cleared by the rebind"
        );

        // The default binding round-trips through to_raw_map → apply so a
        // thin-client attach reproduces the server's resolved keys (#40).
        let raw = Keys::default().to_raw_map();
        assert!(
            raw.get("open-session-manager").is_some(),
            "open-session-manager must appear in to_raw_map"
        );
        let mut round_tripped = Keys { prefix: Keys::default().prefix, bindings: Vec::new() };
        round_tripped.apply(&raw);
        assert_eq!(
            round_tripped.action_for(&KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)),
            Some(Action::OpenSessionManager),
            "default S binding must survive the to_raw_map → apply round trip"
        );
    }

    #[test]
    fn mtyx_mux_config_sniffs_content_when_extension_missing() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = unique_dir("sniff");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // No extension: TOML content (starts with a comment, not `{`).
        let toml_path = dir.join("config");
        std::fs::write(
            &toml_path,
            r##"
# a TOML mtyx config
[theme]
sidebar_rail = 55
"##,
        )
        .unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &toml_path);
        assert_eq!(load().theme.sidebar_rail, Color::Indexed(55));

        // No extension: JSON content (the first non-whitespace char is `{`).
        let json_path = dir.join("cfg");
        std::fs::write(&json_path, r##"{"theme": {"sidebar_rail": 44}}"##).unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &json_path);
        assert_eq!(load().theme.sidebar_rail, Color::Indexed(44));

        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Theme presets (issue #39) ---

    /// Helper: extract the color string from a ColorValue::Text variant.
    fn cv_text(s: &Option<ColorValue>) -> Option<&str> {
        s.as_ref().and_then(|cv| match cv {
            ColorValue::Text(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// Helper: build the path to the bundled themes directory from the
    /// crate's CARGO_MANIFEST_DIR.
    fn themes_dir() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("..");
        p.push("..");
        p.push("themes");
        p
    }

    #[test]
    fn theme_string_preset_deserializes() {
        // Claim 1: a string preset name deserializes as ThemeValue::String.
        let cfg: RawConfig = serde_json::from_str(r#"{"theme": "catpuccin-mocha"}"#).unwrap();
        match &cfg.theme {
            ThemeValue::String(s) if s == "catpuccin-mocha" => {}
            _ => panic!("expected ThemeValue::String(\"catpuccin-mocha\"), got {:?}", cfg.theme),
        }
    }

    #[test]
    fn theme_table_preset_deserializes() {
        // Claim 2: a table theme deserializes as ThemeValue::Table.
        let cfg: RawConfig =
            serde_json::from_str(r##"{"theme": {"sidebar_rail": "#87dcbf"}}"##).unwrap();
        match &cfg.theme {
            ThemeValue::Table(t) => {
                assert_eq!(cv_text(&t.sidebar_rail), Some("#87dcbf"));
            }
            _ => panic!("expected ThemeValue::Table, got {:?}", cfg.theme),
        }
    }

    #[test]
    fn theme_none_disables_preset() {
        // Claim 4 + 13: theme=\"none\" produces Theme::default() in load().
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = unique_dir("theme-none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.json");
        std::fs::write(&path, r#"{"theme": "none"}"#).unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        let config = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);

        // Theme::default() values.
        assert_eq!(config.theme.sidebar_rail, Color::Indexed(110));
        assert_eq!(config.theme.tab_bg, Color::Indexed(236));
        assert_eq!(config.theme.border_inactive, Color::Indexed(238));
    }

    #[test]
    fn preset_catpuccin_mocha_deserializes() {
        // Claim 5: catpuccin-mocha.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("catpuccin-mocha.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.sidebar_rail), Some("#cba6f7"));
        assert_eq!(cv_text(&raw.tab_bg), Some("#313244"));
    }

    #[test]
    fn preset_dracula_deserializes() {
        // Claim 6: dracula.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("dracula.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.sidebar_rail), Some("#bd93f9"));
        assert_eq!(cv_text(&raw.border_inactive), Some("#626262"));
    }

    #[test]
    fn preset_nord_deserializes() {
        // Claim 7: nord.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("nord.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.sidebar_rail), Some("#88a0b8"));
        assert_eq!(cv_text(&raw.tab_active_bg), Some("#eceff4"));
    }

    #[test]
    fn preset_gruvbox_dark_deserializes() {
        // Claim 8: gruvbox-dark.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("gruvbox-dark.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.sidebar_rail), Some("#d79921"));
        assert_eq!(cv_text(&raw.border_inactive), Some("#505050"));
    }

    #[test]
    fn preset_solarized_dark_deserializes() {
        // Claim 9: solarized-dark.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("solarized-dark.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.sidebar_rail), Some("#268bd2"));
        assert_eq!(cv_text(&raw.border_inactive), Some("#586e75"));
    }

    #[test]
    fn preset_solarized_light_deserializes() {
        // Claim 10: solarized-light.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("solarized-light.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.selection_background), Some("#fdf6e3"));
        assert_eq!(cv_text(&raw.tab_active_bg), Some("#073642"));
    }

    #[test]
    fn preset_catpuccin_latte_deserializes() {
        // Claim 11: catpuccin-latte.toml deserializes with correct colors.
        let themes_dir = themes_dir();
        let text = std::fs::read_to_string(themes_dir.join("catpuccin-latte.toml")).unwrap();
        let raw: RawTheme = toml::from_str(&text).unwrap();
        assert_eq!(cv_text(&raw.sidebar_rail), Some("#222218"));
        assert_eq!(cv_text(&raw.border_inactive), Some("#8c8fa1"));
    }

    #[test]
    fn preset_catpuccin_mocha_loads_via_load() {
        // Claim 12: theme=\"catpuccin-mocha\" in config resolves to preset colors.
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = unique_dir("theme-preset-load");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.json");
        std::fs::write(&path, r#"{"theme": "catpuccin-mocha"}"#).unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        // load_preset() resolves themes/ relative to current_dir(); cd to the
        // workspace root so it can find the bundled themes.
        let orig = std::env::current_dir().unwrap();
        let ws = orig.parent().unwrap().parent().unwrap().to_owned();
        std::env::set_current_dir(&ws).unwrap();
        let config = load();
        std::env::set_current_dir(&orig).unwrap();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(config.theme.sidebar_rail, Color::Rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(config.theme.tab_bg, Color::Rgb(0x31, 0x32, 0x44));
    }

    #[test]
    fn theme_table_override_keeps_defaults() {
        // Claim 14: explicit table override keeps Theme::default() for unset fields.
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let dir = unique_dir("theme-table-override");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mux.json");
        std::fs::write(&path, r##"{"theme": {"sidebar_rail": "#87dcbf"}}"##).unwrap();
        std::env::set_var("MTYX_MUX_CONFIG", &path);
        let config = load();
        std::env::remove_var("MTYX_MUX_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(config.theme.sidebar_rail, Color::Rgb(0x87, 0xdc, 0xbf));
        assert_eq!(config.theme.border_inactive, Color::Indexed(238));
    }

    // --- Local config overlay (issue #40) ---

    #[test]
    fn overlay_rejects_browser_and_session_fields() {
        // The overlay is a chrome-only subset: browser panes and the
        // session name are server-side truth. deny_unknown_fields rejects
        // them at parse time so a typo or a stale server-side block does
        // not silently get dropped (AC7).
        let with_browser: Result<RawOverlay, _> =
            serde_json::from_str(r##"{"browser": {"cdp_url": "http://localhost:9222"}}"##);
        assert!(with_browser.is_err(), "overlay must reject a browser field");

        let with_session: Result<RawOverlay, _> = serde_json::from_str(r##"{"session": "main"}"##);
        assert!(with_session.is_err(), "overlay must reject a session field");
    }

    #[test]
    fn overlay_applies_only_chrome_keys() {
        let raw: RawOverlay = toml::from_str(
            r##"
[theme]
sidebar_rail = 42

[tabs]
min_width = 9

[sidebar]
width = 30

[keys]
prefix = "ctrl+s"
"##,
        )
        .unwrap();
        let overlay = Overlay::from_raw(raw);
        assert_eq!(overlay.override_count(), 4);

        let mut config = Config::default();
        let browser_before = config.browser.clone();
        let scrollbar_before = config.scrollbar;
        overlay.apply(&mut config);

        // Chrome fields the overlay is allowed to touch: all applied.
        assert_eq!(config.theme.sidebar_rail, Color::Indexed(42));
        assert_eq!(config.tabs.min_width, 9);
        assert_eq!(config.sidebar.width, 30);
        assert_eq!(
            config.keys.prefix,
            Chord { code: KeyCode::Char('s'), mods: KeyModifiers::CONTROL }
        );

        // Server-side truth is untouched: browser and scrollbar stay as
        // they were (AC3/AC7).
        assert_eq!(config.browser, browser_before);
        assert_eq!(config.browser, Browser::default());
        assert_eq!(config.scrollbar, scrollbar_before);
    }

    #[test]
    fn local_config_path_resolution_order() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();

        // Explicit --config wins over env and XDG, even when the file
        // does not exist (the caller logs the missing path).
        let explicit = Path::new("/tmp/mtyx-overlay-explicit-4af0.toml");
        std::env::set_var("MTYX_LOCAL_CONFIG", "/tmp/mtyx-overlay-env-4af0.json");
        assert_eq!(local_config_path(Some(explicit)), Some(explicit.to_path_buf()));

        // With no explicit path, $MTYX_LOCAL_CONFIG wins over XDG.
        assert_eq!(local_config_path(None), Some(PathBuf::from("/tmp/mtyx-overlay-env-4af0.json")));
        std::env::remove_var("MTYX_LOCAL_CONFIG");

        // XDG mux.local.toml wins over mux.json when both exist.
        let dir = unique_dir("overlay-res");
        let _ = std::fs::remove_dir_all(&dir);
        let mtyx = dir.join("mattyx");
        std::fs::create_dir_all(&mtyx).unwrap();
        std::fs::write(mtyx.join("mux.local.toml"), "[theme]\nsidebar_rail = 1\n").unwrap();
        std::fs::write(mtyx.join("mux.json"), "{\"theme\": {\"sidebar_rail\": 2}}").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        assert_eq!(local_config_path(None), Some(mtyx.join("mux.local.toml")));

        // mux.json is the fallback when mux.local.toml is absent.
        let _ = std::fs::remove_file(mtyx.join("mux.local.toml"));
        assert_eq!(local_config_path(None), Some(mtyx.join("mux.json")));

        // Nothing present: None, so the attach uses server-side config.
        let _ = std::fs::remove_file(mtyx.join("mux.json"));
        assert_eq!(local_config_path(None), None);

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
