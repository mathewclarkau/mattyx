#!/usr/bin/env python3
"""Tests for contrib/first-run/mtyx-first-run.py — stdlib unittest,
pytest-compatible.

Runnable both ways:

    python3 contrib/first-run/test_mtyx_first_run.py
    python3 -m pytest contrib/first-run/test_mtyx_first_run.py

Every test runs against a fresh temp HOME (redirected via os.environ so the
module's single home resolver picks it up) — the real ~/.config, ~/.pi,
~/.claude and ~/.gemini are never touched. Agent detection gets an injectable
`which`; the hook-installer gets an injectable runner, so no test ever shells
out to the real mtyx binary. Bundled theme TOMLs are read from the REAL repo
tree (read-only, resolved relative to the module file).
"""

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest
from types import SimpleNamespace

_MODULE_PATH = pathlib.Path(__file__).with_name("mtyx-first-run.py")
_spec = importlib.util.spec_from_file_location("mtyx_first_run", _MODULE_PATH)
assert _spec is not None and _spec.loader is not None
first_run = importlib.util.module_from_spec(_spec)
sys.modules.setdefault("mtyx_first_run", first_run)
_spec.loader.exec_module(first_run)


def fake_which(found):
    """shutil.which stand-in: finds exactly the given command names."""
    found = set(found)

    def which(name, path=None):
        return f"/usr/bin/{name}" if name in found else None

    return which


def boom_runner(argv):
    raise AssertionError(f"runner must not be called, got {argv}")


def choices(theme=None, sound=True, agents=(), unit=True):
    """choices dict for build_plan; agents=None means 'auto'."""
    return {"theme": theme, "sound": sound, "agents": agents,
            "write_alerts_unit": unit}


