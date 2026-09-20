# Control Socket Protocol

As of protocol v7, every server speaks JSON Lines over a Unix domain socket. Send one JSON object per line. Every request receives one response line. `subscribe` and `attach-surface` also push event lines on the same connection.

For shell use, prefer `mtyx <verb>`; it wraps the same socket commands and preserves JSON output with `--json`.

Default socket path:

```text
$TMPDIR/mtyx-<uid>/<session>.sock
```

`identify` reports the protocol version and, since protocol 7 (issue #88), a `capabilities` record for feature negotiation. The one defined capability is `input-ack`: confirmed (receipted) `send` — see [Confirmed Input](#confirmed-input-receipted-send). A client wanting confirmed input must gate on `capabilities["input-ack"] == true` and refuse (structured `legacy_host_receipt_rejected` error) against a daemon lacking it, never silently downgrade.

```json
{"id":1,"cmd":"identify"}
{"id":1,"ok":true,"data":{"app":"mtyx","version":"...","protocol":7,"capabilities":{"input-ack":true},"session":"main","pid":12345}}
```

Responses have this shape:

```json
{"id":1,"ok":true,"data":{}}
{"id":2,"ok":false,"error":"unknown surface 99"}
{"id":3,"ok":false,"error":"oversized_input: confirmed send payload is 1048577 bytes; the cap is 1048576 bytes (MAX_CONFIRMED_SEND_BYTES)","code":"oversized_input"}
```

Bad JSON returns `ok:false` with no request id. Since protocol 7, structured errors carry a machine-readable `code` field (issue #88); the `error` string keeps the `<code>: ` prefix so string-matching callers see the code too.

## Command Contract

The full API contract lives in `mux/spec/` (`commands.md`, `events.md`); `report-agent`, `list-agents`, and the `agent-state-changed` event documented there as "proposed" are implemented as of this fork. `mux-core/src/server.rs` remains the source of truth for exactly what's live.

The server command set in this branch is:

```text
identify
list-workspaces
send
read-screen
vt-state
new-tab
new-browser-tab
new-workspace
new-screen
split
set-ratio
move-tab
move-workspace
set-default-colors
close-surface
close-pane
close-screen
close-workspace
rename-pane
rename-surface
rename-screen
rename-workspace
resize-surface
focus-pane
select-tab
select-screen
select-workspace
subscribe
attach-surface
scroll-surface
report-agent
list-agents
new-remote-workspace
```

`move-tab` moves a surface to a target pane and insertion index. It supports same-pane reorder and cross-pane moves.

```json
{"id":10,"cmd":"move-tab","surface":4,"pane":2,"index":0}
```

`move-workspace` moves a workspace to an insertion index.

```json
{"id":11,"cmd":"move-workspace","workspace":3,"index":0}
```

## Events

`subscribe` starts event streaming:

```json
{"id":20,"cmd":"subscribe"}
```

Response data is `{}`. Future event lines may interleave with responses.

Subscribed event lines are:

```json
{"event":"surface-output","surface":4}
{"event":"surface-resized","surface":4,"cols":120,"rows":40}
{"event":"surface-exited","surface":4}
{"event":"title-changed","surface":4}
{"event":"bell","surface":4}
{"event":"tree-changed"}
{"event":"empty"}
```

`surface-resized` reports the final clamped cell size and is emitted only when the surface size actually changes.

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

`state` is one of `working`, `blocked`, `idle`, `done`, `unknown`. `source` is `detected`, `socket`, or `hook`, lowest to highest authority: a hook report always applies; a socket report is rejected while the current source is a hook report (so an ad hoc `report-agent` call from a script can't clobber a live hook integration); a `detected` report — mux-core watching the surface's own output, currently just OSC desktop notifications, see [Desktop Notifications](#desktop-notifications) below — is rejected while the current source is `socket` or `hook`. Subscribers see accepted reports as `agent-state-changed`:

```json
{"event":"agent-state-changed","surface":1,"previous":null,"state":"working","source":"hook","session":"abc123","updated_at_ms":1710000000000}
```

`list-agents` returns current records, optionally filtered:

```json
{"id":51,"cmd":"list-agents","state":"blocked"}
{"id":51,"ok":true,"data":{"agents":[{"surface":3,"state":"blocked","source":"hook","session":"abc123","updated_at_ms":1710000000000}]}}
```

## Desktop Notifications

mux-core watches every PTY surface's raw output for an OSC 9, OSC 777 (rxvt), or kitty-protocol desktop notification — the same three formats libghostty's own OSC parser recognizes as one unified command (`ghostty_vt::OscParser`, extending libghostty-vt's public C API with two data-extraction kinds this fork added: `GHOSTTY_OSC_DATA_DESKTOP_NOTIFICATION_TITLE_STR`/`..._BODY_STR`, alongside the existing window-title extraction). Detection needs no configuration — any program in the pane can trigger it just by writing the sequence, e.g. `printf '\033]9;Build failed\007'`.

Each detected notification does two things: emits `report-agent` with `state: "blocked"`, `source: "detected"` (see Agent State above), and broadcasts an `osc-notification` event to subscribers:

```json
{"event":"osc-notification","surface":3,"title":"","body":"Build failed"}
```

The bundled TUI forwards this to the host desktop via `notify-send`. There is no socket command to trigger this deliberately — it is purely a detector on real pty output — see `spec/events.md` for the full event contract and how this differs from the still-unimplemented `notify`/notification-inbox proposal elsewhere in `spec/`.

## Remote Workspaces

`new-remote-workspace` creates a workspace whose tab is a `cmuxd-remote` session over SSH instead of a local shell (see `getting-started.md`'s "Remote (SSH) workspaces" and `remote_pty.rs`'s module doc for the transport). Building/caching the daemon binary for the remote's OS/arch is the caller's job — the bundled `mtyx ssh <host>` CLI command does it and is the intended way to use this, not calling the socket command directly:

```json
{"id":60,"cmd":"new-remote-workspace","host":"myhost","slot":"mtyx","session_id":"mtyx-...","local_binary_path":"/home/me/.cache/mattyx/cmuxd-remote-linux-amd64","name":"work"}
{"id":60,"ok":true,"data":{"surface":7}}
```

`slot` groups sessions under one persistent remote daemon per host; reuse the same slot for multiple sessions on the same host. `session_id` decides fresh-vs-reattach: a new one starts a new remote shell, an existing one (e.g. from a prior run's persisted snapshot) reattaches to it, replaying scrollback. Closing the resulting surface (`close-surface`/`close-pane`/quitting the tab) detaches rather than killing the remote shell — see `getting-started.md` for how that interacts with session persistence, and for how to actually terminate a stale remote session (no socket command does that today).
