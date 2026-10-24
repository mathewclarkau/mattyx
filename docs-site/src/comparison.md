# Honest Comparison: `mtyx` vs `tmux` vs `zellij` vs `herdr` vs `cmux`

Every multiplexer makes trade-offs. This page compares **`mtyx`** (`mattyx`) against **`tmux`**, **`zellij`**, **`herdr`**, and upstream **`cmux`** — including the places where `mtyx` has real gaps today and another tool is a better fit.

## Feature Matrix

| Capability | `mtyx` (`mattyx`) | `tmux` | `zellij` | `herdr` | Upstream `cmux` |
| --- | --- | --- | --- | --- | --- |
| **Primary UI** | Terminal TUI + headless daemon | Terminal TUI + daemon | Terminal TUI + daemon | Terminal TUI | Native macOS Swift/AppKit GUI |
| **OS Support** | Linux, macOS (`arm64` + `x86_64`), Windows (ConPTY) | Linux, macOS, BSD (WSL on Windows) | Linux, macOS, Windows | macOS, Linux | macOS only |
| **Terminal Emulation Engine** | `libghostty-vt` (Zig, SIMD-accelerated) | Built-in C VT parser | `alacritty_terminal` / built-in | Built-in VT | `libghostty` (native Metal/AppKit) |
| **First-Class Agent Verbs** (`agent start/prompt/wait/read/list`) | **Yes** — built into CLI & JSON-Lines socket | No (raw `send-keys` / `capture-pane`) | No (raw `write-chars` / `dump-screen`) | Partial (workspace/pane + state wait) | Partial (macOS GUI + socket) |
| **Receipted Input (`input-ack`)** | **Yes** (default on `mtyx send`; waits for PTY echo/advance) | No (`send-keys` is fire-and-forget) | No (`write-chars` is fire-and-forget) | No | No |
| **Blocked-Send & Busy-Read Guards** | **Yes** (`agent_blocked` refuses writes into approval modals; `agent_not_idle` refuses mid-turn reads) | No | No | No | No |
| **Agent Lifecycle State (`working`/`blocked`/`done`)** | **Yes** — 7 hook integrations + OSC 9/99/777 + process/screen detection | No | No (requires custom WASM plugin) | **Yes** — zero-config process + output heuristics across many agents | **Yes** (Claude Code hooks + OSC notifications) |
| **Event Stream API (`subscribe` + durable `notify-ack`)** | **Yes** — JSON Lines over Unix socket with per-client high-water mark replay | Control mode (`tmux -C`), untyped text | Plugin event bus (WASM) | No public versioned wire protocol | Local socket |
| **Per-Pane Git Worktree Verbs (`pane worktree`)** | **Yes** (`create`, `list`, `remove`) | No (manual git commands) | No | Yes | Yes |
| **Persistent Remote SSH Workspaces** | **Yes** (`mtyx ssh <host>` with auto-uploaded `mtyxd-remote` Go daemon + `--machine` fleet control) | Manual (`ssh` + remote `tmux`) | Manual (`ssh` + remote `zellij`) | No | Yes (`cmuxd-remote`) |
| **In-Pane CDP Browser Tabs** | **Yes** (Kitty graphics protocol over headless Chrome/CDP) | No | No | No | Native WebKit (`WKWebView`) |
| **Self-Update with SHA-256 Verification** | **Yes** (`mtyx update`) | Via system package manager | Via system package manager | Yes | Sparkle (macOS GUI) |
| **Plugin Ecosystem & Maturity** | **Gap:** Manifest registry only; WASM execution is not wired up yet | **Mature:** TPM + decades of shell scripts | **Mature:** Full WASM plugin system + floating panes | Minimal | None |

---

## Where `mtyx` is stronger

1. **Programmatic multi-agent orchestration (the "agent factory" loop):**
   In `tmux`, driving an AI coding agent from a supervisor script means `tmux send-keys` (fire-and-forget) followed by `sleep` loops and regex scraping `tmux capture-pane`. If the agent pops a permission modal right before your script sends a prompt, `send-keys` types blindly into the modal.
   In `mtyx`:
   - `mtyx send` is **receipted by default** (`input-ack` in protocol v8).
   - `mtyx agent prompt <name> "..."` refuses with `agent_blocked` if the agent is waiting on an approval dialog.
   - `mtyx agent read <name> --source recent-unwrapped` refuses with `agent_not_idle` while the agent is mid-turn, and rejoins soft-wrapped terminal lines cleanly.
   - `mtyx agent wait <name> --status blocked --require-transition` blocks on the exact state sequence transition without polling.

2. **Headless-first & TUI-optional:**
   Every session daemon (`mtyx --headless --session <name>`) speaks the exact same protocol v8 JSON-Lines contract whether a TUI is attached or not. CI pipelines and background supervisor agents can run an entire fleet headlessly while a human attaches (`mtyx attach`) and detaches (`Ctrl-b d`) on demand.

3. **Persistent remote SSH workspaces without remote root/packages:**
   `mtyx ssh user@host` cross-compiles and caches a tiny single-file `mtyxd-remote` binary locally, uploads it to the remote user's home directory, and streams the remote PTY over an NDJSON SSH pipe. If your laptop sleeps or SSH drops, re-opening `mtyx` reattaches to the still-running remote shell and replays scrollback.

---

## Honest Gaps — Where other tools beat `mtyx` today

We mark our gaps explicitly so you can pick the right tool:

- **Ubiquity & battle-tested stability (`tmux` wins):**
  `tmux` is pre-installed or one `apt install` away on virtually every POSIX server in the world, has 15+ years of production hardening, and uses minimal memory. If you just want a classic terminal multiplexer on a bastion host and don't orchestrate coding agents over a JSON socket, use `tmux`.
- **Floating panes, stacked layouts, and live WASM plugins (`zellij` wins):**
  `mtyx` uses a hierarchical binary split tree (`workspace → screen → pane → tab`). It does **not** have floating panes or stacked pane modes. Furthermore, while `mtyx` links `wasmtime` and ships `mtyx plugin list/install/enable`, **plugin execution (`mtyx <plugin> <verb>`) is not wired up yet** — `zellij`'s WASM plugin ecosystem is mature today.
- **Native macOS GUI polish & native WebKit (`cmux` wins on macOS desktop):**
  Upstream `cmux` is a native macOS Swift/AppKit app with real `WKWebView` browser panes. `mtyx` runs inside your terminal emulator as a TUI, and its in-pane browser tabs stream screenshots over Chrome DevTools Protocol (CDP) using the Kitty graphics protocol — which requires a Kitty-graphics-capable host terminal (Ghostty, Kitty, WezTerm) and cannot match a native GPU browser view for 60fps video or complex DevTools inspection.
- **Zero-config out-of-the-box agent breadth (`herdr` vs `mtyx` hooks):**
  `mtyx`'s highest-fidelity state (`working` / `blocked` / `done` with exact transition sequence numbers) comes from installing per-agent hooks (`mtyx agents install --all --global`), covering 7 harnesses (`claude`, `codex`, `antigravity`, `aider`, `pi`, `grok`, `opencode`), backed by `detect-agents` process/screen pattern matching and OSC 9/777 detection.
