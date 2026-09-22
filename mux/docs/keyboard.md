# Keyboard

## Prefix Model

`mtyx` uses a tmux-style prefix. The default prefix is `Ctrl-b`. After the prefix, the next key is interpreted as a mux command. Pressing the prefix twice sends a literal `Ctrl-b` to the active surface.

Unknown prefixed keys are swallowed. Unprefixed non-Alt keys go to the active surface. Alt chords that are bound in the key table are modeless commands by default.

### Running tmux inside mtyx

mtyx's default prefix is `Ctrl-b` - the same as tmux's own default. If you run tmux
inside a mtyx pane, mtyx's outer prefix always wins: `Ctrl-b d`, for example, detaches
the *mtyx* TUI (the session daemon keeps running), not the inner tmux
one, since mtyx consumes the prefix before tmux ever sees it. Two ways to avoid this:

- **Rebind one side's prefix.** Either give mtyx a different prefix in `mux.json`
  (`{"keys": {"prefix": "ctrl+a"}}`, see [Configuration](configuration.md)), or rebind
  the *inner* tmux's prefix instead (tmux's own classic advice for nested sessions) -
  either way, the two prefixes stop colliding.
- **Use mtyx's existing double-prefix passthrough** for one-off keystrokes without
  reconfiguring anything: pressing the prefix twice sends a single literal prefix
  keystroke through to the active surface, so `Ctrl-b Ctrl-b d` reaches the inner
  tmux's `d` binding instead of mtyx's.

## Default Bindings

These defaults come from `Keys::default`.

| Binding | Action |
| --- | --- |
| `Ctrl-b t` | New PTY tab in the active pane |
| `Alt-t` | New PTY tab in the active pane |
| `Ctrl-b B` | Open the browser-tab URL prompt |
| `Alt-n` | Smart split into a new pane |
| `Ctrl-b Tab` | Next tab in the active pane |
| `Ctrl-b BackTab` | Previous tab in the active pane |
| `Ctrl-b 1` through `Ctrl-b 9` | Select tab 1 through 9 in the active pane |
| `Ctrl-b %` | Split the active pane right |
| `Ctrl-b "` | Split the active pane down |
| `Ctrl-b x` | Close the active tab |
| `Ctrl-b X` | Close the active pane |
| `Ctrl-b ,` | Rename the active screen |
| `Ctrl-b $` | Rename the active workspace |
| `Ctrl-b &` | Close the active screen |
| `Ctrl-b p` | Previous screen in the active workspace |
| `Alt-[` | Previous screen in the active workspace |
| `Ctrl-b n` | Next screen in the active workspace |
| `Alt-]` | Next screen in the active workspace |
| `Ctrl-b c` | New screen in the active workspace |
| `Ctrl-b w` | Next workspace |
| `Ctrl-b W` | New workspace |
| `Ctrl-b s` | Toggle the workspace sidebar |
| `Ctrl-b h` or `Ctrl-b Left` | Focus left |
| `Alt-h` or `Alt-Left` | Focus left |
| `Ctrl-b l` or `Ctrl-b Right` | Focus right |
| `Alt-l` or `Alt-Right` | Focus right |
| `Ctrl-b k` or `Ctrl-b Up` | Focus up |
| `Alt-k` or `Alt-Up` | Focus up |
| `Ctrl-b j` or `Ctrl-b Down` | Focus down |
| `Alt-j` or `Alt-Down` | Focus down |
| `Alt-=` | Grow the focused split |
| `Alt--` | Shrink the focused split |
| `Ctrl-b PageUp` | Scroll the active PTY viewport up 10 rows |
| `Ctrl-b PageDown` | Scroll the active PTY viewport down 10 rows |
| `Ctrl-b d` | Detach the TUI; the session daemon keeps running |

The screen bindings intentionally use tmux verbs: `c` creates a screen, `n` and `p` switch screens, `&` closes a screen, and `,` renames a screen. Tabs use `t`, `Tab`, `BackTab`, fixed number selectors, and tab-bar mouse actions.

