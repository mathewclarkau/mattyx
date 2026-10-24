# Quickstart

**`mtyx`** (`mattyx`) is an agent-aware terminal multiplexer backed by `libghostty-vt`. It runs on Linux, macOS (`aarch64` and `x86_64`), and Windows (ConPTY), and exposes every workspace, pane, and coding agent over a versioned JSON-Lines control socket (protocol v8).

## 1. Install `mtyx`

### Prebuilt Linux binary (`x86_64` / `aarch64`)

```bash
curl -fsSL -o ~/.local/bin/mtyx \
  "https://github.com/mathewclarkau/mattyx/releases/latest/download/mtyx-linux-$(uname -m)"
chmod +x ~/.local/bin/mtyx
```

### Prebuilt macOS binary (`arm64` / `x86_64`)

```bash
curl -fsSL -o ~/.local/bin/mtyx \
  "https://github.com/mathewclarkau/mattyx/releases/latest/download/mtyx-macos-$(uname -m | sed 's/arm64/aarch64/')"
chmod +x ~/.local/bin/mtyx
```

Once installed, upgrade in place at any time with SHA-256 sidecar verification:

```bash
mtyx update           # download, sha256-verify, and atomically swap the binary
mtyx update --check   # check for a newer release without downloading
```

### Arch Linux (AUR)

```bash
paru -S mtyx-bin
```

### Build from source

```bash
git clone --recurse-submodules https://github.com/mathewclarkau/mattyx.git
cd mattyx
./scripts/bootstrap.sh
ln -sf "$(pwd)/mux/target/release/mtyx" ~/.local/bin/mtyx
```

---

## 2. Start your first session

```bash
mtyx                          # start (or attach to) session "main"
mtyx --session agents         # start (or attach to) a named session
mtyx attach --session agents  # attach a second TUI client to the same session
```

Inside the TUI (`Ctrl-b` prefix, or `Alt+` modeless bindings):

- `Ctrl-b t` — new shell tab in the focused pane
- `Ctrl-b c` — new screen in the active workspace
- `Ctrl-b %` / `Ctrl-b "` — split pane right / down
- `Ctrl-b w` / `Ctrl-b W` — next workspace / new workspace
- `Ctrl-b S` — interactive session manager (switch, rename, or kill sessions)
- `Ctrl-b d` — detach (`mtyx` daemon keeps running in the background)

---

## 3. Run an agent fleet from a script (no TUI required)

Start a headless daemon, spawn named agents, send prompts with receipted input (`input-ack`), wait on semantic state transitions (`working` → `blocked` → `done`), and read unwrapped output:

```bash
# 1. Start a headless session
mtyx --headless --session demo &
sleep 1

# 2. Spawn named agents in their own tabs (registers agent name at hook authority)
mtyx --session demo agent start coder    --cwd ~/my-repo -- claude
mtyx --session demo agent start reviewer --cwd ~/my-repo -- codex

# 3. Send prompts (refuses with `agent_blocked` if an approval dialog is up)
mtyx --session demo agent prompt coder "Fix the failing retry test in src/client.rs"

# 4. Block until the agent needs human/orchestrator input (`blocked`) or finishes (`done`)
mtyx --session demo agent wait coder --status blocked --timeout 60000

# 5. Read the recent unwrapped transcript (refuses with `agent_not_idle` if mid-turn)
mtyx --session demo agent read coder --source recent-unwrapped --lines 50

# 6. Watch live in the TUI at any time
mtyx attach --session demo
```

---

## 4. Install agent hooks for lifecycle state

Install hooks across all 7 supported coding agent harnesses (`claude`, `codex`, `antigravity`, `aider`, `pi`, `grok`, `opencode`) so sidebar badges and `agent-state-changed` events reflect exact hook-grade transitions:

```bash
mtyx agents install --all --global
mtyx agents list --global
```

See [Agent Integrations](./agents.html) for per-harness hook paths and ambient heuristic detection.
