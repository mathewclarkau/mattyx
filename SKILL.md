---
name: mtyx
description: Adopt and orchestrate mtyx (mattyx) — spawn coding agents in isolated panes/worktrees, drive them with receipted prompts, wait on working/blocked/done state transitions, subscribe to live events, and clean up with zero leaked processes.
argument-hint: <describe the multi-agent workflow or layout you want to run>
---

# `mtyx` — Multi-Agent Factory & Terminal Multiplexer Skill

Use this skill whenever you need to run, coordinate, or supervise one or more coding agents (`claude`, `codex`, `pi`, `aider`, `grok`, `opencode`, `antigravity`, or custom harnesses) inside **`mtyx`**.

Unlike raw `tmux` or screen-scraping wrappers, `mtyx` gives every pane a structured lifecycle state (`working` · `blocked` · `idle` · `done`), **receipted input** (input-ACK by default), **busy/blocked guards** so you never type into an approval dialog or read a half-streamed turn by accident, and **named agent addressing** so you address `"reviewer"` instead of tracking pane indices.

Install this skill into any coding agent via the skills CLI:

```bash
npx skills add mathewclarkau/mattyx --skill mtyx
# or use mtyx's built-in installer for your harness:
mtyx agents install --all --global
```

---

## Scenario 1: Stand up a clean headless session (or attach to your current one)

Check whether you are already running inside an `mtyx` pane:

```bash
env | grep '^MTYX_MUX_'
```

- **If `MTYX_MUX_SOCKET` is set:** every `mtyx <verb>` command talks to your current session automatically.
- **If you are running outside `mtyx` (CI, headless orchestrator, or fresh terminal):** start a dedicated headless daemon and wait until its socket is live:

```bash
SESSION="factory-$$"
mtyx --headless --session "$SESSION" &
DAEMON_PID=$!

# Wait for the initial surface (surface 1) to be ready
for _ in $(seq 1 30); do
  if mtyx --session "$SESSION" identify >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
mtyx --session "$SESSION" wait-ready --surface 1 --timeout 5000
```

At any point a human can watch the headless fleet live with:

```bash
mtyx attach --session "$SESSION"
```

---

## Scenario 2: Spawn 3 named agents, wait for the blocked one, inspect, decide, and continue

Use the **`mtyx agent <start|prompt|wait|read|list>`** verb family. `agent start` spawns the process in a new tab and registers its unique name at `hook` authority in a single step, so every subsequent command addresses the agent by name (`coder`, `tester`, `reviewer`).

```bash
# 1. Start three named workers in their own panes/tabs
mtyx --session "$SESSION" agent start coder    --cwd "$PWD" -- claude
mtyx --session "$SESSION" agent start tester   --cwd "$PWD" -- codex
mtyx --session "$SESSION" agent start reviewer --cwd "$PWD" -- pi

# 2. List all registered agents and their current state + surface IDs
mtyx --session "$SESSION" agent list
# (or machine-readable JSON: mtyx --session "$SESSION" --json agent list)

# 3. Dispatch tasks using `agent prompt` (sends text + CR; refuses with
#    `agent_blocked` if an approval dialog is up unless `--force` is passed)
mtyx --session "$SESSION" agent prompt coder    "Implement the retry backoff in src/client.rs"
mtyx --session "$SESSION" agent prompt tester   "Write unit tests for retry backoff edge cases"
mtyx --session "$SESSION" agent prompt reviewer "Review src/client.rs for cancellation safety"

# 4. Block until `coder` transitions to `blocked` (e.g. waiting for tool approval
#    or asking a clarifying question) — defaults to `--status blocked --timeout 60000`
mtyx --session "$SESSION" agent wait coder --status blocked --require-transition --timeout 120000

# 5. Read the tail of `coder`'s recent unwrapped output (soft wraps rejoined)
mtyx --session "$SESSION" agent read coder --source recent-unwrapped --lines 50

# 6. Approve or answer the prompt:
#    - Use `answer` for single-key dialog choices (y/n/enter/down/up/esc/1..9)
#    - Or `agent prompt --force` to reply with a line of text while blocked
mtyx --session "$SESSION" answer --surface 2 --key enter
# or:
mtyx --session "$SESSION" agent prompt coder --force "Yes, keep the cap at 30 seconds"

# 7. Wait until `coder` finishes its turn (`done` or `idle`) before reading the final result
mtyx --session "$SESSION" agent wait coder --status done --timeout 180000
mtyx --session "$SESSION" agent read coder --source recent-unwrapped --lines 80
```

### Why `agent read` and `agent prompt` are safer than raw `send` / `read-screen`
- **Busy guard on `agent read`:** If `coder` is currently `working` (mid-turn), `mtyx agent read coder` refuses with `agent_not_idle` so your orchestrator never parses a half-written code block. Pass `--force` only when you intentionally want a mid-stream peek.
- **Blocked guard on `agent prompt` / `send`:** If an agent is `blocked` on a confirmation modal, `agent prompt` and `send` refuse with `agent_blocked` rather than blindly stuffing keystrokes into the modal. Pass `--force` (or use `mtyx answer`) when intentionally responding to the modal.
- **Receipted `send` by default:** `mtyx send --surface <id> --text "cargo test\n" --shell auto` waits for the daemon to observe PTY consumption (`input-ack`) before exiting `0`.

