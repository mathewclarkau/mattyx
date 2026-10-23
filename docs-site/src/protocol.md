<!-- AUTO-GENERATED FROM mux/docs/protocol.md BY scripts/sync-docs-site.sh — DO NOT EDIT DIRECTLY -->

# Control Socket Protocol

As of protocol v7, every server speaks JSON Lines over a Unix domain socket. Send one JSON object per line. Every request receives one response line. `subscribe` and `attach-surface` also push event lines on the same connection.

For shell use, prefer `mtyx <verb>`; it wraps the same socket commands and preserves JSON output with `--json`.

## Socket Path Resolution

The client (`mtyx <verb>`) resolves the daemon socket in this order:

1. `--socket <path>` — explicit path wins.
2. `$MTYX_MUX_SOCKET` — when set and non-empty. At startup the CLI first runs a legacy shim: if `MTYX_MUX_SOCKET` is unset but `CMUX_MUX_SOCKET` is set, the `CMUX_*` value is copied into the `MTYX_*` variable, so legacy environments keep working.
3. `--session <name>` (default `main`) — the canonical path is
   `$XDG_RUNTIME_DIR/mtyx-<uid>/<session>.sock`, falling back to `$TMPDIR` and
   then `/tmp` when `XDG_RUNTIME_DIR` is unset. If the canonical path is not
   live and the legacy `cmux-<uid>/<session>.sock` at the same base directory
   IS live, the legacy path is used instead.

## identify

