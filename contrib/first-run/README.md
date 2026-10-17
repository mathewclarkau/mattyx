<!--
  First-run onboarding for mtyx — theme, sound, agent hooks, and the alerts
  systemd unit, set up in one pass.

  Detects what is already on the box (agent CLIs, installed hooks, mux.json,
  the alerts unit, audio players), asks only for what is missing, then
  applies: a bundled theme merged into ~/.config/mattyx/mux.json (merge,
  never clobber), agent hooks via `mtyx agents install --only <agent>
  --global`, and optionally the contrib/alerts systemd --user unit.

  mattyx ships no in-process onboarding. This is deliberately a contrib/
  script so the core daemon stays headless and dependency-free.

  Tracking: first-run onboarding — see the mattyx issue tracker
  (https://github.com/mathewclarkau/mattyx/issues).
-->

# mtyx first-run — theme, sound, hooks, and alerts unit in one pass

Interactive (or fully scripted via flags) onboarding for a fresh mattyx
install: applies a bundled theme to `~/.config/mattyx/mux.json` (merged, never
clobbered), installs agent hooks via `mtyx agents install --only <agent>
--global`, and optionally writes the systemd `--user` unit that runs
`contrib/alerts/mattyx-alerts.py`. Every step is detect-first: it reports what
is already installed instead of duplicating it, and re-running applies
nothing the second time.

## What it does

1. **Detects** (read-only pass, printed as a status table — see `--status`):
   which supported agent CLIs exist on PATH, per-agent hook-install status,
   whether `mux.json` exists and already has a `theme` object, whether the
   alerts user unit exists, and whether `paplay`/`pw-play` is available.
2. **Prompts** for anything the flags did not already decide: theme choice
   (numbered list of bundled themes + "keep current"), sound on/off, which
   detected-but-unhooked agents to hook (multi-select, default all detected),
   and whether to write the alerts unit. `--yes` skips all prompts.
3. **Applies**:
   - theme: reads the bundled TOML and merges its keys into the `theme`
     object of `~/.config/mattyx/mux.json`, preserving every other key;
   - hooks: runs `<mtyx> agents install --only <agent> --global` per selected
     agent;
   - alerts unit: writes `~/.config/systemd/user/mtyx-alerts.service` and
     prints (never runs) the two enable commands.

## Run

From the repo root:

```bash
python3 contrib/first-run/mtyx-first-run.py
```

## `--status` — detection table

Strictly read-only, prints one row per check, exit 0:

| Row | What is checked |
|-----|-----------------|
| `<agent> on PATH` | Is a supported agent CLI installed? Supported agents: pi, claude, codex, antigravity, aider, opencode, grok. |
| `<agent> hooks` | pi: `~/.pi/agent/extensions/mtyx.ts` or `~/.pi/agent/mtyx.ts`; claude: any `hooks.*` entry in `~/.claude/settings.json` whose command references the mtyx binary or `claude hook`; antigravity: same idea in `~/.gemini/config/hooks.json`. Other agents: no layout is guessed — the row reports `unknown layout, run "mtyx agents install --only <agent> --global" manually`. |
| `mux.json` | Does `~/.config/mattyx/mux.json` exist, and does it already contain a `theme` object? |
| `mtyx-alerts.service` | Does `~/.config/systemd/user/mtyx-alerts.service` exist? |
| `sound` | Is `paplay` or `pw-play` available (audible dings possible)? |

Already-installed hooks are reported as `already installed` and are not
re-installed.

## Flags

| Flag | Meaning |
|------|---------|
| `--yes` | Non-interactive: skip all prompts. Requires the choice flags (`--theme`, `--sound`, `--agents`, `--write-alerts-unit`). |
| `--theme <name\|none>` | Apply a bundled theme. Bundled: `catpuccin-latte`, `catpuccin-mocha`, `dracula`, `gruvbox-dark`, `nord`, `solarized-dark`, `solarized-light`. `none` leaves `mux.json` alone. |
| `--sound <on\|off>` | Whether the alerts unit gets the audio leg (`MATTYX_ALERTS_SOUND`). |
| `--agents <list\|none\|auto>` | Comma-separated agent names to hook, `none` for none, or `auto` for every supported agent detected on PATH. |
| `--write-alerts-unit <on\|off>` | Whether to write the `mtyx-alerts.service` user unit. |
| `--print-plan` | Print the JSON plan of actions and exit 0 WITHOUT applying anything. Zero side effects. |
| `--status` | Detection table only (read-only), exit 0. |

Examples:

```bash
# see what is already set up — never writes anything
python3 contrib/first-run/mtyx-first-run.py --status

# preview exactly what a scripted run would do, as JSON
python3 contrib/first-run/mtyx-first-run.py --yes \
    --theme gruvbox-dark --sound on --agents auto --write-alerts-unit on \
    --print-plan

# apply it for real, no prompts
python3 contrib/first-run/mtyx-first-run.py --yes \
    --theme gruvbox-dark --sound on --agents auto --write-alerts-unit on

# pick per-question in the terminal, theme only
python3 contrib/first-run/mtyx-first-run.py --theme nord
```

## The alerts unit it writes

`--write-alerts-unit on` writes `~/.config/systemd/user/mtyx-alerts.service`:

```ini
[Unit]
Description=mtyx desktop + audio alerts for agent-state transitions
Documentation=https://github.com/mathewclarkau/mattyx/issues
After=mtyx.service

[Service]
Type=simple
# Environment line follows the sound choice:
#   --sound on  → Environment=MATTYX_ALERTS_SOUND=1
#   --sound off → Environment=MATTYX_ALERTS_SOUND=0
Environment=MATTYX_ALERTS_SOUND=1
ExecStart=/usr/bin/python3 <repo>/contrib/alerts/mattyx-alerts.py --on blocked
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

(`<repo>` is the absolute path of this checkout; the alerts script resolves
the `mtyx` binary itself.)

The tool then PRINTS — never runs — the two commands that turn the unit on:

```bash
systemctl --user daemon-reload
systemctl --user enable --now mtyx-alerts
```

## Interactive mode

Without choice flags, each decision is a numbered prompt; passing a flag
skips its prompt, and `--yes` skips them all. Ctrl-D (EOF) at any prompt
aborts cleanly — nothing is applied.

## Safety guarantees

- `--status` and `--print-plan` are strictly read-only: neither ever writes,
  touches config, installs hooks, or creates units.
- All config writes are atomic: temp file in the same directory + rename.
- `mux.json` edits merge: keys the tool does not manage are preserved
  untouched, including the rest of an existing `theme` object.
- Idempotent: a second run detects everything as already done and applies
  nothing.
- The bundled test suite (`test_mtyx_first_run.py`) runs everything against
  temp directories — it never touches the real `~/.config/mattyx`, `~/.pi`,
  or `~/.claude`.

## Environment

| Variable | Effect |
|----------|--------|
| `MTYX_BIN` | Path to the `mtyx` binary (default `~/.local/bin/mtyx`). Same convention as `contrib/alerts`. |
