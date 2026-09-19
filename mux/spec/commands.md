# Command Contract

This file specifies the JSON command contract for the mtyx protocol. Implemented commands match protocol v7 in `mux/crates/mux-core/src/server.rs`. Proposed commands are future protocol design (protocol 7 shipped the `identify` capabilities record and confirmed/receipted `send`, issue #88).

## Notation

Schema notation is compact and machine-oriented:

| Notation                    | Meaning                                                       |
| --------------------------- | ------------------------------------------------------------- |
| `uint64`                    | Non-negative integer fitting a Rust `u64`                     |
| `uint32`                    | Non-negative integer fitting a Rust `u32`                     |
| `uint16`                    | Non-negative integer fitting a Rust `u16`                     |
| `usize`                     | Non-negative integer fitting a Rust `usize`                   |
| `isize`                     | Signed integer fitting a Rust `isize`                         |
| `float32`                   | JSON number read as Rust `f32`                                |
| `string`, `boolean`, `null` | JSON primitive                                                |
| `T?`                        | Field may be absent or null unless the command says otherwise |
| `array<T>`                  | JSON array                                                    |
| `object{a:T,b?:U}`          | JSON object with required `a` and optional `b`                |
| `Base64`                    | Standard base64 string                                        |
| `ColorHex`                  | `#rrggbb`, exactly 7 bytes, ASCII hex                         |
| `Id`                        | Implemented numeric id, `uint64`                              |
| `IdRef`                     | Proposed id reference, `Id` or short id string                |

The canonical request and response envelope is defined in `transports.md`. Command blocks in this file define the command-specific request fields and response `data` shape.

Malformed JSON, unknown command names, missing required fields, and wrong JSON types fail during request decoding with the transport-level `bad request: ...` envelope.

The v5 server does not explicitly deny unknown JSON fields. Clients must not depend on unknown fields being rejected.

Common CLI exit codes for every mapping are `0` success, `1` command error, `2` CLI usage error, and `3` connection error.

## Shared Implemented Result Types

`Tree`:

```text
object{
  workspaces: array<object{
    id: Id,
    name: string,
    color: ColorHex|null,
    active: boolean,
    screens: array<object{
      id: Id,
      name: string|null,
      active: boolean,
      active_pane: Id,
      layout: Layout,
      panes: array<Pane>
    }>
  }>
}
```

`Layout`:

```text
object{type:"leaf",pane:Id}
| object{type:"split",dir:"right"|"down",ratio:float32,a:Layout,b:Layout}
```

`Pane`:

```text
object{id:Id,name:string|null,active_tab:usize,tabs:array<Tab>}
| object{id:Id,dead:true}
```

`Tab`:

```text
object{
  surface: Id,
  kind: "pty"|"browser",
  browser_source: "external"|"launched"|null,
  name: string|null,
  title: string,
  size: object{cols:uint16,rows:uint16}|null,
  dead: boolean
}
```

The `dead` pane variant is serialized by the v5 server only if the tree references a pane missing from state. That should not occur in normal operation, but clients must tolerate it.

## Implemented Commands

### identify

| Field  | Value       |
| ------ | ----------- |
| name   | `identify`  |
| status | implemented |
| since  | protocol 5  |

Returns process and protocol metadata for the connected mux server. Clients use this command to verify that the socket endpoint is mtyx and to check feature compatibility.

Since protocol 7 (issue #88) the result also carries a `capabilities` record for feature negotiation. The one defined capability is `input-ack` (confirmed/receipted `send`, see [`send`](#send)): a client wanting confirmed input must gate on `capabilities["input-ack"] == true` and refuse with a structured `legacy_host_receipt_rejected`-style error against a daemon lacking it, never silently downgrade. The bundled CLI performs this pre-flight automatically.

Params: none.

Result:

```text
object{app:"mtyx",version:string,protocol:uint32,capabilities:object{input-ack:bool},session:string,pid:uint32}
```

Errors:

| Error              | Condition                  |
| ------------------ | -------------------------- |
| `bad request: ...` | Malformed request envelope |

CLI mapping:

| Item         | Value                                                  |
| ------------ | ------------------------------------------------------ |
| Verb         | `identify`                                             |
| Flags        | none                                                   |
| Plain stdout | `mtyx session=<session> protocol=<protocol> pid=<pid>` |
| JSON stdout  | exact result object                                    |
| Exit codes   | common                                                 |

Example:

```json
{"id":1,"cmd":"identify"}
{"id":1,"ok":true,"data":{"app":"mtyx","version":"0.1.0","protocol":7,"capabilities":{"input-ack":true},"session":"main","pid":12345}}
```

### list-workspaces

| Field  | Value             |
| ------ | ----------------- |
| name   | `list-workspaces` |
| status | implemented       |
| since  | protocol 5        |

Returns the full workspace, screen, pane, tab, and split-tree snapshot. The snapshot includes active flags, active pane ids, active tab indexes, tab titles, tab names, surface kinds, browser source, size, and dead flags.

Params: none.

Result:

```text
Tree
```

Errors:

| Error              | Condition                  |
| ------------------ | -------------------------- |
| `bad request: ...` | Malformed request envelope |

CLI mapping:

| Item         | Value                                                |
| ------------ | ---------------------------------------------------- |
| Verb         | `list-workspaces`                                    |
| Flags        | none                                                 |
| Plain stdout | one stable line per workspace, screen, pane, and tab |
| JSON stdout  | exact result object                                  |
| Exit codes   | common                                               |

Example:

```json
{"id":2,"cmd":"list-workspaces"}
{"id":2,"ok":true,"data":{"workspaces":[{"id":4,"name":"1","color":null,"active":true,"screens":[{"id":3,"name":null,"active":true,"active_pane":2,"layout":{"type":"leaf","pane":2},"panes":[{"id":2,"name":null,"active_tab":0,"tabs":[{"surface":1,"kind":"pty","browser_source":null,"name":null,"title":"","size":{"cols":80,"rows":24},"dead":false}]}]}]}]}}
```

### get-resolved-config

| Field  | Value                 |
| ------ | --------------------- |
| name   | `get-resolved-config` |
| status | implemented           |
| since  | protocol 6            |

Returns the server process's resolved presentation chrome (theme, tabs, sidebar, keys) so a thin-client `mtyx attach --apply-local-config` can fetch it and layer the laptop's local `Overlay` on top of the _server_ config rather than replacing it with the laptop's own `config::load()` (issue #40). Browser and scrollbar are server-side truth and intentionally omitted: the server keeps them, the attach client does not spawn browsers or scrollbars locally. The shape matches `mux-tui`'s `Config::resolved_chrome_value`; a client rebuilds a base `Config` from it via `Config::from_server_chrome` and then applies the local `Overlay`. A server that has registered no chrome (e.g. a `mux-core`-only host with no TUI) returns an empty object `{}`.

Params: none.

Result:

```text
object{
  theme: object{
    selection_background: ColorHex?,
    selection_foreground: ColorHex?|null?,
    sidebar_rail: ColorHex?,
    sidebar_active_bg: ColorHex?,
    tab_rail: ColorHex?,
    tab_bg: ColorHex?,
    tab_active_bg: ColorHex?,
    border_active: ColorHex?,
    border_inactive: ColorHex?
  }?,
  tabs: object{
    min_width: uint16?,
    solid_background: boolean?,
    show_titles: boolean?,
    agents: array<string>?
  }?,
  sidebar: object{
    width: uint16?,
    max_width: uint16?
  }?,
  keys: object<string, string|array<string>>  // Raw keys map; see `Keys::apply` in mux-tui
}
```

Colours are `#rrggbb` for true colour, a bare integer for an xterm-256 index, or `null` when they have no portable raw form (named/reset colours). `keys` carries the same raw shape the config loader consumes: `prefix`, `alt_shortcuts`, and one entry per action config key mapped to a chord string or array of chord strings.

Errors:

| Error              | Condition                                      |
| ------------------ | ---------------------------------------------- |
| `bad request: ...` | Malformed request envelope (e.g. extra fields) |

CLI mapping:

| Item         | Value                                                                               |
| ------------ | ----------------------------------------------------------------------------------- |
| Verb         | `get-resolved-config`                                                               |
| Flags        | none (read-only metadata verb; supports global `--session`/`--socket` and `--json`) |
| Plain stdout | pretty JSON object, the server's resolved chrome                                    |
| JSON stdout  | exact result object                                                                 |
| Exit codes   | common                                                                              |

The same verb is invoked internally by `mtyx attach --apply-local-config`, which fetches the chrome, rebuilds a base `Config`, layers the local `Overlay`, and starts the TUI. `mtyx attach --print-resolved-config` (issue #40) prints the _merged_ chrome (server base + local overlay) as JSON without attaching, for inspecting layering without a live terminal.

Example:

```json
{"id":3,"cmd":"get-resolved-config"}
{"id":3,"ok":true,"data":{"theme":{"sidebar_rail":"#112233"},"tabs":{"min_width":8,"solid_background":true,"show_titles":true,"agents":[]},"sidebar":{"width":24,"max_width":40},"keys":{"alt_shortcuts":false,"prefix":"ctrl+b"}}}
```

### send

| Field  | Value       |
| ------ | ----------- |
| name   | `send`      |
| status | implemented |
| since  | protocol 5  |

Writes input to a PTY surface. `text`, when present, is UTF-8 encoded and written as bytes. `bytes`, when present, is standard base64 decoded and written as raw bytes. If both are present, v5 writes `text` first and `bytes` second. If neither is present, v5 returns success and writes nothing.

Params:

| Name         | JSON type | Required/default | Constraints                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------ | --------- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `surface`    | `Id`      | required         | Must identify a live PTY surface                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `text`       | `string`  | default null     | Written before `bytes` when both are present                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `bytes`      | `Base64`  | default null     | Decoded with standard base64                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `shell`      | `string`  | default null     | One of `auto`, `fish`, `bash`, `zsh`, `sh`, `nu`, `raw` (default `raw` = verbatim passthrough, unchanged from pre-#35). `auto` resolves the pane's shell from `/proc/<pid>/cmdline` on Linux and falls back to `raw` on lookup failure or non-Linux. For a known shell, a leading `\n` is prefixed to `text` when it starts with a shell metacharacter (`$`, `!`, quote, bracket, `~`, `#`) or contains an unclosed quote, so a pasted `$ pwd` is typed literally into a fish pane (issue #35). `bytes` is never transformed. |
| `confirm`    | `bool`    | default null     | Since protocol 7 (issue #88): request a RECEIPT. `true` means the response returns success only after the daemon observes the input consumed — the practical receipt is bytes written to the PTY AND the surface echoed/advanced (the reader thread applied output) or the child exited, within `timeout_ms`. This is a documented heuristic, not a byte-exact consumption proof. Absent/false keeps the pre-#88 fire-and-forget behavior. Concurrent confirmed sends to one surface resolve in submission order (per-surface FIFO). |
| `timeout_ms` | `uint64`  | default 5000     | Receipt timeout for `confirm: true`. Capped at 60000; `0` is rejected. The budget covers both the FIFO queue wait and the receipt wait. Ignored when `confirm` is not `true`.                                                                                                                                                                                                                                                                                    |

Result:

```text
object{}                      // unconfirmed (pre-#88 shape, unchanged)
object{confirmed:true}        // confirmed send that received its receipt
```

Errors:

| Error                                                     | Condition                                                     |
| --------------------------------------------------------- | ------------------------------------------------------------- |
| `unknown surface <id>`                                    | Surface id does not exist                                     |
| `browser surface does not support PTY/VT socket commands` | Surface is a browser                                          |
| base64 decode error                                       | `bytes` is not valid standard base64                          |
| IO error string                                           | PTY write fails                                               |
| `bad request: ...`                                        | Missing `surface` or wrong JSON type                          |
| `oversized_input: ...` (code `oversized_input`)           | Confirmed payload above 1 MiB (`MAX_CONFIRMED_SEND_BYTES`)    |
| `input_ack_timeout: ...` (code `input_ack_timeout`)       | No receipt within `timeout_ms` (or the FIFO turn never came)  |

Since protocol 7, structured errors also carry a machine-readable `"code"` field on the error response (the `error` string keeps the `<code>: ` prefix for string-matching callers). `oversized_input` and `input_ack_timeout` are the codes `send` can emit; `legacy_host_receipt_rejected` is the client-side code for the capability gate refusal (see [`identify`](#identify)).

CLI mapping:

| Item         | Value                                                                                          |
| ------------ | ---------------------------------------------------------------------------------------------- |
| Verb         | `send`                                                                                         |
| Flags        | `--surface <id> [--text <text>] [--bytes <base64>] [--shell <mode>] [--no-confirm] [--timeout-ms N]` |
| Plain stdout | no output                                                                                      |
| JSON stdout  | exact result object                                                                            |
| Exit codes   | common; additionally exit 1 on `input_ack_timeout` / `oversized_input` / `legacy_host_receipt_rejected` |

When neither `--text` nor `--bytes` is supplied, the CLI reads stdin as text and sends it as `text`.

Example:

```json
{"id":3,"cmd":"send","surface":1,"text":"ls\r"}
{"id":3,"ok":true,"data":{}}
```

### read-screen

| Field  | Value         |
| ------ | ------------- |
| name   | `read-screen` |
| status | implemented   |
| since  | protocol 5    |

Returns the current plain-text viewport of a PTY surface. The text is produced by the Ghostty VT terminal state and does not include prior scrollback beyond the current screen.

Params:

| Name      | JSON type | Required/default | Constraints                      |
| --------- | --------- | ---------------- | -------------------------------- |
| `surface` | `Id`      | required         | Must identify a live PTY surface |

Result:

```text
object{text:string}
```

Errors:

| Error                                                     | Condition                            |
| --------------------------------------------------------- | ------------------------------------ |
| `unknown surface <id>`                                    | Surface id does not exist            |
| `browser surface does not support PTY/VT socket commands` | Surface is a browser                 |
| terminal error string                                     | VT plain-text extraction fails       |
| `bad request: ...`                                        | Missing `surface` or wrong JSON type |

CLI mapping:

| Item         | Value               |
| ------------ | ------------------- |
| Verb         | `read-screen`       |
| Flags        | `--surface <id>`    |
| Plain stdout | `text` exactly      |
| JSON stdout  | exact result object |
| Exit codes   | common              |

Example:

```json
{"id":4,"cmd":"read-screen","surface":1}
{"id":4,"ok":true,"data":{"text":"$ ls\nREADME.md\n"}}
```

### vt-state

| Field  | Value       |
| ------ | ----------- |
| name   | `vt-state`  |
| status | implemented |
| since  | protocol 5  |

Returns a one-shot base64 VT replay for a PTY surface, including the current screen, styles, cursor, modes, palette, keyboard protocol state, charsets, and tabstops. Replaying this data into a fresh Ghostty VT terminal reproduces the surface state at the time of the snapshot.

Params:

| Name      | JSON type | Required/default | Constraints                      |
| --------- | --------- | ---------------- | -------------------------------- |
| `surface` | `Id`      | required         | Must identify a live PTY surface |

Result:

```text
object{cols:uint16,rows:uint16,data:Base64}
```

Errors:

| Error                                                     | Condition                            |
| --------------------------------------------------------- | ------------------------------------ |
| `unknown surface <id>`                                    | Surface id does not exist            |
| `browser surface does not support PTY/VT socket commands` | Surface is a browser                 |
| terminal error string                                     | VT replay generation fails           |
| `bad request: ...`                                        | Missing `surface` or wrong JSON type |

CLI mapping:

| Item         | Value                                   |
| ------------ | --------------------------------------- |
| Verb         | `vt-state`                              |
| Flags        | `--surface <id>`                        |
| Plain stdout | `cols=<cols> rows=<rows> data=<base64>` |
| JSON stdout  | exact result object                     |
| Exit codes   | common                                  |

Example:

```json
{"id":5,"cmd":"vt-state","surface":1}
{"id":5,"ok":true,"data":{"cols":80,"rows":24,"data":"G1s/bA=="}}
```

### new-tab

| Field  | Value       |
| ------ | ----------- |
| name   | `new-tab`   |
| status | implemented |
| since  | protocol 5  |

Creates a new PTY tab in a pane and makes it the active tab. If `pane` is absent, the active pane of the active screen is used. If the session has no workspaces and no pane is supplied, v5 creates a new workspace containing the tab. In that empty-session fallback, a supplied `cwd` is silently dropped because v5 delegates to `new_workspace(None, size)`. The new tab inherits the active surface working directory of the target pane when `cwd` is absent.

Params:

| Name     | JSON type | Required/default | Constraints                                   |
| -------- | --------- | ---------------- | --------------------------------------------- |
| `pane`   | `Id`      | default null     | Target pane; unknown ids error                |
| `cwd`    | `string`  | default null     | PTY child working directory                   |
| `cols`   | `uint16`  | default null     | Used only when paired with `rows`             |
| `rows`   | `uint16`  | default null     | Used only when paired with `cols`             |
| `branch` | `string`  | default null     | Issue #77: create a git worktree, spawn inside |
| `label`  | `string`  | default null     | Display label for the worktree record          |

If only one of `cols` or `rows` is present, v5 ignores both because the server uses `cols.zip(rows)`.

With `branch` (issue #77 AC4), the server runs `git worktree add -b <branch> <path>` BEFORE spawning the tab and passes the worktree path as the spawn cwd, so the pane starts inside the worktree; the agent is then launched into it by the existing `send` flow. The repository is resolved from `cwd` (explicit, else the target pane's working directory — the active pane when `pane` is absent). The record is attached to the pane owning the new tab and appears in `pane-worktree-list` and in `list-workspaces` pane JSON.

Result:

```text
object{surface:Id}
```

Errors:

| Error                                 | Condition                             |
| ------------------------------------- | ------------------------------------- |
| `unknown pane <id>`                   | Supplied pane id does not exist       |
| `pane disappeared while creating tab` | Target pane vanished after validation |
| `"<path>" is not inside a git repository` | `branch` set but no repository resolves |
| `cannot resolve a repository: ...`   | Empty session, `branch` set, no `cwd` |
| git error string                      | `git worktree add` failed (bad branch name, dirty tree, existing path) — nothing spawns |
| spawn or PTY error string             | PTY creation or child spawn fails     |
| `bad request: ...`                    | Wrong JSON type                       |

CLI mapping:

| Item         | Value                                                  |
| ------------ | ------------------------------------------------------ |
| Verb         | `new-tab`                                              |
| Flags        | `[--pane <id>] [--cwd <path>] [--cols <n> --rows <n>] [--branch <name> [--label <text>]] [--prompt-file <path>]` |
| Plain stdout | new surface id followed by newline                     |
| JSON stdout  | exact result object                                    |
| Exit codes   | common                                                 |

`--prompt-file` reads a leading `---` frontmatter block (`branch:`/`label:` keys, closed by `---`) from an agent prompt file and maps it onto `--branch`/`--label` (issue #77 AC4: "when a pane starts, if a `branch: <name>` frontmatter is present in the agent prompt, the pane auto-creates the worktree before launching the agent"). Strict: malformed frontmatter (unterminated block, unknown/duplicate keys, empty values) is a usage error, exit 2. `--prompt-file` cannot be combined with `--branch`/`--label`.

Example:

```json
{"id":6,"cmd":"new-tab","pane":2,"cwd":"/tmp","cols":100,"rows":30}
{"id":6,"ok":true,"data":{"surface":5}}
```

### new-browser-tab

| Field  | Value             |
| ------ | ----------------- |
| name   | `new-browser-tab` |
| status | implemented       |
| since  | protocol 5        |

Creates a browser tab in a pane and makes it active. If `pane` is absent, the active pane is used. If the session has no workspaces and no pane is supplied, v5 creates a new workspace containing the browser tab. The browser runtime may connect to an external CDP endpoint or launch Chrome according to mux configuration.

Params:

| Name   | JSON type | Required/default | Constraints                       |
| ------ | --------- | ---------------- | --------------------------------- |
| `url`  | `string`  | required         | Normalized by browser runtime     |
| `pane` | `Id`      | default null     | Target pane; unknown ids error    |
| `cols` | `uint16`  | default null     | Used only when paired with `rows` |
| `rows` | `uint16`  | default null     | Used only when paired with `cols` |

Result:

```text
object{surface:Id}
```

Errors:

| Error                                         | Condition                                                                     |
| --------------------------------------------- | ----------------------------------------------------------------------------- |
| `unknown pane <id>`                           | Supplied pane id does not exist                                               |
| `pane disappeared while creating browser tab` | Target pane vanished after validation                                         |
| browser/CDP error string                      | Browser runtime connect, target create, attach, setup, or Chrome launch fails |
| `bad request: ...`                            | Missing `url` or wrong JSON type                                              |

CLI mapping:

| Item         | Value                                               |
| ------------ | --------------------------------------------------- |
| Verb         | `new-browser-tab`                                   |
| Flags        | `--url <url> [--pane <id>] [--cols <n> --rows <n>]` |
| Plain stdout | new surface id followed by newline                  |
| JSON stdout  | exact result object                                 |
| Exit codes   | common                                              |

Example:

```json
{"id":7,"cmd":"new-browser-tab","url":"https://example.com","pane":2}
{"id":7,"ok":true,"data":{"surface":8}}
```

### new-workspace

| Field  | Value           |
| ------ | --------------- |
| name   | `new-workspace` |
| status | implemented     |
| since  | protocol 5      |

Creates a new workspace with one screen, one pane, and one PTY tab, then makes the new workspace active. If `name` is absent, the workspace name is the next 1-based workspace count at creation time.

Params:

| Name   | JSON type | Required/default | Constraints                              |
| ------ | --------- | ---------------- | ---------------------------------------- |
| `name` | `string`  | default null     | Workspace name; empty string is accepted |
| `cols` | `uint16`  | default null     | Used only when paired with `rows`        |
| `rows` | `uint16`  | default null     | Used only when paired with `cols`        |

Result:

```text
object{surface:Id}
```

Errors:

| Error                     | Condition                         |
| ------------------------- | --------------------------------- |
| spawn or PTY error string | PTY creation or child spawn fails |
| `bad request: ...`        | Wrong JSON type                   |

CLI mapping:

| Item         | Value                                     |
| ------------ | ----------------------------------------- |
| Verb         | `new-workspace`                           |
| Flags        | `[--name <name>] [--cols <n> --rows <n>]` |
| Plain stdout | new surface id followed by newline        |
| JSON stdout  | exact result object                       |
| Exit codes   | common                                    |

Example:

```json
{"id":8,"cmd":"new-workspace","name":"ops"}
{"id":8,"ok":true,"data":{"surface":10}}
```

### new-screen

| Field  | Value        |
| ------ | ------------ |
| name   | `new-screen` |
| status | implemented  |
| since  | protocol 5   |

Creates a new screen in a workspace with one pane and one PTY tab, then makes the new screen active. If `workspace` is absent, the active workspace is used. If no workspace exists and `workspace` is absent, v5 creates a new workspace instead.

Params:

| Name        | JSON type | Required/default | Constraints                         |
| ----------- | --------- | ---------------- | ----------------------------------- |
| `workspace` | `Id`      | default null     | Target workspace; unknown ids error |
| `cols`      | `uint16`  | default null     | Used only when paired with `rows`   |
| `rows`      | `uint16`  | default null     | Used only when paired with `cols`   |

Result:

```text
object{surface:Id}
```

Errors:

| Error                                         | Condition                                  |
| --------------------------------------------- | ------------------------------------------ |
| `unknown workspace <id>`                      | Supplied workspace id does not exist       |
| `workspace disappeared while creating screen` | Target workspace vanished after validation |
| spawn or PTY error string                     | PTY creation or child spawn fails          |
| `bad request: ...`                            | Wrong JSON type                            |

CLI mapping:

| Item         | Value                                        |
| ------------ | -------------------------------------------- |
| Verb         | `new-screen`                                 |
| Flags        | `[--workspace <id>] [--cols <n> --rows <n>]` |
| Plain stdout | new surface id followed by newline           |
| JSON stdout  | exact result object                          |
| Exit codes   | common                                       |

Example:

```json
{"id":9,"cmd":"new-screen","workspace":4}
{"id":9,"ok":true,"data":{"surface":12}}
```

### split

| Field  | Value       |
| ------ | ----------- |
| name   | `split`     |
| status | implemented |
| since  | protocol 5  |

Splits the screen containing `pane`, inserts a new pane after the target leaf, spawns one PTY tab in the new pane, and focuses the new pane. `dir:"right"` creates left/right columns. `dir:"down"` creates top/bottom rows. The new surface inherits the active surface working directory of the target pane when available.

Params:

| Name     | JSON type | Required/default | Constraints                                   |
| -------- | --------- | ---------------- | --------------------------------------------- |
| `pane`   | `Id`      | required         | Target split leaf                             |
| `dir`    | `string`  | required         | `"right"` or `"down"`                         |
| `cols`   | `uint16`  | default null     | Used only when paired with `rows`             |
| `rows`   | `uint16`  | default null     | Used only when paired with `cols`             |
| `branch` | `string`  | default null     | Issue #77: worktree; the NEW pane spawns inside and owns the record |
| `label`  | `string`  | default null     | Display label for the worktree record          |

Result:

```text
object{surface:Id}
```

Errors:

| Error                                        | Condition                                   |
| -------------------------------------------- | ------------------------------------------- |
| `bad dir "<value>" (want "right" or "down")` | `dir` is not allowed                        |
| `pane <id> not found`                        | Target pane is not in any screen split tree |
| `pane <id> has no working directory to resolve a repository from` | `branch` set but the pane has no known cwd |
| git error string                             | `git worktree add` failed; nothing spawns   |
| spawn or PTY error string                    | PTY creation or child spawn fails           |
| `bad request: ...`                           | Missing fields or wrong JSON type           |

CLI mapping:

| Item         | Value                              |
| ------------ | ---------------------------------- |
| Verb         | `split`                            |
| Flags        | `--pane <id> --dir right           | down [--cols <n> --rows <n>] [--branch <name> [--label <text>]] |
| Plain stdout | new surface id followed by newline |
| JSON stdout  | exact result object                |
| Exit codes   | common                             |

Example:

```json
{"id":10,"cmd":"split","pane":2,"dir":"right"}
{"id":10,"ok":true,"data":{"surface":14}}
```

### set-ratio

| Field  | Value       |
| ------ | ----------- |
| name   | `set-ratio` |
| status | implemented |
| since  | protocol 5  |

Sets the deepest split ratio in `dir` on the path to `pane`. The server clamps the supplied ratio to `0.05..0.95` before applying it. The result does not report the clamped value.

Params:

| Name    | JSON type | Required/default | Constraints                                    |
| ------- | --------- | ---------------- | ---------------------------------------------- |
| `pane`  | `Id`      | required         | Pane used to find a split on its ancestor path |
| `dir`   | `string`  | required         | `"right"` or `"down"`                          |
| `ratio` | `float32` | required         | Clamped to `0.05..0.95`                        |

Result:

```text
object{}
```

Errors:

| Error                                        | Condition                                      |
| -------------------------------------------- | ---------------------------------------------- |
| `bad dir "<value>" (want "right" or "down")` | `dir` is not allowed                           |
| `unknown pane/split <id>`                    | Pane is unknown or no ancestor split has `dir` |
| `bad request: ...`                           | Missing fields or wrong JSON type              |

CLI mapping:

| Item         | Value                    |
| ------------ | ------------------------ |
| Verb         | `set-ratio`              |
| Flags        | `--pane <id> --dir right | down --ratio <number>` |
| Plain stdout | no output                |
| JSON stdout  | exact result object      |
| Exit codes   | common                   |

Example:

```json
{"id":11,"cmd":"set-ratio","pane":2,"dir":"right","ratio":0.7}
{"id":11,"ok":true,"data":{}}
```

### set-default-colors

| Field  | Value                |
| ------ | -------------------- |
| name   | `set-default-colors` |
| status | implemented          |
| since  | protocol 5           |

Updates the session default foreground and/or background colors used by PTY surfaces. Missing fields preserve their previous values. Existing PTY surfaces receive the merged defaults. The v5 server emits `surface-output` for every existing surface, including browser surfaces; browser color application is a no-op, but the event is still emitted. Future PTY surfaces start with the merged defaults.

Params:

| Name | JSON type  | Required/default | Constraints      |
| ---- | ---------- | ---------------- | ---------------- |
| `fg` | `ColorHex` | default null     | Foreground color |
| `bg` | `ColorHex` | default null     | Background color |

Result:

```text
object{}
```

Errors:

| Error                                  | Condition                      |
| -------------------------------------- | ------------------------------ |
| `bad color "<value>" (want "#rrggbb")` | Color is not exactly `#rrggbb` |
| `bad request: ...`                     | Wrong JSON type                |

CLI mapping:

| Item         | Value                           |
| ------------ | ------------------------------- |
| Verb         | `set-default-colors`            |
| Flags        | `[--fg #rrggbb] [--bg #rrggbb]` |
| Plain stdout | no output                       |
| JSON stdout  | exact result object             |
| Exit codes   | common                          |

Example:

```json
{"id":12,"cmd":"set-default-colors","fg":"#d8d9da","bg":"#131415"}
{"id":12,"ok":true,"data":{}}
```

### close-surface

| Field  | Value           |
| ------ | --------------- |
| name   | `close-surface` |
| status | implemented     |
| since  | protocol 5      |

Closes one surface tab. The server kills the surface runtime, removes the tab from its pane, collapses an emptied pane out of its split tree, removes emptied screens and workspaces, and may emit `tree-changed` and `empty`.

Params:

| Name      | JSON type | Required/default | Constraints                  |
| --------- | --------- | ---------------- | ---------------------------- |
| `surface` | `Id`      | required         | Must identify a live surface |

Result:

```text
object{}
```

Errors:

| Error                  | Condition                              |
| ---------------------- | -------------------------------------- |
| `unknown surface <id>` | Surface id does not exist before close |
| `bad request: ...`     | Missing `surface` or wrong JSON type   |

CLI mapping:

| Item         | Value               |
| ------------ | ------------------- |
| Verb         | `close-surface`     |
| Flags        | `--surface <id>`    |
| Plain stdout | no output           |
| JSON stdout  | exact result object |
| Exit codes   | common              |

Example:

```json
{"id":13,"cmd":"close-surface","surface":1}
{"id":13,"ok":true,"data":{}}
```

### close-pane

| Field  | Value        |
| ------ | ------------ |
| name   | `close-pane` |
| status | implemented  |
| since  | protocol 5   |

Closes a pane and every tab in it. The pane is collapsed out of the screen split tree. Emptied screens and workspaces are removed.

Params:

| Name   | JSON type | Required/default | Constraints               |
| ------ | --------- | ---------------- | ------------------------- |
| `pane` | `Id`      | required         | Must identify a live pane |

Result:

```text
object{}
```

Errors:

| Error               | Condition                           |
| ------------------- | ----------------------------------- |
| `unknown pane <id>` | Pane id does not exist before close |
| `bad request: ...`  | Missing `pane` or wrong JSON type   |

CLI mapping:

| Item         | Value               |
| ------------ | ------------------- |
| Verb         | `close-pane`        |
| Flags        | `--pane <id>`       |
| Plain stdout | no output           |
| JSON stdout  | exact result object |
| Exit codes   | common              |

Example:

```json
{"id":14,"cmd":"close-pane","pane":2}
{"id":14,"ok":true,"data":{}}
```

### close-screen

| Field  | Value          |
| ------ | -------------- |
| name   | `close-screen` |
| status | implemented    |
| since  | protocol 5     |

Closes a screen and every pane and tab in it. The workspace remains if it still has screens; otherwise the workspace is removed.

Params:

| Name     | JSON type | Required/default | Constraints                 |
| -------- | --------- | ---------------- | --------------------------- |
| `screen` | `Id`      | required         | Must identify a live screen |

Result:

```text
object{}
```

Errors:

| Error                 | Condition                           |
| --------------------- | ----------------------------------- |
| `unknown screen <id>` | Screen id does not exist            |
| `bad request: ...`    | Missing `screen` or wrong JSON type |

CLI mapping:

| Item         | Value               |
| ------------ | ------------------- |
| Verb         | `close-screen`      |
| Flags        | `--screen <id>`     |
| Plain stdout | no output           |
| JSON stdout  | exact result object |
| Exit codes   | common              |

Example:

```json
{"id":15,"cmd":"close-screen","screen":3}
{"id":15,"ok":true,"data":{}}
```

### close-workspace

| Field  | Value             |
| ------ | ----------------- |
| name   | `close-workspace` |
| status | implemented       |
| since  | protocol 5        |

Closes a workspace and every screen, pane, and tab in it. The active workspace selection is adjusted to keep a remaining workspace active when possible.

Params:

| Name        | JSON type | Required/default | Constraints                    |
| ----------- | --------- | ---------------- | ------------------------------ |
| `workspace` | `Id`      | required         | Must identify a live workspace |

Result:

```text
object{}
```

Errors:

| Error                    | Condition                              |
| ------------------------ | -------------------------------------- |
| `unknown workspace <id>` | Workspace id does not exist            |
| `bad request: ...`       | Missing `workspace` or wrong JSON type |

CLI mapping:

| Item         | Value               |
| ------------ | ------------------- |
| Verb         | `close-workspace`   |
| Flags        | `--workspace <id>`  |
| Plain stdout | no output           |
| JSON stdout  | exact result object |
| Exit codes   | common              |

Example:

```json
{"id":16,"cmd":"close-workspace","workspace":4}
{"id":16,"ok":true,"data":{}}
```

### rename-pane

| Field  | Value         |
| ------ | ------------- |
| name   | `rename-pane` |
| status | implemented   |
| since  | protocol 5    |

Sets a pane user-visible name. An empty `name` clears the pane name so display falls back to the active tab title or shell label.

Params:

| Name   | JSON type | Required/default | Constraints               |
| ------ | --------- | ---------------- | ------------------------- |
| `pane` | `Id`      | required         | Must identify a live pane |
| `name` | `string`  | required         | Empty string clears       |

Result:

```text
object{}
```

Errors:

| Error               | Condition                         |
| ------------------- | --------------------------------- |
| `unknown pane <id>` | Pane id does not exist            |
| `bad request: ...`  | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                       |
| ------------ | --------------------------- |
| Verb         | `rename-pane`               |
| Flags        | `--pane <id> --name <name>` |
| Plain stdout | no output                   |
| JSON stdout  | exact result object         |
| Exit codes   | common                      |

Example:

```json
{"id":17,"cmd":"rename-pane","pane":2,"name":"logs"}
{"id":17,"ok":true,"data":{}}
```

### rename-surface

| Field  | Value            |
| ------ | ---------------- |
| name   | `rename-surface` |
| status | implemented      |
| since  | protocol 5       |

Sets a tab user-visible name on a surface. An empty `name` clears the tab name so display falls back to generated tab label and process title.

Params:

| Name      | JSON type | Required/default | Constraints                  |
| --------- | --------- | ---------------- | ---------------------------- |
| `surface` | `Id`      | required         | Must identify a live surface |
| `name`    | `string`  | required         | Empty string clears          |

Result:

```text
object{}
```

Errors:

| Error                  | Condition                         |
| ---------------------- | --------------------------------- |
| `unknown surface <id>` | Surface id does not exist         |
| `bad request: ...`     | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                          |
| ------------ | ------------------------------ |
| Verb         | `rename-surface`               |
| Flags        | `--surface <id> --name <name>` |
| Plain stdout | no output                      |
| JSON stdout  | exact result object            |
| Exit codes   | common                         |

Example:

```json
{"id":18,"cmd":"rename-surface","surface":1,"name":"api"}
{"id":18,"ok":true,"data":{}}
```

### rename-screen

| Field  | Value           |
| ------ | --------------- |
| name   | `rename-screen` |
| status | implemented     |
| since  | protocol 5      |

Sets a screen user-visible name. An empty `name` clears the screen name so display falls back to the screen number.

Params:

| Name     | JSON type | Required/default | Constraints                 |
| -------- | --------- | ---------------- | --------------------------- |
| `screen` | `Id`      | required         | Must identify a live screen |
| `name`   | `string`  | required         | Empty string clears         |

Result:

```text
object{}
```

Errors:

| Error                 | Condition                         |
| --------------------- | --------------------------------- |
| `unknown screen <id>` | Screen id does not exist          |
| `bad request: ...`    | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                         |
| ------------ | ----------------------------- |
| Verb         | `rename-screen`               |
| Flags        | `--screen <id> --name <name>` |
| Plain stdout | no output                     |
| JSON stdout  | exact result object           |
| Exit codes   | common                        |

Example:

```json
{"id":19,"cmd":"rename-screen","screen":3,"name":"build"}
{"id":19,"ok":true,"data":{}}
```

### rename-workspace

| Field  | Value              |
| ------ | ------------------ |
| name   | `rename-workspace` |
| status | implemented        |
| since  | protocol 5         |

Sets a workspace name. Unlike pane, surface, and screen names, an empty `name` is stored as the workspace name and does not clear to a generated fallback in v5.

Params:

| Name        | JSON type | Required/default | Constraints                    |
| ----------- | --------- | ---------------- | ------------------------------ |
| `workspace` | `Id`      | required         | Must identify a live workspace |
| `name`      | `string`  | required         | Empty string is stored         |

Result:

```text
object{}
```

Errors:

| Error                    | Condition                         |
| ------------------------ | --------------------------------- |
| `unknown workspace <id>` | Workspace id does not exist       |
| `bad request: ...`       | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                            |
| ------------ | -------------------------------- |
| Verb         | `rename-workspace`               |
| Flags        | `--workspace <id> --name <name>` |
| Plain stdout | no output                        |
| JSON stdout  | exact result object              |
| Exit codes   | common                           |

Example:

```json
{"id":20,"cmd":"rename-workspace","workspace":4,"name":"prod"}
{"id":20,"ok":true,"data":{}}
```

### set-workspace-color

| Field  | Value                 |
| ------ | --------------------- |
| name   | `set-workspace-color` |
| status | implemented           |
| since  | protocol 6            |

Sets or clears a workspace's display colour. A `#rrggbb` value or named preset in `colour` sets the workspace colour. `colour: null` clears it back to no colour; an absent `colour` key has the same effect as `null` at the protocol level. The CLI always sends the key explicitly and accepts `--color` as the primary spelling plus `--colour` as a back-compatible alias.

Params:

| Name        | JSON type                    | Required/default           | Constraints                                                                                                                                   |
| ----------- | ---------------------------- | -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `workspace` | `Id`                         | required                   | Must identify a live workspace                                                                                                                |
| `colour`    | `ColorHex \| string \| null` | optional (default: `null`) | Absent or `null` clears the colour; `#rrggbb` or `red`, `orange`, `yellow`, `green`, `blue`, `purple`, `pink`, `cyan`, `grey`/`gray` sets it. |

Result:

```text
object{}
```

Errors:

| Error                                                              | Condition                                    |
| ------------------------------------------------------------------ | -------------------------------------------- |
| `unknown workspace <id>`                                           | Workspace id does not exist                  |
| `bad workspace color "<value>" (want "#rrggbb" or a named preset)` | `colour` is not a hex colour or known preset |
| `bad request: ...`                                                 | Missing `workspace` or wrong JSON type       |

CLI mapping:

| Item         | Value                                                                              |
| ------------ | ---------------------------------------------------------------------------------- |
| Verb         | `set-workspace-color`                                                              |
| Flags        | `--workspace <id> --color <hex-or-preset>`; `--colour <hex-or-empty-string>` alias |
| Plain stdout | no output                                                                          |
| JSON stdout  | exact result object                                                                |
| Exit codes   | common                                                                             |

The CLI requires either `--color` or `--colour`; an empty alias value (`--colour ""`) sends `colour:null` to clear the colour, and any non-empty value is sent through for server validation.

The positional shorthand `mtyx workspace-color <name> <color>` updates a workspace by exact name, creating it first when absent.

### set-status

| Field  | Value        |
| ------ | ------------ |
| name   | `set-status` |
| status | implemented  |
| since  | protocol 6   |

Sets a workspace status icon. Bundled names are `folder`, `robot`, `eye`, `gear`, `search`, `magnifier`, `lock`, and `check`. A single Unicode character or a `\\u{HEX}` escape is also accepted.

Params:

| Name        | JSON type | Required/default | Constraints                                        |
| ----------- | --------- | ---------------- | -------------------------------------------------- |
| `icon`      | string    | required         | Bundled name, one Unicode character, or `\\u{HEX}` |
| `workspace` | `Id`      | active workspace | Must identify a live workspace when supplied       |

Result: `object{}`.

Errors:

| Error                                   | Condition                                               |
| --------------------------------------- | ------------------------------------------------------- |
| `unknown workspace <id>`                | Workspace id does not exist                             |
| `unknown workspace icon "<value>"; ...` | Icon is not bundled or a supported Unicode value        |
| `no active workspace`                   | `workspace` is omitted and the session has no workspace |

CLI mapping:

| Item         | Value                              |
| ------------ | ---------------------------------- |
| Verb         | `set-status`                       |
| Flags        | `--icon <name> [--workspace <id>]` |
| Plain stdout | no output                          |
| JSON stdout  | exact result object                |
| Exit codes   | common                             |

Example:

```json
{"id":31,"cmd":"set-status","workspace":4,"icon":"robot"}
{"id":31,"ok":true,"data":{}}
```

Example:

```json
{"id":29,"cmd":"set-workspace-color","workspace":4,"colour":"#ff8800"}
{"id":29,"ok":true,"data":{}}
```

Clearing a color sends `colour:null` instead:

```json
{"id":30,"cmd":"set-workspace-color","workspace":4,"colour":null}
{"id":30,"ok":true,"data":{}}
```

### trigger-flash

| Field  | Value           |
| ------ | --------------- |
| name   | `trigger-flash` |
| status | implemented     |
| since  | protocol 6      |

Emits a transient `flash` event to subscribers for a workspace, intended to draw attention to it (e.g. a sidebar pulse). `surface`, when present, is advisory only — it is passed straight through in the event and not validated against the workspace's own surfaces.

Params:

| Name        | JSON type | Required/default | Constraints                                      |
| ----------- | --------- | ---------------- | ------------------------------------------------ |
| `workspace` | `Id`      | required         | Must identify a live workspace                   |
| `surface`   | `Id`      | default null     | Advisory only; not validated against `workspace` |

Result:

```text
object{}
```

Errors:

| Error                    | Condition                              |
| ------------------------ | -------------------------------------- |
| `unknown workspace <id>` | Workspace id does not exist            |
| `bad request: ...`       | Missing `workspace` or wrong JSON type |

CLI mapping:

| Item         | Value                               |
| ------------ | ----------------------------------- |
| Verb         | `trigger-flash`                     |
| Flags        | `--workspace <id> [--surface <id>]` |
| Plain stdout | no output                           |
| JSON stdout  | exact result object                 |
| Exit codes   | common                              |

Example:

```json
{"id":31,"cmd":"trigger-flash","workspace":4,"surface":1}
{"id":31,"ok":true,"data":{}}
```

### resize-surface

| Field  | Value            |
| ------ | ---------------- |
| name   | `resize-surface` |
| status | implemented      |
| since  | protocol 5       |

Resizes a surface to a cell grid. PTY surfaces resize both the PTY and VT terminal state. Browser surfaces update their cell grid and CDP device metrics. `cols` and `rows` are clamped to at least 1 by the surface runtime. The command result does not report whether the size changed.

Params:

| Name      | JSON type | Required/default | Constraints                       |
| --------- | --------- | ---------------- | --------------------------------- |
| `surface` | `Id`      | required         | Must identify a live surface      |
| `cols`    | `uint16`  | required         | Final value clamped to at least 1 |
| `rows`    | `uint16`  | required         | Final value clamped to at least 1 |

Result:

```text
object{}
```

Errors:

| Error                  | Condition                         |
| ---------------------- | --------------------------------- |
| `unknown surface <id>` | Surface id does not exist         |
| `bad request: ...`     | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                                  |
| ------------ | -------------------------------------- |
| Verb         | `resize-surface`                       |
| Flags        | `--surface <id> --cols <n> --rows <n>` |
| Plain stdout | no output                              |
| JSON stdout  | exact result object                    |
| Exit codes   | common                                 |

Example:

```json
{"id":21,"cmd":"resize-surface","surface":1,"cols":120,"rows":40}
{"id":21,"ok":true,"data":{}}
```

### focus-pane

| Field  | Value        |
| ------ | ------------ |
| name   | `focus-pane` |
| status | implemented  |
| since  | protocol 5   |

Makes `pane` the active pane of its screen and also activates the containing screen and workspace. This is an explicit focus-intent command.

Params:

| Name   | JSON type | Required/default | Constraints                           |
| ------ | --------- | ---------------- | ------------------------------------- |
| `pane` | `Id`      | required         | Must identify a pane in a screen tree |

Result:

```text
object{}
```

Errors:

| Error               | Condition                         |
| ------------------- | --------------------------------- |
| `unknown pane <id>` | Pane id is not in any screen tree |
| `bad request: ...`  | Missing `pane` or wrong JSON type |

CLI mapping:

| Item         | Value               |
| ------------ | ------------------- |
| Verb         | `focus-pane`        |
| Flags        | `--pane <id>`       |
| Plain stdout | no output           |
| JSON stdout  | exact result object |
| Exit codes   | common              |

Example:

```json
{"id":22,"cmd":"focus-pane","pane":2}
{"id":22,"ok":true,"data":{}}
```

### select-tab

| Field  | Value        |
| ------ | ------------ |
| name   | `select-tab` |
| status | implemented  |
| since  | protocol 5   |

Selects a tab within a pane by zero-based `index` or relative `delta`. If both `index` and `delta` are present, v5 uses `index` and ignores `delta`. If `pane` is absent, the active pane is used.

No-op event behavior is split by target resolution. If the target pane cannot be resolved, or if the resolved pane has no tabs, v5 returns success and emits no `tree-changed`. This includes an unknown supplied pane, no supplied pane with no active pane, and an empty pane. If the target pane resolves and has tabs, an out-of-range `index` or missing `index`/`delta` returns success and emits `tree-changed` even though the active tab does not change.

Params:

| Name    | JSON type | Required/default | Constraints                           |
| ------- | --------- | ---------------- | ------------------------------------- |
| `pane`  | `Id`      | default null     | Target pane or active pane            |
| `index` | `usize`   | default null     | Zero-based; ignored if out of range   |
| `delta` | `isize`   | default null     | Relative; wraps with Euclidean modulo |

Result:

```text
object{}
```

Errors:

| Error              | Condition       |
| ------------------ | --------------- |
| `bad request: ...` | Wrong JSON type |

CLI mapping:

| Item         | Value                                            |
| ------------ | ------------------------------------------------ |
| Verb         | `select-tab`                                     |
| Flags        | `[--pane <id>] (--index <n>                      | --delta <n>)` |
| Plain stdout | no output                                        |
| JSON stdout  | exact result object                              |
| Exit codes   | common; CLI rejects missing selector with exit 2 |

Example:

```json
{"id":23,"cmd":"select-tab","pane":2,"index":0}
{"id":23,"ok":true,"data":{}}
```

### select-screen

| Field  | Value           |
| ------ | --------------- |
| name   | `select-screen` |
| status | implemented     |
| since  | protocol 5      |

Selects a screen in the active workspace by zero-based `index` or relative `delta`. If both `index` and `delta` are present, v5 uses `index` and ignores `delta`.

No-op event behavior is split by target resolution. If there is no active workspace or the active workspace has no screens, v5 returns success and emits no `tree-changed`. If the active workspace resolves and has screens, an out-of-range `index` or missing `index`/`delta` returns success and emits `tree-changed` even though the active screen does not change.

Params:

| Name    | JSON type | Required/default | Constraints                           |
| ------- | --------- | ---------------- | ------------------------------------- |
| `index` | `usize`   | default null     | Zero-based; ignored if out of range   |
| `delta` | `isize`   | default null     | Relative; wraps with Euclidean modulo |

Result:

```text
object{}
```

Errors:

| Error              | Condition       |
| ------------------ | --------------- |
| `bad request: ...` | Wrong JSON type |

CLI mapping:

| Item         | Value                                            |
| ------------ | ------------------------------------------------ |
| Verb         | `select-screen`                                  |
| Flags        | `--index <n>                                     | --delta <n>` |
| Plain stdout | no output                                        |
| JSON stdout  | exact result object                              |
| Exit codes   | common; CLI rejects missing selector with exit 2 |

Example:

```json
{"id":24,"cmd":"select-screen","delta":1}
{"id":24,"ok":true,"data":{}}
```

### select-workspace

| Field  | Value              |
| ------ | ------------------ |
| name   | `select-workspace` |
| status | implemented        |
| since  | protocol 5         |

Selects a workspace by zero-based `index` or relative `delta`. If both `index` and `delta` are present, v5 uses `index` and ignores `delta`.

No-op event behavior is split by target resolution. If the session has no workspaces, v5 returns success and emits no `tree-changed`. If at least one workspace exists, an out-of-range `index` or missing `index`/`delta` returns success and emits `tree-changed` even though the active workspace does not change.

Params:

| Name    | JSON type | Required/default | Constraints                           |
| ------- | --------- | ---------------- | ------------------------------------- |
| `index` | `usize`   | default null     | Zero-based; ignored if out of range   |
| `delta` | `isize`   | default null     | Relative; wraps with Euclidean modulo |

Result:

```text
object{}
```

Errors:

| Error              | Condition       |
| ------------------ | --------------- |
| `bad request: ...` | Wrong JSON type |

CLI mapping:

| Item         | Value                                            |
| ------------ | ------------------------------------------------ |
| Verb         | `select-workspace`                               |
| Flags        | `--index <n>                                     | --delta <n>` |
| Plain stdout | no output                                        |
| JSON stdout  | exact result object                              |
| Exit codes   | common; CLI rejects missing selector with exit 2 |

Example:

```json
{"id":25,"cmd":"select-workspace","index":0}
{"id":25,"ok":true,"data":{}}
```

### move-tab

| Field  | Value       |
| ------ | ----------- |
| name   | `move-tab`  |
| status | implemented |
| since  | protocol 5  |

Moves an existing tab, identified by `surface`, into `pane` at zero-based `index`. Moving a tab to its current pane and current index is an `ok:true` no-op. This command is documented from the consumer-side landed contract; it is not present in this branch's `server.rs`, so out-of-range index behavior and event emission could not be verified here.

Params:

| Name      | JSON type | Required/default | Constraints                  |
| --------- | --------- | ---------------- | ---------------------------- |
| `surface` | `Id`      | required         | Surface tab to move          |
| `pane`    | `Id`      | required         | Destination pane             |
| `index`   | `usize`   | required         | Zero-based destination index |

Result:

```text
object{}
```

Errors:

| Error                   | Condition                                                                         |
| ----------------------- | --------------------------------------------------------------------------------- |
| `unknown surface <id>`  | Surface id does not exist                                                         |
| `unknown pane <id>`     | Destination pane does not exist                                                   |
| `bad request: ...`      | Missing fields or wrong JSON type                                                 |
| unverified error string | Non-same-position out-of-range index behavior could not be checked in this branch |

CLI mapping:

| Item         | Value                                    |
| ------------ | ---------------------------------------- |
| Verb         | `move-tab`                               |
| Flags        | `--surface <id> --pane <id> --index <n>` |
| Plain stdout | no output                                |
| JSON stdout  | exact result object                      |
| Exit codes   | common                                   |

Example:

```json
{"id":26,"cmd":"move-tab","surface":1,"pane":2,"index":0}
{"id":26,"ok":true,"data":{}}
```

### move-workspace

| Field  | Value            |
| ------ | ---------------- |
| name   | `move-workspace` |
| status | implemented      |
| since  | protocol 5       |

Moves an existing workspace to zero-based `index`. Moving a workspace to its current index is an `ok:true` no-op. This command is documented from the consumer-side landed contract; it is not present in this branch's `server.rs`, so out-of-range index behavior and event emission could not be verified here.

Params:

| Name        | JSON type | Required/default | Constraints                  |
| ----------- | --------- | ---------------- | ---------------------------- |
| `workspace` | `Id`      | required         | Workspace to move            |
| `index`     | `usize`   | required         | Zero-based destination index |

Result:

```text
object{}
```

Errors:

| Error                    | Condition                                                                         |
| ------------------------ | --------------------------------------------------------------------------------- |
| `unknown workspace <id>` | Workspace id does not exist                                                       |
| `bad request: ...`       | Missing fields or wrong JSON type                                                 |
| unverified error string  | Non-same-position out-of-range index behavior could not be checked in this branch |

CLI mapping:

| Item         | Value                          |
| ------------ | ------------------------------ |
| Verb         | `move-workspace`               |
| Flags        | `--workspace <id> --index <n>` |
| Plain stdout | no output                      |
| JSON stdout  | exact result object            |
| Exit codes   | common                         |

Example:

```json
{"id":27,"cmd":"move-workspace","workspace":4,"index":0}
{"id":27,"ok":true,"data":{}}
```

### scroll-surface

| Field  | Value            |
| ------ | ---------------- |
| name   | `scroll-surface` |
| status | implemented      |
| since  | protocol 5       |

Scrolls a PTY surface viewport by row delta. Negative values scroll up. Positive values scroll down. This changes the terminal viewport state used by `read-screen` and renderers.

Params:

| Name      | JSON type | Required/default | Constraints                      |
| --------- | --------- | ---------------- | -------------------------------- |
| `surface` | `Id`      | required         | Must identify a live PTY surface |
| `delta`   | `isize`   | required         | Negative up, positive down       |

Result:

```text
object{}
```

Errors:

| Error                                                     | Condition                         |
| --------------------------------------------------------- | --------------------------------- |
| `unknown surface <id>`                                    | Surface id does not exist         |
| `browser surface does not support PTY/VT socket commands` | Surface is a browser              |
| `bad request: ...`                                        | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                        |
| ------------ | ---------------------------- |
| Verb         | `scroll-surface`             |
| Flags        | `--surface <id> --delta <n>` |
| Plain stdout | no output                    |
| JSON stdout  | exact result object          |
| Exit codes   | common                       |

Example:

```json
{"id":26,"cmd":"scroll-surface","surface":1,"delta":-10}
{"id":26,"ok":true,"data":{}}
```

### subscribe

| Field  | Value       |
| ------ | ----------- |
| name   | `subscribe` |
| status | implemented |
| since  | protocol 5  |

Subscribes the connection to mux events. After this command, response lines and event lines may be interleaved on the same connection. `subscribe` does not send an initial tree snapshot; clients should call `list-workspaces` when they need state.

Params: none.

Result:

```text
object{}
```

Errors:

| Error                     | Condition                                    |
| ------------------------- | -------------------------------------------- |
| thread spawn error string | Server cannot create the event writer thread |
| `bad request: ...`        | Malformed request envelope                   |

CLI mapping:

| Item         | Value                                               |
| ------------ | --------------------------------------------------- |
| Verb         | `subscribe`                                         |
| Flags        | none in v5                                          |
| Plain stdout | JSON event object per line                          |
| JSON stdout  | JSON event object per line                          |
| Exit codes   | common; runs until connection closes or interrupted |

Example:

```json
{"id":27,"cmd":"subscribe"}
{"id":27,"ok":true,"data":{}}
{"event":"tree-changed"}
```

### attach-surface

| Field  | Value            |
| ------ | ---------------- |
| name   | `attach-surface` |
| status | implemented      |
| since  | protocol 5       |

Attaches the connection to a PTY surface stream. In protocol v5, the server first sends a `vt-state` event for the current surface state, then sends live `output` events for subsequent PTY bytes, and finally sends `detached` when the stream ends. The command response is sent after the initial `vt-state` event in v5.

Protocol v6 changes the attach stream ordering to `vt-state -> (resized | output)* -> detached`. A v6 `resized` attach event carries a fresh replay and requires clients to discard the old mirror and replace it from that replay. Clients that support only protocol 5 or older must refuse protocol v6 attach streams rather than treating `resized` as a normal resize. The v6 field name `replay` could not be verified against this branch's code.

Params:

| Name      | JSON type | Required/default | Constraints                      |
| --------- | --------- | ---------------- | -------------------------------- |
| `surface` | `Id`      | required         | Must identify a live PTY surface |

Result:

```text
object{}
```

Errors:

| Error                                             | Condition                                     |
| ------------------------------------------------- | --------------------------------------------- |
| `unknown surface <id>`                            | Surface id does not exist                     |
| `browser panes are not supported over attach yet` | Surface is a browser                          |
| terminal error string                             | VT replay generation fails                    |
| thread spawn error string                         | Server cannot create the attach writer thread |
| `bad request: ...`                                | Missing `surface` or wrong JSON type          |

CLI mapping:

| Item         | Value                                                            |
| ------------ | ---------------------------------------------------------------- |
| Verb         | `attach-surface`                                                 |
| Flags        | `--surface <id>`                                                 |
| Plain stdout | JSON event object per line                                       |
| JSON stdout  | JSON event object per line                                       |
| Exit codes   | common; runs until `detached`, connection closes, or interrupted |

Example:

```json
{"id":28,"cmd":"attach-surface","surface":1}
{"event":"vt-state","surface":1,"cols":80,"rows":24,"data":"G1s/bA=="}
{"id":28,"ok":true,"data":{}}
```

### get-resolved-config

| Field  | Value                 |
| ------ | --------------------- |
| name   | `get-resolved-config` |
| status | implemented           |
| since  | protocol 6            |

Returns the server process's resolved presentation chrome (the keys relevant to a thin-client `Overlay`: theme, tabs, sidebar, keys) so a `mtyx attach --apply-local-config` client can layer its local overlay on top of the _server's_ config rather than replacing it with the laptop's own (issue #40). Browser and scrollbar are server-side truth and are intentionally not part of the payload. The shape matches what `mux-tui`'s `Config::resolved_chrome_value()` emits and `Config::from_server_chrome()` consumes, so the client round-trips it back through the same `apply_*` resolution helpers `load()` uses.

The server publishes this from its own `config::load()` at startup; if it has not registered any chrome (for example a `mux-core`-only host with no TUI), it returns an empty object `{}` and the client falls back to its local config.

Params: none.

Result:

```text
object{
  theme: object{
    selection_background: ColorHex|uint16,
    selection_foreground: ColorHex|uint16|null,
    sidebar_rail: ColorHex|uint16,
    sidebar_active_bg: ColorHex|uint16,
    tab_rail: ColorHex|uint16,
    tab_bg: ColorHex|uint16,
    tab_active_bg: ColorHex|uint16|null,
    border_active: ColorHex|uint16,
    border_inactive: ColorHex|uint16
  },
  tabs: object{min_width:uint16,solid_background:boolean,show_titles:boolean,agents:array<string>},
  sidebar: object{width:uint16,max_width:uint16},
  keys: object<string,string|boolean|array<string>>
}
```

Errors:

| Error              | Condition       |
| ------------------ | --------------- |
| `bad request: ...` | Wrong JSON type |

CLI mapping:

| Item         | Value                                                                                                                            |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| Verb         | `get-resolved-config` (also consumed internally by `mtyx attach --print-resolved-config` and `mtyx attach --apply-local-config`) |
| Flags        | n/a                                                                                                                              |
| Plain stdout | n/a                                                                                                                              |
| JSON stdout  | n/a                                                                                                                              |
| Exit codes   | n/a                                                                                                                              |

Example:

```json
{"id":32,"cmd":"get-resolved-config"}
{"id":32,"ok":true,"data":{"theme":{"sidebar_rail":"#778899",...},"tabs":{...},"sidebar":{...},"keys":{...}}}
```

## Local Verb Groups (no wire protocol)

These verb groups run entirely in the `mtyx` client binary; they do not
send JSON commands over the control socket and have no entry in the
protocol command set. They are documented here so the surface has one
home for every `mtyx <verb> ...` invocation.

### plugin

| Field  | Value                                             |
| ------ | ------------------------------------------------- |
| name   | `plugin` (verb group: `mtyx plugin <subcommand>`) |
| status | implemented (manifest + registry only)            |
| since  | local client surface, issue #42 scoped first PR   |

Manages `mtyx-plugin.toml` manifests and a small JSON registry under the
mtyx data directory (`$XDG_DATA_HOME/mattyx`, or `~/.local/share/mattyx`
by default). Layout:

```text
<base>/plugins.json          registry: {"plugins":[{name,enabled,entry,verbs}, ...]}
<base>/plugins/<name>/         one directory per installed plugin
                mtyx-plugin.toml   manifest copied verbatim at install time
```

Manifest (`mtyx-plugin.toml`):

| Field            | Type             | Required | Notes                                                                                                             |
| ---------------- | ---------------- | -------- | ----------------------------------------------------------------------------------------------------------------- |
| `[plugin] name`  | string           | yes      | non-empty, single path component (used as the on-disk dir name)                                                   |
| `[plugin] entry` | string           | yes      | non-empty; path to the plugin entry artefact, stored verbatim, not resolved or validated as executable in this PR |
| `[plugin] verbs` | array of strings | yes      | non-empty, each entry non-empty; the verbs this plugin claims, stored verbatim, not proxied in this PR            |

CLI mappings:

| Subcommand                       | Required args     | Optional flags | Plain stdout                                     | JSON stdout                                                       |
| -------------------------------- | ----------------- | -------------- | ------------------------------------------------ | ----------------------------------------------------------------- |
| `plugin list`                    | none              | `--json`       | one line per plugin `<name> <enabled             | disabled> <entry> <verb,verb>`; `no plugins installed` when empty | the registry object `{"plugins":[...]}` |
| `plugin install <manifest-path>` | `<manifest-path>` | none           | `installed plugin <name> from <path>`            | n/a (exit code only)                                              |
| `plugin uninstall <name>`        | `<name>`          | none           | `uninstalled plugin <name>`                      | n/a (exit code only)                                              |
| `plugin enable <name>`           | `<name>`          | none           | `plugin <name> enabled` (or `already enabled`)   | n/a (exit code only)                                              |
| `plugin disable <name>`          | `<name>`          | none           | `plugin <name> disabled` (or `already disabled`) | n/a (exit code only)                                              |

Errors:

| Error                                       | Condition                                                        |
| ------------------------------------------- | ---------------------------------------------------------------- |
| `malformed mtyx-plugin.toml: ...`           | manifest is not valid TOML or a required field is missing/empty  |
| `a plugin named "..." is already installed` | `install` against a name already in the registry (AC5 collision) |
| `no plugin named "..." is installed`        | `uninstall`/`enable`/`disable` against an unknown name           |
| `plugin name "..." must not be a path`      | `install` where `name` contains a separator or is `.`/`..`       |

NOT IMPLEMENTED (deferred to a follow-up PR): plugin _execution_
(proxying `mtyx <plugin-name> <verb>` to a running plugin process,
WASM/WASI sandboxing, the permission model) is out of scope for this
PR. These verbs only manage manifest state and must not be read as
implying that any plugin code runs.

### agents

| Field     | Value                                             |
| --------- | ------------------------------------------------- |
| name      | `agents` (verb group: `mtyx agents <subcommand>`) |
| status    | implemented                                       |
| transport | local filesystem only; no control-socket request  |

The registry contains `claude`, `antigravity`, `codex`, `aider`, `pi`, and
`grok`. Hook install paths are resolved from the current project for local
installs and from the user's home directory for `--global` installs.

CLI mappings:

| Subcommand                      | Required args    | Optional flags            | Plain stdout                                                                                                 |
| ------------------------------- | ---------------- | ------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `agents list`                   | none             | `--global`                | `agent<TAB>status<TAB>version<TAB>last-installed<TAB>install-path`, followed by one row per registered agent |
| `agents install --all`          | `--all`          | `--uninstall`, `--global` | one per-agent result; processing continues after failures                                                    |
| `agents install --only <agent>` | `--only <agent>` | `--uninstall`, `--global` | one result for the selected agent                                                                            |

A listed agent is `installed` when its registered install path exists. The
version is the managed hook version and `last-installed` is the install path's
modification time in Unix epoch seconds. Missing values are printed as `-`.
`--uninstall` is passed to each selected installer. If an installer returns a
non-zero code, its failure is printed and the remaining selected installers
still run; the umbrella command then returns exit code `1`. Invalid command
shapes and unknown agent names return exit code `2`.

## Proposed Commands

### wait-for

| Field  | Value               |
| ------ | ------------------- |
| name   | `wait-for`          |
| status | proposed            |
| since  | proposed protocol 6 |

Blocks until a regular expression matches the current plain-text screen for a PTY surface. The server polls the same text source as `read-screen` and returns as soon as a match is found or the timeout expires. This is the primary automation synchronization primitive.

Params:

| Name         | JSON type | Required/default | Constraints                        |
| ------------ | --------- | ---------------- | ---------------------------------- |
| `surface`    | `IdRef`   | required         | PTY surface                        |
| `pattern`    | `string`  | required         | Rust regex syntax                  |
| `timeout_ms` | `uint64`  | required         | `0` means a single immediate check |

Result:

```text
object{matched:true,text:string,elapsed_ms:uint64}
```

Errors:

| Error                                                     | Condition                         |
| --------------------------------------------------------- | --------------------------------- |
| `unknown surface <id>`                                    | Surface id does not exist         |
| `browser surface does not support PTY/VT socket commands` | Surface is a browser              |
| `bad regex: <message>`                                    | Pattern cannot compile            |
| `timeout waiting for pattern`                             | Timeout expires before match      |
| `bad request: ...`                                        | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                                               |
| ------------ | --------------------------------------------------- |
| Verb         | `wait-for`                                          |
| Flags        | `--surface <id> --pattern <regex> --timeout-ms <n>` |
| Plain stdout | no output on success                                |
| JSON stdout  | exact result object                                 |
| Exit codes   | common; timeout is exit code 1                      |

Example:

```json
{"id":101,"cmd":"wait-for","surface":1,"pattern":"ready> $","timeout_ms":5000}
{"id":101,"ok":true,"data":{"matched":true,"text":"ready> ","elapsed_ms":143}}
```

### run

| Field  | Value               |
| ------ | ------------------- |
| name   | `run`               |
| status | proposed            |
| since  | proposed protocol 6 |

Spawns a command in a new PTY tab and returns the new surface id. `argv` executes directly without a shell. `command` executes through the session shell as `shell -lc <command>`. Exactly one of `argv` or `command` is required. By default the tab is created in the active pane. With `pane`, it is created in that pane. With `new_workspace:true`, a new workspace is created instead.

Params:

| Name            | JSON type       | Required/default             | Constraints                                                      |
| --------------- | --------------- | ---------------------------- | ---------------------------------------------------------------- |
| `argv`          | `array<string>` | required if `command` absent | Non-empty; direct exec                                           |
| `command`       | `string`        | required if `argv` absent    | Executed via shell `-lc`                                         |
| `cwd`           | `string`        | default null                 | Working directory                                                |
| `pane`          | `IdRef`         | default null                 | Mutually exclusive with `new_workspace:true`                     |
| `new_workspace` | `boolean`       | default false                | Create isolated workspace                                        |
| `name`          | `string`        | default null                 | Sets surface name; also workspace name when `new_workspace:true` |
| `cols`          | `uint16`        | default null                 | Used only with `rows`                                            |
| `rows`          | `uint16`        | default null                 | Used only with `cols`                                            |

Result:

```text
object{surface:Id,pane:Id,screen:Id,workspace:Id}
```

Errors:

| Error                                     | Condition                         |
| ----------------------------------------- | --------------------------------- |
| `argv or command is required`             | Neither is supplied               |
| `argv and command are mutually exclusive` | Both are supplied                 |
| `unknown pane <id>`                       | Supplied pane does not exist      |
| spawn or PTY error string                 | PTY creation or child spawn fails |
| `bad request: ...`                        | Wrong JSON type                   |

CLI mapping:

| Item         | Value                              |
| ------------ | ---------------------------------- |
| Verb         | `run`                              |
| Flags        | `[--pane <id>                      | --new-workspace] [--cwd <path>] [--name <name>] -- <argv...>`or`--command <cmd>` |
| Plain stdout | new surface id followed by newline |
| JSON stdout  | exact result object                |
| Exit codes   | common                             |

Example:

```json
{"id":102,"cmd":"run","argv":["python3","-m","http.server"],"cwd":"/tmp","name":"server"}
{"id":102,"ok":true,"data":{"surface":31,"pane":2,"screen":3,"workspace":4}}
```

### send-key

| Field  | Value               |
| ------ | ------------------- |
| name   | `send-key`          |
| status | proposed            |
| since  | proposed protocol 6 |

Sends named key chords to a surface without requiring callers to hand-encode escape sequences. PTY surfaces use the same Ghostty key encoder as the TUI, synced to the surface terminal modes. Browser surfaces translate supported keys to CDP keyboard input when the browser runtime is local.

Params:

| Name      | JSON type       | Required/default | Constraints              |
| --------- | --------------- | ---------------- | ------------------------ |
| `surface` | `IdRef`         | required         | Target surface           |
| `keys`    | `array<string>` | required         | Non-empty key chord list |

Key chord syntax is lower-case tokens joined with `+`. Supported names are `enter`, `tab`, `backtab`, `escape`, `backspace`, `delete`, `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `f1` through `f24`, printable single characters, `ctrl+<key>`, `alt+<key>`, and `shift+<key>` where the encoder supports it.

Result:

```text
object{}
```

Errors:

| Error                                | Condition                         |
| ------------------------------------ | --------------------------------- |
| `unknown surface <id>`               | Surface id does not exist         |
| `unknown key <key>`                  | Key token is not supported        |
| `surface does not support key input` | Surface kind cannot accept keys   |
| IO or CDP error string               | Input write fails                 |
| `bad request: ...`                   | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                     |
| ------------ | ------------------------- |
| Verb         | `send-key`                |
| Flags        | `--surface <id> <key>...` |
| Plain stdout | no output                 |
| JSON stdout  | exact result object       |
| Exit codes   | common                    |

Example:

```json
{"id":103,"cmd":"send-key","surface":1,"keys":["ctrl+c","enter"]}
{"id":103,"ok":true,"data":{}}
```

### copy

| Field  | Value               |
| ------ | ------------------- |
| name   | `copy`              |
| status | proposed            |
| since  | proposed protocol 6 |

Extracts text from a surface. `screen` returns the current plain-text viewport. `selection` returns the current mux-owned selection. `scrollback` returns available scrollback followed by the current viewport.

Params:

| Name      | JSON type | Required/default | Constraints                                  |
| --------- | --------- | ---------------- | -------------------------------------------- |
| `surface` | `IdRef`   | required         | PTY surface                                  |
| `mode`    | `string`  | required         | `"screen"`, `"selection"`, or `"scrollback"` |

Result:

```text
object{text:string,mode:"screen"|"selection"|"scrollback"}
```

Errors:

| Error                                                     | Condition                                              |
| --------------------------------------------------------- | ------------------------------------------------------ |
| `unknown surface <id>`                                    | Surface id does not exist                              |
| `browser surface does not support PTY/VT socket commands` | Surface is a browser                                   |
| `bad mode <mode>`                                         | Mode is not allowed                                    |
| `no selection`                                            | Mode is `selection` and no selection exists            |
| `scrollback unavailable`                                  | Mode is `scrollback` and the terminal cannot export it |
| `bad request: ...`                                        | Missing fields or wrong JSON type                      |

CLI mapping:

| Item         | Value                         |
| ------------ | ----------------------------- |
| Verb         | `copy`                        |
| Flags        | `--surface <id> --mode screen | selection | scrollback` |
| Plain stdout | extracted text exactly        |
| JSON stdout  | exact result object           |
| Exit codes   | common                        |

Example:

```json
{"id":104,"cmd":"copy","surface":1,"mode":"screen"}
{"id":104,"ok":true,"data":{"text":"ready> ","mode":"screen"}}
```

### ids

| Field  | Value               |
| ------ | ------------------- |
| name   | `ids`               |
| status | proposed            |
| since  | proposed protocol 6 |

Returns the session id mapping and establishes the short-id scheme. Every workspace, screen, pane, and surface has a numeric id and a stable short id for the lifetime of the session. Short ids are content-independent, collision-checked per session, and usable anywhere an `IdRef` is accepted.

Params:

| Name   | JSON type | Required/default | Constraints                                                          |
| ------ | --------- | ---------------- | -------------------------------------------------------------------- |
| `kind` | `string`  | default null     | Optional filter: `"workspace"`, `"screen"`, `"pane"`, or `"surface"` |

Short id format:

```text
[a-z0-9]{6}
```

Generation rule: the server derives a candidate from a per-session random seed plus numeric id, encodes it base36, and checks for collisions across all live ids. On collision, it rehashes with an incrementing salt. Short ids never depend on names, titles, command text, cwd, or layout position. A short id is not reused while the session is alive.

Resolution rule: numeric JSON ids resolve first. String ids matching `[0-9]+` are rejected as ambiguous. String ids matching the short-id format resolve by exact short id. Unknown or ambiguous strings error.

Result:

```text
object{ids:array<object{kind:"workspace"|"screen"|"pane"|"surface",id:Id,short_id:string}>}
```

Errors:

| Error              | Condition                  |
| ------------------ | -------------------------- |
| `bad kind <kind>`  | Filter kind is not allowed |
| `bad request: ...` | Wrong JSON type            |

CLI mapping:

| Item         | Value                                     |
| ------------ | ----------------------------------------- |
| Verb         | `ids`                                     |
| Flags        | `[--kind workspace                        | screen | pane | surface]` |
| Plain stdout | one line per id: `<kind> <id> <short_id>` |
| JSON stdout  | exact result object                       |
| Exit codes   | common                                    |

Example:

```json
{"id":105,"cmd":"ids","kind":"surface"}
{"id":105,"ok":true,"data":{"ids":[{"kind":"surface","id":1,"short_id":"a8f3k2"}]}}
```

### notify

| Field  | Value               |
| ------ | ------------------- |
| name   | `notify`            |
| status | proposed            |
| since  | proposed protocol 6 |

Posts a notification into the mux notification area. This is a telemetry command and must not change app focus or pane selection.

Params:

| Name      | JSON type | Required/default | Constraints                         |
| --------- | --------- | ---------------- | ----------------------------------- |
| `title`   | `string`  | required         | Non-empty                           |
| `body`    | `string`  | required         | May be empty                        |
| `level`   | `string`  | default `"info"` | `"info"`, `"warning"`, or `"error"` |
| `surface` | `IdRef`   | default null     | Optional originating surface        |

Result:

```text
object{notification:Id}
```

Errors:

| Error                  | Condition                          |
| ---------------------- | ---------------------------------- |
| `title is required`    | Title is empty                     |
| `bad level <level>`    | Level is not allowed               |
| `unknown surface <id>` | Optional surface id does not exist |
| `bad request: ...`     | Missing fields or wrong JSON type  |

CLI mapping:

| Item         | Value                                        |
| ------------ | -------------------------------------------- |
| Verb         | `notify`                                     |
| Flags        | `--title <title> --body <body> [--level info | warning | error] [--surface <id>]` |
| Plain stdout | notification id followed by newline          |
| JSON stdout  | exact result object                          |
| Exit codes   | common                                       |

Example:

```json
{"id":106,"cmd":"notify","title":"Build failed","body":"api tests failed","level":"error","surface":1}
{"id":106,"ok":true,"data":{"notification":44}}
```

### list-agents

| Field  | Value         |
| ------ | ------------- |
| name   | `list-agents` |
| status | implemented   |
| since  | protocol 6    |

Returns known agent status records. Records may come from detection, explicit reports, or hooks — all three are implemented in this fork: `source: "detected"` comes from watching a surface's own pty output for an OSC 9 / OSC 777 / kitty desktop notification (see `mux-core/src/notify.rs`), which sets state `blocked`. Detected reports have the lowest authority: a `socket` or `hook` report always wins over a `detected` one, regardless of order (see `report-agent`'s authority rules below). Explicit hook-authority reports override detection for the same surface until another explicit report changes the state or the surface closes.

Params:

| Name      | JSON type | Required/default | Constraints             |
| --------- | --------- | ---------------- | ----------------------- |
| `surface` | `IdRef`   | default null     | Optional surface filter |
| `state`   | `string`  | default null     | Optional state filter   |

Result:

```text
object{
  agents: array<object{
    surface: Id,
    state: "working"|"blocked"|"idle"|"done"|"unknown",
    source: "detected"|"socket"|"hook",
    session: string|null,
    agent: string|null,
    message: string|null,
    updated_at_ms: uint64
  }>
}
```

`agent` is the pane's reported agent name (issue #75's `--agent`; preserved across later reports that omit it) and `message` is the latest report's free-text context (absent = cleared). Every pane in the `list-workspaces` per-tab JSON also carries `agent_status` (always a string; `"unknown"` for bare panes), `agent_message`, and `agent_updated_at_ms` alongside the older nullable `agent_state`/`agent_session`.

Errors:

| Error                  | Condition                          |
| ---------------------- | ---------------------------------- |
| `unknown surface <id>` | Optional surface id does not exist |
| `bad state <state>`    | State filter is not allowed        |
| `bad request: ...`     | Wrong JSON type                    |

CLI mapping:

| Item         | Value                                                          |
| ------------ | -------------------------------------------------------------- |
| Verb         | `list-agents`                                                  |
| Flags        | `[--surface <id>] [--state working                             | blocked | idle | done | unknown]` |
| Plain stdout | one line per agent: `<surface> <state> <source> <session-or-> <agent-or--> <message-or->` |
| JSON stdout  | exact result object                                            |
| Exit codes   | common                                                         |

Example:

```json
{"id":107,"cmd":"list-agents","state":"blocked"}
{"id":107,"ok":true,"data":{"agents":[{"surface":1,"state":"blocked","source":"hook","session":"abc","agent":"worker-1","message":"needs input","updated_at_ms":1710000000000}]}}
```

### report-agent

| Field  | Value          |
| ------ | -------------- |
| name   | `report-agent` |
| status | implemented    |
| since  | protocol 6     |

Reports agent state for a surface. This is a telemetry command and must not change focus. Reports with `source:"hook"` have hook authority and override detector-derived state. Reports with `source:"socket"` override detector-derived state but are lower priority than a newer hook report (concretely: a `socket` report is rejected outright while the current report's source is `hook`, regardless of timing).

Params:

| Name      | JSON type | Required/default | Constraints                                                  |
| --------- | --------- | ---------------- | ------------------------------------------------------------ |
| `surface` | `IdRef`   | required         | Surface associated with the agent                            |
| `state`   | `string`  | required         | `"working"`, `"blocked"`, `"idle"`, `"done"`, or `"unknown"` |
| `source`  | `string`  | default `"socket"` | `"detected"`, `"socket"`, or `"hook"`                       |
| `session` | `string`  | default null     | Optional upstream agent session id                           |
| `agent`   | `string`  | default null     | Optional agent name (issue #75); preserved by later reports that omit it, used for the name-addressed verbs |
| `message` | `string`  | default null     | Optional free-text context (issue #75); the latest applied report wins |

Result:

```text
object{surface:Id,state:string,source:string,session:string|null,agent:string|null,message:string|null,updated_at_ms:uint64}
```

Errors:

| Error                  | Condition                         |
| ---------------------- | --------------------------------- |
| `unknown surface <id>` | Surface id does not exist         |
| `bad state <state>`    | State is not allowed              |
| `bad source <source>`  | Source is not allowed             |
| `bad request: ...`     | Missing fields or wrong JSON type |

CLI mapping:

| Item         | Value                           |
| ------------ | ------------------------------- |
| Verb         | `report-agent`                  |
| Flags        | `[--surface <id>] --state working | blocked | idle | done | unknown [--source detected | socket | hook] [--agent-session <id>] [--agent <name>] [--message <text>]` |
| Plain stdout | no output                       |
| JSON stdout  | exact result object             |
| Exit codes   | common                          |

Example:

```json
{"id":108,"cmd":"report-agent","surface":1,"state":"working","source":"socket","session":"abc"}
{"id":108,"ok":true,"data":{"surface":1,"state":"working","source":"socket","session":"abc","agent":null,"message":null,"updated_at_ms":1710000000000}}
```

Issue #75 defaults: on the CLI, `--surface` falls back to `$MTYX_MUX_SURFACE` (set for every pane child, so an agent can self-report from inside its pane without knowing its id) and `--source` defaults to `socket`, keeping hook reports the authority — an in-pane socket self-report is rejected (state unchanged, exit 0) while a hook report is in effect. A report omitting `--agent` keeps the pane's established name; omitting `--message` clears the message.

### agent-read

| Field  | Value         |
| ------ | ------------- |
| name   | `agent-read`  |
| status | implemented   |
| since  | protocol 6 (issue #75; additive — old servers reject the unknown cmd) |

Reads a pane's screen text, addressing the pane by its reported agent name (`report-agent --agent`) or by numeric surface id. Duplicate names are an error; use surface ids to disambiguate.

Params:

| Name     | JSON type | Required/default   | Constraints                                       |
| -------- | --------- | ------------------ | ------------------------------------------------- |
| `target` | `string`  | required           | Agent name or numeric surface id                  |
| `source` | `string`  | default `"visible"` | `"visible"`, `"recent"`, or `"recent-unwrapped"` |
| `lines`  | `usize`   | default 40         | Tail the last N lines (trailing blank rows dropped first) |

`source` mirrors Herdr's visibility ladder: `visible` reads the viewport rows only (what is on screen now, honouring a scrolled-up viewport); `recent` reads a bottom-anchored window that includes SCROLLBACK (the last `max(lines, viewport)` rows of the screen); `recent-unwrapped` is the recent window with soft line-wraps re-joined. All sources tail the result to `lines`.

Result:

```text
object{surface:Id,text:string}
```

Errors: `unknown agent <target>`, `ambiguous agent name <n> (surfaces a, b)`, `bad source <s>`, `bad request: ...`.

CLI mapping:

| Item         | Value                                                                                       |
| ------------ | ------------------------------------------------------------------------------------------- |
| Verb         | `agent-read`                                                                                |
| Flags        | `--target <name-or-id> [--source visible | recent | recent-unwrapped] [--lines <n>]`        |
| Plain stdout | the text itself                                                                             |
| JSON stdout  | exact result object                                                                         |
| Exit codes   | common (bad `--source` values exit 2; target resolution errors exit 1)                      |

Example:

```json
{"id":110,"cmd":"agent-read","target":"worker-1","lines":5}
{"id":110,"ok":true,"data":{"surface":4,"text":"$ "}}
```

### agent-send

| Field  | Value         |
| ------ | ------------- |
| name   | `agent-send`  |
| status | implemented   |
| since  | protocol 6 (issue #75; additive) |

Types literal text into a pane addressed by agent name or surface id, WITHOUT a trailing CR — the caller submits separately (e.g. `mtyx send --surface <id> --text "" --send-cr 1`). Same shell-aware sanitisation as `send` (`shell`, default `raw`).

Params:

| Name     | JSON type | Required/default  | Constraints                        |
| -------- | --------- | ----------------- | ---------------------------------- |
| `target` | `string`  | required          | Agent name or numeric surface id   |
| `text`   | `string`  | required          | Text to type (no Enter appended)   |
| `shell`  | `string`  | default `"raw"`   | `auto`, `fish`, `bash`, `zsh`, `sh`, `nu`, `raw` |

Result:

```text
object{surface:Id}
```

Errors: target errors as for `agent-read`; `bad shell <s>`.

CLI mapping:

| Item         | Value                                                                                       |
| ------------ | ------------------------------------------------------------------------------------------- |
| Verb         | `agent-send`                                                                                |
| Flags        | `--target <name-or-id> --text <text> [--shell auto | fish | bash | zsh | sh | nu | raw]`    |
| Plain stdout | no output                                                                                   |
| JSON stdout  | exact result object                                                                         |
| Exit codes   | common                                                                                      |

### wait-agent-status

| Field  | Value                 |
| ------ | --------------------- |
| name   | `wait-agent-status`   |
| status | implemented           |
| since  | protocol 6 (issue #75; additive) |

Blocks until the target agent's reported state reaches `state`, then returns the matched report plus the pane's current text (the read payload). Herdr semantics: if the state already matches, return immediately. `timeout_ms: 0` is a single immediate check. Timeout expiry is the error `timeout waiting for agent status <state>` (CLI exit 1). If the target surface exits while waiting, the wait fails immediately with `surface <id> exited while waiting for agent status <state>` instead of parking until the deadline. Timeouts are capped at 600 000 ms server-side so a leaked waiter thread can't park on its connection forever. Blocking holds only this command's own connection thread; the event channel is unbounded, so reporters never block.

Params:

| Name         | JSON type | Required/default | Constraints                                               |
| ------------ | --------- | ---------------- | --------------------------------------------------------- |
| `target`     | `string`  | required         | Agent name or numeric surface id                          |
| `state`      | `string`  | required         | `"idle"`, `"working"`, `"blocked"`, `"done"`, `"unknown"` |
| `timeout_ms` | `uint64`  | required         | `0` = single immediate check; max 600 000                 |

Result:

```text
object{matched:true,surface:Id,state:string,agent:string|null,message:string|null,updated_at_ms:uint64,elapsed_ms:uint64,text:string}
```

Errors: `bad state <s>`, `timeout <n>ms exceeds the 600000ms cap`, target errors as for `agent-read`, `surface <id> exited while waiting for agent status <state>`, `timeout waiting for agent status <state>`.

CLI mapping:

| Item         | Value                                                                                       |
| ------------ | ------------------------------------------------------------------------------------------- |
| Verb         | `wait-agent-status`                                                                         |
| Flags        | `--target <name-or-id> --status idle | working | blocked | done | unknown --timeout <ms>`   |
| Plain stdout | the read payload (text)                                                                     |
| JSON stdout  | exact result object                                                                         |
| Exit codes   | common; the client extends its socket read timeout to `--timeout + 5 s` so long waits aren't killed by the default 10 s transport timeout |

Example:

```json
{"id":109,"cmd":"wait-agent-status","target":"worker-1","state":"idle","timeout_ms":300000}
{"id":109,"ok":true,"data":{"matched":true,"surface":4,"state":"idle","agent":"worker-1","message":"done for now","updated_at_ms":1710000005000,"elapsed_ms":412,"text":"$ "}}
```

### wait-ready

| Field  | Value            |
| ------ | ---------------- |
| name   | `wait-ready`     |
| status | implemented      |
| since  | protocol 7       |

Issue #85: block until a surface is *ready* — its visible screen shows a recognised prompt (a shell prompt or an identified agent) **and** its PTY has a running process-tree child — or `timeout_ms` elapses. Read-only observation: it never writes to the pane or mutates mux state. This is the post-spawn health check for headless orchestrators, which otherwise cannot distinguish a completed launch from a silent partial one (`send` succeeds once bytes reach the PTY buffer).

A timeout is **not** an error: the reply is `ok:true` with `ready:false`, so the caller reads the structured result. The `mtyx` CLI maps `ready:false` to exit 1 while still printing the JSON payload.

Params:

| Name         | JSON type | Required/default          | Constraints                                  |
| ------------ | --------- | ------------------------- | -------------------------------------------- |
| `surface`    | `Id`      | required                  | PTY surface to observe (browser surfaces error) |
| `timeout_ms` | `u64`     | default 5000              | milliseconds; `0` is a single immediate check; capped at 600000 |

Result:

```text
object{ready:bool,surface:Id,prompt_seen:bool,child:object{pid:u32,comm:string}|null,elapsed_ms:u64}
```

Example (ready):

```json
{"id":110,"cmd":"wait-ready","surface":4,"timeout_ms":5000}
{"id":110,"ok":true,"data":{"ready":true,"surface":4,"prompt_seen":true,"child":{"pid":48213,"comm":"fish"},"elapsed_ms":128}}
```

Example (timeout):

```json
{"id":111,"cmd":"wait-ready","surface":4,"timeout_ms":300}
{"id":111,"ok":true,"data":{"ready":false,"surface":4,"prompt_seen":false,"child":null,"elapsed_ms":301}}
```

### pane-worktree-create

| Field  | Value                    |
| ------ | ------------------------ |
| name   | `pane-worktree-create`   |
| status | implemented              |
| since  | protocol 6               |

Creates a git worktree for `branch` rooted at the pane's repository, records it on the pane, and `cd`s the pane's active tab into it (issue #77 AC1). The worktree path pattern is `<repo>/../<repo>.<branch>/` by default; a `[[worktree_pattern]]` entry in `mux.toml`/`mux.json` overrides it (the issue text names this config block `mtyx.toml`; mtyx reads `mux.toml`/`mux.json` via `MTYX_MUX_CONFIG` or `~/.config/mattyx/`). `/` in a branch name maps to `-` in the directory component. On any failure the error propagates (`ok:false`, CLI exit 1) and the pane is untouched — cwd unchanged, no record kept (AC7).

The record is session-scoped: a daemon restart loses the registry (the on-disk worktrees remain and stay visible to `git worktree list`). `pane worktree remove` after a restart reports "no worktree for branch …".

Params:

| Name     | JSON type | Required/default | Constraints                          |
| -------- | --------- | ---------------- | ------------------------------------ |
| `pane`   | `Id`      | required         | Pane whose active tab resolves the repo |
| `branch` | `string`  | required         | Branch for `git worktree add -b`     |
| `label`  | `string`  | default null     | Display badge (sidebar / list)       |

Result:

```text
object{pane:Id,branch:string,path:string}
```

Errors:

| Error                                        | Condition                                    |
| -------------------------------------------- | -------------------------------------------- |
| `unknown pane <id>`                          | Pane id does not exist                       |
| `pane <id> has no active tab`                | Pane has no surfaces                         |
| `pane <id>'s active tab is not a pty surface`| Active tab is a browser surface              |
| `"<cwd>" is not inside a git repository`     | The pane's working directory is outside git  |
| `pane <id> has no working directory yet`     | No OSC 7 report and no spawn cwd             |
| git error string                             | `git worktree add` failed (bad branch name, dirty tree, existing path, branch already checked out) |
| `bad request: ...`                           | Missing fields or wrong JSON type            |

CLI mapping:

| Item         | Value                                                     |
| ------------ | --------------------------------------------------------- |
| Verb         | `pane-worktree-create` (alias: `pane worktree create`)    |
| Flags        | `--pane <id> --branch <name> [--label <text>]`            |
| Plain stdout | the worktree path followed by newline                     |
| JSON stdout  | exact result object                                       |
| Exit codes   | common                                                    |

Example:

```json
{"id":201,"cmd":"pane-worktree-create","pane":3,"branch":"feat-auth","label":"auth"}
{"id":201,"ok":true,"data":{"pane":3,"branch":"feat-auth","path":"/src/proj.feat-auth"}}
```

### pane-worktree-list

| Field  | Value                  |
| ------ | ---------------------- |
| name   | `pane-worktree-list`   |
| status | implemented            |
| since  | protocol 6             |

Returns every worktree attached to the pane over its lifetime, in creation order (issue #77 AC2 — a pane accumulates records as it moves between branches; removal drops a record but later ones stay).

Params:

| Name   | JSON type | Required/default | Constraints            |
| ------ | --------- | ---------------- | ---------------------- |
| `pane` | `Id`      | required         | Unknown ids are an error |

Result:

```text
object{worktrees:array{branch:string,path:string,label:string|null,created_at_ms:uint64}}
```

Errors:

| Error                 | Condition                   |
| --------------------- | --------------------------- |
| `unknown pane <id>`   | Pane id does not exist      |
| `bad request: ...`    | Missing fields or wrong type |

CLI mapping:

| Item         | Value                                                   |
| ------------ | ------------------------------------------------------- |
| Verb         | `pane-worktree-list` (alias: `pane worktree list`)      |
| Flags        | `--pane <id>`                                           |
| Plain stdout | one line per worktree: `branch path label` (`-` when no label) |
| JSON stdout  | exact result object                                     |
| Exit codes   | common                                                  |

Example:

```json
{"id":202,"cmd":"pane-worktree-list","pane":3}
{"id":202,"ok":true,"data":{"worktrees":[{"branch":"feat-auth","path":"/src/proj.feat-auth","label":"auth","created_at_ms":1787010000000}]}}
```

### pane-worktree-remove

| Field  | Value                   |
| ------ | ----------------------- |
| name   | `pane-worktree-remove`  |
| status | implemented             |
| since  | protocol 6              |

Tears down one of a pane's worktrees: `git worktree remove` plus `git worktree prune`, then drops the record (issue #77 AC3). No force flag: a dirty worktree fails with git's own error, and the remove is REFUSED while the pane's working directory is inside the target worktree (cd elsewhere first). The main repository is resolved through the worktree's own `.git`/`commondir` pointers, so the pane may sit anywhere at remove time.

Params:

| Name     | JSON type | Required/default | Constraints                 |
| -------- | --------- | ---------------- | --------------------------- |
| `pane`   | `Id`      | required         | Pane owning the record      |
| `branch` | `string`  | required         | Which record to tear down   |

Result:

```text
object{}
```

Errors:

| Error                                          | Condition                                   |
| ---------------------------------------------- | ------------------------------------------- |
| `unknown pane <id>`                            | Pane id does not exist                      |
| `no worktree for branch "<b>" on pane <id>`    | No matching record (also after a restart)   |
| `pane <id> is inside <path>; cd elsewhere before removing it` | The pane's cwd is inside the target |
| git error string                               | `git worktree remove` failed (dirty worktree, missing admin data) |
| `bad request: ...`                             | Missing fields or wrong JSON type           |

CLI mapping:

| Item         | Value                                                    |
| ------------ | -------------------------------------------------------- |
| Verb         | `pane-worktree-remove` (alias: `pane worktree remove`)   |
| Flags        | `--pane <id> --branch <name>`                            |
| Plain stdout | none                                                      |
| JSON stdout  | exact result object                                       |
| Exit codes   | common                                                    |

Example:

```json
{"id":203,"cmd":"pane-worktree-remove","pane":3,"branch":"feat-auth"}
{"id":203,"ok":true,"data":{}}
```

The three-word alias (`mtyx pane worktree create|list|remove ...`) is rewritten to the flat verb at argv level in `main.rs`; the wire protocol only ever carries the flat kebab-case form (the issue documents the three-word spelling; every other mtyx verb is flat).

### new-remote-workspace

| Field  | Value                  |
| ------ | ---------------------- |
| name   | `new-remote-workspace` |
| status | implemented            |
| since  | protocol 6             |

Creates a workspace whose single tab is a remote shell reached through `cmuxd-remote` over SSH (see `docs/protocol.md`'s "Remote Workspaces" section and `mux-core/src/remote_pty.rs`) instead of a local pty. The caller is responsible for having already built/uploaded a `cmuxd-remote` binary for the target's OS/arch and passing its local path; the bundled `mtyx ssh <host>` CLI does this and is the intended entry point, not this command directly.

Params:

| Name                | JSON type | Required/default | Constraints                                                                                             |
| ------------------- | --------- | ---------------- | ------------------------------------------------------------------------------------------------------- |
| `host`              | `string`  | required         | SSH destination, passed to `ssh`/`scp` as-is (may be a full `user@host` or an entry in `~/.ssh/config`) |
| `slot`              | `string`  | required         | Groups sessions under one persistent remote daemon per host                                             |
| `session_id`        | `string`  | required         | Fresh id starts a new remote shell; an existing id (e.g. from a persisted snapshot) reattaches to it    |
| `local_binary_path` | `string`  | required         | Local filesystem path to a `cmuxd-remote` binary built for the remote's OS/arch                         |
| `name`              | `string?` | default null     | Workspace display name                                                                                  |
| `cols`              | `uint16?` | default null     | Initial surface width; `rows` must also be set to take effect                                           |
| `rows`              | `uint16?` | default null     | Initial surface height; `cols` must also be set to take effect                                          |

Result:

```text
object{surface:Id}
```

Errors:

| Error                | Condition                                                                                            |
| -------------------- | ---------------------------------------------------------------------------------------------------- |
| `bad request: ...`   | Missing fields or wrong JSON type                                                                    |
| other `Err` messages | SSH/upload/daemon-handshake failure; message is passed through from `remote_pty.rs`, not a fixed set |

CLI mapping:

| Item         | Value                                                                          |
| ------------ | ------------------------------------------------------------------------------ |
| Verb         | `ssh`                                                                          |
| Flags        | `<host> [--name <workspace-name>] [--session <mux-session>] [--socket <path>]` |
| Plain stdout | no output                                                                      |
| JSON stdout  | exact result object                                                            |
| Exit codes   | common                                                                         |

Example:

```json
{"id":60,"cmd":"new-remote-workspace","host":"myhost","slot":"mtyx","session_id":"mtyx-...","local_binary_path":"/home/me/.cache/mattyx/cmuxd-remote-linux-amd64","name":"work"}
{"id":60,"ok":true,"data":{"surface":7}}
```

Closing the resulting surface detaches the remote shell (`pty.detach`) rather than killing it (`pty.close`); see `docs/getting-started.md`'s "Remote (SSH) workspaces" section for the persistence/restore story and how to actually terminate a stale remote session.

### detect-agent

| Field  | Value           |
| ------ | --------------- |
| name   | `detect-agent`  |
| status | implemented     |
| since  | protocol 6      |

Ambient agent detection for one surface (issue #78; the issue's `pane detect-agent --pane <id>` — a pane's content lives on its active tab surface, which is what this repo's flat verbs target). Detects which AI agent (claude, codex, pi, opencode, cursor, aider, or a user-added name) is running by (a) walking the pane PTY's process tree (`/proc/<pid>/comm` + `/proc/<pid>/cmdline`, token-exact process patterns) and (b) scraping the visible screen text for marker patterns (substring/`*`-glob, NOT regex). Any process match outranks any screen match; among process matches the most recently spawned process (`/proc/<pid>/stat` starttime, field 22) wins, and on a starttime tie the agent that also has screen evidence wins (the implementable approximation of the issue's "prefer the most recent output"). The result is cached on the surface and surfaces in `list-workspaces` as per-tab `agent_name`/`agent_confidence`; it does NOT set `AgentState` (that wiring, via the existing `source: "detected"` tier, is a follow-up to the agent_status work — issue #1; detection and self-report are deliberately separate).

Params:

| Name      | JSON type | Required/default | Constraints                |
| --------- | --------- | ---------------- | -------------------------- |
| `surface` | `IdRef`   | required         | The pane-content surface   |

Result:

```text
object{
  surface: Id,
  agent: string,            // "claude"|"codex"|"pi"|"opencode"|"cursor"|"aider"|"unknown"|user-added
  confidence: "high"|"medium"|"low"|null,  // null iff agent == "unknown"
  evidence: string          // e.g. "process 'claude' (pid 1234)" / "screen marker 'pi> '"
}
```

Errors:

| Error                                        | Condition                                    |
| -------------------------------------------- | -------------------------------------------- |
| `unknown surface <id>`                       | Surface id does not exist                    |
| `agent detection disabled by configuration`  | `[[agent_detection]] enabled = false`        |
| `bad request: ...`                           | Wrong JSON type                              |

CLI mapping:

| Item         | Value                                                        |
| ------------ | ------------------------------------------------------------ |
| Verb         | `detect-agent`                                               |
| Flags        | `--surface <id>`                                             |
| Plain stdout | one line: `<surface> <agent> <confidence> <evidence>`        |
| JSON stdout  | exact result object                                          |
| Exit codes   | common                                                       |

Example:

```json
{"id":120,"cmd":"detect-agent","surface":3}
{"id":120,"ok":true,"data":{"surface":3,"agent":"claude","confidence":"high","evidence":"process 'claude' (pid 1234)"}}
```

### detect-agents

| Field  | Value           |
| ------ | --------------- |
| name   | `detect-agents` |
| status | implemented     |
| since  | protocol 6      |

Runs `detect-agent` on every live surface in one call (issue #78's `agent detect-batch`, for fleet dashboards). Keys of the map are surface ids — this repo's pane-content ids (`Workspace → Screen → Pane → Surface`).

Params: none.

Result:

```text
object{agents: map<string, string>}  // {"3": "claude", "7": "unknown", ...}
```

Errors: same `agent detection disabled by configuration` gate as `detect-agent`.

CLI mapping:

| Item         | Value                                     |
| ------------ | ----------------------------------------- |
| Verb         | `detect-agents`                           |
| Flags        | none                                      |
| Plain stdout | one `<surface> <agent>` row per surface   |
| JSON stdout  | exact result object                       |
| Exit codes   | common                                    |

### agent-pattern-add

| Field  | Value                |
| ------ | -------------------- |
| name   | `agent-pattern-add`  |
| status | implemented          |
| since  | protocol 6           |

Adds a user pattern to the daemon's live detection registry, layered on top of the bundled one (issue #78 AC4). Patterns are substring/glob (`*` wildcard spans any run of characters), NOT regex — no runtime crate in the workspace links `regex`. Process patterns match whole argv tokens (a bare `pi` pattern matches the pi CLI but never `spider`/`pip`); screen patterns match anywhere in the visible text. User patterns live for the daemon's session (not persisted across restarts in v1); duplicates are rejected.

Params:

| Name              | JSON type | Required/default | Constraints                                        |
| ----------------- | --------- | ---------------- | -------------------------------------------------- |
| `name`            | `string`  | required         | Agent name; not empty, no whitespace, not `unknown`|
| `pattern`         | `string`  | required         | Non-empty marker (substring/glob)                   |
| `kind`            | `string`  | default `screen`  | `process` or `screen`                               |
| `confidence`      | `string`  | default `medium`  | `high`, `medium`, or `low`                          |
| `case_insensitive`| `bool`    | default `false`   | Case-fold before matching                           |

Result: the registered pattern object (`name`, `kind`, `pattern`, `confidence`, `case_insensitive`).

Errors:

| Error                        | Condition                                       |
| ---------------------------- | ----------------------------------------------- |
| `agent name cannot be ...`   | Empty / whitespace / reserved `unknown`         |
| `pattern cannot be empty`    | Empty pattern                                   |
| `pattern ... already registered` | Exact duplicate                             |
| `bad kind <k>`               | Kind not process/screen                         |
| `bad confidence <c>`         | Confidence not high/medium/low                 |

CLI mapping: noun form `mtyx agent-pattern add <name> --pattern <marker> [--kind process|screen] [--confidence high|medium|low] [--case-insensitive]` (translated to this verb, the `workspace-color` precedent); no output on success.

### agent-pattern-list

| Field  | Value                 |
| ------ | --------------------- |
| name   | `agent-pattern-list`  |
| status | implemented           |
| since  | protocol 6            |

Lists the effective pattern registry: the bundled top-6 patterns (embedded from `mux-core/src/agent_detect/agents.json`; cannot be removed) plus user adds. Params: none. Result: `object{patterns: array<object{name, kind, pattern, confidence, case_insensitive}>}`. CLI: `mtyx agent-pattern list` → one `<name> <kind> <confidence> <pattern>` row per pattern.

### agent-pattern-remove

| Field  | Value                   |
| ------ | ----------------------- |
| name   | `agent-pattern-remove`  |
| status | implemented             |
| since  | protocol 6              |

Removes every user-added pattern named `name`. Bundled patterns cannot be removed. Params: `name: string` (required). Result: empty object. Errors: `no user pattern for agent <name> ...` when nothing matched. CLI: `mtyx agent-pattern remove <name>`; no output on success.

## Proposed Hooks Config

Hooks are proposed protocol v6 config, not a socket command. They are declared in `~/.config/mattyx/mux.json` under `hooks`.

Schema:

```text
object{
  hooks?: object{
    on-bell?: array<HookCommand>,
    on-agent-blocked?: array<HookCommand>,
    on-agent-done?: array<HookCommand>,
    on-surface-exit?: array<HookCommand>
  }
}

HookCommand =
  object{
    argv: array<string>,
    cwd?: string|null,
    timeout_ms?: uint64,
    env?: object<string,string>
  }
| object{
    command: string,
    cwd?: string|null,
    timeout_ms?: uint64,
    env?: object<string,string>
  }
```

Exactly one of `argv` or `command` is required. `argv` executes directly. `command` executes through the session shell as `shell -lc <command>`. The default timeout is 5000 ms. Hook failures are reported through the debug log and may post a `warning` notification; they must not block the mux event loop indefinitely.

Common environment:

| Env var                  | Meaning                                     |
| ------------------------ | ------------------------------------------- |
| `MTYX_MUX_SESSION`       | Session name                                |
| `MTYX_MUX_SOCKET`        | Unix socket path when available             |
| `MTYX_MUX_EVENT`         | Hook event name                             |
| `MTYX_MUX_SURFACE`       | Surface id when the event is surface-scoped |
| `MTYX_MUX_WORKSPACE`     | Workspace id when known                     |
| `MTYX_MUX_SCREEN`        | Screen id when known                        |
| `MTYX_MUX_PANE`          | Pane id when known                          |
| `MTYX_MUX_AGENT_STATE`   | Agent state for agent hooks                 |
| `MTYX_MUX_AGENT_SOURCE`  | Agent source for agent hooks                |
| `MTYX_MUX_AGENT_SESSION` | Upstream agent session id when reported     |

Hook event mapping:

| Hook               | Trigger                                 |
| ------------------ | --------------------------------------- |
| `on-bell`          | Implemented `bell` event                |
| `on-agent-blocked` | Proposed agent state becomes `blocked`  |
| `on-agent-done`    | Proposed agent state becomes `done`     |
| `on-surface-exit`  | Implemented surface exits and is reaped |

## Compatibility Notes

The following v5 behaviors are awkward for generated bindings and should be normalized in protocol v6:

| Area                     | v5 behavior                                                                                       | Proposed v6 normalization                                                                                   |
| ------------------------ | ------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| Create commands          | `new-tab`, `new-browser-tab`, `new-screen`, `new-workspace`, and `split` return only `{surface}`  | Return `{surface,pane,screen,workspace}`                                                                    |
| Selection commands       | `select-*` returns success for unknown targets, out-of-range indexes, and missing selector fields | Return a changed boolean or reject invalid target/index                                                     |
| Resize command           | `resize-surface` does not report whether size changed or final clamped size                       | Return `{changed,cols,rows}`                                                                                |
| Ratio command            | `set-ratio` silently clamps and does not return final ratio                                       | Return `{ratio}` after clamping                                                                             |
| Naming commands          | Empty string clears pane/surface/screen names but stores an empty workspace name                  | Make empty string clear all optional display names, including workspace                                     |
| Attach response ordering | v5 `attach-surface` sends `vt-state` before the command response                                  | v6 keeps attach as an event stream and adds `resized` replay events; clients must gate behavior by protocol |
| Error taxonomy           | Errors are strings from `anyhow`, IO, base64, and terminal layers                                 | Add stable machine error codes while preserving messages                                                    |
| Optional size pair       | Supplying only one of `cols` or `rows` is silently ignored                                        | Reject partial size pairs                                                                                   |
| Unknown fields           | Unknown request fields are ignored by serde                                                       | Reject unknown fields or define extension slots                                                             |
