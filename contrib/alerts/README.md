<!--
  Desktop + audio alerts for mtyx agent-state transitions.

  Listens to `mtyx subscribe`, watches for transitions into a target agent
  state (blocked by default, optionally done), and fires a desktop
  notification plus a short chime. Born from the Herdr "little ding" parity
  feature: mattyx already classifies and emits agent-state changes, this is
  the consumer that makes noise.

  mattyx has no in-process audio. This is deliberately a contrib/ script so
  the core daemon stays headless and dependency-free; run it under systemd
  --user (example below) if you want it supervised.

  Tracking: audio-alerts / Herdr "little ding" parity feature — see the
  mattyx issue tracker (https://github.com/mathewclarkau/mattyx/issues).
-->

# mattyx alerts — desktop + audio ding on agent-state transitions

Watches the `mtyx subscribe` JSONL stream and dings when an agent pane enters
a state that needs you (blocked, optionally done). Paired with a desktop
notification, so you can tab away and still hear/see when an agent is waiting.

## What it does

1. Spawns `mtyx subscribe` and reads its JSON lines.
2. Picks out the flat `{"event":"agent-state-changed", ...}` events and ignores
   every other event type.
3. **Edge-triggers**: fires only when `state` is a target **and**
   `previous != state`. The daemon also emits same-state reports
   (`blocked → blocked`), so this dedup is required, not merely defensive.
   A `null` `previous` (first-ever report) counts as a transition.
4. On a ding:
   - `notify-send` a desktop notification (title: pane label if resolvable,
     else `mtyx <surface>`; body: `previous → blocked`).
   - Plays `contrib/alerts/ding.wav` through the first available player:
     `paplay` → `pw-play` → `ffplay` → `aplay`.
5. Keeps a per-surface last-state map and a per-surface cooldown so a flapping
   pane cannot machine-gun the sound.
6. Never crashes on a malformed line (logs and continues), and restarts
   `mtyx subscribe` with capped backoff (1s → 2s → 4s … 30s) so it survives
   daemon restarts.

The event payload carries only a numeric `surface` id. If a human label is
wanted, the listener resolves it out-of-band via `mtyx list-workspaces --json`
and caches the map, falling back to `mtyx <surface>`.

## Requirements

- `mtyx` on `PATH`, or `MTYX_BIN=/path/to/mtyx` (it defaults to
  `~/.local/bin/mtyx`).
- `notify-send` for the desktop leg (disable with `MATTYX_ALERTS_NOTIFY=0`).
- One of `paplay`, `pw-play`, `ffplay`, `aplay` for the audio leg
  (disable with `MATTYX_ALERTS_SOUND=0`).
- Python 3, standard library only.

## Run

From the repo root:

```bash
python3 contrib/alerts/mattyx-alerts.py
```

Useful invocations:

```bash
# default: ding on transitions into "blocked"
python3 contrib/alerts/mattyx-alerts.py

# also ding when an agent finishes
python3 contrib/alerts/mattyx-alerts.py --on blocked,done

# throttle per-surface dings to one every 60s
python3 contrib/alerts/mattyx-alerts.py --cooldown 60

# print dings instead of notifying/playing (CI, tests, debugging)
python3 contrib/alerts/mattyx-alerts.py --dry-run
```

## Flags

| Flag | Default | Meaning |
|------|---------|---------|
| `--on blocked[,done,...]` | `blocked` | Comma-separated target states. States are lower-case (`blocked`, `done`, `working`, `idle`). |
| `--cooldown <secs>` | `30` | Minimum per-surface interval between dings. |
| `--dry-run` | off | Print the ding to stdout instead of running `notify-send`/audio. |

## Environment

| Variable | Effect |
|----------|--------|
| `MTYX_BIN` | Path to the `mtyx` binary (default `~/.local/bin/mtyx`, else `PATH`). |
| `MATTYX_ALERTS_SOUND=0` | Disable the audio leg. |
| `MATTYX_ALERTS_NOTIFY=0` | Disable the desktop-notification leg. |

## Event shape

The `mtyx subscribe` stream is JSONL with a flat envelope keyed by a literal
`"event"` field, no handshake line. Agent-state transitions look like:

```json
{"agent":"claude","event":"agent-state-changed","message":"waiting on user","previous":"working","session":null,"source":"hook","state":"blocked","state_seq":2,"surface":1,"updated_at_ms":1792248529767}
```

States are lower-case (`working` / `blocked` / `idle` / `done` / `unknown`);
`previous` is `null` on the first report. `surface` is a number.

## systemd --user unit

Save as `~/.config/systemd/user/mtyx-alerts.service`:

```ini
[Unit]
Description=mtyx desktop + audio alerts for agent-state transitions
Documentation=https://github.com/mathewclarkau/mattyx/issues
After=mtyx.service

[Service]
Type=simple
# Point at your checkout; the script finds the daemon via the inherited
# MTYX_MUX_SOCKET / mtyx session resolution.
WorkingDirectory=%h/Projects/mattyx
ExecStart=/usr/bin/python3 %h/Projects/mattyx/contrib/alerts/mattyx-alerts.py --on blocked,done
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Install and start it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now mtyx-alerts.service
systemctl --user status mtyx-alerts.service
```

## Jump to the blocked pane

Pair the notification with the focus primitive so a click lands you on the pane
that needs attention. `mtyx focus-pane --surface <id>` focuses an arbitrary
surface by id, and the event's `surface` field is exactly that id:

```bash
mtyx focus-pane --surface 1
```

Where the desktop notification daemon supports actions, wiring the notify-send
action to `mtyx focus-pane --surface <id>` turns the ding into a one-click jump
to the blocked pane. (`notify-send` support for click actions varies by
notification daemon; this is a nice-to-have, not required for the alert.)

## The chime

`contrib/alerts/ding.wav` is generated, not downloaded — an ~180 ms two-tone
sine ding (880 Hz then 1318 Hz) with a click-free fade-out. Regenerate it at any
time:

```bash
python3 contrib/alerts/make_ding.py
```

Format: 44100 Hz, 16-bit PCM, mono, ~16 KB.