class TempHomeTestCase(unittest.TestCase):
    """Fresh temp HOME per test; HOME/MTYX_BIN saved and restored."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.home = pathlib.Path(tmp.name)
        self._old_env = {k: os.environ.get(k) for k in ("HOME", "MTYX_BIN")}
        os.environ["HOME"] = str(self.home)
        os.environ["MTYX_BIN"] = str(self.home / "bin" / "mtyx")

        def restore():
            for key, value in self._old_env.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

        self.addCleanup(restore)

    def write(self, rel, text):
        path = self.home / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def tree_digest(self):
        items = []
        for path in sorted(self.home.rglob("*")):
            rel = str(path.relative_to(self.home))
            if path.is_dir():
                items.append(("d", rel))
            else:
                items.append(("f", rel,
                              hashlib.sha256(path.read_bytes()).hexdigest()))
        return items


class ThemeLoadingTestCase(unittest.TestCase):
    def test_gruvbox_dark_maps_onto_theme_keys(self):
        theme = first_run.load_theme("gruvbox-dark")  # real repo TOML
        self.assertLessEqual(set(theme), set(first_run.THEME_KEYS))
        self.assertEqual(theme["selection_background"], "#32302f")
        self.assertEqual(theme["sidebar_rail"], "#d79921")
        self.assertEqual(theme["tab_active_bg"], "#d79921")
        self.assertEqual(theme["border_inactive"], "#505050")

    def test_all_bundled_themes_are_valid(self):
        names = first_run.bundled_themes()
        self.assertIn("gruvbox-dark", names)
        for name in names:
            theme = first_run.load_theme(name)
            self.assertTrue(theme, name)
            self.assertLessEqual(set(theme), set(first_run.THEME_KEYS), name)

    def test_missing_theme_raises(self):
        with self.assertRaises(FileNotFoundError):
            first_run.load_theme("no-such-theme-xyz")


class ThemeMergeTestCase(TempHomeTestCase):
    def test_merge_preserves_foreign_keys(self):
        foreign = {"theme": {"sidebar_rail": "#000000"},
                   "tabs": {"min_width": 7},
                   "browser": {"cdp_url": "http://127.0.0.1:9222"},
                   "keys": {"prefix": "ctrl+b"}}
        self.write(".config/mattyx/mux.json", json.dumps(foreign))
        detection = first_run.detect(self.home, which=fake_which([]))
        plan = first_run.build_plan(
            detection, choices(theme="gruvbox-dark", agents=(), unit=False))
        results = first_run.apply_plan(plan, runner=boom_runner)
        self.assertTrue(any("theme" in r for r in results), results)

        data = json.loads(
            (self.home / ".config/mattyx/mux.json").read_text())
        # Foreign top-level keys preserved untouched.
        self.assertEqual(data["tabs"], {"min_width": 7})
        self.assertEqual(data["browser"], {"cdp_url": "http://127.0.0.1:9222"})
        self.assertEqual(data["keys"], {"prefix": "ctrl+b"})
        # Theme keys merged: overwritten, added, old extra kept.
        self.assertEqual(data["theme"]["sidebar_rail"], "#d79921")
        self.assertEqual(data["theme"]["border_inactive"], "#505050")
        # No stray temp files left by the atomic write.
        files = sorted(p.name
                       for p in (self.home / ".config/mattyx").iterdir())
        self.assertEqual(files, ["mux.json"])


class IdempotenceTestCase(TempHomeTestCase):
    def test_second_apply_writes_nothing(self):
        opts = choices(theme="gruvbox-dark", sound=True, agents=(), unit=True)
        plan1 = first_run.build_plan(
            first_run.detect(self.home, which=fake_which([])), opts)
        kinds = sorted(a["action"] for a in plan1["actions"])
        self.assertEqual(kinds, ["write-alerts-unit", "write-theme"])
        first_run.apply_plan(plan1, runner=boom_runner)
        self.assertTrue((self.home / ".config/mattyx/mux.json").is_file())
        self.assertTrue(
            (self.home / ".config/systemd/user/mtyx-alerts.service").is_file())

        plan2 = first_run.build_plan(
            first_run.detect(self.home, which=fake_which([])), opts)
        self.assertEqual(plan2["actions"], [])
        reasons = {s["action"]: s["reason"] for s in plan2["skipped"]}
        self.assertIn("already applied", reasons["write-theme"])
        self.assertIn("identical", reasons["write-alerts-unit"])

        before = self.tree_digest()
        first_run.apply_plan(plan2, runner=boom_runner)
        self.assertEqual(self.tree_digest(), before)


class PlanBuilderTestCase(TempHomeTestCase):
    def setUp(self):
        super().setUp()
        # Fixtures: pi/claude/antigravity hooked; opencode CLI present but
        # unknown layout; codex/aider/grok CLIs absent; paplay available.
        self.write(".pi/agent/extensions/mtyx.ts", "// mtyx hook\n")
        self.write(".claude/settings.json", json.dumps({
            "hooks": {"PreToolUse": [
                {"hooks": [{"type": "command",
                            "command": "/somewhere/mtyx claude hook"}]}]}}))
        self.write(".gemini/config/hooks.json", json.dumps({
            "hooks": [{"event": "PreToolUse",
                       "command": "/somewhere/mtyx report-agent --surface 1"}]}))
        self.which = fake_which(("pi", "claude", "antigravity", "opencode",
                                 "paplay"))
        self.detection = first_run.detect(self.home, which=self.which)

    def test_detection_statuses(self):
        agents = self.detection["agents"]
        self.assertEqual(agents["pi"]["hook"], "installed")
        self.assertEqual(agents["claude"]["hook"], "installed")
        self.assertEqual(agents["antigravity"]["hook"], "installed")
        self.assertTrue(agents["opencode"]["cli"])
        self.assertEqual(agents["opencode"]["hook"], "unknown")
        self.assertIn("install --only opencode", agents["opencode"]["detail"])
        self.assertFalse(agents["codex"]["cli"])
        self.assertFalse(self.detection["mux_config"]["exists"])
        self.assertTrue(self.detection["sound"]["possible"])

    def test_auto_plan_shape_and_apply(self):
        plan = first_run.build_plan(
            self.detection,
            choices(theme="nord", sound=False, agents=None, unit=True))
        kinds = [a["action"] for a in plan["actions"]]
        self.assertEqual(kinds.count("write-theme"), 1)
        self.assertEqual(kinds.count("write-alerts-unit"), 1)
        hook_actions = [a for a in plan["actions"]
                        if a["action"] == "install-hook"]
        # auto = detected CLI + not known-installed -> only opencode.
        self.assertEqual([a["agent"] for a in hook_actions], ["opencode"])
        self.assertEqual(
            hook_actions[0]["command"],
            [self.detection["mtyx_bin"], "agents", "install", "--only",
             "opencode", "--global"])
        theme_action = next(a for a in plan["actions"]
                            if a["action"] == "write-theme")
        self.assertEqual(theme_action["values"],
                         first_run.load_theme("nord"))
        self.assertEqual(theme_action["path"],
                         str(self.home / ".config/mattyx/mux.json"))
        unit_action = next(a for a in plan["actions"]
                           if a["action"] == "write-alerts-unit")
        self.assertIn("Environment=MATTYX_ALERTS_SOUND=0",
                      unit_action["content"])
        self.assertIn(f"ExecStart=python3 "
                      f"{self.detection['alerts_script']} --on blocked",
                      unit_action["content"])
        self.assertEqual(plan["next_steps"],
                         list(first_run.SYSTEMCTL_NEXT_STEPS))
        json.loads(json.dumps(plan))  # plan must stay JSON-serializable

        # Apply with a recording runner — never shells out to real mtyx.
        calls = []

        def rec_runner(argv):
            calls.append(list(argv))
            return SimpleNamespace(returncode=0, stdout="", stderr="")

        first_run.apply_plan(plan, runner=rec_runner)
        self.assertEqual(
            calls, [[self.detection["mtyx_bin"], "agents", "install",
                     "--only", "opencode", "--global"]])
        self.assertTrue((self.home / ".config/mattyx/mux.json").is_file())
        self.assertTrue(
            (self.home / ".config/systemd/user/mtyx-alerts.service").is_file())

    def test_explicit_agents_and_installed_skip(self):
        plan = first_run.build_plan(
            self.detection,
            choices(theme=None, sound=True,
                    agents=["pi", "claude", "codex"], unit=False))
        hook_actions = [a for a in plan["actions"]
                        if a["action"] == "install-hook"]
        # pi/claude already installed -> skipped; codex explicitly requested.
        self.assertEqual([a["agent"] for a in hook_actions], ["codex"])
        skipped_agents = {s.get("agent") for s in plan["skipped"]
                          if s["action"] == "install-hook"}
        self.assertEqual(skipped_agents, {"pi", "claude"})

    def test_unit_declined_has_no_next_steps(self):
        plan = first_run.build_plan(
            self.detection, choices(theme=None, sound=True, agents=(),
                                    unit=False))
        self.assertEqual(plan["next_steps"], [])
        self.assertTrue(all(a["action"] != "write-alerts-unit"
                            for a in plan["actions"]))


class HookStatusTestCase(TempHomeTestCase):
    def test_pi_variants(self):
        self.assertEqual(first_run.pi_hook_status(self.home)[0],
                         "not-installed")
        self.write(".pi/agent/extensions/mtyx.ts", "// hook\n")
        self.assertEqual(first_run.pi_hook_status(self.home)[0], "installed")
        (self.home / ".pi/agent/extensions/mtyx.ts").unlink()
        self.write(".pi/agent/mtyx.ts", "// hook\n")
        self.assertEqual(first_run.pi_hook_status(self.home)[0], "installed")

    def test_claude_states(self):
        mtyx_bin = str(self.home / "bin" / "mtyx")
        self.assertEqual(
            first_run.claude_hook_status(self.home, mtyx_bin)[0],
            "not-installed")  # no settings.json
        self.write(".claude/settings.json",
                   json.dumps({"agentPushNotifEnabled": True}))
        self.assertEqual(
            first_run.claude_hook_status(self.home, mtyx_bin)[0],
            "not-installed")  # no hooks
        self.write(".claude/settings.json", json.dumps(
            {"hooks": {"PreToolUse": [
                {"hooks": [{"type": "command",
                            "command": f"{mtyx_bin} claude hook"}]}]}}))
        self.assertEqual(
            first_run.claude_hook_status(self.home, mtyx_bin)[0], "installed")
        self.write(".claude/settings.json", "{corrupt json")
        self.assertEqual(
            first_run.claude_hook_status(self.home, mtyx_bin)[0], "unknown")

    def test_antigravity_states(self):
        mtyx_bin = str(self.home / "bin" / "mtyx")
        self.assertEqual(
            first_run.antigravity_hook_status(self.home, mtyx_bin)[0],
            "not-installed")
        self.write(".gemini/config/hooks.json", json.dumps(
            {"hooks": [{"event": "Stop",
                        "command": "'/x/dist/cmux-linux-x86_64' report-agent "
                                   "--source antigravity"}]}))
        self.assertEqual(
            first_run.antigravity_hook_status(self.home, mtyx_bin)[0],
            "installed")

    def test_unknown_layout_agents_never_guess(self):
        detection = first_run.detect(self.home, which=fake_which(["codex"]))
        for name in ("codex", "opencode", "aider", "grok"):
            self.assertEqual(detection["agents"][name]["hook"], "unknown",
                             name)
            self.assertIn(f"install --only {name}",
                          detection["agents"][name]["detail"], name)


class UnitContentTestCase(unittest.TestCase):
    def test_sound_env_lines(self):
        on = first_run.build_unit_content(True, "/x/mattyx-alerts.py")
        self.assertIn("Environment=MATTYX_ALERTS_SOUND=1", on)
        off = first_run.build_unit_content(False, "/x/mattyx-alerts.py")
        self.assertIn("Environment=MATTYX_ALERTS_SOUND=0", off)

    def test_execstart_references_alerts_script(self):
        content = first_run.build_unit_content(True, "/x/mattyx-alerts.py")
        self.assertIn("ExecStart=python3 /x/mattyx-alerts.py --on blocked",
                      content)
        self.assertIn("[Install]", content)
        self.assertIn("WantedBy=default.target", content)


class ParseAgentsTestCase(unittest.TestCase):
    def test_parse_agents_values(self):
        detection = {}  # unused by _parse_agents
        self.assertIsNone(first_run._parse_agents("auto", detection))
        self.assertEqual(first_run._parse_agents("none", detection), [])
        self.assertEqual(first_run._parse_agents("pi,claude", detection),
                         ["pi", "claude"])
        self.assertEqual(first_run._parse_agents(" Pi , GROK ", detection),
                         ["pi", "grok"])
        with self.assertRaises(ValueError):
            first_run._parse_agents("pi,not-an-agent", detection)


class MainEndToEndTestCase(TempHomeTestCase):
    def test_print_plan_zero_writes(self):
        before = self.tree_digest()
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = first_run.main(
                ["--yes", "--theme", "gruvbox-dark", "--sound", "on",
                 "--agents", "none", "--write-alerts-unit", "on",
                 "--print-plan"])
        self.assertEqual(rc, 0)
        plan = json.loads(buf.getvalue())
        self.assertEqual({a["action"] for a in plan["actions"]},
                         {"write-theme", "write-alerts-unit"})
        self.assertEqual(plan["next_steps"],
                         list(first_run.SYSTEMCTL_NEXT_STEPS))
        # Hard guarantee: the temp HOME is byte-identical afterwards.
        self.assertEqual(self.tree_digest(), before)

    def test_yes_requires_choice_flags(self):
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(err):
            rc = first_run.main(["--yes", "--theme", "gruvbox-dark"])
        self.assertEqual(rc, 2)
        self.assertIn("--yes requires", err.getvalue())

    def test_unknown_theme_flag_errors(self):
        err = io.StringIO()
        with self.assertRaises(SystemExit) as ctx, \
                contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(err):
            first_run.main(["--theme", "nope", "--print-plan"])
        self.assertEqual(ctx.exception.code, 2)

    def test_status_read_only(self):
        before = self.tree_digest()
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = first_run.main(["--status"])
        self.assertEqual(rc, 0)
        out = buf.getvalue()
        self.assertIn("mtyx first-run status", out)
        self.assertIn("Agents", out)
        self.assertIn("mux.json", out)
        self.assertIn("absent", out)
        self.assertEqual(self.tree_digest(), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
