# mtyx layout JSON schema (v1)

Issue #76 — `layout export` / `layout apply`: save a workspace's
workspace + screen + pane-BSP + tab + agent-argv topology to a
versioned JSON file, and replay it later (the "save this fleet so I can
boot it tomorrow" daily-resume workflow).

The verbs:

```
mtyx layout-export --workspace <name-or-id> --output fleet.json
mtyx layout-export-all --output-dir ./fleet/          # one file per workspace
mtyx layout-apply --input fleet.json --workspace <name>
```

The internal sibling of this format is `persist.rs`'s session snapshot
(what the daemon writes to `$XDG_STATE_HOME/mattyx/sessions/<name>.json`
on every tree change). That format deliberately does **not** record tab
commands; this one does, which is the whole point.

## Versioning

```json
{ "schema_version": 1, "mtyx_version": "0.17.2", "workspace": { ... } }
```

- `schema_version` is the only gate. A file with any other version is
  **rejected loudly** by `layout-apply` (exit 1, error names the file's
  version) — never silently misparsed.
- `mtyx_version` is the exporting build's `mux_core::VERSION`
  (informational; never `CARGO_PKG_VERSION` — see issue #71).
- Additive, optional fields use `#[serde(default)]` liberally, so a
  future v1 writer adding a field stays readable by older v1 readers.
  The version bumps to 2 only on a breaking change.

## Document shape

```json
{
  "schema_version": 1,
  "mtyx_version": "0.17.2",
  "workspace": {
    "name": "fleet",
    "color": "#ff8800",              // optional, "#rrggbb" or a named preset
    "icon": "robot",                  // optional
    "active_screen": 0,
    "screens": [
      {
        "name": "main",               // optional
        "active_pane": 0,
        "layout": {                   // BSP tree, pane-INDEX leaves
          "type": "split",
          "dir": "right",
          "ratio": 0.6,
          "a": { "type": "leaf", "pane": 0 },
          "b": { "type": "leaf", "pane": 1 }
        },
        "panes": [
          {
            "name": "build",          // optional
            "active_tab": 0,
            "tabs": [ { ... } ]
          }
        ]
      }
    ]
  }
}
```

`layout` mirrors the socket's `list-workspaces` `node_json` shape
(`{"type":"split","dir":"right"|"down","ratio":f32,"a":…,"b":…}` /
`{"type":"leaf","pane":<index>}`), but leaves index the sibling
`panes` array (the `persist.rs` pattern) instead of live pane ids —
ids are session-local and never stable across restarts.

### Tab kinds

| kind | fields | notes |
|---|---|---|
| `pty` | `name?`, `cwd?`, `command?`, `env?` | `command` is the exact argv the tab was spawned with, recorded **at spawn time**. `null` for a default login shell. `env` is a map of injected variables. |
| `browser` | `name?`, `url` | the browser tab's URL. |
| `remote` | `name?`, `host`, `slot`, `session_id`, `local_binary_path` | a `mtyxd-remote` session (see below for the reattach limits). |

### Env exclusion list

`env` never contains `MTYX_MUX_SOCKET` or `MTYX_SOCKET_PATH`: those are
auto-injected (and dual-written) into every spawn from the *applying*
daemon's live socket path. Round-tripping a stale socket path would
detach the restored fleet, so capture filters them out and apply
re-derives them.

## How argv is captured

`PtySurface` records `SurfaceOptions.command` / `extra_env` at spawn
time. Reading `/proc/<child>/cmdline` back at export time is not enough:
agents started by *typing into a shell* (`mtyx send`) are grandchildren
of the PTY child — the direct child is the shell, and the agent argv is
unrecoverable. Consequences:

- Tabs spawned via `new-tab --exec -- <argv...>` (or the socket
  `command`/`env` fields) round-trip exactly (AC5 parity).
- Tabs where a human typed `pi --print hi` into a shell export with
  `command: null`; apply restores a shell in the recorded `cwd`
  (**R1**). Migrate fleets to `--exec` spawns for full parity
  (follow-up: pifactory bootstrap scripts).

## Validation (apply-time, fail-loud)

`LayoutDocument::validate` gates every apply (AC7) — nothing is spawned
before the whole document checks out:

- `schema_version == 1` (hard fail naming the version otherwise),
- every layout leaf indexes an existing pane, exactly once, and every
  pane is referenced (no orphaned/dead panes),
- split ratios strictly inside `(0,1)`,
- `active_screen` / `active_pane` / `active_tab` in range,
- ≥1 screen, ≥1 pane per screen, ≥1 tab per pane,
- workspace `color`/`icon` parse.

During replay, a pane whose tabs cannot be recreated aborts the apply
with `pane <index> (pane-id <id>): <error>`; a pane that could not even
be created names its index. **Already-created panes are left in place**
— apply is fail-loud, not transactional; a `--replace`/rollback flag is
follow-up scope (**R2**). Re-run `layout-apply` under a fresh name after
closing the partial workspace.

## Round-trip guarantees and limits

- Split geometry (directions + ratios), names (workspace/screen/pane/
  tab), colors, icons, and selections (active screen/pane/tab) restore
  exactly.
- Ratios are re-clamped to mtyx's `[0.05, 0.95]` on apply.
- `layout-apply --workspace <name>` uses the **flag's** name, not the
  document's embedded name — one fleet file can be booted under many
  names. The workspace is created if missing (AC2); applying onto an
  existing name is refused (close it first).
- Short-lived recorded commands close their pane when they exit — normal
  mtyx pane semantics; long-running agents should self-daemonize or
  `exec sleep` as their tail.
- Browser tabs re-open their URL (the page state itself — logins,
  scroll — is not captured).
- **Remote tabs (R3):** only a workspace's *first* tab can reattach to
  the same `mtyxd-remote` session (via the `new-remote` workspace
  bootstrap — the inherited `persist.rs` limitation). A remote tab
  anywhere else downgrades to a local shell with a loud
  `MuxEvent::Status` rather than failing silently. If the recorded
  `local_binary_path` is gone or stale, the reattach fails cleanly —
  reconnect manually via `mtyx ssh <host>`.

## Composing with `--apply-local-config` (AC6)

`--apply-local-config` (issue #40) is a *client-side, attach-path*
chrome overlay (theme/tabs/sidebar/keys); the layout document is
*server-side topology*. They compose without any extra flag:

```
mtyx --socket /path/to/remote.sock layout-apply --input fleet.json --workspace ops
mtyx --socket /path/to/remote.sock attach --apply-local-config
```

The first command rebuilds the fleet on the remote daemon; the second
attaches the local TUI with the local `mux.local.toml`/`mux.json`
layered over the remote server's resolved chrome
(`get-resolved-config`). `workspace.color`/`icon` are server state and
ride in the layout document itself.

## Exit codes (CLI)

| code | meaning |
|---|---|
| 0 | success |
| 1 | server-reported error (unknown workspace, schema mismatch, apply failure) or file write error |
| 2 | bad/missing flags, unreadable/unparseable `--input` file, unsafe output dir |
| 3 | socket connect/transport failure |
