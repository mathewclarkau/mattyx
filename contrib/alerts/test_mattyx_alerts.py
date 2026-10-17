#!/usr/bin/env python3
"""Tests for contrib/alerts/mattyx-alerts.py — stdlib unittest, pytest-compatible.

Runnable both ways:

    python3 contrib/alerts/test_mattyx_alerts.py
    python3 -m pytest contrib/alerts/test_mattyx_alerts.py

All tests feed synthetic JSON to the parser/state-machine directly — no mtyx
spawn, no sound, no notifications. Import-safe regardless of cwd: the module
(whose filename contains a hyphen) is loaded via importlib from a path resolved
relative to *this file*, not the working directory.
"""

import contextlib
import importlib.util
import io
import json
import pathlib
import sys
import unittest

_MODULE_PATH = pathlib.Path(__file__).with_name("mattyx-alerts.py")
_spec = importlib.util.spec_from_file_location("mattyx_alerts", _MODULE_PATH)
assert _spec is not None and _spec.loader is not None
mattyx_alerts = importlib.util.module_from_spec(_spec)
sys.modules.setdefault("mattyx_alerts", mattyx_alerts)
_spec.loader.exec_module(mattyx_alerts)

AlertMachine = mattyx_alerts.AlertMachine
Ding = mattyx_alerts.Ding
LabelResolver = mattyx_alerts.LabelResolver
DryRunEmitter = mattyx_alerts.DryRunEmitter

# Verbatim sample from the verified wire capture (scout report §2).
VERBATIM_BLOCKED_LINE = (
    '{"agent":"claude","event":"agent-state-changed","message":"waiting on user",'
    '"previous":"working","session":"sess-abc","source":"hook","state":"blocked",'
    '"state_seq":2,"surface":1,"updated_at_ms":1792248529767}'
)


class FakeClock:
    """Injectable time source: AlertMachine calls now() for cooldown gating."""

    def __init__(self, start=1000.0):
        self.now = float(start)

    def __call__(self):
        return self.now

    def advance(self, secs):
        self.now += secs


def event(surface=1, state="blocked", previous=None, **extra):
    """A synthetic agent-state-changed envelope matching the verified schema."""
    base = {
        "event": "agent-state-changed",
        "surface": surface,
        "previous": previous,
        "state": state,
        "source": "hook",
        "agent": "claude",
        "session": None,
        "message": None,
        "state_seq": 1,
        "updated_at_ms": 1792248529265,
    }
    base.update(extra)
    return base


def line(obj):
    return json.dumps(obj)


