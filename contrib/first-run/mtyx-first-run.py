#!/usr/bin/env python3
"""mtyx-first-run — interactive first-run onboarding for mattyx.

Detects the current install state (agent CLIs, agent hooks, mux.json theme,
alerts systemd unit, sound players), prompts for choices (or takes them as
flags), then applies: theme merge into ~/.config/mattyx/mux.json, agent hook
installs via `<mtyx> agents install --only <agent> --global`, and the alerts
systemd user unit. Idempotent: a second run with the same choices writes
nothing.

Read-only/zero-write modes:
    --status      print the detection table and exit 0 (never writes)
    --print-plan  print the JSON action plan and exit 0 (never writes)

Flags (scriptable; --yes requires all four choice flags):
    --yes
    --theme <name|none>            bundled theme from mux/themes/ or "none"
    --sound <on|off>
    --agents <auto|none|a,b,...>   supported: pi, claude, codex, antigravity,
                                   aider, opencode, grok
    --write-alerts-unit <on|off>

Path policy: every user path derives from ONE home resolver (HOME env), so
tests and safe dry-runs can redirect the whole world with HOME=<tempdir>.
The mtyx binary resolves like contrib/alerts/mattyx-alerts.py:
MTYX_BIN env, else <home>/.local/bin/mtyx.

Stdlib only (Python 3.11+ for tomllib).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Callable, Dict, List, Optional, Sequence, Tuple

SUPPORTED_AGENTS = ("pi", "claude", "codex", "antigravity", "aider",
                    "opencode", "grok")
# Theme object keys of ~/.config/mattyx/mux.json (mux-tui config.rs doc-comment).
THEME_KEYS = ("selection_background", "selection_foreground", "sidebar_rail",
              "sidebar_active_bg", "tab_rail", "tab_bg", "tab_active_bg",
              "border_active", "border_inactive")

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
THEMES_DIR = REPO_ROOT / "mux" / "themes"
ALERTS_SCRIPT = REPO_ROOT / "contrib" / "alerts" / "mattyx-alerts.py"

SYSTEMCTL_NEXT_STEPS = ("systemctl --user daemon-reload",
                        "systemctl --user enable --now mtyx-alerts")


class UserAbort(Exception):
    """Interactive input closed (EOF) — abort cleanly, nothing was changed."""


# --------------------------------------------------------------------------
# Single home resolver — every path the tool touches derives from here.
# Tests redirect everything by setting HOME to a temp dir.
# --------------------------------------------------------------------------

def resolve_home() -> Path:
    return Path(os.environ.get("HOME") or os.path.expanduser("~")).expanduser()


def resolve_mtyx_bin(home: Path) -> str:
    """Mirror contrib/alerts/mattyx-alerts.py: MTYX_BIN else ~/.local/bin/mtyx."""
    return os.environ.get("MTYX_BIN") or str(home / ".local/bin/mtyx")


def mux_config_path(home: Path) -> Path:
    return home / ".config/mattyx/mux.json"


def pi_hook_paths(home: Path) -> Tuple[Path, Path]:
    return (home / ".pi/agent/extensions/mtyx.ts", home / ".pi/agent/mtyx.ts")


def claude_settings_path(home: Path) -> Path:
    return home / ".claude/settings.json"


def gemini_hooks_path(home: Path) -> Path:
    return home / ".gemini/config/hooks.json"


def systemd_unit_path(home: Path) -> Path:
    return home / ".config/systemd/user/mtyx-alerts.service"


# --------------------------------------------------------------------------
# Themes
# --------------------------------------------------------------------------

def bundled_themes(themes_dir: Path = THEMES_DIR) -> List[str]:
    if not themes_dir.is_dir():
        return []
    return sorted(p.stem for p in themes_dir.glob("*.toml"))


def load_theme(name: str, themes_dir: Path = THEMES_DIR) -> Dict[str, object]:
    """Read a bundled theme TOML; keys map 1:1 onto the mux.json theme object."""
    path = themes_dir / f"{name}.toml"
    if not path.is_file():
        raise FileNotFoundError(f"no bundled theme {name!r} at {path}")
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    unknown = set(data) - set(THEME_KEYS)
    if unknown:
        raise ValueError(f"theme {name!r} has non-theme keys: {sorted(unknown)}")
    if not data:
        raise ValueError(f"theme {name!r} is empty")
    return dict(data)


# --------------------------------------------------------------------------
# Detection (read-only)
# --------------------------------------------------------------------------

def _first_marker(blob: str, markers: Sequence[str]) -> Optional[str]:
    for marker in markers:
        if marker and marker in blob:
            return marker
    return None


def pi_hook_status(home: Path) -> Tuple[str, str]:
    for path in pi_hook_paths(home):
        if path.is_file():
            return "installed", str(path)
    return "not-installed", f"no {pi_hook_paths(home)[0]}"


def claude_hook_status(home: Path, mtyx_bin: str) -> Tuple[str, str]:
    path = claude_settings_path(home)
    if not path.is_file():
        return "not-installed", f"no {path}"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return "unknown", f"unparseable {path} ({exc})"
    blob = json.dumps(data.get("hooks")) if isinstance(data, dict) else ""
    # Real installs reference the mtyx binary, the literal "claude hook"
    # subcommand, or the report-agent hook protocol (dist-binary installs).
    marker = _first_marker(blob, (mtyx_bin, "claude hook", "report-agent",
                                  "mtyx"))
    if marker:
        return "installed", f"hooks reference {marker!r}"
    return "not-installed", f"no mtyx hooks in {path}"


def antigravity_hook_status(home: Path, mtyx_bin: str) -> Tuple[str, str]:
    path = gemini_hooks_path(home)
    if not path.is_file():
        return "not-installed", f"no {path}"
    try:
        blob = path.read_text(encoding="utf-8")
        json.loads(blob)  # validate shape, we only need the blob
    except (OSError, ValueError) as exc:
        return "unknown", f"unparseable {path} ({exc})"
    marker = _first_marker(blob, (mtyx_bin, "report-agent", "mtyx"))
    if marker:
        return "installed", f"hooks reference {marker!r}"
    return "not-installed", f"no mtyx hooks in {path}"


def detect(home: Path, which: Callable[[str], Optional[str]] = shutil.which,
           themes_dir: Path = THEMES_DIR) -> Dict:
    """Read-only detection pass. `which` is injectable for tests."""
    home = Path(home)
    mtyx_bin = resolve_mtyx_bin(home)
    agents: Dict[str, Dict] = {}
    for name in SUPPORTED_AGENTS:
        cli = which(name) is not None
        if name == "pi":
            hook, detail = pi_hook_status(home)
        elif name == "claude":
            hook, detail = claude_hook_status(home, mtyx_bin)
        elif name == "antigravity":
            hook, detail = antigravity_hook_status(home, mtyx_bin)
        else:
            # Do NOT guess file layouts for the other agents; the installer
            # verb knows how to install them, we just can't detect them.
            hook = "unknown"
            detail = (f"unknown layout — run: {mtyx_bin} agents install "
                      f"--only {name} --global")
        agents[name] = {"cli": cli, "hook": hook, "detail": detail}

    mux = mux_config_path(home)
    mux_info: Dict = {"exists": mux.is_file(), "has_theme": False,
                      "theme": {}, "theme_keys": []}
    if mux_info["exists"]:
        try:
            data = json.loads(mux.read_text(encoding="utf-8"))
            theme = data.get("theme") if isinstance(data, dict) else None
            if isinstance(theme, dict):
                mux_info["has_theme"] = True
                mux_info["theme"] = theme
                mux_info["theme_keys"] = sorted(theme)
        except (OSError, ValueError):
            mux_info["unparseable"] = True

    players = [p for p in ("paplay", "pw-play") if which(p)]
    return {
        "home": str(home),
        "mtyx_bin": mtyx_bin,
        "agents": agents,
        "mux_config": mux_info,
        "alerts_unit": {"exists": systemd_unit_path(home).is_file(),
                        "path": str(systemd_unit_path(home))},
        "sound": {"possible": bool(players), "players": players},
        "themes": bundled_themes(themes_dir),
        "alerts_script": str(ALERTS_SCRIPT),
    }


def render_status(detection: Dict) -> str:
    lines = ["mtyx first-run status",
             f"  home:        {detection['home']}",
             f"  mtyx binary: {detection['mtyx_bin']}",
             "",
             "Agents (CLI on PATH / hook status):"]
    for name in SUPPORTED_AGENTS:
        st = detection["agents"][name]
        cli = "cli: found" if st["cli"] else "cli: absent"
        if st["hook"] == "unknown":
            lines.append(f"  {name:<12} {cli}; hooks: {st['detail']}")
        else:
            state = "installed" if st["hook"] == "installed" else "not installed"
            lines.append(f"  {name:<12} {cli}; hooks: {state} ({st['detail']})")
    mux = detection["mux_config"]
    if not mux["exists"]:
        mux_desc = "absent"
    elif mux.get("unparseable"):
        mux_desc = "present but unparseable"
    elif mux["has_theme"]:
        present = [k for k in THEME_KEYS if k in mux["theme"]]
        mux_desc = "present, theme keys: " + (", ".join(present) or "(empty)")
    else:
        mux_desc = "present, no theme object"
    unit = "present" if detection["alerts_unit"]["exists"] else "absent"
    players = detection["sound"]["players"]
    sound = (f"available ({', '.join(players)})" if players
             else "not available (no paplay/pw-play on PATH)")
    lines += ["",
              f"  mux.json:       {mux_desc}",
              f"                  ({mux_config_path(Path(detection['home']))})",
              f"  alerts unit:    {unit}",
              f"                  ({detection['alerts_unit']['path']})",
              f"  sound:          {sound}",
              f"  bundled themes: {', '.join(detection['themes']) or '(none found)'}"]
    return "\n".join(lines)


# --------------------------------------------------------------------------
# Prompts (interactive only; skipped when flags are passed or in --yes mode)
# --------------------------------------------------------------------------

def prompt_theme(detection: Dict) -> Optional[str]:
    themes = detection["themes"]
    print("Theme:")
    for i, name in enumerate(themes, 1):
        print(f"  {i}. {name}")
    print("  0. keep current")
    while True:
        raw = input(f"Choose theme [0-{len(themes)}, default 0]: ").strip()
        if raw == "":
            return None
        if raw.isdigit() and int(raw) <= len(themes):
            return None if int(raw) == 0 else themes[int(raw) - 1]
        if raw in themes:
            return raw
        print(f"  invalid choice: {raw!r}")


def prompt_sound(detection: Dict) -> bool:
    default = "on" if detection["sound"]["possible"] else "off"
    while True:
        raw = input(f"Sound on alerts? [on/off, default {default}]: "
                    ).strip().lower()
        if raw == "":
            return default == "on"
        if raw in ("on", "off"):
            return raw == "on"
        print("  enter 'on' or 'off'")


def prompt_agents(detection: Dict) -> List[str]:
    candidates = [n for n in SUPPORTED_AGENTS
                  if detection["agents"][n]["cli"]
                  and detection["agents"][n]["hook"] != "installed"]
    if not candidates:
        print("Hooks: every detected agent already has mtyx hooks.")
        return []
    print("Install mtyx hooks for:")
    for i, name in enumerate(candidates, 1):
        print(f"  {i}. {name}")
    print("  (comma-separated numbers, 'all', or 'none'; default all)")
    while True:
        raw = input(f"Agents to hook [1-{len(candidates)}/all/none]: "
                    ).strip().lower()
        if raw in ("", "all"):
            return list(candidates)
        if raw == "none":
            return []
        picks: List[str] = []
        valid = True
        for tok in raw.replace(" ", "").split(","):
            if tok.isdigit() and int(tok) <= len(candidates):
                picks.append(candidates[int(tok) - 1])
            elif tok in candidates:
                picks.append(tok)
            else:
                valid = False
                break
        if valid and picks:
            return picks
        print(f"  invalid selection: {raw!r}")


def prompt_unit(detection: Dict) -> bool:
    while True:
        raw = input("Write the alerts systemd user unit? [y/n, default y]: "
                    ).strip().lower()
        if raw == "":
            return True
        if raw in ("y", "yes", "n", "no"):
            return raw in ("y", "yes")
        print("  enter 'y' or 'n'")


# --------------------------------------------------------------------------
# Plan building (pure; no writes)
# --------------------------------------------------------------------------

def theme_already_applied(existing_theme: Dict, theme: Dict) -> bool:
    return all(existing_theme.get(k) == v for k, v in theme.items())


def build_unit_content(sound_on: bool, alerts_script: str) -> str:
    return (
        "[Unit]\n"
        "Description=mattyx agent-state alerts (audio + desktop notification)\n"
        f"Documentation=file://{Path(alerts_script).parent / 'README.md'}\n"
        "\n"
        "[Service]\n"
        "Type=simple\n"
        f"Environment=MATTYX_ALERTS_SOUND={'1' if sound_on else '0'}\n"
        "Environment=MATTYX_ALERTS_NOTIFY=1\n"
        f"ExecStart=python3 {alerts_script} --on blocked\n"
        "Restart=on-failure\n"
        "RestartSec=5\n"
        "\n"
        "[Install]\n"
        "WantedBy=default.target\n"
    )


def auto_agents(detection: Dict) -> List[str]:
    """Detected CLI agents whose hooks are not known-installed."""
    return [n for n in SUPPORTED_AGENTS
            if detection["agents"][n]["cli"]
            and detection["agents"][n]["hook"] != "installed"]


def build_plan(detection: Dict, choices: Dict) -> Dict:
    """Build the action plan. Pure: reads detection, writes nothing."""
    home = Path(detection["home"])
    actions: List[Dict] = []
    skipped: List[Dict] = []

    # Theme
    if not choices["theme"]:
        skipped.append({"action": "write-theme",
                        "reason": "keep current theme"})
    else:
        theme = load_theme(choices["theme"])
        if theme_already_applied(detection["mux_config"]["theme"], theme):
            skipped.append({"action": "write-theme",
                            "reason": f"{choices['theme']} already applied"})
        else:
            actions.append({"action": "write-theme",
                            "theme": choices["theme"],
                            "path": str(mux_config_path(home)),
                            "values": theme})

    # Agent hooks
    selected = (auto_agents(detection) if choices["agents"] is None
                else choices["agents"])
    for name in selected:
        st = detection["agents"][name]
        if st["hook"] == "installed":
            skipped.append({"action": "install-hook", "agent": name,
                            "reason": "already installed"})
            continue
        actions.append({"action": "install-hook", "agent": name,
                        "command": [detection["mtyx_bin"], "agents",
                                    "install", "--only", name, "--global"],
                        "cli_found": st["cli"]})

    # Alerts systemd unit
    if not choices["write_alerts_unit"]:
        skipped.append({"action": "write-alerts-unit", "reason": "declined"})
    else:
        content = build_unit_content(bool(choices["sound"]),
                                     detection["alerts_script"])
        unit_path = Path(detection["alerts_unit"]["path"])
        identical = (unit_path.is_file()
                     and unit_path.read_text(encoding="utf-8") == content)
        if identical:
            skipped.append({"action": "write-alerts-unit",
                            "reason": "already installed (identical)"})
        else:
            actions.append({"action": "write-alerts-unit",
                            "path": str(unit_path), "content": content})

    unit_written = any(a["action"] == "write-alerts-unit" for a in actions)
    unit_identical = any(s["action"] == "write-alerts-unit"
                         and "identical" in s["reason"] for s in skipped)
    return {
        "home": str(home),
        "mtyx_bin": detection["mtyx_bin"],
        "detection_summary": {
            "agents": {n: {"cli": detection["agents"][n]["cli"],
                           "hook": detection["agents"][n]["hook"]}
                       for n in SUPPORTED_AGENTS},
            "mux_config_exists": detection["mux_config"]["exists"],
            "mux_has_theme": detection["mux_config"]["has_theme"],
            "alerts_unit_exists": detection["alerts_unit"]["exists"],
            "sound_possible": detection["sound"]["possible"],
        },
        "choices": {"theme": choices["theme"], "sound": choices["sound"],
                    "agents": choices["agents"],
                    "write_alerts_unit": choices["write_alerts_unit"]},
        "actions": actions,
        "skipped": skipped,
        "next_steps": (list(SYSTEMCTL_NEXT_STEPS)
                       if (unit_written or unit_identical) else []),
    }


# --------------------------------------------------------------------------
# Apply
# --------------------------------------------------------------------------

def atomic_write_text(path: Path, text: str) -> None:
    """Temp file in the target dir + rename, so readers never see a torn file."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=path.name + ".",
                               suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(text)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def default_runner(argv: List[str]):
    return subprocess.run(argv, capture_output=True, text=True)


