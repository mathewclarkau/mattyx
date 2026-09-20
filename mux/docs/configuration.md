# Configuration

`mtyx` reads `~/.config/mattyx/mux.json`, or `$XDG_CONFIG_HOME/mattyx/mux.json` when `XDG_CONFIG_HOME` is set. Set `MTYX_MUX_CONFIG` to use another file; it takes precedence over both. Every documented key is optional. Unknown keys in the typed sections make the raw config invalid, so the TUI logs an error and falls back to defaults.

A TOML config file is also supported: `~/.config/mattyx/mux.toml` (or `$XDG_CONFIG_HOME/mattyx/mux.toml`) is loaded when `mux.json` is absent. When both files exist, `mux.json` wins (it is the explicit override, kept for tooling compatibility). `MTYX_MUX_CONFIG` may point at either a `.toml` or `.json` file; files with no recognised extension are sniffed by content (a leading `{` means JSON, otherwise TOML). Every documented `mux.json` key has an identical TOML equivalent; see the TOML example below.

Colors accept `#rrggbb`, `#rgb`, an xterm-256 number, or a numeric string. Workspace colours additionally accept the named presets `red`, `orange`, `yellow`, `green`, `blue`, `purple`, `pink`, `cyan`, and `grey`/`gray`.

## Workspaces

Define named workspaces that mtyx creates or updates when the server starts. Each entry requires `name`; `color` and `icon` are optional. Icons accept `folder`, `robot`, `eye`, `gear`, `search`, `magnifier`, `lock`, `check`, one Unicode character, or a `\\u{HEX}` escape.

```toml
[[workspaces]]
name = "Production Line"
color = "purple"
icon = "robot"

[[workspaces]]
name = "Quality Control"
color = "#00aaff"
icon = "eye"
```

The equivalent JSON shape is:

```json
{
  "workspaces": [
    {"name": "Production Line", "color": "purple", "icon": "robot"},
    {"name": "Quality Control", "color": "#00aaff", "icon": "eye"}
  ]
}
```

## Theme

Selection colors are resolved in this order: explicit `mux.json`, Ghostty config keys `selection-background` and `selection-foreground`, then built-in defaults. Ghostty configs are read from `$XDG_CONFIG_HOME/ghostty/config` (when set), `~/.config/ghostty/config`, and on macOS `~/Library/Application Support/com.mitchellh.ghostty/config`; later entries in the file win.

The `theme` key accepts either a string preset name or the nested table below. A string selects a bundled preset (all table fields populated from that preset); `"none"` disables preset loading and falls back to the built-in defaults. An explicit table overrides individual fields while leaving unspecified fields at their defaults.

### Presets

Bundled presets live in `mux/themes/`. Each `.toml` file supplies values for the theme fields it can map; any field the preset does not set falls back to the built-in default.

| Preset | Variant | Source |
| --- | --- | --- |
| `catpuccin-mocha` | dark | `mux/themes/catpuccin-mocha.toml` |
| `catpuccin-latte` | light | `mux/themes/catpuccin-latte.toml` |
| `dracula` | dark | `mux/themes/dracula.toml` |
| `solarized-dark` | dark | `mux/themes/solarized-dark.toml` |
| `solarized-light` | light | `mux/themes/solarized-light.toml` |
| `nord` | dark | `mux/themes/nord.toml` |
| `gruvbox-dark` | dark | `mux/themes/gruvbox-dark.toml` |

Use `mtyx theme list` to print available preset names and their bundled paths.

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `theme.selection_background` | color | `#3a3a3a`, seeded from Ghostty when present | Selection background in PTY panes |
| `theme.selection_foreground` | color or null | `null`, seeded from Ghostty when present | Selection foreground; `null` keeps each cell's foreground |
| `theme.sidebar_rail` | color | `110` | Rail color for the active workspace rows |
| `theme.sidebar_active_bg` | color | `236` | Background for the active workspace rows |
| `theme.tab_rail` | color | `110` | Rail color inside the active tab chip |
| `theme.tab_bg` | color | `236` | Background for inactive solid tab chips |
| `theme.tab_active_bg` | color or null | `null` | Overrides the focused and unfocused active-tab chip backgrounds |
| `theme.border_active` | color | `110` | Focused pane border |
| `theme.border_inactive` | color | `238` | Unfocused pane border |

