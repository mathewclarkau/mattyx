# Getting started

## Prerequisites

Builds need zig 0.15.2, a Rust toolchain, and the `ghostty` submodule. `ghostty-vt-sys` compiles `libghostty-vt.a` from that submodule, so an uninitialized submodule fails before the TUI starts.

```bash
cd mux
cargo build -p mux-tui
```

## Local session

A normal run starts a **detached session daemon** and attaches the TUI as a client.

```bash
cd mux
cargo run -p mux-tui
cargo run -p mux-tui -- --session agents
```

The default session is `main`. If a session with that name is already live, `mtyx` attaches to it instead of starting a second daemon. Prefix `d` (`Ctrl-b d`) detaches the TUI; the daemon keeps running. Re-run `mtyx` (or `mtyx attach`) to reconnect. `mtyx kill-session` ends the daemon.

Use `--term <value>` to set `TERM` for child PTYs. Without it, children get `xterm-256color`; the surface layer also honors `MTYX_MUX_TERM` when no CLI value is supplied.

## Headless server and attach

Headless mode starts only the mux backend and control socket.

```bash
cd mux
cargo run -p mux-tui -- --headless --session agents
```

Attach a TUI to that session from another terminal.

```bash
cd mux
cargo run -p mux-tui -- attach --session agents
```

Detach from an attached TUI with prefix `d`. With default keys, that is `Ctrl-b d`. The server keeps running, and another `attach` (or a plain `mtyx --session <name>`) reconnects to the same tree. PTY tabs attach with a Ghostty VT-state replay followed by a live output stream. `--headless` is still the foreground-server form used by scripts, systemd, and the daemon child that a normal `mtyx` start launches.

### SSH and remote attach with local config

For a remote box, run the server headless there, then attach from your
laptop carrying your local colours and key bindings onto the remote
session. The point is that the *laptop's* mtyx process does the
attaching, so the laptop's `~/.config/mattyx/mux.local.toml` (not the
remote host's) is what applies. Run the mtyx **client** locally and
forward the remote control socket back to the laptop over SSH, so the
local mtyx process attaches to the forwarded socket and layers the
local overlay on top of the *server's* resolved config.

The remote `mtyx --headless` server is reached over an OpenSSH Unix
domain socket forward: `-L <local-path>:<remote-path>`. The remote
path must be a concrete filesystem path: OpenSSH does not expand
environment variables or command substitution in the forward target
(the remote sshd connects to the path directly, no shell). The
easiest way to keep the two ends in sync is to start the headless
server with an explicit `--socket <path>`, then forward that exact
path:

```bash
# on the remote box: serve headless on a known socket, no TUI
remotehost$ mtyx --headless --session agents \
            --socket /run/user/$(id -u)/mtyx-agents.sock

# on the laptop: forward that remote socket to a local path (-Nf runs
# ssh in the background with no shell), then run mtyx attach LOCALLY
# against the forwarded socket with --apply-local-config so the
# laptop's config overlays the server's resolved config.
laptop$ ssh -Nf -L /tmp/mtyx-agents.sock:/run/user/1000/mtyx-agents.sock remotehost
laptop$ mtyx attach --socket /tmp/mtyx-agents.sock --apply-local-config
```