def apply_plan(plan: Dict, runner: Callable[[List[str]], object] = default_runner
               ) -> List[str]:
    """Apply plan actions. Failures are reported per action, never fatal."""
    results: List[str] = []
    for act in plan["actions"]:
        kind = act["action"]
        if kind == "write-theme":
            path = Path(act["path"])
            try:
                data: Dict = {}
                if path.is_file():
                    parsed = json.loads(path.read_text(encoding="utf-8"))
                    if not isinstance(parsed, dict):
                        raise ValueError("mux.json root is not an object")
                    data = parsed
                theme = data.get("theme")
                if not isinstance(theme, dict):
                    theme = {}
                theme.update(act["values"])
                data["theme"] = theme
                atomic_write_text(path, json.dumps(data, indent=2) + "\n")
                results.append(f"theme: wrote {act['theme']} -> {path}")
            except Exception as exc:  # noqa: BLE001 — report, don't crash
                results.append(f"theme: FAILED ({exc!r}); "
                               f"{path} left untouched")
        elif kind == "install-hook":
            try:
                proc = runner(act["command"])
                code = getattr(proc, "returncode", 1)
                if code == 0:
                    results.append(f"hooks: installed for {act['agent']}")
                else:
                    err = (getattr(proc, "stderr", None)
                           or getattr(proc, "stdout", None) or "")
                    results.append(f"hooks: FAILED for {act['agent']} "
                                   f"(exit {code}): {str(err)[:200]}")
            except Exception as exc:  # noqa: BLE001
                results.append(f"hooks: FAILED for {act['agent']} ({exc!r})")
        elif kind == "write-alerts-unit":
            try:
                atomic_write_text(Path(act["path"]), act["content"])
                results.append(f"alerts: wrote {act['path']}")
            except Exception as exc:  # noqa: BLE001
                results.append(f"alerts: FAILED ({exc!r})")
    return results


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="mtyx-first-run.py",
        description="mattyx first-run onboarding: detect, prompt, apply "
                    "(theme, agent hooks, alerts systemd unit).")
    parser.add_argument("--yes", action="store_true",
                        help="non-interactive; requires --theme, --sound, "
                             "--agents and --write-alerts-unit")
    parser.add_argument("--theme", metavar="NAME",
                        help="bundled theme name (mux/themes/*.toml) or 'none' "
                             "to keep the current theme")
    parser.add_argument("--sound", choices=("on", "off"),
                        help="sound leg of the alerts listener")
    parser.add_argument("--agents", metavar="LIST",
                        help="'auto' (detected, not yet hooked), 'none', or a "
                             f"comma list of: {', '.join(SUPPORTED_AGENTS)}")
    parser.add_argument("--write-alerts-unit", choices=("on", "off"),
                        dest="write_alerts_unit",
                        help="write ~/.config/systemd/user/mtyx-alerts.service")
    parser.add_argument("--print-plan", action="store_true",
                        help="print the JSON action plan and exit 0 "
                             "WITHOUT applying anything")
    parser.add_argument("--status", action="store_true",
                        help="print the detection table and exit 0 "
                             "(read-only)")
    return parser