## Tabs

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `tabs.min_width` | integer | `7` | Minimum tab label width, clamped to 3 through 40 |
| `tabs.solid_background` | boolean | `true` | Renders tab chips with solid backgrounds |
| `tabs.show_titles` | boolean | `false` | Shows full process titles after tab numbers |
| `tabs.agents` | string array | `["claude","codex","grok","opencode","pi"]` | Agent names surfaced in tab labels when `show_titles` is false |

Tabs are numbered by default. A recognized agent program can appear after the number. A user-assigned tab name replaces the generated label.

## Sidebar

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `sidebar.width` | integer | `22` | Sidebar width, clamped to 10 through 60 on load |
| `sidebar.max_width` | integer | `0` | Maximum live drag width; `0` means no configured maximum |

Live sidebar dragging also leaves at least 40 columns for pane content.

## Browser

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `browser.chrome_binary` | string | `null` | Chrome/Chromium binary to launch when no external CDP endpoint is used |
| `browser.cdp_url` | string | `null` | External CDP endpoint, accepted as `http://host:port` or `ws://...` |
| `browser.discover` | boolean | `true` | Probe discovery ports before launching Chrome |
| `browser.discover_ports` | integer array | `[9222]` | Local ports to probe for `/json/version` |
| `browser.user_data_dir` | string | `null` | Persistent profile directory for launched Chrome |
| `browser.ephemeral` | boolean | `false` | Use a temporary launched Chrome profile and delete it on shutdown |

When `browser.ephemeral` is true, it takes precedence over `browser.user_data_dir`: launched Chrome uses a fresh temporary profile, and the configured directory is not deleted.

The default launched profile is `~/Library/Application Support/mtyx/chrome-profile` on macOS. On non-macOS targets it is `$XDG_DATA_HOME/mattyx/chrome-profile` when `XDG_DATA_HOME` is set, then `~/.local/share/mattyx/chrome-profile`.

## Scrollbar

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `scrollbar.position` | `"column"` or `"border"` | `"column"` | Dedicated scrollbar column or right-border overlay |

## Keys

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `keys.prefix` | chord string | `"ctrl+b"` | Prefix chord |
| `keys.alt_shortcuts` | boolean | `true` | Enables default modeless Alt bindings when true |
| `keys.new-tab` | chord string or array or `"none"` | `["t","alt+t"]` | New PTY tab |
| `keys.new_browser_tab` | chord string or array or `"none"` | `"B"` | Browser URL prompt |
| `keys.new-pane-smart` | chord string or array or `"none"` | `"alt+n"` | New pane using smart split direction |
| `keys.next-tab` | chord string or array or `"none"` | `"tab"` | Next tab |
| `keys.prev-tab` | chord string or array or `"none"` | `"backtab"` | Previous tab |
| `keys.split-right` | chord string or array or `"none"` | `"%"` | Split right |
| `keys.split-down` | chord string or array or `"none"` | `"\""` | Split down |
| `keys.close-tab` | chord string or array or `"none"` | `"x"` | Close active tab |
| `keys.close-pane` | chord string or array or `"none"` | `"X"` | Close active pane |
| `keys.rename-tab` | chord string or array or `"none"` | unbound | Rename active tab |
| `keys.rename-pane` | chord string or array or `"none"` | alias | Alias for `rename-tab` |
| `keys.rename-screen` | chord string or array or `"none"` | `","` | Rename active screen |
| `keys.rename-workspace` | chord string or array or `"none"` | `"$"` | Rename active workspace |
| `keys.close-screen` | chord string or array or `"none"` | `"&"` | Close active screen |
| `keys.prev-screen` | chord string or array or `"none"` | `["p","alt+["]` | Previous screen |
| `keys.next-screen` | chord string or array or `"none"` | `["n","alt+]"]` | Next screen |
| `keys.new-screen` | chord string or array or `"none"` | `"c"` | New screen |
| `keys.next-workspace` | chord string or array or `"none"` | `"w"` | Next workspace |
| `keys.new-workspace` | chord string or array or `"none"` | `"W"` | New workspace |
| `keys.toggle-sidebar` | chord string or array or `"none"` | `"s"` | Toggle sidebar |
| `keys.focus-left` | chord string or array or `"none"` | `["h","left","alt+h","alt+left"]` | Focus left |
| `keys.focus-right` | chord string or array or `"none"` | `["l","right","alt+l","alt+right"]` | Focus right |
| `keys.focus-up` | chord string or array or `"none"` | `["k","up","alt+k","alt+up"]` | Focus up |
| `keys.focus-down` | chord string or array or `"none"` | `["j","down","alt+j","alt+down"]` | Focus down |
| `keys.resize-grow` | chord string or array or `"none"` | `"alt+="` | Grow the focused split |
| `keys.resize-shrink` | chord string or array or `"none"` | `"alt+-"` | Shrink the focused split |
| `keys.scroll-up` | chord string or array or `"none"` | `"pageup"` | Scroll active PTY up 10 rows |
| `keys.scroll-down` | chord string or array or `"none"` | `"pagedown"` | Scroll active PTY down 10 rows |
| `keys.detach` | chord string or array or `"none"` | `"d"` | Detach the TUI (session daemon keeps running) |

