//! mtyx: a tmux-like terminal multiplexer TUI.
//!
//! Runs the mux core (workspaces → split panes → tabs on real PTYs,
//! terminal state from libghostty-vt) with a Ratatui frontend, and always
//! exposes the JSON control socket so external frontends can attach.
//! `mtyx attach` connects the same TUI to an existing (usually
//! headless) session over that socket, which is how detach/reattach works.

mod agents;
mod aider_hook;
mod antigravity_hook;
mod app;
mod browser_input;
mod claude_hook;
mod cli;
mod clipboard;
mod codex_hook;
mod config;
mod desktop_notify;
mod finder;
mod git_info;
mod grok_hook;
mod help;
mod hook_merge;
mod host_colors;
mod keys;
mod opencode_hook;
mod pi_hook;
mod plugin;
mod plugin_host;
mod session;
mod session_manager;
mod session_picker;
mod skill_content;
mod socket_watchdog;
mod ssh_bootstrap;
mod theme;
mod ui;
#[cfg(windows)]
mod win_console;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use mux_core::{Mux, SurfaceOptions};
use session::{RemoteSession, Session};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
}

/// Transition shim (rename compat): map every cmux-era `CMUX_*` env var
/// onto its canonical `MTYX_*` spelling, but only where the `MTYX_*`
/// counterpart is not already set, so an explicit new-name value always
/// wins. Runs before anything else in `main` so every downstream
/// `MTYX_*` read (socket discovery, config, plugin paths, hook
/// installers, the env inherited by PTY children) sees the merged view.
/// Build-time-only variables such as `MTYX_VERSION` are handled by the
/// build scripts' own fallbacks, not here.
fn honor_legacy_env() {
    for (name, value) in legacy_env_pairs(std::env::vars()) {
        if std::env::var_os(&name).is_none() {
            std::env::set_var(&name, &value);
        }
    }
}

/// Pure core of [`honor_legacy_env`]: from an env var stream, the
/// `(MTYX_*, value)` pairs implied by each `CMUX_*` entry. Split out so
/// the mapping is unit-testable without mutating process env.
fn legacy_env_pairs(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.filter_map(|(key, value)| {
        let suffix = key.strip_prefix("CMUX_")?;
        Some((format!("MTYX_{suffix}"), value))
    })
    .collect()
}

pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}

/// Install the terminate-shutdown hook: SIGTERM/SIGINT/SIGHUP on unix;
/// CTRL_C/CTRL_BREAK/CTRL_CLOSE via `SetConsoleCtrlHandler` on Windows
/// (routed to the same `SHUTDOWN_REQUESTED` flag, so the shutdown path
/// below is shared verbatim). The handler only flips the flag — the
/// main loops poll it, so everything stays async-signal-safe.
#[cfg(unix)]
fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handle_signal as *const () as libc::sighandler_t);
    }
}