def _parse_agents(value: str, detection: Dict) -> Optional[List[str]]:
    """'auto' -> None sentinel; 'none' -> []; else validated name list."""
    v = value.strip().lower()
    if v == "auto":
        return None
    if v == "none" or not v:
        return []
    names = [t.strip().lower() for t in v.split(",") if t.strip()]
    bad = [n for n in names if n not in SUPPORTED_AGENTS]
    if bad:
        raise ValueError(f"unsupported agent(s) {bad}; "
                         f"supported: {', '.join(SUPPORTED_AGENTS)}")
    return names


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    home = resolve_home()
    detection = detect(home)

    if args.status:
        print(render_status(detection))
        return 0

    choices: Dict = {"theme": None, "sound": None, "agents": None,
                     "write_alerts_unit": None}
    provided = set()
    if args.theme is not None:
        name = args.theme.strip()
        if name != "none":
            if name not in detection["themes"]:
                parser.error(f"unknown theme {name!r}; bundled themes: "
                             f"{', '.join(detection['themes'])}")
        choices["theme"] = None if name == "none" else name
        provided.add("theme")
    if args.sound is not None:
        choices["sound"] = args.sound == "on"
        provided.add("sound")
    if args.agents is not None:
        try:
            choices["agents"] = _parse_agents(args.agents, detection)
        except ValueError as exc:
            parser.error(str(exc))
        provided.add("agents")
    if args.write_alerts_unit is not None:
        choices["write_alerts_unit"] = args.write_alerts_unit == "on"
        provided.add("unit")

    if args.yes:
        missing = ({"theme", "sound", "agents", "unit"} - provided)
        if missing:
            print(f"error: --yes requires all choice flags; missing: "
                  f"{', '.join(sorted(missing))}", file=sys.stderr)
            return 2
    elif not args.print_plan:
        # Interactive: prompt only for choices not given as flags.
        try:
            if "theme" not in provided:
                choices["theme"] = prompt_theme(detection)
            if "sound" not in provided:
                choices["sound"] = prompt_sound(detection)
            if "agents" not in provided:
                choices["agents"] = prompt_agents(detection)
            if "unit" not in provided:
                choices["write_alerts_unit"] = prompt_unit(detection)
        except EOFError:
            raise UserAbort("input closed (EOF)")
        except KeyboardInterrupt:
            raise UserAbort("interrupted")
    else:
        # --print-plan without --yes: non-interactive defaults for anything
        # not passed as a flag (documented; zero writes regardless).
        if "sound" not in provided:
            choices["sound"] = detection["sound"]["possible"]
        if "agents" not in provided:
            choices["agents"] = None  # auto
        if "unit" not in provided:
            choices["write_alerts_unit"] = True

    plan = build_plan(detection, choices)

    # --print-plan: HARD GUARANTEE — exit before any apply step, zero writes.
    if args.print_plan:
        print(json.dumps(plan, indent=2, sort_keys=True))
        return 0

    if not plan["actions"]:
        print("Nothing to do — everything already configured.")
    else:
        print(f"Applying {len(plan['actions'])} action(s) "
              f"({len(plan['skipped'])} already done/skipped):")
        for line in apply_plan(plan):
            print(f"  {line}")
    if plan["skipped"]:
        for s in plan["skipped"]:
            who = s.get("agent") or s["action"]
            print(f"  skipped: {who} ({s['reason']})")
    if plan["next_steps"]:
        print("Next steps (run these manually — NOT run by this tool):")
        for cmd in plan["next_steps"]:
            print(f"  {cmd}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except UserAbort as abort:
        print(f"\naborted before any changes: {abort}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("\naborted before any changes", file=sys.stderr)
        sys.exit(1)