`identify` reports the protocol version and, since protocol 7 (issue #88), a `capabilities` record for feature negotiation. The one defined capability is `input-ack`: confirmed (receipted) `send` — see [Confirmed Input](#confirmed-input-receipted-send). A client wanting confirmed input must gate on `capabilities["input-ack"] == true` and refuse (structured `legacy_host_receipt_rejected` error) against a daemon lacking it, never silently downgrade.

```json
{"id":1,"cmd":"identify"}
{"id":1,"ok":true,"data":{"app":"mtyx","version":"...","protocol":7,"capabilities":{"input-ack":true},"session":"main","pid":12345}}
```

## Reply Envelope

Responses have this shape:

```json
{"id":1,"ok":true,"data":{}}
{"id":2,"ok":false,"error":"unknown surface 99"}
{"id":3,"ok":false,"error":"oversized_input: confirmed send payload is 1048577 bytes; the cap is 1048576 bytes (MAX_CONFIRMED_SEND_BYTES)","code":"oversized_input"}
```

`data` is present on success, `error` on failure, both omitted (`skip_serializing_if`) when absent. Bad JSON returns `ok:false` with no request id (`"id":null`). Since protocol 7, structured errors carry a machine-readable `code` field (issue #88); the `error` string keeps the `<code>: ` prefix so string-matching callers see the code too.

## Compatibility Promise

`PROTOCOL_VERSION` (currently **7**) increments on breaking wire changes; additive changes (new commands, new optional request fields, new reply keys, new event kinds) do not bump it. Consumers pin by checking `identify`'s `protocol` field and gating optional features on the `capabilities` record rather than on version arithmetic. The golden wire-shape tests in `mux-core/tests/wire_contract.rs` pin the reply shape of the read-only command families so a shape cannot silently drift.

## Command Contract

The full API contract lives in `mux/spec/` (`commands.md`, `events.md`); `report-agent`, `list-agents`, and the `agent-state-changed` event documented there as "proposed" are implemented as of this fork. `mux-core/src/server.rs` remains the source of truth for exactly what's live.

The socket command set (`Command` enum, `#[serde(tag="cmd", rename_all="kebab-case")]`) is:

```text
identify
send                     write-bytes (alias: answer)   read-screen
wait-for                 run                           vt-state
new-tab                  new-browser-tab               set-cell-pixels
browser-mouse            browser-wheel                 browser-key
browser-insert-text      browser-navigate              browser-back
browser-forward          browser-reload                browser-activate
new-workspace            new-remote-workspace          get-resolved-config
new-screen               split                         set-ratio
move-tab                 move-workspace                set-default-colors
close-surface            close-pane                    close-screen
close-workspace          rename-pane                   rename-surface
rename-screen            rename-workspace              set-workspace-color
set-status               workspace-color               trigger-flash
resize-surface           focus-pane                    zoom-pane
select-tab               select-screen                 select-workspace
subscribe                notify-ack                    attach-surface
scroll-surface           report-agent                  list-agents
detect-agent             detect-agents                 agent-pattern-add
agent-pattern-list       agent-pattern-remove          pane-worktree-create
pane-worktree-list       pane-worktree-remove          agent-read
agent-send               agent-prompt                  agent-register
wait-agent-status        wait-ready
rename-session           layout-export                 layout-export-all
layout-apply
```

Notes on the mapping between CLI verbs and socket commands:

- `answer` is a serde alias of `write-bytes`: the CLI maps a friendly key name
  (`down`, `up`, `enter`, `esc`, `ctrl-c`, `1`–`9`) to the raw bytes and sends
  a base64 `write-bytes` request. On the wire there is no separate `answer`
  command.
- The CLI verbs `list-sessions`, `kill-session`, `kill-stale` (alias `gc`),
  and `screenshot` are client-side operations (they inspect the runtime
  directory / render locally) — they issue no dedicated socket command of
  the same name. `kill-stale --dry-run` is a read-only preview of the same
  reap: it reports the stale `.sock`/`.pid` pairs and unlinks nothing.
  Human-readable `list-sessions` lazily reaps stale `.sock`/`.pid` pairs on
  enumerate by default (`--no-gc` opts out); `list-sessions --json` stays
  read-only by default unless `--gc` is passed, in which case stale pairs are
  swept and `"swept": <n>` is included in the JSON envelope (issue #138).
- `mtyx agent <start|prompt|wait|read|list>` is a CLI-only noun rewritten
  onto flat verbs before dispatch (the `agent-pattern` pattern): `agent
  start` composes `new-tab` + `agent-register` in one CLI flow, `agent
  list` joins `list-agents` with `list-workspaces` (client-side), and
  `agent prompt`/`wait`/`read` map to `agent-prompt`/`wait-agent-status`/
  `agent-read`. The noun defaults `agent wait --status blocked --timeout
  60000` when the flags are omitted.
- `theme list`, `agents`, `plugin`, `ssh`, `machine`, `completions`, and the
  agent-integration subcommands (`claude`, `codex`, `pi`, `aider`, `grok`,
  `antigravity`, `opencode`) are CLI-only special cases, not socket commands.

`move-tab` moves a surface to a target pane and insertion index. It supports same-pane reorder and cross-pane moves.

```json
{"id":10,"cmd":"move-tab","surface":4,"pane":2,"index":0}
```

`move-workspace` moves a workspace to an insertion index.

```json
{"id":11,"cmd":"move-workspace","workspace":3,"index":0}
```

### Read-only command shapes (pinned by golden tests)

The reply shapes below are pinned by `mux-core/tests/wire_contract.rs`:

```json
{"id":1,"cmd":"list-workspaces"}
{"id":1,"ok":true,"data":{"workspaces":[{"id":1,"name":"main","active":true,"screens":[...],"color":null,"icon":null,"short_id":"000001"}, ...]}}

{"id":2,"cmd":"get-resolved-config"}
{"id":2,"ok":true,"data":{ ...resolved presentation chrome (theme/tabs/sidebar/keys), or {} when unset... }}

{"id":3,"cmd":"list-agents"}
{"id":3,"ok":true,"data":{"agents":[{"surface":3,"state":"blocked","source":"hook","session":"abc123","updated_at_ms":1710000000000}, ...]}}

{"id":4,"cmd":"agent-pattern-list"}
{"id":4,"ok":true,"data":{"patterns":[...]}}

{"id":5,"cmd":"pane-worktree-list","pane":2}
{"id":5,"ok":true,"data":{"worktrees":[...]}}

{"id":6,"cmd":"layout-export","workspace":"main"}
{"id":6,"ok":true,"data":{ ...layout document for the workspace... }}

{"id":7,"cmd":"read-screen","surface":1}
{"id":7,"ok":true,"data":{"text":"..."}}

{"id":8,"cmd":"notify-ack","client":"my-client","surface":3}
{"id":8,"ok":true,"data":{"client":"my-client","acked":[{"surface":3,"notification_id":12}],"unread":0}}
```

Note the asymmetry: `read-screen` returns only `{"text": ...}`; `agent-read`
returns `{"surface": ..., "text": ...}`.

### wait-ready

`wait-ready` (issue #85) blocks until a surface is *ready* — its screen shows a recognised prompt (or an agent) AND its PTY has a running process-tree child — or `timeout_ms` elapses (default and cap are server-side constants). Read-only observation: it never writes to the pane or mutates mux state. The reply is always `ok:true`; a timeout is a payload with `"ready": false`, NOT an error:

```json
{"id":9,"cmd":"wait-ready","surface":1,"timeout_ms":5000}
{"id":9,"ok":true,"data":{"ready":true,"surface":1,"prompt_seen":true,"child":{"pid":1234,"comm":"fish"},"elapsed_ms":820}}
```

The CLI maps `ready:false` to a nonzero exit.

### agent-read and the busy guard

`agent-read` reads a pane addressed by agent name or surface id. `source` is one of `visible` (default: viewport rows only), `recent` (a bottom-anchored window including scrollback), or `recent-unwrapped` (the recent window with soft line-wraps re-joined). `lines` tails the last N lines (default 40).

**Busy guard:** when the target's effective agent state (see [Agent State](#agent-state)) is `working`, `agent-read` refuses with the structured error code `agent_not_idle` rather than returning a mid-turn snapshot. Passing `"force": true` (CLI: `--force`) bypasses the guard. Only `working` gates; `blocked`, `idle`, `done`, and unreported surfaces read normally.

```json
{"id":12,"cmd":"agent-read","target":"claude","source":"recent","lines":80}
{"id":12,"ok":false,"error":"agent_not_idle: surface 3 has an agent mid-turn ...; pass --force to read anyway.","code":"agent_not_idle"}
```

**Alternate-screen boundary:** when the target app uses the VT alternate screen (full-screen agent UIs), `recent`/`recent-unwrapped` return the same visible viewport as `visible` — ghostty creates the alternate screen with `max_scrollback = 0` (`ghostty/src/terminal/Terminal.zig`), its `PageList::scrollbar()` is viewport-only in that mode, and the ghostty C API only exposes the *active* screen. Paging alt-screen history would require a ghostty VT change (nonzero alternate-screen `max_scrollback` plus a C-API accessor for the non-active screen); mux deliberately stops at that boundary and does not fork ghostty semantics.

### Named agents: `agent-register` and `agent-prompt`

`agent-register` binds a unique agent NAME to a surface at HOOK authority — the registration primitive behind `mtyx agent start <name> ... -- <argv>` (which composes `new-tab --exec` + `agent-register` in one CLI flow). Names are the addressing key for the name-addressed verbs (`agent-read`, `agent-send`, `agent-prompt`, `wait-agent-status`).

Why hook authority and not a plain socket-source `report-agent --agent <name>` after the spawn: the report authority ladder is Detected < Socket < Hook, and a fast-starting agent's first HOOK report can land before the registration. A socket report is then rejected (`source >= existing.source` fails), and the name is never stored. Registering at hook authority closes that window; later hook reports — which never carry `agent` — keep the established name (reports omitting `agent` preserve it), so the registration survives the pane's lifetime. An existing report's state/session/message are preserved (a hook that already fired is not clobbered); a fresh pane starts at `unknown`.

```json
{"id":70,"cmd":"agent-register","surface":3,"agent":"demo"}
{"id":70,"ok":true,"data":{"surface":3,"state":"unknown","source":"hook","session":null,"agent":"demo","message":null,"updated_at_ms":1710000000000,"state_seq":1}}
```

Errors (both stable `code`s): a name already registered to ANOTHER surface fails with `agent_name_taken` (names must be unique per session — duplicates would make name targets ambiguous); an empty or all-digit name fails with `agent_name_invalid` (an all-digit name would be ambiguous with a surface id in `resolve_agent_target`). Re-registering the same name on the same surface is idempotent (no `state_seq` bump, so `--require-transition` waits cannot be satisfied by it).

`agent-prompt` types text + a literal CR into the pane addressed by agent name or surface id — the `mtyx agent prompt` primitive. It carries the SAME blocked-send gate as `send`: when the target's effective state is `blocked`, the write is refused with the structured `agent_blocked` error and nothing is written, unless `"force": true` (CLI: `--force`). It is deliberately NOT `agent-send`, which is the un-gated, no-CR raw hatch. Text is written verbatim (the target is an agent REPL, not a shell line editor).

```json
{"id":71,"cmd":"agent-prompt","target":"demo","text":"run the tests"}
{"id":71,"ok":true,"data":{"surface":3}}

{"id":72,"cmd":"agent-prompt","target":"demo","text":"run the tests"}
{"id":72,"ok":false,"error":"agent_blocked: surface 3 is blocked by an agent approval/dialog ...; pass --force to send anyway.","code":"agent_blocked"}
```

The reply shapes of both commands are pinned by golden tests in `mux-core/tests/wire_contract.rs` (`agent_register_shape`, `agent_prompt_shape_and_blocked_gate`).

## Events

`subscribe` starts event streaming:

```json
{"id":20,"cmd":"subscribe"}
{"id":21,"cmd":"subscribe","client":"my-client"}
```

Response data is `{}`. Future event lines may interleave with responses.

The optional `client` string selects the durable-notification replay set (see [Durable Notifications](#durable-notifications)): on subscribe, the server first replays that client's unread durable notifications (oldest first) as `notification` events on the same line stream, strictly before any live event. Omitting `client` disables replay.

Live subscribed event lines are flat objects keyed by `"event"`:

```json
{"event":"surface-output","surface":4}
{"event":"surface-resized","surface":4,"cols":120,"rows":40}
{"event":"surface-exited","surface":4}
{"event":"title-changed","surface":4}
{"event":"bell","surface":4}
{"event":"flash","workspace":3,"surface":4}
{"event":"status","message":"..."}
{"event":"tree-changed"}
{"event":"empty"}
{"event":"agent-state-changed","surface":1,"previous":null,"state":"working","source":"hook","session":"abc123","agent":"claude","message":null,"updated_at_ms":1710000000000,"state_seq":7}
{"event":"osc-notification","surface":3,"title":"","body":"Build failed"}
{"event":"notification","surface":3,"id":12,"title":"Build failed","body":"...","timestamp_ms":1710000000000}
```

`surface-resized` reports the final clamped cell size and is emitted only when the surface size actually changes. `notification` is a durable record (issue #92) — see below; the same shape is used for subscribe-time replay and live delivery, so a client has one parse path.

## Durable Notifications and notify-ack

Durable notifications (`notification` events, issue #92) are recorded server-side per surface with a monotonically increasing `id`. Read state is tracked PER CLIENT STRING: each `client` (default `""`) has a high-water mark, and `notify-ack` advances it:

```json
{"id":22,"cmd":"notify-ack","client":"my-client","surface":3,"notification_id":12}
{"id":22,"ok":true,"data":{"client":"my-client","acked":[{"surface":3,"notification_id":12}],"unread":0}}
```

Both `surface` and `notification_id` are optional — omitting them acks everything outstanding for that client. The reply echoes the client, lists the records actually acked (`acked`, possibly empty, so "acked N" is distinguishable from "nothing to ack" without a second round-trip), and reports the remaining `unread` count. A second `subscribe` with the same `client` after `notify-ack` replays nothing.

## Attach Surface

`attach-surface` streams a PTY surface. Browser surfaces return `browser panes are not supported over attach yet`.

```json
{"id":30,"cmd":"attach-surface","surface":4}
```

The server first sends:

```json
{"event":"vt-state","surface":4,"cols":120,"rows":40,"data":"<base64-vt-replay>"}
```

Then it sends ordered stream frames:

```json
{"event":"output","surface":4,"data":"<base64-pty-bytes>"}
{"event":"resized","surface":4,"cols":132,"rows":43,"data":"<base64-vt-replay>"}
```

The `resized` attach frame carries the new cell size and a fresh VT replay captured at that size. It is delivered in the same attach stream as output frames, so a client can reset its local terminal, apply the replay, and continue consuming later output in order.

When the stream ends, it sends:

```json
{"event":"detached","surface":4}
```

## Client Compatibility

The remote TUI requires protocol v7. It refuses servers reporting any other protocol version because attach streams need resize markers carrying replay data (v6) and it ships with the v7 `identify` capabilities record (issue #88).

Attach clients mirror PTY surfaces locally. On first render, a client can resize the server surface before requesting `attach-surface`, so the initial VT replay is captured at the visible geometry.

When several attach clients render the same surface at different sizes, sizing follows latest local interaction. A client reasserts its visible sizes after key input, mouse input, paste, focus gained, or terminal resize. Mux-driven redraws update local mirrors from `surface-resized` without reasserting an idle client's viewport.

## Confirmed Input (Receipted Send)

Protocol 7 (issue #88) adds a confirmed mode to `send`: `"confirm": true` (plus optional `"timeout_ms"`, default 5000, cap 60000) returns success only after the daemon OBSERVES the input consumed — the practical receipt is: bytes written to the PTY AND the surface echoed/advanced (the pty reader thread applied output), or the child exited, within the timeout. This is a documented heuristic receipt, not a byte-exact consumption proof.

```json
{"id":40,"cmd":"send","surface":4,"text":"cargo test\n","confirm":true,"timeout_ms":10000}
{"id":40,"ok":true,"data":{"confirmed":true}}
```

Ordering: confirmed sends serialize per-surface on a FIFO ticket, so concurrent confirmed sends to one surface resolve in submission order. Unconfirmed send (absent/false `confirm`) is unchanged fire-and-forget and bypasses the queue.

Bounds and errors:

- Confirmed payloads above 1 MiB (`MAX_CONFIRMED_SEND_BYTES`) are rejected up front with `oversized_input` rather than applying receipt backpressure to an unbounded write.
- A receipt that never arrives fails with `input_ack_timeout` (the bytes were written; delivery is unproven, not failed).
- A client requesting confirmed send against a daemon whose `identify` lacks `input-ack` (protocol <= 6) must refuse with `legacy_host_receipt_rejected` — never silently downgrade. The bundled CLI does this pre-flight automatically; `mtyx send` is confirmed BY DEFAULT, and `--no-confirm` preserves fire-and-forget.

`send` is also gated on agent state: when the target's effective agent state is `blocked` (an agent approval/dialog is up), `send` fails with the structured `agent_blocked` error and nothing is written to the PTY, unless `"force": true` (CLI: `--force`) is passed.

## Browser Limitations

Browser surfaces appear in `list-workspaces` as `kind: "browser"` with `browser_source: "external"` or `"launched"`. PTY and VT commands against browser surfaces return errors. `attach-surface` does not stream browser pixels as of protocol v6, and the remote TUI shows a placeholder for browser panes.

## Working Directory

Each PTY tab in `list-workspaces` carries `cwd`: the shell's live OSC 7 report when available, otherwise the directory the surface was spawned in. It is `null` for browser surfaces and for PTY surfaces spawned without a resolvable home directory.

## Agent State

`report-agent` records coding-agent lifecycle state for a PTY surface (full contract in `spec/commands.md`); every PTY tab in `list-workspaces` also carries `agent_state` (`null` until something reports for that surface). A hook script (see the Claude Code hook layer) is the intended source, but any client can report:

```json
{"id":50,"cmd":"report-agent","surface":1,"state":"working","source":"hook","session":"abc123"}
{"id":50,"ok":true,"data":{"surface":1,"state":"working","source":"hook","session":"abc123","updated_at_ms":1710000000000}}
```

`state` is one of `working`, `blocked`, `idle`, `done`, `unknown`. `source` is `heuristic`, `detected`, `socket`, or `hook`, lowest to highest authority: a hook report always applies; a socket report is rejected while the current source is a hook report (so an ad hoc `report-agent` call from a script can't clobber a live hook integration); a `detected` report — mux-core watching the surface's own output, currently just OSC desktop notifications, see [Desktop Notifications](#desktop-notifications) below — is rejected while the current source is `socket` or `hook`; and a `heuristic` report (protocol 8, issue #140) — the zero-config screen-marker classifier behind `detect-agent` and the daemon's background poller — is rejected while any of the other three is current. Hooks (and explicit reports generally) always win over heuristics. Every PTY tab in `list-workspaces` also carries `agent_source` (protocol 8): the applied report's tier, `null` alongside `agent_state` when nothing has reported, so frontends can render hook-grade and heuristic-grade state differently. Subscribers see accepted reports as `agent-state-changed`:

```json
{"event":"agent-state-changed","surface":1,"previous":null,"state":"working","source":"hook","session":"abc123","agent":"claude","message":null,"updated_at_ms":1710000000000,"state_seq":7}
```

`agent` (reporter-supplied agent name), `message` (optional free-text), and `state_seq` (per-surface monotonic sequence, used by `wait-agent-status --require-transition` to distinguish a fresh transition from a cached state) round out the record.

`list-agents` returns current records, optionally filtered:

```json
{"id":51,"cmd":"list-agents","state":"blocked"}
{"id":51,"ok":true,"data":{"agents":[{"surface":3,"state":"blocked","source":"hook","session":"abc123","updated_at_ms":1710000000000}]}}
```

The effective agent state used by the `send`/`agent-read` gates is: the latest explicit report (socket/hook/detected, by the authority ladder above) if any, otherwise a fresh classification of the screen contents by the built-in agent detector (`detect-agent`/`detect-agents`/`agent-pattern-*` manage the pattern table).

## Desktop Notifications

mux-core watches every PTY surface's raw output for an OSC 9, OSC 777 (rxvt), or kitty-protocol desktop notification — the same three formats libghostty's own OSC parser recognizes as one unified command (`ghostty_vt::OscParser`, extending libghostty-vt's public C API with two data-extraction kinds this fork added: `GHOSTTY_OSC_DATA_DESKTOP_NOTIFICATION_TITLE_STR`/`..._BODY_STR`, alongside the existing window-title extraction). Detection needs no configuration — any program in the pane can trigger it just by writing the sequence, e.g. `printf '\033]9;Build failed\007'`.

Each detected notification does two things: emits `report-agent` with `state: "blocked"`, `source: "detected"` (see Agent State above), and broadcasts an `osc-notification` event to subscribers:

```json
{"event":"osc-notification","surface":3,"title":"","body":"Build failed"}
```

The bundled TUI forwards this to the host desktop via `notify-send`. There is no socket command to trigger this deliberately — it is purely a detector on real pty output — see `spec/events.md` for the full event contract and how this differs from the still-unimplemented `notify`/notification-inbox proposal elsewhere in `spec/`.

## Remote Workspaces

`new-remote-workspace` creates a workspace whose tab is a `mtyxd-remote` session over SSH instead of a local shell (see `getting-started.md`'s "Remote (SSH) workspaces" and `remote_pty.rs`'s module doc for the transport). Building/caching the daemon binary for the remote's OS/arch is the caller's job — the bundled `mtyx ssh <host>` CLI command does it and is the intended way to use this, not calling the socket command directly:

```json
{"id":60,"cmd":"new-remote-workspace","host":"myhost","slot":"mtyx","session_id":"mtyx-...","local_binary_path":"/home/me/.cache/mattyx/mtyxd-remote-linux-amd64","name":"work"}
{"id":60,"ok":true,"data":{"surface":7}}
```

`slot` groups sessions under one persistent remote daemon per host; reuse the same slot for multiple sessions on the same host. `session_id` decides fresh-vs-reattach: a new one starts a new remote shell, an existing one (e.g. from a prior run's persisted snapshot) reattaches to it, replaying scrollback. Closing the resulting surface (`close-surface`/`close-pane`/quitting the tab) detaches rather than killing the remote shell — see `getting-started.md` for how that interacts with session persistence, and for how to actually terminate a stale remote session (no socket command does that today).
