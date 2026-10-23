# Changelog & Release History

**Release Versioning Law (Issue #71):** In `mattyx`, the pushed `v*` git tag is the single source of truth for release versions. `mux-core/build.rs` bakes `mux_core::VERSION` into the binary at compile time from `$MTYX_VERSION` / `git describe --tags`, and `.github/workflows/release.yml` verifies that the compiled binary's `mtyx --version` matches the tag before publishing.

Full release assets (`linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `macos-x86_64`, `windows-x86_64.exe` + `.sha256` sidecars) and auto-generated release notes are published at:

- **[GitHub Releases — `mathewclarkau/mattyx/releases`](https://github.com/mathewclarkau/mattyx/releases)**

---

## Recent Highlights (0.17.x – 0.27.x → 1.0)

### Multi-Agent Factory & Control Protocol v7
- **First-class `mtyx agent` noun (`start`, `prompt`, `wait`, `read`, `list`):** Spawn named agents (`agent-register` at `hook` authority), send prompts with blocked-send gating (`agent-prompt`), block on state transitions (`wait-agent-status --require-transition`), and read unwrapped output (`agent-read --source recent-unwrapped`) guarded against mid-turn reads (`agent_not_idle`).
- **Receipted input (`input-ack`) by default (`mtyx send`):** `send` blocks until the daemon observes PTY output consumption or process exit (`capabilities["input-ack"] == true` in `identify`).
- **Durable notifications & `notify-ack` (Issue #92):** Per-client high-water mark tracking on `subscribe --client <id>` replays unread notifications before live events.
- **`wait-ready` (Issue #85):** Read-only readiness probe verifying both a prompt on screen and a live PTY child process.

### Agent Integrations & Detection
- **7 Harness Hook Installers (`mtyx agents install --all`):** First-class lifecycle hooks for Claude Code, Codex CLI, Antigravity (`agy`), Pi (`pi`), OpenCode, Grok CLI, and Aider.
- **Ambient Agent Detection (`detect-agent` / `detect-agents` / `agent-pattern`):** Process-tree (`/proc` `comm`/`cmdline`) and screen-pattern matching with runtime pattern registration.

### Session Hygiene, Self-Update & Cross-Platform Parity
- **`mtyx update` (Issues #132, #133):** In-place binary self-update with pure-Rust FIPS 180-4 SHA-256 verification against `<asset>.sha256` sidecars, staged `--version` check, and symlink refusal.
- **Three-point Stale Session GC (Issues #136, #137, #138):** Double-signal liveness check (`.pid` + socket probe with pre-unlink re-verification) running at daemon startup, via `mtyx kill-stale` (`mtyx gc`), and lazily on `mtyx list-sessions` / session picker enumeration.
- **Layout Export & Apply (Issue #76) + Per-Pane Worktrees (Issue #77):** Save and replay workspace topologies (`layout-export` / `layout-apply`) and isolate panes in git worktrees (`pane worktree create/list/remove`).
- **Multi-Machine SSH Fleet (Issue #94):** Register SSH hosts with `mtyx machine add <label> <user@host>` and run any control verb remotely via `mtyx --machine <label> <verb>`.