#[cfg(windows)]
fn install_signal_handlers() {
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    };

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        if matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
            SHUTDOWN_REQUESTED.store(true, Ordering::Release);
            // Handled: ask the OS not to also terminate us before the
            // main loop finishes its graceful pass.
            1
        } else {
            0
        }
    }

    unsafe {
        // Failure is non-fatal (same posture as the unix path, where a
        // failed signal(2) is silently ignored): worst case Ctrl-C gets
        // the OS default handling.
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

const USAGE: &str = "\
mtyx - terminal multiplexer backed by libghostty-vt

USAGE:
  mtyx [OPTIONS]           Start a session (TUI + control socket)
  mtyx attach [OPTIONS]    Attach to an existing session's socket
  mtyx <verb> [OPTIONS]    Run one control-socket command
  mtyx workspace-color <name> <color>  Set a named workspace colour
  mtyx claude <subcommand> Claude Code hook integration (see below)
  mtyx antigravity install-hooks  Antigravity CLI hook integration (see below)
  mtyx codex install-hooks        Codex CLI hook integration (see below)
  mtyx pi install-hooks           Pi agent extension integration (see below)
  mtyx aider install-hooks        Aider wrapper integration (see below)
  mtyx grok install-hooks         Grok CLI hook integration (see below)
  mtyx opencode install-hooks     opencode plugin integration (see below)
  mtyx agents <list|install>     Manage all agent hook integrations (see below)
  mtyx plugin <subcommand> Manage mtyx-plugin.toml manifests (see below)
  mtyx ssh <host> [OPTS]   Open a remote workspace over SSH (see below)

OPTIONS:
  --session <name>   Session name (default: main). Determines the socket path.
  --socket <path>    Explicit control socket path.
  --headless         Run only the control socket, no TUI.
  --term <value>     TERM for child shells (default: xterm-256color).
  --apply-local-config
                    Attach only: overlay the local mux.local.toml/mux.json
                    (theme, tabs, sidebar, keys) on top of the server config.
  --config <path>    Attach only: explicit local overlay file (overrides
                    $MTYX_LOCAL_CONFIG and the XDG defaults).
  --show-local-config-resolution
                    Attach only: print which local config would apply and
                    how many keys it overrides, then exit without attaching.
  --print-resolved-config
                    Attach only: fetch the server's resolved presentation
                    chrome, layer the local overlay on top (requires
                    --apply-local-config), print the merged chrome as JSON,
                    and exit without starting the TUI. For inspecting
                    overlay layering without a live terminal.
  --session-list     Attach only: discover sessions and either print them
                    (--json) or open the interactive picker instead of
                    attaching directly.
  --json             With --session-list: print the discovered sessions as
                    JSON (one object per session, including socket_path)
                    and exit without attaching.
  -V, --version      Print the mtyx version and exit.
  -h, --help         Show this help.

SESSION PICKER  (mtyx attach --session-list, without --json)
  Lists every discovered mtyx session (newest first) and lets you pick one
  to attach in-process. Stale (unconnectable) sessions are shown grey and
  labelled [unreachable]. Exit codes: 0 clean quit, 1 after a destructive
  kill + quit, 2 Ctrl-C, 0 on attach (then the normal attach/detach flow).
    ↑/↓ or j/k  move focus        Enter  attach to focused (live only)
    x  kill focused session (y/N)    s  kill every stale session (y/N)
    n  new session (inline name)     r  rename focused session (inline)
    q / Esc  quit                    Ctrl-C  abort (exit 2)

KEYS (prefix: Ctrl-b)
  c  new tab in pane   B    new browser tab    n/p  next/prev tab
  1-9  select tab
  %  split right       \"  split down          x    close tab
  ,  rename pane       $    rename workspace
  Tab  next screen     S    session manager
  h/j/k/l or arrows    move focus              d    quit (attach: detach)
  w  next workspace    W    new workspace       s    toggle sidebar
  <  browser back      >    browser forward     r/u  browser reload/edit URL
  ?  show key binding help
  Ctrl-b  send a literal Ctrl-b

MOUSE
  Right-click a pane for rename/new tab/split/close; right-click a
  sidebar workspace or a status-bar screen for rename/close. Click
  tab-bar entries to switch tabs (+ for a new tab), and status-bar
  screen entries to switch screens (+ for a new screen).

CLI VERBS
  identify, list-workspaces, send, read-screen, vt-state, new-tab,
  new-browser-tab, new-workspace, new-screen, split, set-ratio,
  set-default-colors, close-surface, close-pane, close-screen,
  close-workspace, rename-pane, rename-surface, rename-screen,
  rename-workspace, set-workspace-color, set-status, workspace-color,
  trigger-flash, resize-surface,
  focus-pane, select-tab, select-screen, select-workspace, move-tab,
  move-workspace, scroll-surface, subscribe, attach-surface, report-agent,
  list-agents, agent-read, agent-send, wait-agent-status, detect-agent,
  detect-agents, agent-pattern-add, agent-pattern-list,
  agent-pattern-remove, browser-reload, list-sessions,
  kill-session, kill-stale, rename-session, layout-export, layout-apply,
  layout-export-all, theme list,
  pane-worktree-create, pane-worktree-list, pane-worktree-remove
      (also spelled `mtyx pane worktree <create|list|remove>`; issue #77)

SEND
  mtyx send --surface <id> --text <text> [--shell auto|fish|bash|zsh|sh|nu|raw]
      Writes input to a PTY surface (stdin is used when neither --text nor
      --bytes is given). --shell enables shell-aware sanitisation (issue
      #35): with fish/bash/zsh/nu, a leading newline is prefixed when the
      text starts with a shell metacharacter ($, !, quote, bracket, ~, #)
      or contains an unclosed quote, so '$ pwd\n' is typed literally
      instead of being interpreted by the shell's line editor. auto
      resolves the pane's shell from /proc on Linux. Default: raw
      (verbatim passthrough, unchanged from before).

LAYOUT EXPORT/APPLY (issue #76)
  mtyx layout-export --workspace <name-or-id> --output <file>.json
      Save one workspace's tab/pane/agent-argv topology as versioned JSON
      (schema 1, see spec/layout-schema.md). Client-side atomic write
      (tmp + rename); symlinked outputs are refused.
  mtyx layout-export-all --output-dir <dir>
      Save every workspace in the session as <dir>/<name>.json (mkdir -p).
  mtyx layout-apply --input <file>.json --workspace <name>
      Replay a saved layout: the workspace is created if missing; applying
      onto an existing name is refused (close it first or pick a new
      name). Panes spawn with the recorded argv/env/cwd; a failure aborts
      loudly naming the pane (index + pane-id).
  mtyx new-tab [--pane N] [--cwd P] [--env K=V,...] --exec -- <argv...>
  mtyx split --pane N --dir <right|down> [--env K=V,...] --exec -- <argv...>
      Spawn with an explicit command (agent start): everything after the
      literal `--` that follows --exec is the verbatim argv, so --exec
      must be the LAST flag. --env is a comma-separated K=V list.
      Layout-export records these argv/env pairs; for remote sessions
      compose `layout-apply` against the remote socket with a follow-up
      `mtyx attach --apply-local-config`.

AGENT DETECTION
  mtyx detect-agent --surface <id>
      Ambiently detect which AI agent is running in a pane (issue #78):
      walks the pane PTY's process tree (/proc comm/cmdline) and scrapes
      the visible screen against the pattern registry. Prints
      `<surface> <agent> <confidence> <evidence>` (agent is one of
      claude, codex, pi, opencode, cursor, aider, unknown — plus any
      user-added names; confidence is high/medium/low or none). The
      result is cached and surfaces in `list-workspaces` as agent_name.
  mtyx detect-agents
      Detection on every pane in one call: `<surface> <agent>` rows
      (the issue's `agent detect-batch`; --json prints
      {\"agents\":{\"<surface>\":\"<agent>\"}} — keys are surface ids).
  mtyx agent-pattern <add|list|remove>
      Manage the live detection registry. add: `mtyx agent-pattern add
      <name> --pattern <marker> [--kind process|screen] [--confidence
      high|medium|low] [--case-insensitive]`. Patterns are
      substring/glob ('*' wildcard), NOT regex; process patterns match
      whole argv tokens (a bare `pi` pattern never matches `spider`).
      kind defaults to screen; confidence defaults to medium. Bundled
      patterns for the top-6 agents ship in agents.json and cannot be
      removed. Custom patterns live for the daemon's session (v1).
  Config: [[agent_detection]] in mux.toml (or \"agent_detection\" in
      mux.json) — `enabled = true|false` (default true) and
      `min_confidence = \"high\"|\"medium\"|\"low\"` (default \"low\"). With
      detection disabled, the detect verbs error with
      `agent detection disabled by configuration`.

AGENT STATE (issue #75)
  mtyx report-agent [--surface <id>] --state <idle|working|blocked|done|unknown>
                    [--source <detected|socket|hook>] [--agent-session <id>]
                    [--agent <name>] [--message <text>]
      Agent self-report. --surface defaults to $MTYX_MUX_SURFACE (set in
      every pane, so an agent can report from inside its pane) and
      --source defaults to socket (hook reports keep authority).
  mtyx list-agents [--surface <id>] [--state <state>]
      Every pane with a report: surface, state, source, session, agent
      name, last message. JSON via --json (includes updated_at_ms).
  mtyx agent-read --target <name-or-surface-id>
                   [--source visible|recent|recent-unwrapped] [--lines <n>]
      Read an agent's pane by name; tails the last N lines (default 40).
  mtyx agent-send --target <name-or-surface-id> --text <text> [--shell <mode>]
      Type text into an agent's pane WITHOUT Enter; submit separately
      (e.g. mtyx send --surface <id> --text \"\" --send-cr 1).
  mtyx wait-agent-status --target <name-or-surface-id>
                         --status <state> --timeout <ms>
      Block until the agent reaches --status; prints the pane text,
      exit 1 on timeout. --timeout 0 = single immediate check.

RENAME-SESSION
  mtyx rename-session --old <name> --new <name> [--json]
      Renames a live mtyx session in place: moves its .sock/.pid to the new
      name, updates the session name, reparents the snapshot file, and keeps
      the SAME daemon serving at the new path (the listener is never rebound
      — rename(2) reparents the socket dirent while the kernel keeps it
      bound). A live target is refused; a stale target is cleared first.
      Exit codes: 0 success · 1 source not found / server error · 2 bad/missing
      flags, invalid name, or target already live · 3 connect failure.
      Lifetime guarantee (AC4): EXISTING panes keep the MTYX_MUX_SOCKET they
      inherited at spawn (the old path) for their lifetime — this is
      intentional, not a bug. Panes spawned AFTER the rename inherit the new
      path. See the server.rs docstring for the mechanism and the watchdog
      caveat (a SIGKILL after rename is cleaned by the next serve()/kill-stale).

CLAUDE CODE HOOK INTEGRATION
  mtyx claude install-hooks [--uninstall]
      Wires ~/.claude/settings.json's hooks to call `mtyx claude hook`
      on every lifecycle event, merged alongside any hooks already there.
  mtyx claude install-skill [--uninstall] [--global]
      Installs the orchestration skill to .claude/skills/mtyx-orchestration/SKILL.md
      (or ~/.claude/skills/mtyx-orchestration/SKILL.md if --global).
  mtyx claude sessions
      Lists recorded Claude Code sessions (session id, cwd, last event).
  mtyx claude resume <session-id>
      Opens a new pane in the recorded cwd and runs `claude --resume`.
  mtyx claude hook
      Not for interactive use — this is what install-hooks points Claude
      Code's own hook config at.

ANTIGRAVITY CLI INTEGRATION
  mtyx antigravity install-hooks [--uninstall] [--global]
      Installs hooks into .agents/hooks.json (or ~/.gemini/config/hooks.json if --global)
      to automatically report state changes to mtyx.
  mtyx antigravity install-skill [--uninstall] [--global]
      Installs the orchestration skill to .agents/skills/mtyx-orchestration/SKILL.md
      (or ~/.gemini/antigravity-cli/skills/mtyx-orchestration/SKILL.md if --global).

CODEX CLI INTEGRATION
  mtyx codex install-hooks [--uninstall] [--global]
      Installs hooks into .codex/hooks.json (or ~/.codex/hooks.json if --global) and
      enables hooks feature in config.toml to report state to mtyx.
  mtyx codex install-skill [--uninstall] [--global]
      Installs the orchestration skill to .agents/skills/mtyx-orchestration/SKILL.md
      (or ~/.codex/skills/mtyx-orchestration/SKILL.md if --global).

PI AGENT INTEGRATION
  mtyx pi install-hooks [--uninstall] [--global]
      Installs TypeScript extensions into .pi/extensions/ (or ~/.pi/agent/extensions/
      if --global) to report state changes.
  mtyx pi install-skill [--uninstall] [--global]
      Appends the orchestration skill to .pi/APPEND_SYSTEM.md
      (or ~/.pi/agent/APPEND_SYSTEM.md if --global).

AIDER INTEGRATION
  mtyx aider install-hooks [--uninstall] [--global]
      Creates a wrapper script at .bin/aider (or ~/.local/bin/aider if --global)
      that wraps the real aider binary to report working/done state.

GROK CLI INTEGRATION
  mtyx grok install-hooks [--uninstall] [--global]
      Installs hooks into .grok/hooks/mtyx-agent-state.json (or
      ~/.grok/hooks/mtyx-agent-state.json if --global) in the schema Grok
      Build actually loads, so panes report working/idle/blocked/done.
  mtyx grok install-skill [--uninstall] [--global]
      Installs the orchestration skill to .agents/skills/mtyx-orchestration/SKILL.md
      (or ~/.grok/skills/mtyx-orchestration/SKILL.md if --global).

AGENT HOOK INTEGRATION
  mtyx agents list [--global]
      Lists installed status, version, timestamp, and path for all six agents.
  mtyx agents install --all [--uninstall] [--global]
      Installs or removes every registered agent hook, continuing after failures.
  mtyx agents install --only <agent> [--uninstall] [--global]
      Installs or removes one registered agent hook.

PLUGIN LOADER (manifest + registry only; no execution yet)
  mtyx plugin list                       List installed plugins (read-only)
  mtyx plugin install <manifest-path>    Install a plugin from a mtyx-plugin.toml
  mtyx plugin uninstall <name>           Remove an installed plugin
  mtyx plugin enable <name>              Mark a plugin enabled
  mtyx plugin disable <name>             Mark a plugin disabled

      These verbs only manage on-disk manifest state and a small JSON
      registry under ~/.local/share/mattyx/plugins.json. Plugin *execution*
      (proxying `mtyx <plugin-name> <verb>` to a running plugin process,
      WASM/WASI sandboxing) is NOT implemented by this verb group and is
      deferred to a follow-up PR.

REMOTE (SSH) WORKSPACES
  mtyx ssh <host> [--name <workspace-name>] [--session <mux-session>]
      Opens a workspace whose tab is a shell on <host> instead of local.
      Builds and caches a cmuxd-remote binary for the remote's OS/arch
      the first time (needs Go on PATH), uploads it, and starts it in
      persistent mode: closing the tab detaches without killing the
      remote shell, and this session's own daemon restarting reattaches
      to it automatically (see mux/docs/getting-started.md).
";

#[derive(Clone)]
struct Args {
    attach: bool,
    session: String,
    socket: Option<PathBuf>,
    headless: bool,
    term: Option<String>,
    apply_local_config: bool,
    show_local_config_resolution: bool,
    print_resolved_config: bool,
    config: Option<PathBuf>,
    // Issue #63 L1: `mtyx attach --session-list [--json]` — discover
    // sessions and either dump them as JSON or open the interactive picker
    // before attaching. Parsed on the `attach` subcommand in parse_args.
    session_list: bool,
    json: bool,
}

/// mtyx version, resolved at build time from the release tag and baked
/// into the binary by `mux-core`'s build script. Surfaced by
/// `mtyx --version` / `mtyx -V` (issue #59), by the control socket's
/// `identify` reply, and stamped into the `cmuxd-remote` daemon we
/// cross-compile for `mtyx ssh`. Reading `CARGO_PKG_VERSION` here is
/// what made `-V` report a stale `0.1.0` (issue #71).
const VERSION: &str = mux_core::VERSION;

fn parse_args(args: impl IntoIterator<Item = String>) -> Args {
    let mut out = Args {
        attach: false,
        session: "main".to_string(),
        socket: None,
        headless: false,
        term: None,
        apply_local_config: false,
        show_local_config_resolution: false,
        print_resolved_config: false,
        config: None,
        session_list: false,
        json: false,
    };
    let mut args = args.into_iter().peekable();
    if args.peek().map(|s| s.as_str()) == Some("attach") {
        out.attach = true;
        args.next();
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => {
                out.session = args.next().unwrap_or_else(|| usage_exit("--session needs a value"))
            }
            "--socket" => {
                out.socket =
                    Some(args.next().unwrap_or_else(|| usage_exit("--socket needs a value")).into())
            }
            "--headless" => out.headless = true,
            // Issue #63 L1: attach-only session discovery flags.
            "--session-list" => out.session_list = true,
            "--json" => out.json = true,
            "--term" => {
                out.term = Some(args.next().unwrap_or_else(|| usage_exit("--term needs a value")))
            }
            "--apply-local-config" => out.apply_local_config = true,
            "--show-local-config-resolution" => out.show_local_config_resolution = true,
            "--print-resolved-config" => out.print_resolved_config = true,
            "--config" => {
                out.config =
                    Some(args.next().unwrap_or_else(|| usage_exit("--config needs a value")).into())
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            // Issue #59: print version and exit. Sits next to `-h`/`--help`
            // so it works in any position (e.g. `mtyx --headless -V`).
            "-V" | "--version" => {
                println!("mtyx {VERSION}");
                std::process::exit(0);
            }
            other => usage_exit(&format!("unknown argument {other:?}")),
        }
    }
    out
}

fn main() {
    honor_legacy_env();
    install_signal_handlers();
    let mut raw_args = std::env::args().skip(1).collect::<Vec<_>>();
    if raw_args.first().map(|arg| arg.as_str()) == Some("help") {
        print!("{USAGE}");
        std::process::exit(0);
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("claude") {
        std::process::exit(claude_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("antigravity") {
        std::process::exit(antigravity_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("codex") {
        std::process::exit(codex_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("pi") {
        std::process::exit(pi_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("aider") {
        std::process::exit(aider_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("grok") {
        std::process::exit(grok_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("opencode") {
        std::process::exit(opencode_hook::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("theme") {
        match raw_args.get(1).map(String::as_str) {
            Some("list") => std::process::exit(theme::run_list()),
            _ => {
                eprintln!("mtyx: usage: mtyx theme list");
                std::process::exit(2);
            }
        }
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("agents") {
        std::process::exit(agents::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("plugin") {
        std::process::exit(plugin::run(&raw_args[1..]));
    }
    // `mtyx <plugin-name> <verb> [args]` — if the first positional arg
    // names an installed, enabled plugin, route the rest of the argv
    // through plugin_host::invoke. Falls through to the standard
    // verb dispatch if the name doesn't match a plugin.
    if let Some(first) = raw_args.first().map(String::as_str) {
        if !first.starts_with('-')
            && first != "workspace-color"
            && first != "agents"
            && first != "agent-pattern"
            && first != "ssh"
            && first != "socket-watchdog"
        {
            if plugin::lookup_plugin(first).is_ok() {
                // Resolve the session's control-socket path the same
                // way cli::list does. The plugin's cmux_call host
                // imports write back to this socket.
                let raw_socket: Option<PathBuf> =
                    std::env::var_os("MTYX_MUX_SOCKET").map(PathBuf::from).or_else(|| {
                        let mut idx = 0;
                        while idx + 1 < raw_args.len() {
                            if raw_args[idx] == "--socket" {
                                return Some(PathBuf::from(&raw_args[idx + 1]));
                            }
                            idx += 1;
                        }
                        None
                    });
                let socket_path = raw_socket.unwrap_or_else(|| {
                    // Default: ~/.local/share/mattyx/mtyx-<pid>.sock
                    // (matches mux_core::platform::default_socket_path
                    // when --session is "main"). Plugins running in
                    // an attached mtyx usually want to talk back to
                    // the parent mtyx's control socket, so honour
                    // MTYX_MUX_SOCKET first.
                    if let Some(home) = std::env::var_os("HOME") {
                        if !home.is_empty() {
                            return PathBuf::from(home)
                                .join(".local")
                                .join("share")
                                .join("mattyx")
                                .join("mtyx-main.sock");
                        }
                    }
                    PathBuf::from("/tmp/mtyx-main.sock")
                });
                std::process::exit(plugin::cmd_call(first, &raw_args[1..], &socket_path));
            }
        }
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("ssh") {
        std::process::exit(ssh_bootstrap::run(&raw_args[1..]));
    }
    if raw_args.first().map(|arg| arg.as_str()) == Some("socket-watchdog") {
        std::process::exit(socket_watchdog::run(&raw_args[1..]));
    }
    let mut command_index = 0;
    while command_index < raw_args.len() {
        match raw_args[command_index].as_str() {
            "--session" | "--socket" => command_index += 2,
            "--json" => command_index += 1,
            _ => break,
        }
    }
    if raw_args.get(command_index).map(String::as_str) == Some("workspace-color") {
        if raw_args.len() != command_index + 3 {
            eprintln!("mtyx: usage: mtyx workspace-color <name> <color>");
            std::process::exit(2);
        }
        let mut args = raw_args[..command_index].to_vec();
        args.extend([
            "workspace-color".to_string(),
            "--name".to_string(),
            raw_args[command_index + 1].clone(),
            "--color".to_string(),
            raw_args[command_index + 2].clone(),
        ]);
        std::process::exit(cli::run(&args, USAGE));
    }
    // `mtyx agent-pattern <add|list|remove> ...` (issue #78 AC4): the
    // issue's noun form, translated into the flat wire verbs the way
    // `workspace-color` is. The daemon owns the live registry, so adds
    // survive across CLI invocations within a session.
    if raw_args.get(command_index).map(String::as_str) == Some("agent-pattern") {
        let rest = &raw_args[command_index + 1..];
        let mut args = raw_args[..command_index].to_vec();
        match rest.first().map(String::as_str) {
            Some("add") => {
                let Some(name) = rest.get(1).filter(|n| !n.starts_with('-')) else {
                    eprintln!("mtyx: usage: mtyx agent-pattern add <name> --pattern <pattern>");
                    std::process::exit(2);
                };
                args.extend(["agent-pattern-add".to_string(), "--name".to_string(), name.clone()]);
                args.extend(rest[2..].to_vec());
            }
            Some("list") => args.push("agent-pattern-list".to_string()),
            Some("remove") => {
                let Some(name) = rest.get(1).filter(|n| !n.starts_with('-')) else {
                    eprintln!("mtyx: usage: mtyx agent-pattern remove <name>");
                    std::process::exit(2);
                };
                args.extend([
                    "agent-pattern-remove".to_string(),
                    "--name".to_string(),
                    name.clone(),
                ]);
            }
            _ => {
                eprintln!("mtyx: usage: mtyx agent-pattern <add|list|remove> ...");
                std::process::exit(2);
            }
        }
        std::process::exit(cli::run(&args, USAGE));
    }
    // Issue #77: accept the documented three-word form `mtyx pane
    // worktree create ...` by rewriting it to the flat verb before CLI
    // dispatch (must run before `is_cli_invocation`, which would not
    // recognise the triple as a verb position).
    cli::rewrite_pane_worktree_alias(&mut raw_args);
    if cli::is_cli_invocation(&raw_args) {
        std::process::exit(cli::run(&raw_args, USAGE));
    }
    let mut args = parse_args(raw_args);
    if args.show_local_config_resolution {
        return show_local_config_resolution(args);
    }
    if args.session_list {
        let global = cli::GlobalArgs {
            session: Some(args.session.clone()),
            socket: args.socket.clone(),
            json: args.json,
        };
        if args.json {
            std::process::exit(cli::run_attach_session_list_json(&global));
        }
        // Interactive picker (Claims 2-7). It restores the terminal on every
        // exit path before returning, so the subsequent app::run (Attach)
        // starts from a clean screen.
        match session_picker::run(&global) {
            Ok(session_picker::PickerOutcome::Attach { socket_path, name }) => {
                // Use the EXACT discovered socket_path (not a recomputed
                // default_socket_path(name)) so --socket-scoped discovery
                // reconnects even if runtime_dir() would resolve elsewhere.
                args.socket = Some(socket_path);
                args.session = name;
            }
            Ok(session_picker::PickerOutcome::Quit { destructive }) => {
                std::process::exit(if destructive { 1 } else { 0 });
            }
            Ok(session_picker::PickerOutcome::CtrlC) => std::process::exit(2),
            Err(e) => {
                eprintln!("mtyx: {e}");
                std::process::exit(1);
            }
        }
    }
    let result = if args.attach { run_attach(args, None) } else { run_server(args) };
    if let Err(e) = result {
        eprintln!("mtyx: {e}");
        std::process::exit(1);
    }
}

fn run_attach(mut args: Args, fallback: Option<PathBuf>) -> anyhow::Result<()> {
    // `--print-resolved-config` only fires on the first attach; a session-
    // manager reattach (RunOutcome::Reattach) re-enters this loop without it.
    let mut first = true;
    // last_good: the most recent socket we successfully connected to, so a
    // dead swap target can recover back to it in-process (issue #69) instead
    // of exiting to the shell. Seeded by run_server with the still-listening
    // origin socket; `main()` passes None (a genuine first attach has no
    // origin to recover to, so a dead target still exits 1).
    let mut last_good: Option<PathBuf> = fallback;
    // pending_status: a status-message string carried to the NEXT run_tui
    // call (the recovery iteration), so the user sees why a handoff failed.
    let mut pending_status: Option<String> = None;
    loop {
        let overlay = if args.apply_local_config {
            resolve_local_overlay(args.config.as_deref())
        } else {
            None
        };
        // Rename compat: client_socket_path falls back to a LIVE
        // cmux-era socket when the canonical mtyx one is not up.
        let socket_path = args
            .socket
            .clone()
            .unwrap_or_else(|| mux_core::server::client_socket_path(&args.session));
        // Issue #69: retry once on a transiently-unconnectable socket, then
        // recover in-process to last_good when this is a swap (last_good is
        // Some) instead of propagating the error to exit 1. A genuine first
        // attach (last_good is None) still propagates -- exit 1, unchanged.
        let remote = match session::connect_with_retry(
            &socket_path,
            1,
            std::time::Duration::from_millis(250),
        ) {
            Ok(r) => r,
            Err(e) => {
                let failed_name = socket_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                match session::plan_swap_recovery(last_good.as_deref(), &socket_path, failed_name) {
                    session::SwapRecovery::Propagate => {
                        return Err(e).with_context(|| {
                            format!("attaching to mtyx session socket at {}", socket_path.display())
                        });
                    }
                    session::SwapRecovery::Recover { socket, status } => {
                        pending_status = Some(status);
                        let session = socket
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| args.session.clone());
                        args.socket = Some(socket);
                        args.session = session;
                        // Recovery is not a first attach: never fire
                        // --print-resolved-config on the recovery iteration.
                        first = false;
                        continue;
                    }
                }
            }
        };
        // Remember the socket we just successfully used so the next swap
        // failure can recover back to it.
        last_good = Some(socket_path.clone());
        // `--print-resolved-config` is an inspection escape for thin-client
        // attaches (issue #40 blocker 1): fetch the server's resolved chrome,
        // layer the local overlay on top, print the merged chrome as JSON,
        // and exit without starting the TUI.
        if first && args.print_resolved_config {
            return print_resolved_config(remote, overlay);
        }
        first = false;
        let initial_status = pending_status.take();
        match run_tui(Session::Remote(remote), args.session.clone(), overlay, initial_status)? {
            app::RunOutcome::Done => return Ok(()),
            app::RunOutcome::Reattach(socket) => {
                // Switch the running TUI to another session: derive the
                // session name from the new socket's stem and loop back into
                // the attach path. Terminal restore/re-init already bracket
                // each run_tui call, so the handoff is a clean re-init.
                let session = socket
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| args.session.clone());
                args.socket = Some(socket);
                args.session = session;
                continue;
            }
        }
    }
}

/// Print the merged resolved chrome (server base + local overlay) as a
/// JSON object to stdout and exit 0 without attaching the TUI. The shape
/// matches `Config::resolved_chrome_value` so a caller can assert the
/// server's theme survived alongside the local overlay's key bindings.
fn print_resolved_config(
    remote: Arc<RemoteSession>,
    overlay: Option<config::Overlay>,
) -> anyhow::Result<()> {
    let data = remote.request(serde_json::json!({ "cmd": "get-resolved-config" }))?;
    let mut config = config::Config::from_server_chrome(&data);
    if let Some(o) = &overlay {
        o.apply(&mut config);
    }
    let json = serde_json::to_string_pretty(&config.resolved_chrome_value())?;
    println!("{json}");
    Ok(())
}

/// Resolve the local overlay for an attach: log which file applies (or that
/// none was found) and return the parsed `Overlay`. Returns `None` when no
/// path resolves or the file fails to parse, so the attach degrades to the
/// server-side config instead of failing.
fn resolve_local_overlay(explicit: Option<&std::path::Path>) -> Option<config::Overlay> {
    match config::local_config_path(explicit) {
        Some(path) => match config::load_overlay_file(&path) {
            Some(overlay) => {
                eprintln!(
                    "mtyx: applying local config from {} (overrides {} keys)",
                    path.display(),
                    overlay.override_count()
                );
                Some(overlay)
            }
            None => {
                eprintln!("mtyx: no local config found at {}", path.display());
                None
            }
        },
        None => {
            eprintln!("mtyx: no local config found");
            None
        }
    }
}

/// Dry-run for `--show-local-config-resolution`: print which local file
/// would apply and how many keys it overrides, then exit without
/// attaching. Exits 0 whether or not a file resolved.
fn show_local_config_resolution(args: Args) {
    if let Some(path) = config::local_config_path(args.config.as_deref()) {
        if let Some(overlay) = config::load_overlay_file(&path) {
            println!(
                "mtyx: local config resolves to {} (overrides {} keys)",
                path.display(),
                overlay.override_count()
            );
        } else {
            eprintln!("mtyx: no local config found at {}", path.display());
        }
    } else {
        eprintln!("mtyx: no local config found");
    }
    std::process::exit(0);
}

fn run_server(args: Args) -> anyhow::Result<()> {
    // Snapshot before any field is moved: a session-manager reattach
    // (RunOutcome::Reattach) re-dispatches into run_attach with the chosen
    // socket, carrying the local config overlay over.
    let original_args = args.clone();
    // Issue #28: inherit orphaned pane grandchildren so mux.shutdown()
    // can reap them instead of leaving them under PID 1.
    let _ = mux_core::process::set_child_subreaper();

    let mut surface_options = SurfaceOptions::default();
    let config = config::load();
    surface_options.chrome_binary = config.browser.chrome_binary.clone();
    surface_options.cdp_url = config.browser.cdp_url.clone();
    surface_options.browser_discover = config.browser.discover;
    surface_options.browser_discover_ports = config.browser.discover_ports.clone();
    surface_options.browser_user_data_dir = config.browser.user_data_dir.clone();
    surface_options.browser_ephemeral = config.browser.ephemeral;
    surface_options.browser_max_capture_megapixels = config.browser.max_capture_megapixels;
    surface_options.browser_capture_scale = config.browser.capture_scale;
    if let Some(term) = args.term {
        surface_options.term = term;
    }
    // Issue #99: headless VT geometry from mux.json (`headless.vt_size`,
    // e.g. "100x30") — the size surfaces spawn at when no client is
    // attached. Only applied when `MTYX_MUX_VT_SIZE` (read in
    // `SurfaceOptions::default`) is unset, so a per-process env override
    // still wins over the config file.
    if std::env::var_os("MTYX_MUX_VT_SIZE").is_none() {
        if let Some((cols, rows)) = config.headless.vt_size {
            surface_options.cols = cols;
            surface_options.rows = rows;
        }
    }
    // Compute the socket path up front so surface children inherit it.
    let socket_path =
        args.socket.clone().unwrap_or_else(|| mux_core::server::default_socket_path(&args.session));
    surface_options.extra_env.push(("MTYX_MUX_SOCKET".into(), socket_path.display().to_string()));

    let mux = Mux::new(args.session.clone(), surface_options);
    // Issue #40 blocker 1: publish this server's resolved presentation
    // chrome (theme/tabs/sidebar/keys) so a thin-client `mtyx attach
    // --apply-local-config` can fetch it via the `get-resolved-config`
    // verb and layer its local overlay on top instead of replacing the
    // server config with the laptop's own. Browser and scrollbar stay
    // server-side truth and are not published here.
    mux.set_resolved_chrome(config.resolved_chrome_value());
    // Issue #78 AC7: push the resolved [[agent_detection]] settings into
    // the daemon so the detect verbs honour them.
    mux.set_agent_detection(mux_core::agent_detect::DetectionSettings {
        enabled: config.agent_detection.enabled,
        min_confidence: config.agent_detection.min_confidence,
    });
    // Issue #77 AC6: the operator's [[worktree_pattern]] override, or
    // None for the default `<repo>/../<repo>.<branch>/`.
    mux.set_worktree_pattern(config.worktree_pattern.clone());
    mux.restore_session();
    for workspace in &config.workspaces {
        let id = mux.with_state(|state| {
            state.workspaces.iter().find(|ws| ws.name == workspace.name).map(|ws| ws.id)
        });
        let id = match id {
            Some(id) => id,
            None => {
                mux.new_workspace(Some(workspace.name.clone()), None)
                    .with_context(|| format!("creating workspace {}", workspace.name))?;
                mux.with_state(|state| state.workspaces.last().unwrap().id)
            }
        };
        if let Some(color) = &workspace.color {
            mux.set_workspace_color(id, Some(mux_core::server::parse_workspace_color(color)?));
        }
        if let Some(icon) = &workspace.icon {
            mux.set_workspace_icon(id, Some(mux_core::server::parse_workspace_icon(icon)?));
        }
    }
    mux.enable_persistence();
    mux_core::server::serve(mux.clone(), Some(socket_path.clone()))
        .with_context(|| format!("binding control socket at {}", socket_path.display()))?;
    // Issue #27: detached companion that unlinks .sock/.pid if we die via
    // SIGKILL (handlers/atexit never run). Harmless no-op on graceful exit.
    socket_watchdog::spawn(std::process::id(), &socket_path);

    let result = if args.headless {
        run_headless(&mux, &socket_path).map(|()| app::RunOutcome::Done)
    } else {
        run_tui(Session::Local(mux.clone()), args.session.clone(), None, None)
    };
    if let Ok(app::RunOutcome::Reattach(socket)) = &result {
        // The session-manager overlay asked to switch the TUI to another
        // session. Keep THIS local server alive (headless) so the user can
        // return to it later — skip shutdown/cleanup — and re-dispatch into
        // the attach path against the chosen socket. (On a reattach from a
        // local session the local server survives as a headless daemon.)
        let mut attach_args = original_args.clone();
        if let Some(session) = socket.file_stem().and_then(|s| s.to_str()) {
            attach_args.session = session.to_string();
        }
        attach_args.socket = Some(socket.clone());
        attach_args.attach = true;
        return run_attach(attach_args, Some(socket_path.clone()));
    }
    mux.shutdown();
    // Issue #28: after known surfaces are killed, sweep anything that
    // reparented to us via PR_SET_CHILD_SUBREAPER (grandchildren whose
    // intermediate parent already exited before surface.kill ran).
    #[cfg(target_os = "linux")]
    {
        mux_core::process::kill_remaining_children();
    }
    mux_core::server::cleanup(&mux.socket_path().unwrap_or_else(|| socket_path.clone()));
    result.map(|_| ())
}

fn run_tui(
    session: Session,
    session_label: String,
    overlay: Option<config::Overlay>,
    initial_status: Option<String>,
) -> anyhow::Result<app::RunOutcome> {
    crossterm::terminal::enable_raw_mode()?;
    let colors = host_colors::probe_default_colors();
    let color_result = session.set_default_colors(colors);
    let raw_result = crossterm::terminal::disable_raw_mode();
    if let Err(err) = color_result {
        eprintln!("mtyx: failed to set default colors: {err}");
    }
    raw_result?;
    app::run(session, session_label, overlay, initial_status)
}

fn run_headless(mux: &Arc<Mux>, socket_path: &std::path::Path) -> anyhow::Result<()> {
    eprintln!("mtyx: headless, control socket at {}", socket_path.display());
    // Keep the process alive; the control socket drives everything and
    // the mux reaps exited surfaces itself.
    let events = mux.subscribe();
    loop {
        if shutdown_requested() {
            break;
        }
        match events.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::park_timeout(std::time::Duration::from_millis(250))
            }
        }
    }
    Ok(())
}

fn usage_exit(msg: &str) -> ! {
    eprintln!("mtyx: {msg}\n\n{USAGE}");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_env_pairs_maps_every_cmux_var() {
        // Rename compat: every CMUX_* var, whatever its suffix (the full
        // inventory is 60+ names and grows), implies an MTYX_* pair.
        let vars = vec![
            ("CMUX_MUX_SOCKET".to_string(), "/tmp/s.sock".to_string()),
            ("CMUX_REMOTE_DAEMON_PORT".to_string(), "9001".to_string()),
            ("CMUX_CLAUDE_TEAMS_CMUX_BIN".to_string(), "/bin/cmux".to_string()),
        ];
        assert_eq!(
            legacy_env_pairs(vars.into_iter()),
            vec![
                ("MTYX_MUX_SOCKET".to_string(), "/tmp/s.sock".to_string()),
                ("MTYX_REMOTE_DAEMON_PORT".to_string(), "9001".to_string()),
                ("MTYX_CLAUDE_TEAMS_CMUX_BIN".to_string(), "/bin/cmux".to_string()),
            ]
        );
    }

    #[test]
    fn legacy_env_pairs_ignores_non_prefixed_and_bare_names() {
        // Only the exact CMUX_ prefix maps: unrelated vars pass through
        // untouched, and a bare CMUX (no underscore) is left alone.
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("CMUX".to_string(), "bare".to_string()),
            ("MTYX_MUX_SOCKET".to_string(), "/already/canonical.sock".to_string()),
            ("MY_CMUX_THING".to_string(), "not-a-prefix".to_string()),
        ];
        assert!(legacy_env_pairs(vars.into_iter()).is_empty());
    }

    #[test]
    fn honor_legacy_env_does_not_override_existing_mtyx_values() {
        // An explicit MTYX_* value always wins over the shimmed CMUX_*
        // one; the shim only fills gaps.
        std::env::set_var("CMUX_SHIM_TEST", "legacy");
        std::env::set_var("MTYX_SHIM_TEST", "canonical");
        std::env::remove_var("CMUX_SHIM_TEST_GAP");
        std::env::remove_var("MTYX_SHIM_TEST_GAP");
        std::env::set_var("CMUX_SHIM_TEST_GAP", "fills-gap");
        honor_legacy_env();
        assert_eq!(std::env::var("MTYX_SHIM_TEST").unwrap(), "canonical");
        assert_eq!(std::env::var("MTYX_SHIM_TEST_GAP").unwrap(), "fills-gap");
        std::env::remove_var("CMUX_SHIM_TEST");
        std::env::remove_var("MTYX_SHIM_TEST");
        std::env::remove_var("CMUX_SHIM_TEST_GAP");
        std::env::remove_var("MTYX_SHIM_TEST_GAP");
    }
}