Each action override replaces all default chords for that action. Values may be a string, an array of strings, or `"none"`. Non-string array entries are ignored. Set `keys.alt_shortcuts` to `false` to remove default Alt chords before applying user overrides; explicitly configured Alt chords still work. Prefix `1` through `9` stay fixed to tab selection.

Chord strings can be single characters or a key name with optional `ctrl`, `control`, `alt`, `option`, or `shift` modifiers. Examples: `"c"`, `"%"`, `"ctrl+b"`, `"alt+enter"`, `"tab"`, `"backtab"`, `"shift+tab"`, `"pageup"`, `"pagedown"`, `"esc"`, `"space"`, `"left"`, `"right"`, `"up"`, `"down"`, `"home"`, and `"end"`.

## Example

```json
{
  "theme": {
    "selection_background": "#355c7d",
    "selection_foreground": null,
    "sidebar_rail": "#87afd7",
    "sidebar_active_bg": 236,
    "tab_rail": "#87afd7",
    "tab_bg": 236,
    "tab_active_bg": null,
    "border_active": "#87afd7",
    "border_inactive": "#444444"
  },
  "tabs": {
    "min_width": 9,
    "solid_background": true,
    "show_titles": false,
    "agents": ["claude", "codex", "grok", "opencode", "pi"]
  },
  "sidebar": {
    "width": 24,
    "max_width": 40
  },
  "browser": {
    "chrome_binary": "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "cdp_url": "http://127.0.0.1:9222",
    "discover": true,
    "discover_ports": [9222, 9223],
    "user_data_dir": "/Users/me/Library/Application Support/mtyx/chrome-profile",
    "ephemeral": false
  },
  "scrollbar": {
    "position": "column"
  },
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
```

## TOML example

The same config expressed as `mux.toml`. TOML is the user-facing surface:
nested tables read more naturally than JSON, comments are allowed, and
the enum field (`scrollbar.position`) and multi-value chord arrays map
cleanly. Keys whose JSON value is `null` (for example `selection_foreground`
or `tab_active_bg`) are simply omitted in TOML; omitting a key means "no
override", matching the JSON absent-key semantics.

```toml
# mtyx TOML config: the user-facing surface. When both mux.json and
# mux.toml exist, mux.json wins (it is the explicit override).

[theme]
selection_background = "#355c7d"
# selection_foreground omitted: keep the Ghostty-seeded value (or the default).
sidebar_rail = "#87afd7"
sidebar_active_bg = 236
tab_rail = "#87afd7"
tab_bg = 236
# tab_active_bg omitted: keep the focused/unfocused two-tone default.
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
user_data_dir = "/Users/me/Library/Application Support/mtyx/chrome-profile"
ephemeral = false

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
```

## Local config overlay (attach)

`mtyx attach --apply-local-config` layers the *local* `mux.local.toml`/`mux.json`
on top of the server-side session config, so the laptop acts as a thin client:
the server keeps the truth for the workspace tree (panes, browser, session
name), while the client wins for presentation chrome and key bindings (theme,
tabs, sidebar, keys). Per-key override, not replace: only the fields present in
the local file are applied, everything else stays server-side.

Resolution order for the overlay file:

1. explicit `--config <path>`
2. `$MTYX_LOCAL_CONFIG`
3. `~/.config/mattyx/mux.local.toml`
4. `~/.config/mattyx/mux.json`
5. server-side config (no overlay applied)

A local overlay is a typed subset of the full config: `theme`, `tabs`,
`sidebar`, and `keys` only. Browser panes, scrollbar, and the session name are
server-side truth and are rejected at parse time with a clear error (a stale
server-side `browser` block copied into the overlay will not be silently
ignored). When an overlay applies, attach logs:

```text
mtyx: applying local config from /home/me/.config/mattyx/mux.local.toml (overrides 2 keys)
```

`mtyx attach --show-local-config-resolution` is a dry run: it resolves and loads
the local file the same way, prints the resolved path plus the override count,
then exits without attaching. Use it to sanity check which config travels to a
remote box before connecting.
