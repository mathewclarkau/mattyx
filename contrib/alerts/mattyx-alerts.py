#!/usr/bin/env python3
"""mattyx-alerts — desktop notification + audio "ding" on agent-state transitions.

Subscribes to the mtyx daemon event stream (`mtyx subscribe`) and fires a
desktop notification (notify-send) plus a short sound (contrib/alerts/ding.wav,
resolved relative to this file) whenever a pane transitions into a target
agent state (default: "blocked").

Wire schema (verified against the daemon's subscribe stream — flat JSON object,
one per line, states serialized lower-case):

    {"event":"agent-state-changed","surface":1,"previous":"working"|null,
     "state":"blocked","source":"hook","agent":"claude"|null,"message":null,
     "session":null,"state_seq":2,"updated_at_ms":1792248549767}

Key behaviours:

- Edge-triggered only: the daemon DOES emit same-state events (blocked ->
  blocked re-fires); we ding only when `state` is in the target set AND
  `previous != state`. `previous: null` (first-ever report) counts as a
  transition -> ding.
- All other event names in the stream (surface-output, tree-changed, bell,
  ...) are skipped; `tree-changed` additionally invalidates the label cache.
- The payload carries no human-readable label. surface -> label is resolved
  lazily via `mtyx list-workspaces --json` (priority: tab.name -> tab.title ->
  workspace.name -> "mtyx <surface>") and cached. Resolution failure never
  crashes the listener; the fallback label is used.
- Malformed lines are logged and skipped. If `mtyx subscribe` exits, we retry
  with exponential backoff (1s -> 2s -> 4s ... capped at 30s) so daemon
  restarts are survived.
- Audio leg: first working player of paplay / pw-play / ffplay -nodsp
  -autoexit / aplay, probed once and cached.

Environment:

    MTYX_BIN                path to the mtyx binary (default ~/.local/bin/mtyx)
    MATTYX_ALERTS_SOUND=0   disable the audio leg
    MATTYX_ALERTS_NOTIFY=0  disable the notify-send leg

Usage:

    python3 mattyx-alerts.py [--on blocked,done] [--cooldown 30] [--dry-run]

Stdlib only. See contrib/alerts/README.md for install/systemd notes.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from collections import namedtuple
from typing import Callable, Dict, Optional, Set, Tuple

AGENT_STATE_CHANGED = "agent-state-changed"
TREE_CHANGED = "tree-changed"
KNOWN_STATES = frozenset({"working", "blocked", "idle", "done", "unknown"})
DEFAULT_TARGETS = frozenset({"blocked"})
DEFAULT_COOLDOWN_SECS = 30.0
DEFAULT_MTYX_BIN = "~/.local/bin/mtyx"
MAX_BACKOFF_SECS = 30.0
# A subscribe connection that lived at least this long resets the retry backoff.
HEALTHY_RUN_SECS = 60.0

# First available player wins; probed once, then cached.
PLAYER_SPECS: Tuple[Tuple[str, Tuple[str, ...]], ...] = (
    ("paplay", ("paplay",)),
    ("pw-play", ("pw-play",)),
    ("ffplay", ("ffplay", "-nodsp", "-autoexit")),
    ("aplay", ("aplay",)),
)

# An alert that should be emitted: surface + transition + advisory fields.
Ding = namedtuple("Ding", ("surface", "previous", "state", "agent", "message"))


def log(message: str) -> None:
    print(f"[mattyx-alerts] {time.strftime('%Y-%m-%d %H:%M:%S')} {message}",
          file=sys.stderr, flush=True)


def env_flag_disabled(name: str) -> bool:
    value = os.environ.get(name)
    return value is not None and value.strip().lower() in {"0", "false", "no", "off"}


def resolve_mtyx_bin() -> str:
    return os.environ.get("MTYX_BIN") or os.path.expanduser(DEFAULT_MTYX_BIN)


def ding_wav_path() -> str:
    """contrib/alerts/ding.wav, resolved relative to this script file."""
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "ding.wav")


def parse_json_object(line: str) -> Optional[dict]:
    """Parse one JSONL stream line; return a dict, or None if malformed/not an object."""
    try:
        obj = json.loads(line)
    except (ValueError, TypeError):
        return None
    return obj if isinstance(obj, dict) else None


def normalize_surface(value) -> Optional[int]:
    """Coerce a JSON surface id to int (surfaces are numbers on the wire)."""
    if isinstance(value, bool):  # bool is an int subclass — not a surface
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return None


def parse_targets(spec: str) -> Set[str]:
    """Parse a comma-separated --on value into a set of lower-case states."""
    targets = {tok.strip().lower() for tok in spec.split(",") if tok.strip()}
    if not targets:
        return set(DEFAULT_TARGETS)
    unknown = targets - KNOWN_STATES
    if unknown:
        log(f"warning: --on states not in {sorted(KNOWN_STATES)}: {sorted(unknown)}")
    return targets


def format_transition(ding: Ding) -> str:
    previous = ding.previous if ding.previous is not None else "null"
    return f"{previous} → {ding.state}"


class AlertMachine:
    """Edge-triggered, per-surface agent-state alert state machine.

    feed() consumes one parsed `agent-state-changed` event dict and returns a
    Ding when the event should alert:

      * `state` is in `targets` (lower-case strings), AND
      * it is a transition: the event's `previous` differs from `state`.
        `previous: null` (the first-ever report for a surface) counts as a
        transition. If the `previous` key is missing entirely (off-schema),
        we fall back to comparing against our own tracked last state, AND
      * the per-surface cooldown has elapsed since the last emitted ding
        (suppressed flaps do NOT extend the window).

    `now` is injectable so cooldown behaviour is testable without sleeping.
    """

    def __init__(self, targets=DEFAULT_TARGETS, cooldown=DEFAULT_COOLDOWN_SECS,
                 now: Callable[[], float] = time.monotonic):
        self.targets = {str(t).strip().lower() for t in targets} or set(DEFAULT_TARGETS)
        self.cooldown = max(0.0, float(cooldown))
        self._now = now
        self._last_state: Dict[int, str] = {}
        self._last_ding: Dict[int, float] = {}

    @property
    def last_states(self) -> Dict[int, str]:
        """Copy of the per-surface last-known-state tracking dict."""
        return dict(self._last_state)

    def feed_line(self, line: str) -> Optional[Ding]:
        """Parse a raw stream line and feed it. Malformed lines and non
        agent-state-changed events return None without raising."""
        obj = parse_json_object(line)
        if obj is None or obj.get("event") != AGENT_STATE_CHANGED:
            return None
        return self.feed(obj)

    def feed(self, event) -> Optional[Ding]:
        """Consume one parsed event dict; return a Ding or None."""
        if not isinstance(event, dict):
            return None
        surface = normalize_surface(event.get("surface"))
        if surface is None:
            return None
        raw_state = event.get("state")
        if not isinstance(raw_state, str):
            return None
        state = raw_state.strip().lower()
        raw_previous = event.get("previous")
        previous = raw_previous.strip().lower() if isinstance(raw_previous, str) else None

        if "previous" in event:
            transition = previous != state
        else:
            # Off-schema (no previous field): compare against our own tracking.
            transition = self._last_state.get(surface) != state

        ding = None
        if state in self.targets and transition:
            now = self._now()
            last = self._last_ding.get(surface)
            if last is None or (now - last) >= self.cooldown:
                self._last_ding[surface] = now
                ding = Ding(surface=surface, previous=previous, state=state,
                            agent=event.get("agent"), message=event.get("message"))
        # Track the latest state even when no ding fires, so retained/duplicate
        # reports can never re-ding later.
        self._last_state[surface] = state
        return ding


def extract_surface_labels(tree) -> Dict[int, str]:
    """Map surface id -> label from a `mtyx list-workspaces --json` payload.

    Label priority: tab.name -> tab.title -> workspace.name -> "mtyx <surface>".
    Tolerates missing/null levels; never raises.
    """
    labels: Dict[int, str] = {}
    if not isinstance(tree, dict):
        return labels
    for ws in tree.get("workspaces") or []:
        if not isinstance(ws, dict):
            continue
        ws_name = ws.get("name")
        for screen in ws.get("screens") or []:
            if not isinstance(screen, dict):
                continue
            for pane in screen.get("panes") or []:
                if not isinstance(pane, dict):
                    continue
                for tab in pane.get("tabs") or []:
                    if not isinstance(tab, dict):
                        continue
                    surface = normalize_surface(tab.get("surface"))
                    if surface is None:
                        continue
                    label = None
                    for candidate in (tab.get("name"), tab.get("title"), ws_name):
                        if isinstance(candidate, str) and candidate.strip():
                            label = candidate.strip()
                            break
                    labels[surface] = label or f"mtyx {surface}"
    return labels


class LabelResolver:
    """Lazily maps surface id -> human label via `mtyx list-workspaces --json`.

    The map is fetched at most once and cached; invalidate() (called on
    tree-changed events) clears it so the next lookup re-fetches. Any failure
    (missing binary, daemon down, bad JSON) is logged once and degrades to the
    "mtyx <surface>" fallback — resolution must never crash the listener.
    """

    def __init__(self, mtyx_bin: str, run=subprocess.run):
        self._bin = mtyx_bin
        self._run = run
        self._cache: Dict[int, str] = {}
        self._fetched = False

    def invalidate(self) -> None:
        self._cache.clear()
        self._fetched = False

    def label(self, surface: int) -> str:
        cached = self._cache.get(surface)
        if cached is not None:
            return cached
        self._fetch()
        return self._cache.get(surface, f"mtyx {surface}")

    def _fetch(self) -> None:
        if self._fetched:
            return
        # Mark fetched even on failure so a broken daemon is not hammered;
        # tree-changed invalidation re-enables fetching later.
        self._fetched = True
        try:
            proc = self._run([self._bin, "list-workspaces", "--json"],
                             capture_output=True, text=True, timeout=5)
            self._cache.update(extract_surface_labels(json.loads(proc.stdout)))
        except Exception as exc:  # noqa: BLE001 — resolution must never crash us
            log(f"label resolution failed ({exc!r}); using fallback labels")


class Notifier:
    """notify-send leg. Failures are logged and swallowed."""

    def __init__(self, run=subprocess.run):
        self._run = run
        self._available = shutil.which("notify-send") is not None
        self._warned = False

    def send(self, title: str, body: str) -> None:
        if not self._available:
            if not self._warned:
                log("notify-send not found; desktop notification leg disabled")
                self._warned = True
            return
        try:
            self._run(["notify-send", title, body],
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                      timeout=10)
        except Exception as exc:  # noqa: BLE001
            log(f"notify-send failed ({exc!r})")


class SoundPlayer:
    """Audio leg: play ding.wav with the first working player.

    Candidates are probed (existence via PATH) once, in order; the first
    player that succeeds is cached and reused. If a cached player later fails,
    we fall through to the next candidate on the following ding.
    """

    def __init__(self, wav_path: str, specs=PLAYER_SPECS,
                 which=shutil.which, run=subprocess.run):
        self._wav = wav_path
        self._specs = tuple(specs)
        self._which = which
        self._run = run
        self._candidate = 0
        self._argv: Optional[Tuple[str, ...]] = None  # cached working player
        self._warned_missing = False
        self._warned_none = False

    def play(self) -> None:
        if not os.path.exists(self._wav):
            if not self._warned_missing:
                log(f"sound file not found: {self._wav}; audio leg muted")
                self._warned_missing = True
            return
        argv = self._argv
        if argv is None:
            argv = self._probe()
            if argv is None:
                if not self._warned_none:
                    log("no audio player found (paplay/pw-play/ffplay/aplay); "
                        "audio leg muted")
                    self._warned_none = True
                return
        try:
            proc = self._run(list(argv) + [self._wav],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             timeout=15)
            if proc.returncode == 0:
                self._argv = argv  # cache the working player
                return
            reason = f"exit {proc.returncode}"
        except Exception as exc:  # noqa: BLE001
            reason = repr(exc)
        log(f"audio player {argv[0]} failed ({reason}); trying next candidate")
        self._argv = None  # next play() probes the next candidate

    def _probe(self) -> Optional[Tuple[str, ...]]:
        while self._candidate < len(self._specs):
            name, argv = self._specs[self._candidate]
            self._candidate += 1
            if self._which(name):
                return argv
        return None


class AlertEmitter:
    """Real ding: notify-send (title: label, body: "previous → state") + sound.

    Either leg can be disabled via MATTYX_ALERTS_NOTIFY=0 / MATTYX_ALERTS_SOUND=0.
    """

    def __init__(self, resolver: LabelResolver, wav_path: str):
        self._resolver = resolver
        self._notify_enabled = not env_flag_disabled("MATTYX_ALERTS_NOTIFY")
        self._sound_enabled = not env_flag_disabled("MATTYX_ALERTS_SOUND")
        self._notifier = Notifier()
        self._player = SoundPlayer(wav_path)

    def emit(self, ding: Ding) -> None:
        label = self._resolver.label(ding.surface)
        body = format_transition(ding)
        if self._notify_enabled:
            self._notifier.send(label, body)
        if self._sound_enabled:
            self._player.play()
        log(f"ding: surface={ding.surface} label={label!r}: {body}")


class DryRunEmitter:
    """--dry-run: print dings to stdout instead of notify-send/audio."""

    def __init__(self, resolver: LabelResolver):
        self._resolver = resolver

    def emit(self, ding: Ding) -> None:
        label = self._resolver.label(ding.surface)
        print(f"[dry-run] ding: surface={ding.surface} label={label!r}: "
              f"{format_transition(ding)}", flush=True)


def _reap(proc: subprocess.Popen) -> None:
    """Terminate and reap a subscribe child; best-effort, never raises."""
    try:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    except Exception:  # noqa: BLE001
        pass
    finally:
        if proc.stdout is not None:
            try:
                proc.stdout.close()
            except OSError:
                pass


def run_loop(mtyx_bin: str, machine: AlertMachine, resolver: LabelResolver,
             emitter, sleep=time.sleep, monotonic=time.monotonic) -> None:
    """Spawn `mtyx subscribe`, feed lines to the machine, retry on exit.

    Backoff 1s -> 2s -> 4s ... capped at 30s; reset after a healthy run.
    """
    backoff = 1.0
    while True:
        started = monotonic()
        try:
            proc = subprocess.Popen([mtyx_bin, "subscribe", "--json"],
                                    stdout=subprocess.PIPE, text=True)
        except OSError as exc:
            log(f"failed to spawn {mtyx_bin} subscribe ({exc!r}); "
                f"retrying in {backoff:.0f}s")
            sleep(backoff)
            backoff = min(backoff * 2, MAX_BACKOFF_SECS)
            continue
        log(f"listening to `{mtyx_bin} subscribe` for agent-state-changed")
        try:
            for raw in proc.stdout:
                line = raw.strip()
                if not line:
                    continue
                obj = parse_json_object(line)
                if obj is None:
                    log(f"skipping malformed line: {line[:120]!r}")
                    continue
                event = obj.get("event")
                if event == AGENT_STATE_CHANGED:
                    ding = machine.feed(obj)
                    if ding is not None:
                        emitter.emit(ding)
                elif event == TREE_CHANGED:
                    resolver.invalidate()
        finally:
            _reap(proc)
        if monotonic() - started >= HEALTHY_RUN_SECS:
            backoff = 1.0
        log(f"mtyx subscribe exited (code {proc.returncode}); "
            f"reconnecting in {backoff:.0f}s")
        sleep(backoff)
        backoff = min(backoff * 2, MAX_BACKOFF_SECS)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="mattyx-alerts.py",
        description="Desktop + audio alerts when mtyx agent panes transition "
                    "into a target state (e.g. blocked). Subscribes to the "
                    "mtyx daemon event stream and never exits on its own.")
    parser.add_argument("--on", default="blocked", metavar="STATES",
                        help="comma-separated target states "
                             "(working|blocked|idle|done|unknown); "
                             "default: %(default)s")
    parser.add_argument("--cooldown", type=float, default=DEFAULT_COOLDOWN_SECS,
                        metavar="SECS",
                        help="per-surface minimum seconds between dings "
                             "(default: %(default)s)")
    parser.add_argument("--dry-run", action="store_true",
                        help="print dings to stdout instead of "
                             "notify-send/audio")
    return parser


def main(argv=None) -> int:
    # Exit cleanly on SIGTERM (e.g. systemd stop) so the child is reaped.
    signal.signal(signal.SIGTERM, lambda signum, frame: sys.exit(0))
    args = build_parser().parse_args(argv)
    targets = parse_targets(args.on)
    machine = AlertMachine(targets=targets, cooldown=args.cooldown)
    mtyx_bin = resolve_mtyx_bin()
    resolver = LabelResolver(mtyx_bin)
    if args.dry_run:
        emitter = DryRunEmitter(resolver)
    else:
        emitter = AlertEmitter(resolver, ding_wav_path())
    log(f"starting: on={sorted(targets)} cooldown={args.cooldown:g}s "
        f"binary={mtyx_bin} dry_run={args.dry_run}")
    run_loop(mtyx_bin, machine, resolver, emitter)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        pass