## Copy and paste

These chords are handled by mtyx and do not need the prefix. They are not in the `keys` table.

| Binding | Action |
| --- | --- |
| `Ctrl-Shift-C` or `Cmd-C` | Copy the current selection |
| `Ctrl-Shift-V` or `Cmd-V` | Paste the system clipboard into the focused pane |

Bare `Ctrl-C` is still interrupt for the program in the pane. Bare `Ctrl-V` is still passed through to that program.

Apple Terminal.app does not send `Cmd` or `Ctrl-Shift` to the program. `Cmd-C` and `Cmd-V` are Terminal.app menu shortcuts, and `Ctrl-Shift-C` / `Ctrl-Shift-V` beep because Terminal.app has no binding for them. Drag to select instead: mtyx writes the text with `pbcopy`, and `Cmd-V` pastes it. Hold Option while dragging to make a native Terminal.app selection that `Cmd-C` can copy.

## Modeless Alt Layer

Any configured Alt chord is active without the prefix. Default modeless commands are `Alt-t`, `Alt-n`, `Alt-[`, `Alt-]`, `Alt-h/j/k/l`, Alt arrows, `Alt-=`, and `Alt--`. `Alt-n` is the default zellij-style smart split binding.

Set `keys.alt_shortcuts` to `false` to remove the default Alt bindings. This kill switch only removes defaults; Alt chords explicitly configured in `mux.json` still work.

## Fixed Number Selection

`1` through `9` select tabs by visible tab number after the prefix. These number bindings are fixed and are not configured through `mux.json`.

## Remapping

Keys are read from `~/.config/mattyx/mux.json`, or from the file named by `MTYX_MUX_CONFIG`.

Each action accepts a string, an array of strings, or `"none"`. Setting an action replaces all default chords for that action before adding the configured chords. `"none"` leaves the action unbound.

```json
{
  "keys": {
    "prefix": "ctrl+a",
    "alt_shortcuts": false,
    "new-tab": ["t", "alt+t"],
    "new-pane-smart": "alt+n",
    "next-screen": ["n", "alt+]"],
    "prev-screen": ["p", "alt+["],
    "focus-left": ["h", "left", "alt+h", "alt+left"],
    "rename-tab": "r",
    "rename-screen": ",",
    "close-pane": "none"
  }
}
```

Supported action keys are:

```text
new-tab
new_browser_tab
new-pane-smart
next-tab
prev-tab
split-right
split-down
close-tab
close-pane
rename-tab
rename-screen
rename-workspace
close-screen
prev-screen
next-screen
new-screen
next-workspace
new-workspace
toggle-sidebar
focus-left
focus-right
focus-up
focus-down
resize-grow
resize-shrink
scroll-up
scroll-down
detach
```

`rename-pane` is still accepted as an alias for `rename-tab`.

## Chord Format

Chord strings are case-sensitive for single characters. Uppercase letters and symbols represent the shifted character.

Supported examples include `"c"`, `"%"`, `"ctrl+b"`, `"alt+enter"`, `"tab"`, `"backtab"`, `"shift+tab"`, `"pageup"`, `"pagedown"`, `"esc"`, `"space"`, `"left"`, `"right"`, `"up"`, `"down"`, `"home"`, and `"end"`.

## Fuzzy Finder

Press `Ctrl-b G` (`leader G`) to open the fuzzy finder overlay, which lists
workspaces, panes, and surfaces for type-ahead navigation. While the finder
is open, the following state-filter keys narrow the result set to one agent
state. Pressing the same key again, or `Esc`, clears the filter back to the
full list. The active filter is shown on the finder title row (for example
`[blocked: B W I D A]`), not a separate footer.

| Key | Filter |
| --- | --- |
| `B` | blocked agents |
| `W` | working agents |
| `I` | idle agents |
| `D` | done agents |
| `A` | all agents (no filter) |

The finder subscribes to `agent-state-changed` events, so the filtered list
updates in real time as agents report new states while the overlay is open.