If the remote server was started without `--socket` (so its socket lives
at the default `$XDG_RUNTIME_DIR/mtyx-<uid>/<session>.sock`, e.g.
`/run/user/1000/mtyx-1000/agents.sock`), sub that path into both the
remote command above and the `-L` target. `ssh remotehost 'mtyx
get-sessions'` (or the session's startup log) reports the live socket.

Inspecting layering without a live terminal:

```bash
# fetch the server chrome, layer the local overlay, print merged JSON, exit
laptop$ mtyx attach --socket /tmp/mtyx-agents.sock \
            --apply-local-config --print-resolved-config
# server chrome only (no overlay), for ops scripts
laptop$ mtyx --socket /tmp/mtyx-agents.sock get-resolved-config
```

The local `~/.config/mattyx/mux.local.toml` (or `mux.json`, or
`$MTYX_LOCAL_CONFIG`, or an explicit `--config <path>`) is layered on
top of the server config the laptop fetches over that forwarded socket
via the `get-resolved-config` verb: the server keeps the truth for the
workspace tree, browser, and scrollbar; your laptop wins for theme,
tabs, sidebar, and keys. So your preferred leader key and colour
scheme work the same way they do locally.

Do NOT run `mtyx attach` inside the SSH command string (e.g.
`ssh remotehost 'mtyx attach ... --apply-local-config'`): that executes
mtyx on the remote host and resolves the *remote*
`~/.config/mattyx/mux.local.toml`, the opposite of what this feature is
for. The laptop mtyx process must be the one that loads the overlay.

Check which local file would apply before connecting with the dry run:

```bash
mtyx attach --show-local-config-resolution
mtyx attach --session agents --config ~/.config/mattyx/mux.local.toml
```

The attach logs `mtyx: applying local config from <path> (overrides N keys)`.
See [Configuration > Local config overlay](configuration.md#local-config-overlay-attach).

## Sessions and sockets

The default socket path is:

```text
$TMPDIR/mtyx-<uid>/<session>.sock
```

The usual default is `$XDG_RUNTIME_DIR/mtyx-<uid>/main.sock` when `XDG_RUNTIME_DIR` is set, then `$TMPDIR/mtyx-<uid>/main.sock`, then `/tmp/mtyx-<uid>/main.sock`. `--session <name>` changes the final file name. `--socket <path>` bypasses the session-derived path. Server-started child processes receive `MTYX_MUX_SOCKET` with the socket path.

## Session persistence

Every session (headless or local TUI) writes a snapshot of its workspace/screen/pane layout — split shape and ratios, names, and each tab's cwd — to `$XDG_STATE_HOME/mattyx/sessions/<session>.json` (falling back to `~/.local/state/...`), debounced a few hundred ms after each structural change and again on clean shutdown. Starting a session with the same `--session` name again (a real daemon restart, or just restarting the local TUI) replays that snapshot: same panes, same directories. Closing every workspace deletes the file rather than leaving a stale one to resurrect later.

Not restored: a tab's *command*. Every restored tab is the default shell, `cd`'d into its recorded directory (visibly, briefly, before a `clear`) — if something specific was running there (a dev server, `claude --resume ...`), you'll need to relaunch it. See `mux-core/src/persist.rs` for why, and `Mux::restore_session`/`Mux::enable_persistence` for the implementation.

## Remote (SSH) workspaces

```bash
cargo run -p mux-tui -- ssh <host>
cargo run -p mux-tui -- ssh <host> --name my-remote-work
```

Opens a workspace whose tab is a shell on `<host>` instead of local, backed by
[`cmuxd-remote`](../../daemon/remote) (vendored from upstream cmux, unmodified) speaking
NDJSON RPC over an SSH-exec'd pipe (not a real allocated local pty — see
`mux-core/src/remote_pty.rs`'s module doc for how `portable_pty`'s traits get
implemented against that RPC channel instead). The first connection to a host
builds and caches a `cmuxd-remote` binary for its OS/arch (needs Go on `PATH`;
cross-compiles via `GOOS`/`GOARCH`, no toolchain needed on the remote), uploads
it, and starts it in **persistent** mode: it forks a detached background daemon
on the remote that outlives both the SSH connection and this local process.

Two consequences:

- **Closing the tab detaches, not kills.** The remote shell keeps running; the
  session survives disconnecting.
- **Restarting this session's daemon reattaches automatically**, the same way
  local tabs' layout restores (see Session Persistence above) — a workspace's
  first tab being remote is recorded in the snapshot (host, slot, session id,
  and the cached binary path) and `Mux::restore_session` calls
  `Mux::new_remote_workspace` with the same session id instead of spawning a
  local shell. This only works for a workspace's very first tab today — a
  second tab in a pane, or any pane a split created, has no such path
  (`new_tab`/`split` only ever spawn local shells) and downgrades to an
  ordinary local tab on restore, with a status message noting it.

There's no verb to actually end a remote session (only detach it) — a stale one
needs manual cleanup on the remote host (`rm -rf ~/.mattyx/daemon ~/.cache/mattyx`
there, or `kill` its `cmuxd-remote serve --persistent-server` process).

## Platforms and XDG

mtyx supports macOS and Linux; Windows support via ConPTY is planned for phase 2. The TUI config path resolves `MTYX_MUX_CONFIG`, then `$XDG_CONFIG_HOME/mattyx/mux.json`, then `~/.config/mattyx/mux.json`.

Launched Chrome profile paths are platform-specific. On macOS the default is `~/Library/Application Support/mtyx/chrome-profile`. On Linux and other non-macOS targets, `XDG_DATA_HOME` is used when set, then `~/.local/share/mattyx/chrome-profile`.

## Development flow

Run tests from `mux/`.

```bash
cargo test
```

Run the smoke scripts against a built binary. Set `MTYX_MUX_BIN` to test a non-default binary.

```bash
cargo build -p mux-tui
python3 scripts/smoke-tui.py
python3 scripts/smoke-attach.py
```

This checkout does not contain `scripts/mux-dev.sh`; use the cargo and smoke commands above for the TUI flow.
