# Agent Integrations & State Detection

`mtyx` tracks a semantic lifecycle state (`working`, `blocked`, `idle`, `done`, `unknown`) for every PTY surface. State comes from three sources ordered by strict authority:

```text
detected (lowest)  <  socket  <  hook (highest)
```

1. **`hook` (highest authority):** Installed harness hooks call `mtyx report-agent --source hook` (or `mtyx claude hook`) on real tool/turn lifecycle events (`UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `Stop`). Hook state can never be overwritten by a lower-authority `socket` or `detected` report.
2. **`socket` (medium authority):** Scripts or custom wrappers call `mtyx report-agent --state <state>` over the session control socket.
3. **`detected` (lowest authority / zero-config fallback):** Built-in process-tree inspection (`/proc` `comm`/`cmdline`), terminal screen-marker matching (`detect-agent` / `detect-agents`), and OSC 9 / 99 / 777 desktop notification parsing.

---

## One-Command Hook Management (`mtyx agents`)

Manage all 7 built-in coding agent integrations at once:

```bash
# Check installation status, version, timestamp, and config path for all 7 harnesses
mtyx agents list
mtyx agents list --global

# Install hooks for every registered harness (idempotent; preserves existing user hooks)
mtyx agents install --all --global

# Install or uninstall a single harness
mtyx agents install --only claude --global
mtyx agents install --only codex --uninstall --global
```

### Supported Harnesses & Hook Locations

| Agent Harness | CLI Subcommand | Workspace Path | Global Path (`--global`) | Mechanism |
| --- | --- | --- | --- | --- |
| **Claude Code** | `mtyx claude install-hooks` | `~/.claude/settings.json` | `~/.claude/settings.json` | JSON lifecycle hooks (`UserPromptSubmit`, `PreToolUse`, `Notification`, `Stop`) + session resume index |
| **Codex CLI** | `mtyx codex install-hooks` | `.codex/hooks.json` + `.codex/config.toml` | `~/.codex/hooks.json` + `~/.codex/config.toml` | JSON hooks + `codex_hooks = true` feature toggle |
| **Antigravity (`agy`)** | `mtyx antigravity install-hooks` | `.agents/hooks.json` | `~/.gemini/config/hooks.json` | JSON hooks (`PreToolUse`, `PostToolUse`, `Notification`, `Stop`) |
| **Pi (`pi`)** | `mtyx pi install-hooks` | `.pi/extensions/mtyx.ts` | `~/.pi/agent/extensions/mtyx.ts` | TypeScript event extension (`execFile` argv-safe) |
| **OpenCode** | `mtyx opencode install-hooks` | `.opencode/plugin/mtyx.ts` | `~/.config/opencode/plugin/mtyx.ts` | TypeScript plugin reporting turn/tool/permission states |
| **Grok CLI** | `mtyx grok install-hooks` | `.grok/hooks/mtyx-agent-state.json` | `~/.grok/hooks/mtyx-agent-state.json` | Grok Build hook schema |
| **Aider** | `mtyx aider install-hooks` | `.bin/aider` | `~/.local/bin/aider` | Exec wrapper reporting `working` on start and `done` on exit |

### Per-Agent State Mapping

| State | Sidebar Badge / Dot | Meaning | Gate Behavior (`send` / `agent prompt` / `agent read`) |
| --- | --- | --- | --- |
| `working` | `[work]` (Amber) | Agent is actively streaming a turn or running a tool | `agent-read` refuses with `agent_not_idle` unless `--force` |
| `blocked` | `[WAIT]` (Red) | Agent is waiting on an approval prompt or confirmation modal | `send` and `agent-prompt` refuse with `agent_blocked` unless `--force` |
| `done` | `[done]` (Green) | Agent completed its turn and returned to its input prompt | Reads and sends allowed |
| `idle` | `[idle]` (Dim/Gray) | Agent is idle at prompt | Reads and sends allowed |
| `unknown` | — | Registered or detected, no transition recorded yet | Reads and sends allowed |

---

## Zero-Config Ambient Detection (`detect-agent` / `detect-agents`)

Even before hooks are installed, `mtyx` can identify which coding agent is running in each pane by inspecting the PTY's child process tree and matching visible screen markers against its pattern registry:

```bash
# Detect the agent on a single surface (prints: <surface> <agent> <confidence> <evidence>)
mtyx detect-agent --surface 1

# Detect across all surfaces in the session in one call
mtyx detect-agents
mtyx detect-agents --json
# -> {"agents":{"1":"claude","2":"codex","3":"unknown"}}
```

### Extending the Pattern Registry at Runtime

Bundled patterns ship in `mux/crates/mux-core/src/agents.json`. You can register additional process or screen markers for custom internal agents at runtime:

```bash
mtyx agent-pattern add my-agent --pattern "my-agent-bin" --kind process --confidence high
mtyx agent-pattern add my-agent --pattern "Allow tool execution?" --kind screen --confidence medium
mtyx agent-pattern list --json
mtyx agent-pattern remove my-agent
```

### Passive OSC Desktop Notifications

Any process inside a pane that emits an OSC 9, OSC 777, or kitty notification sequence (for example `printf '\033]9;Approval needed\007'`) automatically:
1. Sets the pane's `detected` state to `blocked` (unless overridden by a higher-authority `hook` or `socket` report), and
2. Broadcasts an `osc-notification` event on the socket and forwards it to the desktop via `notify-send`.