class EdgeTriggerTestCase(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.machine = AlertMachine(targets={"blocked"}, cooldown=30,
                                    now=self.clock)

    def test_working_to_blocked_dings(self):
        self.assertIsNone(self.machine.feed(event(state="working",
                                                  previous=None)))
        ding = self.machine.feed(event(state="blocked", previous="working"))
        self.assertIsNotNone(ding)
        self.assertEqual(ding.surface, 1)
        self.assertEqual(ding.state, "blocked")
        self.assertEqual(ding.previous, "working")

    def test_blocked_to_blocked_does_not_re_ding(self):
        # Same-state events DO appear on the wire; they must not re-ding.
        self.assertIsNotNone(
            self.machine.feed(event(state="blocked", previous="working")))
        self.assertIsNone(
            self.machine.feed(event(state="blocked", previous="blocked")))
        self.assertIsNone(
            self.machine.feed(event(state="blocked", previous="blocked")))

    def test_new_edge_after_recovery_dings_again(self):
        self.assertIsNotNone(
            self.machine.feed(event(state="blocked", previous="working")))
        self.clock.advance(60)  # past the cooldown window
        self.assertIsNone(
            self.machine.feed(event(state="working", previous="blocked")))
        self.assertIsNotNone(
            self.machine.feed(event(state="blocked", previous="working")))

    def test_state_matching_is_lower_case(self):
        # Wire states are lower-case; a capitalised variant is normalised
        # (tolerance), so it dings exactly like its lower-case twin — and a
        # same-state capitalised re-report still no-dings.
        self.assertIsNotNone(self.machine.feed(event(state="Blocked",
                                                     previous=None)))
        self.assertIsNone(
            self.machine.feed(event(state="Blocked", previous="blocked")))
        self.clock.advance(30)  # past cooldown so the next edge is observable
        self.assertIsNotNone(
            self.machine.feed(event(state="blocked", previous="Working")))

    def test_tracks_last_state_per_surface(self):
        self.machine.feed(event(surface=1, state="working", previous=None))
        self.machine.feed(event(surface=2, state="blocked", previous=None))
        self.assertEqual(self.machine.last_states,
                         {1: "working", 2: "blocked"})


class PreviousNullTestCase(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.machine = AlertMachine(targets={"blocked"}, cooldown=30,
                                    now=self.clock)

    def test_previous_null_first_report_dings(self):
        ding = self.machine.feed(event(state="blocked", previous=None))
        self.assertIsNotNone(ding)
        self.assertIsNone(ding.previous)  # surfaced as None; "null" in body

    def test_previous_null_into_non_target_does_not_ding(self):
        self.assertIsNone(self.machine.feed(event(state="working",
                                                  previous=None)))


class CooldownTestCase(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.machine = AlertMachine(targets={"blocked"}, cooldown=30,
                                    now=self.clock)

    def test_cooldown_suppresses_rapid_flaps_and_allows_after_window(self):
        # t=1000: first blocked edge dings.
        self.assertIsNotNone(
            self.machine.feed(event(state="blocked", previous="working")))
        # t=1012: flap working -> blocked is a real edge, but suppressed.
        self.clock.advance(12)
        self.assertIsNone(
            self.machine.feed(event(state="working", previous="blocked")))
        self.assertIsNone(
            self.machine.feed(event(state="blocked", previous="working")))
        # t=1025 (25s after the ding): still suppressed.
        self.clock.advance(13)
        self.assertIsNone(
            self.machine.feed(event(state="working", previous="blocked")))
        self.assertIsNone(
            self.machine.feed(event(state="blocked", previous="working")))
        # t=1030 (30s after the ding, cooldown boundary): allowed again.
        # Note the window is anchored at the last EMITTED ding — the suppressed
        # flap at t=1012 did not extend it.
        self.clock.advance(5)
        self.assertIsNone(
            self.machine.feed(event(state="working", previous="blocked")))
        self.assertIsNotNone(
            self.machine.feed(event(state="blocked", previous="working")))
        # t=1031: new window in force, next flap is suppressed.
        self.clock.advance(1)
        self.assertIsNone(
            self.machine.feed(event(state="working", previous="blocked")))
        self.assertIsNone(
            self.machine.feed(event(state="blocked", previous="working")))

    def test_cooldown_is_per_surface(self):
        ding1 = self.machine.feed(event(surface=1, state="blocked",
                                        previous="working"))
        ding2 = self.machine.feed(event(surface=2, state="blocked",
                                        previous="working"))
        self.assertIsNotNone(ding1)
        self.assertIsNotNone(ding2)
        self.assertEqual(ding1.surface, 1)
        self.assertEqual(ding2.surface, 2)

    def test_cooldown_zero_dings_every_edge(self):
        machine = AlertMachine(targets={"blocked"}, cooldown=0,
                               now=FakeClock())
        self.assertIsNotNone(machine.feed(event(state="blocked",
                                                previous="working")))
        self.assertIsNone(machine.feed(event(state="working",
                                             previous="blocked")))
        self.assertIsNotNone(machine.feed(event(state="blocked",
                                                previous="working")))


class TargetsTestCase(unittest.TestCase):
    def test_default_targets_do_not_ding_on_done(self):
        machine = AlertMachine(targets=mattyx_alerts.parse_targets("blocked"),
                               cooldown=0, now=FakeClock())
        self.assertIsNone(machine.feed(event(state="done",
                                             previous="working")))

    def test_blocked_done_targets_ding_on_done(self):
        machine = AlertMachine(
            targets=mattyx_alerts.parse_targets("blocked,done"),
            cooldown=0, now=FakeClock())
        ding = machine.feed(event(state="done", previous="working"))
        self.assertIsNotNone(ding)
        self.assertEqual(ding.state, "done")
        # blocked still dings under the extended target set.
        self.assertIsNotNone(machine.feed(event(state="blocked",
                                                previous="done")))

    def test_parse_targets(self):
        self.assertEqual(mattyx_alerts.parse_targets("blocked"), {"blocked"})
        self.assertEqual(mattyx_alerts.parse_targets("blocked,done"),
                         {"blocked", "done"})
        self.assertEqual(mattyx_alerts.parse_targets(" Blocked , DONE "),
                         {"blocked", "done"})
        self.assertEqual(mattyx_alerts.parse_targets(""), {"blocked"})

    def test_parser_flags(self):
        args = mattyx_alerts.build_parser().parse_args([])
        self.assertEqual(args.on, "blocked")
        self.assertEqual(args.cooldown, 30.0)
        self.assertFalse(args.dry_run)
        args = mattyx_alerts.build_parser().parse_args(
            ["--on", "blocked,done", "--cooldown", "5", "--dry-run"])
        self.assertEqual(mattyx_alerts.parse_targets(args.on),
                         {"blocked", "done"})
        self.assertEqual(args.cooldown, 5.0)
        self.assertTrue(args.dry_run)


class FeedLineTestCase(unittest.TestCase):
    def setUp(self):
        self.machine = AlertMachine(targets={"blocked"}, cooldown=0,
                                    now=FakeClock())

    def test_verbatim_wire_line_dings(self):
        ding = self.machine.feed_line(VERBATIM_BLOCKED_LINE)
        self.assertIsNotNone(ding)
        self.assertEqual(ding.surface, 1)
        self.assertEqual(ding.state, "blocked")
        self.assertEqual(ding.previous, "working")
        self.assertEqual(ding.agent, "claude")

    def test_other_event_names_are_skipped(self):
        for name in ("surface-output", "surface-exited", "tree-changed",
                     "bell", "title-changed", "status", "osc-notification"):
            self.assertIsNone(
                self.machine.feed_line(line({"event": name, "surface": 1})))
        # Machine is still healthy afterwards.
        self.assertIsNotNone(self.machine.feed_line(
            line(event(state="blocked", previous="working"))))

    def test_malformed_lines_do_not_raise(self):
        for bad in ("", "   ", "not json at all",
                    '{"event": "agent-state-changed"',  # truncated JSON
                    "[]", "null", "42", '"a string"'):
            self.assertIsNone(self.machine.feed_line(bad))
        # Machine is still healthy afterwards.
        self.assertIsNotNone(self.machine.feed_line(
            line(event(state="blocked", previous="working"))))

    def test_non_numeric_surface_is_skipped(self):
        for surface in ("1", None, 1.5, True):
            self.assertIsNone(self.machine.feed_line(line(
                {"event": "agent-state-changed", "surface": surface,
                 "state": "blocked", "previous": "working"})))

    def test_non_string_state_is_skipped(self):
        self.assertIsNone(self.machine.feed_line(line(
            {"event": "agent-state-changed", "surface": 1,
             "state": 7, "previous": "working"})))


def _fake_run(payload):
    """subprocess.run stand-in returning a canned JSON stdout."""
    class _Proc:
        returncode = 0
        stdout = payload
    return lambda cmd, **kwargs: _Proc()


def _fake_run_state(state):
    """subprocess.run stand-in reading stdout from a mutable dict (so a
    second fetch after LabelResolver.invalidate() sees fresh payload)."""
    class _Proc:
        returncode = 0

    def run(cmd, **kwargs):
        proc = _Proc()
        proc.stdout = state["payload"]
        return proc
    return run


CANNED_TREE = {
    "workspaces": [
        {"name": "Worker-9", "screens": [{"panes": [{"tabs": [
            {"surface": 1, "name": None, "title": "π - mattyx"},   # title wins
            {"surface": 2, "name": "build pane", "title": "other"},  # name wins
            {"surface": 3, "name": None, "title": None},          # ws name wins
        ]}]}]},
        {"name": "Scout", "screens": [{"panes": [{"tabs": [
            {"surface": 4, "name": None, "title": ""},            # ws name wins
        ]}]}]},
    ]
}


class LabelResolverTestCase(unittest.TestCase):
    def test_label_priority_and_fallback(self):
        resolver = LabelResolver("fake-mtyx",
                                 run=_fake_run(json.dumps(CANNED_TREE)))
        self.assertEqual(resolver.label(1), "π - mattyx")
        self.assertEqual(resolver.label(2), "build pane")
        self.assertEqual(resolver.label(3), "Worker-9")
        self.assertEqual(resolver.label(4), "Scout")
        self.assertEqual(resolver.label(99), "mtyx 99")  # unknown surface

    def test_resolution_failure_never_crashes(self):
        def boom(cmd, **kwargs):
            raise RuntimeError("daemon down")
        resolver = LabelResolver("fake-mtyx", run=boom)
        self.assertEqual(resolver.label(7), "mtyx 7")

    def test_invalidate_forces_refetch(self):
        state = {"payload": json.dumps(
            {"workspaces": [{"name": "W", "screens": [{"panes": [{"tabs": [
                {"surface": 1, "name": "old", "title": None}]}]}]}]})}
        resolver = LabelResolver("fake-mtyx", run=_fake_run_state(state))
        self.assertEqual(resolver.label(1), "old")
        state["payload"] = json.dumps(
            {"workspaces": [{"name": "W", "screens": [{"panes": [{"tabs": [
                {"surface": 1, "name": "new", "title": None}]}]}]}]})
        self.assertEqual(resolver.label(1), "old")  # cached
        resolver.invalidate()  # what tree-changed triggers
        self.assertEqual(resolver.label(1), "new")



class DryRunEmitterTestCase(unittest.TestCase):
    def test_prints_ding_to_stdout(self):
        resolver = LabelResolver("fake-mtyx",
                                 run=_fake_run(json.dumps(CANNED_TREE)))
        emitter = DryRunEmitter(resolver)
        ding = Ding(surface=1, previous="working", state="blocked",
                    agent="claude", message=None)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            emitter.emit(ding)
        out = buf.getvalue()
        self.assertIn("surface=1", out)
        self.assertIn("blocked", out)
        self.assertIn("working", out)
        self.assertIn("π - mattyx", out)

    def test_formats_null_previous(self):
        body = mattyx_alerts.format_transition(
            Ding(surface=1, previous=None, state="blocked",
                 agent=None, message=None))
        self.assertEqual(body, "null → blocked")


if __name__ == "__main__":
    unittest.main(verbosity=2)