---

## Scenario 3: Event-driven orchestration with `subscribe` and `report-agent`

Instead of polling in a sleep loop, stream live JSON-Lines events from the session socket with `mtyx subscribe`:

```bash
# Stream live events (agent-state-changed, surface-exited, notification, osc-notification)
mtyx --session "$SESSION" subscribe --client orchestrator | while IFS= read -r line; do
  case "$line" in
    *'"event":"agent-state-changed"'*'"state":"blocked"'*)
      echo "An agent needs attention: $line"
      ;;
    *'"event":"agent-state-changed"'*'"state":"done"'*)
      echo "Agent completed work: $line"
      # Acknowledge durable notifications for this client high-water mark:
      mtyx --session "$SESSION" notify-ack --client orchestrator
      break
      ;;
  esac
done
```

Inside a custom worker script or wrapper running *inside* a pane (`$MTYX_MUX_SURFACE` is exported automatically), report your own state transitions with `report-agent`:

```bash
mtyx report-agent --state working --agent my-worker --message "running cargo test"
cargo test
mtyx report-agent --state done    --agent my-worker --message "all tests green"
```

---

## Scenario 4: Zero-config ambient detection (`detect-agents`) & hook installation

### Ambient detection (no hooks required)
When inspecting panes started manually by a user, discover what agent is running in each surface via process-tree + screen-pattern heuristics:

```bash
mtyx --session "$SESSION" detect-agents --json
# -> {"agents":{"1":"claude","2":"codex","3":"unknown"}}

# Inspect evidence + confidence for one surface:
mtyx --session "$SESSION" detect-agent --surface 1

# Add a custom process or screen marker at runtime:
mtyx --session "$SESSION" agent-pattern add my-bot --pattern "my-bot-cli" --kind process --confidence high
```

### Upgrading to hook-grade state (`mtyx agents`)
Hooks report exact lifecycle transitions (`UserPromptSubmit` → `working`, `PermissionRequest` → `blocked`, `Stop` → `done`) at `hook` authority (which always takes precedence over `socket` and `detected` heuristics):

```bash
# Check which of the 7 built-in integrations (claude, codex, antigravity,
# aider, pi, grok, opencode) are installed:
mtyx agents list --global

# Install hooks for all 7 harnesses at once (idempotent, preserves existing user hooks):
mtyx agents install --all --global
```

---

## Scenario 5: Worktree isolation per pane (`pane worktree`)

When running parallel agents that edit files concurrently, isolate each pane in its own git worktree so agents never clobber each other's working tree:

```bash
# Create and attach a dedicated git worktree for pane 2 on branch `fix/racer-1`
mtyx --session "$SESSION" pane worktree create --pane 2 --branch fix/racer-1

# List active pane worktrees
mtyx --session "$SESSION" pane worktree list --pane 2

# Clean up the worktree when done
mtyx --session "$SESSION" pane worktree remove --pane 2 --branch fix/racer-1
```

---

## Scenario 6: Session hygiene (`list-sessions`, `kill-stale` / `gc`) and `update`

Always leave the host with **zero leaked daemons or stale sockets**:

```bash
# List sessions (stale ones are reported, not removed):
mtyx list-sessions --json

# Reap stale sockets/pids across the runtime dirs:
mtyx gc            # or: mtyx kill-stale --json  → {"ok":true,"cleaned":N}

# Tear down your headless factory session cleanly when finished
mtyx kill-session --session "$SESSION"

# Check for or apply a sha256-verified self-update of the mtyx binary
mtyx update --check
```

---

## Quick Verb Reference

| Goal | Command |
| :--- | :--- |
| Start headless session | `mtyx --headless --session <name> &` |
| Wait for shell prompt ready | `mtyx wait-ready --surface <id> --timeout 5000` |
| Spawn + register named agent | `mtyx agent start <name> --cwd <dir> -- <argv...>` |
| Send prompt + Enter (blocked-gated) | `mtyx agent prompt <name> "<prompt>"` |
| Wait on state transition | `mtyx agent wait <name> --status <blocked\|done\|idle\|working> --timeout <ms>` |
| Read agent output (busy-gated) | `mtyx agent read <name> --source recent-unwrapped --lines 60` |
| Answer modal key (`enter`/`y`/`esc`) | `mtyx answer --surface <id> --key <key>` |
| List all agents & states | `mtyx agent list [--json]` |
| Ambient agent detection | `mtyx detect-agents [--json]` |
| Stream live events | `mtyx subscribe [--client <id>]` |
| Export / replay workspace layout | `mtyx layout-export --workspace <w> --output layout.json` / `mtyx layout-apply --input layout.json --workspace <w>` |
| Reap stale sessions / kill session | `mtyx gc` / `mtyx kill-session --session <name>` |
