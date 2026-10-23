# mattyx

**`mtyx`** — an agent-aware terminal multiplexer for Linux. Run Claude Code,
Codex, Pi, and other coding agents in parallel panes with git-worktree
isolation, then **orchestrate them from scripts**: every pane is addressable
over a versioned JSON-lines Unix socket with receipted input (input-ACK),
blocked-send gating, and post-spawn health checks.

A Linux-native repurposing of [manaflow-ai/cmux](https://github.com/manaflow-ai/cmux),
a terminal environment built for running AI coding agents (Claude Code, Codex, etc.)
in parallel with git-worktree-style workspace isolation.

The upstream `cmux` app is a macOS-only Swift/AppKit application and isn't portable.
This repo instead builds on `mux/`, upstream's own cross-platform Rust + Zig backend
(`cmux` / `mux-tui`), which already reimplements the same session → workspace →
screen → pane → surface model over a JSON-Lines Unix-socket control protocol. See
[`PROVENANCE.md`](./PROVENANCE.md) for exactly what was vendored from where, and
[`UPSTREAM.md`](./UPSTREAM.md) for release↔upstream mapping and the re-sync
procedure.

This is an unofficial, community-maintained port and is not affiliated with,
endorsed by, or supported by Manaflow, Inc. Upstream `cmux` is dual-licensed
(GPL-3.0-or-later or a commercial license from Manaflow); this repo only
carries forward the GPL-3.0-or-later grant — see [`LICENSE`](./LICENSE).

## Agent factory in 60 seconds

One daemon, three verbs, a fleet of agents — no TUI required:

```bash
# 1. install (prebuilt binary, x86_64/aarch64)
curl -fsSL -o ~/.local/bin/mtyx \
  "https://github.com/mathewclarkau/mattyx/releases/latest/download/mtyx-linux-$(uname -m)"
chmod +x ~/.local/bin/mtyx

# 2. start a headless session and create two agent panes
mtyx --headless --session demo &
sleep 1
mtyx new-tab --cwd ~/my-repo --exec -- claude    # any agent CLI works
mtyx new-tab --cwd ~/my-repo --exec -- aider

# 3. drive + read them from a script
mtyx send --surface 1 --text "fix the failing test" --send-cr 1
mtyx agent-read --target 1 --source recent-unwrapped
mtyx wait-agent-status --target 1 --status blocked --require-transition --timeout 60000   # blocks until it needs you
mtyx read-screen --surface 1
```

Panes report semantic state (`working` / `blocked` / `done`) through agent
hooks (`mtyx agents install --all --global`); everything above is also exposed
on the socket — see [`mux/docs/protocol.md`](./mux/docs/protocol.md) for the
versioned wire contract every verb obeys. Attach the TUI any time with
`mtyx attach` to watch the fleet live.

### Packaged Agent Skill & Dogfood Demo

Give any coding agent (`claude`, `codex`, `pi`, `antigravity`, `opencode`, `grok`, etc.) autonomous `mtyx` orchestration recipes (`agent start/prompt/wait/read/list`, `subscribe`, `detect-agents`, `pane worktree`, `gc`) in one command:

```bash
npx skills add mathewclarkau/mattyx --skill mtyx
```

See [`SKILL.md`](./SKILL.md) ([`skills/mtyx/SKILL.md`](./skills/mtyx/SKILL.md)) for the full scenario-driven recipes, or run the self-contained dogfood factory demo (spawns 3 named worker panes in an isolated runtime dir, waits on `blocked` approval, answers, verifies `done`, and cleans up with zero leaked processes):

```bash
./scripts/demo-factory.sh
```

## Installation

### 1. Install script (Linux & macOS, `x86_64` / `aarch64`)

Downloads the release asset for your OS/arch, verifies its `.sha256` sidecar before installation, and installs to `~/.local/bin/mtyx` (no `sudo`):

```bash
curl -fsSL https://raw.githubusercontent.com/mathewclarkau/mattyx/main/scripts/install.sh | bash
```

### 2. Homebrew (macOS & Linux)

```bash
brew tap mathewclarkau/mattyx https://github.com/mathewclarkau/mattyx
brew install mattyx
```

*(Or install directly from the release formula asset: `brew install https://github.com/mathewclarkau/mattyx/releases/latest/download/mattyx.rb`)*

### 3. Nix flake (`x86_64-linux`, `aarch64-linux`, `aarch64-darwin`, `x86_64-darwin`)

```bash
nix run github:mathewclarkau/mattyx?submodules=1#mtyx
# or install into your profile:
nix profile install github:mathewclarkau/mattyx?submodules=1#mtyx
```

### Direct binary download & in-place updates

Release assets are published for **5 platforms** (`mtyx-linux-x86_64`, `mtyx-linux-aarch64`, `mtyx-macos-aarch64`, `mtyx-macos-x86_64`, `mtyx-windows-x86_64.exe`), each accompanied by a `<asset>.sha256` sidecar:

```bash
curl -fsSL -o ~/.local/bin/mtyx \
  "https://github.com/mathewclarkau/mattyx/releases/latest/download/mtyx-linux-$(uname -m)"
chmod +x ~/.local/bin/mtyx
```

Built by [`.github/workflows/release.yml`](./.github/workflows/release.yml) from a tagged commit, the same way
`bootstrap.sh` builds it locally.

Once installed, upgrade in place at any time (downloads the matching release asset, verifies its `.sha256` sidecar, and atomically swaps the binary):

```bash
mtyx update           # download, sha256-verify, and install the latest release
mtyx update --check   # check for a newer release without downloading
```

### Arch Linux (AUR) & Cargo Binstall

```bash
paru -S mtyx-bin        # or: yay -S mtyx-bin
cargo binstall mux-tui  # via cargo-binstall metadata
```


### Build from source

#### Prerequisites

- **Rust** (stable, via [rustup](https://rustup.rs) or your distro) — builds `mux-core`/`mux-tui`.
- **`clang`/libclang** — `ghostty-vt-sys` uses `bindgen` to generate FFI bindings against libclang at build time.
  Debian/Ubuntu: `apt install clang libclang-dev`. Fedora: `dnf install clang clang-devel`. Arch: `pacman -S clang`.
- **`git`, `curl`, `tar`** — standard on most systems; `bootstrap.sh` uses them to fetch the pinned zig toolchain.
- Zig is **not** a prerequisite — `scripts/bootstrap.sh` downloads the exact pinned version (0.15.2) into
  `.tools/` itself, without touching any system zig install. The pin isn't paranoia: zig is pre-1.0 and zig
  0.16 breaks this build with at least two unrelated stdlib signature changes (`Dir.readFileAlloc`'s new
  `Io`-threaded signature, `std.process.EnvMap` moving) within the first few seconds of the build graph —
  confirmed by patching around both and hitting more.
- **Go** is only needed for [remote/SSH workspaces](./mux/docs/getting-started.md#remote-ssh-workspaces) — `mtyx
  ssh <host>` shells out to `go build` on first connection to a given host, to cross-compile the vendored
  `mtyxd-remote` daemon for that host's OS/arch. Everything else builds and runs without Go installed.

#### Clone, build, and put it on your `PATH`

```bash
git clone --recurse-submodules https://github.com/mathewclarkau/mattyx.git
cd mattyx
./scripts/bootstrap.sh                                          # fetches zig, builds mux-tui in release mode
ln -sf "$(pwd)/mux/target/release/mtyx" ~/.local/bin/mtyx  # requires ~/.local/bin on PATH
```

Forgot `--recurse-submodules`? `bootstrap.sh` initializes the `ghostty` submodule itself if it's missing, so a
plain `git clone` followed by `./scripts/bootstrap.sh` also works. It also applies this repo's patches to the
submodule (see [`patches/README.md`](./patches/README.md) — the submodule pointer stays on the real upstream
commit; local changes are layered on top so a fresh clone can always fetch it), and is idempotent to re-run
(skips the zig download and does an incremental `cargo build` if nothing changed).

### Run it

```bash
mtyx                          # start (or attach to) session "main"
mtyx --session agents         # start (or attach to) a differently-named session
mtyx attach --session agents  # attach a second TUI to an already-running session
# Ctrl-b d detaches; the session daemon keeps running. mtyx kill-session ends it.
```

See [`mux/docs/getting-started.md`](./mux/docs/getting-started.md) for headless mode, socket paths, and session
persistence/restore semantics.

## Status

The vendored `mux-tui` builds and runs as-is on Linux (verified on this machine).
What's missing before this feels like `mtyx` rather than a bare multiplexer:

1. ~~Git branch / cwd info in the sidebar~~ — done. Every pane tracks a cwd (live
   OSC 7 report when the shell sends one, else the directory it was spawned in —
   see `Surface::cwd()` in `mux-core`), and the sidebar shows the git branch for
   it (`crates/mux-tui/src/git_info.rs`). PR status is not included — it needs
   `gh`/GitHub API access and felt like a separate, heavier addition.
2. ~~Claude Code hook layer (session tracking, restore)~~ — done. `mtyx claude
   install-hooks` wires `~/.claude/settings.json` to call `mtyx claude hook` on
   every lifecycle event (merged alongside any hooks already there, safely
   idempotent). It reports agent state over `report-agent`/`list-agents`
   (`crates/mux-tui/src/claude_hook.rs`) and records sessions to
   `$XDG_STATE_HOME/mattyx/claude-sessions.json` for `mtyx claude sessions`
   / `mtyx claude resume <session-id>`.
3. ~~Agent-state notifications (OSC 9/99/777 → desktop notification)~~ — done.
   Every pane's raw output is watched for an OSC 9, OSC 777, or kitty-protocol
   desktop notification (`crates/mux-core/src/notify.rs`); each one sets
   `agent_state: blocked` (lowest-authority "detected" source — a hook or
   `report-agent` call still wins) and forwards to the desktop via
   `notify-send`. Needed extending libghostty-vt's C API by two data-extraction
   values — see `patches/` — since it could already *detect* this OSC command
   but not extract its title/body text.
4. ~~Session persistence across daemon restarts~~ — done. Every session
   (headless or local TUI) debounce-writes a snapshot of its workspace/screen/
   pane layout — split shape+ratios, names, each tab's cwd, active selections —
   to `$XDG_STATE_HOME/mattyx/sessions/<session>.json`
   (`crates/mux-core/src/persist.rs`), and replays it on next start with the
   same `--session` name. Closing every workspace deletes the file instead of
   leaving something to resurrect later. Deliberately not restored: a tab's
   *command* — every restored tab is the default shell, `cd`'d into its
   recorded directory; something you had running there needs relaunching.
   Verified via a real kill-and-restart of the compiled binary, not just
   library tests — see `mux/docs/getting-started.md`'s "Session persistence".
5. ~~Remote/SSH workspaces~~ — done. `mtyx ssh <host>` opens a workspace
   backed by the Go `mtyxd-remote` daemon (vendored in `daemon/remote/`,
   already cross-compiles for `linux/{amd64,arm64}`) instead of a local shell.
   `mux-core/src/remote_pty.rs` implements `portable_pty`'s
   `MasterPty`/`SlavePty`/`Child` traits against an SSH-exec'd NDJSON RPC pipe —
   no real local pty involved. The first connection to a host builds/uploads/
   starts `mtyxd-remote` in persistent mode, so it survives both the SSH
   connection and the local `mtyx` process dying; closing the tab detaches
   rather than kills, and restarting the session daemon reattaches
   automatically for a workspace's first tab (same mechanism as #4). Verified
   against a real sshd (localhost), including a kill-and-restart of the
   compiled binary that reattached to the still-running remote shell — see
   `mux/docs/getting-started.md`'s "Remote (SSH) workspaces".

### Known environment quirk (not a mtyx bug)

Every new pane spawns your login shell (`$SHELL`) fresh. If you use zsh with
Powerlevel10k and haven't completed its setup wizard yet (no `~/.p10k.zsh`),
that wizard launches in every new pane and blocks on an interactive prompt.
Run `p10k configure` once in a normal terminal (outside mtyx) to fix it
for good.

## Usage

### Desktop notifications

No setup needed: any program in any pane gets a real desktop notification (via
`notify-send`) and a red sidebar dot just by writing an OSC 9 sequence, e.g.
`printf '\033]9;Build failed\007'`. OSC 777 and the kitty notification protocol
(both can include a title) work the same way. This also feeds the Claude Code
hook layer's status dot: a `report-agent` call (from a hook or the socket) always
takes priority over this passive detection.

### LLM Harness Integrations

`mattyx` provides first-class integrations to automatically report agent status (e.g., active, idle, done) for display on sidebar tabs.

#### 1. Claude Code
```bash
mtyx claude install-hooks        # wire up ~/.claude/settings.json
mtyx claude install-hooks --uninstall
mtyx claude sessions             # recorded sessions: id, cwd, last event
mtyx claude resume <session-id>  # new pane in the recorded cwd, runs claude --resume
```
Once installed, panes running Claude Code show status dots next to the git branch (amber while working, red when blocked, green when done).

#### 2. Antigravity CLI (`agy`)
```bash
mtyx antigravity install-hooks            # installs workspace-level hooks in .agents/hooks.json
mtyx antigravity install-hooks --global   # installs global hooks in ~/.gemini/config/hooks.json
mtyx antigravity install-hooks --uninstall
```
Triggers state updates automatically during tool execution phases (`PreToolUse`, `PostToolUse`, `Stop`).

#### 3. Codex CLI
```bash
mtyx codex install-hooks            # installs hooks in .codex/hooks.json and enables in config.toml
mtyx codex install-hooks --global   # installs hooks in ~/.codex/hooks.json and enables globally
mtyx codex install-hooks --uninstall
```

#### 4. Pi Coding Agent (`pi`)
```bash
mtyx pi install-hooks            # installs TypeScript extensions into .pi/extensions/mtyx.ts
mtyx pi install-hooks --global   # installs extensions globally in ~/.pi/agent/extensions/mtyx.ts
mtyx pi install-hooks --uninstall
```

#### 5. Aider
```bash
mtyx aider install-hooks            # creates a wrapper executable at .bin/aider
mtyx aider install-hooks --global   # creates a wrapper globally at ~/.local/bin/aider
mtyx aider install-hooks --uninstall
```
*Note: For the local wrapper, ensure `.bin/` is prepended to your `$PATH` or call `./.bin/aider` directly.*

#### 6. Grok CLI
```bash
mtyx grok install-hooks            # workspace hooks in .grok/hooks/mtyx-agent-state.json
mtyx grok install-hooks --global   # global hooks in ~/.grok/hooks/mtyx-agent-state.json
mtyx grok install-hooks --uninstall
```

## Documentation

The full multiplexer docs live in [`mux/docs/`](./mux/docs/) — still accurate here since `mux/` was vendored
verbatim and this fork's additions (hooks, notifications, persistence, remote workspaces) are documented inline
alongside the rest:

| Doc | Covers |
| --- | --- |
| [`getting-started.md`](./mux/docs/getting-started.md) | Build prerequisites, local/headless runs, sockets, detach/attach, session persistence, remote (SSH) workspaces |
| [`concepts.md`](./mux/docs/concepts.md) | Session tree, focus, collapse behavior, tab naming, smart split, PTY and browser surfaces |
| [`keyboard.md`](./mux/docs/keyboard.md) | Prefix model, modeless Alt layer, default bindings, `mux.json` key remapping |
| [`mouse.md`](./mux/docs/mouse.md) | Clickable UI, drag reorder, resize, scrollbars, menus, selection, pointer shape |
| [`configuration.md`](./mux/docs/configuration.md) | Full `mux.json` reference with defaults and a worked example |
| [`protocol.md`](./mux/docs/protocol.md) | Control-socket JSON-lines framing, attach streams, events, agent state, remote workspaces |
| [`browser-panes.md`](./mux/docs/browser-panes.md) | CDP-backed browser tabs, rendering, input, profiles, current limitations |
| [`1.0-gate.md`](./mux/docs/1.0-gate.md) | **1.0 Stability Contract:** semver surface, 0.x→1.x migration guarantees, upgrade/rollback, no-leak promise, Windows graduation criteria, and security posture |

For the formal wire-protocol contract (exact request/response schemas, error tables, CLI mappings — the source
generated clients would target), see [`mux/spec/`](./mux/spec/), starting with
[`mux/spec/README.md`](./mux/spec/README.md).

For what's vendored from upstream versus new to this fork, and under what license, see
[`PROVENANCE.md`](./PROVENANCE.md).
