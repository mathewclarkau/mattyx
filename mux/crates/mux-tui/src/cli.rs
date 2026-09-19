use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use mux_core::platform::transport;
use serde_json::{json, Value};

const REQUEST_ID: u64 = 1;

type BuildFn = fn(&FlagMap) -> Result<Value, UsageError>;
type PrintFn = fn(&Value, &mut dyn Write) -> io::Result<()>;

#[derive(Debug)]
pub struct UsageError(String);

struct CliArgs {
    global: GlobalArgs,
    verb: &'static VerbSpec,
    flags: FlagMap,
}

#[derive(Default)]
pub(crate) struct GlobalArgs {
    pub(crate) session: Option<String>,
    pub(crate) socket: Option<PathBuf>,
    pub(crate) json: bool,
}

/// Verb flags that are boolean and accept the bare form (`--group`) —
/// a missing or flag-looking following token means `true` instead of an
/// error or swallowing the next flag as a value (issue #100). Valued
/// forms (`--group 1`, `--group 0`) still work. `confirm`/`no-confirm`
/// (issue #88) join the same convention.
const BARE_BOOL_FLAGS: &[&str] = &["group", "confirm", "no-confirm"];

/// Verbs that accept one bare positional argument alongside their flags
/// (issue #84: `mtyx screenshot --surface <id> <file>`), mapped onto the
/// named flag of the same meaning (`<file>` == `--output <file>`). Passing
/// both forms is a usage error.
const POSITIONAL_FLAG_VERBS: &[(&str, &str)] = &[("screenshot", "output")];

#[derive(Default)]
struct FlagMap {
    values: BTreeMap<String, String>,
    /// The verbatim argv captured by `--exec -- <argv...>` (issue #76).
    /// Kept out of `values`: it is a list, not a `--flag value` pair, and
    /// it consumes the rest of the command line.
    exec: Option<Vec<String>>,
}

struct VerbSpec {
    name: &'static str,
    allowed: &'static [&'static str],
    build: BuildFn,
    print: PrintFn,
    stream: bool,
}

const VERBS: &[VerbSpec] = &[
    VerbSpec {
        name: "identify",
        allowed: &[],
        build: build_no_args,
        print: print_identify,
        stream: false,
    },
    VerbSpec {
        name: "list-workspaces",
        allowed: &[],
        build: build_no_args,
        print: print_tree,
        stream: false,
    },
    VerbSpec {
        // Issue #40: returns the server's resolved presentation chrome
        // (theme/tabs/sidebar/keys) for a thin-client attach to layer its
        // local `Overlay` on top of. Read-only; `mtyx attach
        // --apply-local-config` invokes the same verb internally, and
        // `mtyx attach --print-resolved-config` shows the merged
        // (server + local overlay) chrome for inspection.
        name: "get-resolved-config",
        allowed: &[],
        build: build_no_args,
        print: print_get_resolved_config,
        stream: false,
    },
    VerbSpec {
        name: "send",
        // Issue #88: --confirm (the default) / --no-confirm and
        // --timeout-ms for receipted input.
        allowed: &[
            "surface",
            "text",
            "bytes",
            "send-cr",
            "shell",
            "confirm",
            "no-confirm",
            "timeout-ms",
        ],
        build: build_send,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "read-screen",
        allowed: &["surface"],
        build: build_surface,
        print: print_read_screen,
        stream: false,
    },
    VerbSpec {
        name: "vt-state",
        allowed: &["surface"],
        build: build_surface,
        print: print_vt_state,
        stream: false,
    },
    VerbSpec {
        // "exec"/"env" (issue #76) carry an explicit child argv/env —
        // `mtyx new-tab --exec -- <argv...>` is the agent-start primitive
        // that `layout export` records and `layout apply` replays.
        name: "new-tab",
        allowed: &["pane", "cwd", "cols", "rows", "branch", "label", "prompt-file", "exec", "env"],
        build: build_new_tab,
        print: print_surface,
        stream: false,
    },
    VerbSpec {
        name: "new-browser-tab",
        allowed: &["url", "pane", "cols", "rows"],
        build: build_new_browser_tab,
        print: print_surface,
        stream: false,
    },
    VerbSpec {
        name: "new-workspace",
        allowed: &["name", "cols", "rows"],
        build: build_new_workspace,
        print: print_surface,
        stream: false,
    },
    VerbSpec {
        name: "new-screen",
        allowed: &["workspace", "cols", "rows"],
        build: build_new_screen,
        print: print_surface,
        stream: false,
    },
    VerbSpec {
        name: "split",
        allowed: &["pane", "dir", "cols", "rows", "branch", "label", "exec", "env"],
        build: build_split,
        print: print_surface,
        stream: false,
    },
    VerbSpec {
        name: "set-ratio",
        allowed: &["pane", "dir", "ratio"],
        build: build_set_ratio,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "set-default-colors",
        allowed: &["fg", "bg"],
        build: build_set_default_colors,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "close-surface",
        allowed: &["surface"],
        build: build_surface,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "close-pane",
        allowed: &["pane"],
        build: build_pane,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "close-screen",
        allowed: &["screen"],
        build: build_screen,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "close-workspace",
        allowed: &["workspace", "group"],
        build: build_close_workspace,
        print: print_close_workspace,
        stream: false,
    },
    VerbSpec {
        name: "rename-pane",
        allowed: &["pane", "name"],
        build: build_rename_pane,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "rename-surface",
        allowed: &["surface", "name"],
        build: build_rename_surface,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "rename-screen",
        allowed: &["screen", "name"],
        build: build_rename_screen,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "rename-workspace",
        allowed: &["workspace", "name"],
        build: build_rename_workspace,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "set-workspace-color",
        allowed: &["workspace", "color", "colour"],
        build: build_set_workspace_color,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "set-status",
        allowed: &["icon", "workspace"],
        build: build_set_status,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "workspace-color",
        allowed: &["name", "color"],
        build: build_workspace_color,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "trigger-flash",
        allowed: &["workspace", "surface"],
        build: build_trigger_flash,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "resize-surface",
        allowed: &["surface", "cols", "rows"],
        build: build_resize_surface,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "focus-pane",
        allowed: &["pane"],
        build: build_pane,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "select-tab",
        allowed: &["pane", "index", "delta"],
        build: build_select_tab,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "select-screen",
        allowed: &["index", "delta"],
        build: build_select_screen,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "select-workspace",
        allowed: &["index", "delta"],
        build: build_select_workspace,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "move-tab",
        allowed: &["surface", "pane", "index"],
        build: build_move_tab,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "move-workspace",
        allowed: &["workspace", "index"],
        build: build_move_workspace,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "scroll-surface",
        allowed: &["surface", "delta"],
        build: build_scroll_surface,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "subscribe",
        allowed: &[],
        build: build_no_args,
        print: print_empty,
        stream: true,
    },
    VerbSpec {
        name: "attach-surface",
        allowed: &["surface"],
        build: build_surface,
        print: print_empty,
        stream: true,
    },
    VerbSpec {
        name: "report-agent",
        // "session" collides with the global --session (mux session name)
        // flag, so the agent's own session id is --agent-session on the
        // CLI even though the wire protocol field is plain "session".
        // Issue #75: --agent names the pane for the name-addressed verbs,
        // --message carries free-text context, --surface may be omitted
        // inside a pane ($MTYX_MUX_SURFACE) and --source defaults to
        // socket (hooks stay the authority).
        allowed: &["surface", "state", "source", "agent-session", "agent", "message"],
        build: build_report_agent,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "list-agents",
        allowed: &["surface", "state"],
        build: build_list_agents,
        print: print_agents,
        stream: false,
    },
    // Per-pane git worktrees (issue #77). The issue documents these as
    // the three-word form `pane worktree create`; the CLI accepts that
    // verbatim via `rewrite_pane_worktree_alias` (main.rs rewrites the
    // argv triple to the flat form before verb dispatch).
    VerbSpec {
        name: "pane-worktree-create",
        allowed: &["pane", "branch", "label"],
        build: build_pane_worktree_create,
        print: print_worktree_created,
        stream: false,
    },
    VerbSpec {
        name: "pane-worktree-list",
        allowed: &["pane"],
        build: build_pane_worktree_list,
        print: print_worktrees,
        stream: false,
    },
    VerbSpec {
        name: "pane-worktree-remove",
        allowed: &["pane", "branch"],
        build: build_pane_worktree_remove,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        // Issue #78 AC1: ambient detection on one surface (the repo's
        // flat-verb spelling of the issue's `pane detect-agent --pane`;
        // a pane's content lives on its active tab surface, which is
        // what every sibling verb — read-screen, report-agent — targets).
        name: "detect-agent",
        allowed: &["surface"],
        build: build_surface,
        print: print_detect_agent,
        stream: false,
    },
    VerbSpec {
        // Issue #78 AC2: the issue's `agent detect-batch`, spelled to
        // mirror the plural `list-agents` convention.
        name: "detect-agents",
        allowed: &[],
        build: build_no_args,
        print: print_detect_agents,
        stream: false,
    },
    VerbSpec {
        name: "agent-pattern-add",
        allowed: &["name", "pattern", "kind", "confidence", "case-insensitive"],
        build: build_agent_pattern_add,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "agent-pattern-list",
        allowed: &[],
        build: build_no_args,
        print: print_agent_patterns,
        stream: false,
    },
    VerbSpec {
        name: "agent-pattern-remove",
        allowed: &["name"],
        build: build_agent_pattern_remove,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        // Issue #75 AC3: read an agent's pane by name (or surface id).
        name: "agent-read",
        allowed: &["target", "source", "lines"],
        build: build_agent_read,
        print: print_read_screen,
        stream: false,
    },
    VerbSpec {
        // Issue #75 AC4: type literal text into an agent's pane by name
        // (or surface id) WITHOUT Enter — submit separately.
        name: "agent-send",
        allowed: &["target", "text", "shell"],
        build: build_agent_send,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        // Issue #75 AC5: block until the named agent reaches --status.
        // The response can take up to --timeout ms, so run_command
        // overrides this verb's socket read timeout.
        name: "wait-agent-status",
        allowed: &["target", "status", "timeout"],
        build: build_wait_agent_status,
        print: print_read_screen,
        stream: false,
    },
    VerbSpec {
        name: "browser-reload",
        allowed: &["surface"],
        build: build_surface,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "list-sessions",
        allowed: &[],
        build: build_no_args,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "kill-session",
        allowed: &["session"],
        build: build_kill_session,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "kill-stale",
        allowed: &[],
        build: build_no_args,
        print: print_empty,
        stream: false,
    },
    // `rename-session` is special-cased in `run_command` (it does its own
    // socket discovery + connect + exit-code map, like list/kill-session).
    // The VerbSpec exists so `verb_by_name` recognises it during arg
    // parsing; `build_rename_session` only carries the flags.
    VerbSpec {
        name: "rename-session",
        allowed: &["old", "new"],
        build: build_rename_session,
        print: print_empty,
        stream: false,
    },
    // Issue #76: the layout export/apply verbs are special-cased in
    // `run_command` (they do local file I/O around the socket round-trip,
    // like list/kill/rename-session). The VerbSpecs exist so `parse`
    // recognises the verbs and their flags.
    VerbSpec {
        name: "layout-export",
        allowed: &["workspace", "output"],
        build: build_layout_export,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "layout-apply",
        allowed: &["input", "workspace"],
        build: build_layout_apply,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        name: "layout-export-all",
        allowed: &["output-dir"],
        build: build_layout_export_all,
        print: print_empty,
        stream: false,
    },
    VerbSpec {
        // Issue #84: capture a surface's visible text to a file with the
        // exact bytes `read-screen` prints to stdout. Like the layout
        // verbs it is special-cased in `run_command` (client-side file
        // I/O around the socket round-trip); it rides the plain
        // `read-screen` request — no new server command.
        name: "screenshot",
        allowed: &["surface", "output"],
        build: build_screenshot,
        print: print_empty,
        stream: false,
    },
];

pub fn is_cli_invocation(args: &[String]) -> bool {
    matches!(first_command_arg(args), FirstCommand::Help | FirstCommand::Verb)
}

pub fn run(args: &[String], usage: &str) -> i32 {
    // Issue #98: CLI invocations write their payload to stdout, so a
    // downstream pipe closing early (`... | head -1`) must end the
    // process quietly via SIGPIPE (the shell's 141), never a Rust
    // panic (exit 101) from `println!` hitting EPIPE. Only the CLI
    // paths install this: the TUI/server keep Rust's SIG_IGN so a dead
    // client socket stays a handled EPIPE error, never a signal death.
    crate::reset_sigpipe_default();
    match parse(args) {
        Ok(Parsed::Help) => {
            print!("{usage}");
            0
        }
        Ok(Parsed::Command(args)) => run_command(args),
        Err(err) => {
            // Issue #98: the parse aborted before `--json` could be
            // recorded in GlobalArgs, so scan the raw argv for the flag
            // to decide envelope vs human string — `agent-read --file
            // /nope --json` still gets the machine-readable form.
            let json = args.iter().any(|a| a == "--json");
            cli_error(json, 2, &format!("mtyx: {}", err.0))
        }
    }
}

enum FirstCommand {
    None,
    Help,
    Verb,
}

enum Parsed {
    Help,
    Command(CliArgs),
}

fn first_command_arg(args: &[String]) -> FirstCommand {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" | "--session" => i += 2,
            // Issue #98: `--flag=value` spellings of the global flags.
            arg if arg.starts_with("--socket=") || arg.starts_with("--session=") => i += 1,
            "--json" => i += 1,
            "-h" | "--help" => return FirstCommand::Help,
            arg if arg.starts_with("--") => return FirstCommand::None,
            "help" => return FirstCommand::Help,
            arg if verb_by_name(arg).is_some() => return FirstCommand::Verb,
            _ => return FirstCommand::None,
        }
    }
    FirstCommand::None
}

fn parse(args: &[String]) -> Result<Parsed, UsageError> {
    if matches!(first_command_arg(args), FirstCommand::Help) {
        return Ok(Parsed::Help);
    }

    let mut global = GlobalArgs::default();
    let mut flags = FlagMap::default();
    let mut verb: Option<&'static VerbSpec> = None;
    // Issue #98: set once a bare `--` is seen after the verb — every
    // token past it is a positional, never a flag.
    let mut positional_only = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            // Issue #98: after `--`, even flag-SHAPED tokens (and the
            // global-flag literals below) are positionals.
            _ if positional_only => {
                take_positional(verb.unwrap(), &mut flags, arg)?;
                i += 1;
            }
            "-h" | "--help" | "help" => return Ok(Parsed::Help),
            "--json" => {
                global.json = true;
                i += 1;
            }
            "--socket" => {
                global.socket = Some(PathBuf::from(value_after(args, i, "--socket")?));
                i += 2;
            }
            // Issue #98: `--flag=value` is accepted everywhere the
            // space form is (global flags included).
            _ if arg.starts_with("--socket=") => {
                global.socket = Some(PathBuf::from(&arg["--socket=".len()..]));
                i += 1;
            }
            "--session" => {
                global.session = Some(value_after(args, i, "--session")?);
                i += 2;
            }
            _ if arg.starts_with("--session=") => {
                global.session = Some(arg["--session=".len()..].to_string());
                i += 1;
            }
            _ if verb.is_none() && verb_by_name(arg).is_some() => {
                verb = verb_by_name(arg);
                i += 1;
            }
            // Issue #98: bare `--` is the end-of-options marker — no
            // token after it is ever parsed as a flag. Distinct from
            // the `--` that must follow `--exec`: that one is consumed
            // by the `--exec` arm below.
            "--" if verb.is_some() => {
                positional_only = true;
                i += 1;
            }
            // Issue #76: `--exec -- <argv...>` — everything after the
            // literal `--` is the child's verbatim argv (no quoting
            // loss). Must be the verb's LAST flag: it eats the rest of
            // the command line.
            _ if arg == "--exec" && verb.is_some() => {
                let spec = verb.unwrap();
                if !spec.allowed.contains(&"exec") {
                    return Err(UsageError(format!("unknown flag --exec for {}", spec.name)));
                }
                if flags.exec.is_some() {
                    return Err(UsageError("duplicate --exec".to_string()));
                }
                if args.get(i + 1).map(|s| s.as_str()) != Some("--") {
                    return Err(UsageError(
                        "--exec must be followed by \"--\" and the command argv".to_string(),
                    ));
                }
                let argv: Vec<String> = args[i + 2..].to_vec();
                if argv.is_empty() {
                    return Err(UsageError("--exec needs a command after \"--\"".to_string()));
                }
                flags.exec = Some(argv);
                i = args.len();
            }
            _ if arg.starts_with("--") => {
                let Some(spec) = verb else {
                    return Err(UsageError(format!("unknown global flag {arg:?}")));
                };
                // Issue #98: both spellings — `--flag value` and
                // `--flag=value` — land in the same FlagMap slot, in any
                // position relative to the verb's positionals (the
                // linear scan below is already order-neutral about them).
                let (name, inline) = split_flag(arg);
                if !spec.allowed.contains(&name) {
                    return Err(UsageError(format!("unknown flag --{name} for {}", spec.name)));
                }
                let bare_bool = inline.is_none()
                    && BARE_BOOL_FLAGS.contains(&name)
                    && args.get(i + 1).map(|s| s.starts_with("--")).unwrap_or(true);
                let value = if let Some(value) = inline {
                    value.to_string()
                } else if bare_bool {
                    "true".to_string()
                } else {
                    value_after(args, i, &format!("--{name}"))?
                };
                if flags.values.insert(name.to_string(), value).is_some() {
                    return Err(UsageError(format!("duplicate flag --{name}")));
                }
                i += if bare_bool || inline.is_some() { 1 } else { 2 };
            }
            _ if verb.is_some() => {
                take_positional(verb.unwrap(), &mut flags, arg)?;
                i += 1;
            }
            _ => return Err(UsageError(format!("unknown argument {arg:?}"))),
        }
    }

    let Some(verb) = verb else { return Err(UsageError("missing verb".to_string())) };
    Ok(Parsed::Command(CliArgs { global, verb, flags }))
}

/// Issue #98: split `--flag[=value]` (the caller guarantees the `--`
/// prefix) into its name and optional inline value. Splits on the FIRST
/// `=` so a value may itself contain one (`--env=A=B` → `("env", "A=B")`).
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg[2..].split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (&arg[2..], None),
    }
}

/// Consume one bare positional for `verb` (issue #84's screenshot
/// <file>, and since #98 anything after a bare `--`): verbs listed in
/// POSITIONAL_FLAG_VERBS map it onto its named flag; every other verb
/// rejects it. Shared by the pre-`--` and post-`--` arms of `parse` so
/// both positions behave identically.
fn take_positional(verb: &VerbSpec, flags: &mut FlagMap, arg: &str) -> Result<(), UsageError> {
    match POSITIONAL_FLAG_VERBS.iter().find(|(name, _)| *name == verb.name) {
        Some((_, flag)) if !flags.values.contains_key(*flag) => {
            flags.values.insert(flag.to_string(), arg.to_string());
            Ok(())
        }
        Some((_, flag)) => {
            Err(UsageError(format!("pass --{flag} once: positional and --{flag} given twice")))
        }
        None => Err(UsageError(format!("unexpected argument {arg:?}"))),
    }
}

fn value_after(args: &[String], index: usize, flag: &str) -> Result<String, UsageError> {
    args.get(index + 1).cloned().ok_or_else(|| UsageError(format!("{flag} needs a value")))
}

fn verb_by_name(name: &str) -> Option<&'static VerbSpec> {
    VERBS.iter().find(|verb| verb.name == name)
}

/// Issue #98: the `--json` error envelope — `{"ok":false,"error":{...}}`
/// carrying the process exit code and the human message, so a caller
/// can branch on `ok` instead of scraping stderr text.
fn error_envelope(exit: i32, message: &str) -> Value {
    json!({ "ok": false, "error": { "code": exit, "message": message } })
}

/// Issue #98: one error surface for the control-socket CLI. Non-JSON
/// output is byte-identical to the historical bare string on stderr;
/// with `--json` the same message rides the machine-readable envelope
/// on stdout. The exit code is the caller's and is passed through
/// unchanged.
fn cli_error(json_output: bool, exit: i32, message: &str) -> i32 {
    if json_output {
        println!("{}", error_envelope(exit, message));
    } else {
        eprintln!("{message}");
    }
    exit
}

/// Issue #88: pre-flight capability negotiation for a confirmed `send`.
/// Sends `identify` on the same connection and requires the input-ACK
/// capability record (`mux_core::server::require_input_ack_capability`).
/// A daemon without the capability (protocol <= 6) yields the structured
/// `legacy_host_receipt_rejected` error — never a silent downgrade to
/// fire-and-forget. Transport/protocol failures return exit code 3; the
/// capability rejection is a server-level error (exit 1).
fn input_ack_capability_gate(stream: &mut Box<dyn transport::Stream>) -> Result<(), (i32, String)> {
    let identify = json!({"id": REQUEST_ID, "cmd": "identify"});
    let mut line = match serde_json::to_vec(&identify) {
        Ok(mut line) => {
            line.push(b'\n');
            line
        }
        Err(err) => return Err((2, format!("failed to encode identify pre-flight: {err}"))),
    };
    if let Err(err) = stream.write_all(&line).and_then(|_| stream.flush()) {
        return Err((3, format!("transport error during identify pre-flight: {err}")));
    }
    // Read the response byte-wise: the same stream is later handed to a
    // BufReader, and buffering here could swallow bytes it needs.
    line.clear();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err((3, "transport closed before identify response".to_string())),
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(err) => {
                return Err((3, format!("transport error reading identify response: {err}")));
            }
        }
    }
    let value: Value = match serde_json::from_slice(&line) {
        Ok(value) => value,
        Err(err) => return Err((3, format!("bad identify response: {err}"))),
    };
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = value.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        return Err((3, format!("identify pre-flight failed: {error}")));
    }
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    match mux_core::server::require_input_ack_capability(&data) {
        Ok(()) => Ok(()),
        Err(err) => Err((1, err.to_string())),
    }
}

fn run_command(args: CliArgs) -> i32 {
    match args.verb.name {
        "list-sessions" => return run_list_sessions(&args.global, &args.flags),
        "kill-session" => return run_kill_session(&args.global, &args.flags),
        "kill-stale" => return run_kill_stale(&args.global, &args.flags),
        "rename-session" => return run_rename_session(&args.global, &args.flags),
        "layout-export" => return run_layout_export(&args.global, &args.flags),
        "layout-apply" => return run_layout_apply(&args.global, &args.flags),
        "layout-export-all" => return run_layout_export_all(&args.global, &args.flags),
        "screenshot" => return run_screenshot(&args.global, &args.flags),
        _ => {}
    }
    let request = match (args.verb.build)(&args.flags) {
        Ok(mut value) => {
            value["cmd"] = json!(args.verb.name);
            value["id"] = json!(REQUEST_ID);
            value
        }
        Err(err) => return cli_error(args.global.json, 2, &format!("mtyx: {}", err.0)),
    };
    let socket_path = resolve_socket(&args.global);
    let mut stream = match transport::connect(&socket_path) {
        Ok(stream) => stream,
        Err(err) => {
            return cli_error(
                args.global.json,
                3,
                &format!("cannot connect to session socket {}: {err}", socket_path.display()),
            );
        }
    };
    if args.verb.stream {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    } else if args.verb.name == "wait-agent-status" {
        // The wait verb's reply legitimately arrives after up to
        // `--timeout` ms, so give the socket read that budget plus
        // slack instead of the default 10 s (which would kill every
        // longer wait with a spurious "transport error").
        let wait_ms =
            args.flags.optional("timeout").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(wait_ms.saturating_add(5_000))));
    } else if args.verb.name == "send"
        && request.get("confirm").and_then(Value::as_bool) == Some(true)
    {
        // Issue #88: a confirmed send's reply legitimately arrives after
        // up to `--timeout-ms` (plus the identify pre-flight below), so
        // budget the socket read like wait-agent-status does.
        let ack_ms = request
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(mux_core::server::DEFAULT_INPUT_ACK_TIMEOUT_MS);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(ack_ms.saturating_add(5_000))));
    } else {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    }
    // Issue #88: confirmed send is capability-gated client-side. The
    // identify pre-flight refuses a daemon that lacks input-ACK with
    // `legacy_host_receipt_rejected` (exit 1) instead of silently
    // downgrading to fire-and-forget.
    if args.verb.name == "send" && request.get("confirm").and_then(Value::as_bool) == Some(true) {
        if let Err((code, message)) = input_ack_capability_gate(&mut stream) {
            return cli_error(args.global.json, code, &message);
        }
    }
    let mut line = match serde_json::to_vec(&request) {
        Ok(line) => line,
        Err(err) => {
            return cli_error(args.global.json, 2, &format!("failed to encode request: {err}"));
        }
    };
    line.push(b'\n');
    if let Err(err) = stream.write_all(&line) {
        return cli_error(args.global.json, 3, &format!("transport error: {err}"));
    }

    let mut reader = BufReader::new(stream);
    if args.verb.stream {
        run_stream(reader, args.global.json)
    } else {
        run_one_response(&mut reader, args.global.json, args.verb.print)
    }
}

fn resolve_socket(global: &GlobalArgs) -> PathBuf {
    if let Some(path) = &global.socket {
        return path.clone();
    }
    if let Some(path) = std::env::var_os("MTYX_MUX_SOCKET") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let session = global.session.as_deref().unwrap_or("main");
    // Rename compat: falls back to a LIVE cmux-era socket when the
    // canonical mtyx one is not up (probe only; never creates).
    mux_core::server::client_socket_path(session)
}

fn run_one_response(
    reader: &mut BufReader<Box<dyn transport::Stream>>,
    json_output: bool,
    print_human: PrintFn,
) -> i32 {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return cli_error(json_output, 3, "transport closed before response"),
            Ok(_) => {}
            Err(err) => return cli_error(json_output, 3, &format!("transport error: {err}")),
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(err) => return cli_error(json_output, 3, &format!("bad response: {err}")),
        };
        if value.get("event").is_some() {
            continue;
        }
        return print_response(&value, json_output, print_human);
    }
}

/// Issue #98: `json_output` threads the `--json` flag through so a
/// server-reported error on a streaming verb surfaces as the envelope
/// too. Behaviour is otherwise unchanged: the loop keeps streaming
/// until the transport closes; on a closed stdout pipe the process now
/// ends quietly via SIGPIPE (see `reset_sigpipe_default` in main.rs)
/// instead of panicking inside `println!`.
fn run_stream(mut reader: BufReader<Box<dyn transport::Stream>>, json_output: bool) -> i32 {
    let mut line = String::new();
    loop {
        if crate::shutdown_requested() {
            return 0;
        }
        match reader.read_line(&mut line) {
            Ok(0) => {
                if line.is_empty() {
                    return 0;
                }
                return cli_error(json_output, 3, "transport closed with partial stream line");
            }
            Ok(_) if !line.ends_with('\n') => {
                return cli_error(json_output, 3, "transport closed with partial stream line");
            }
            Ok(_) => {}
            Err(err)
                if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(err) => return cli_error(json_output, 3, &format!("transport error: {err}")),
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(err) => return cli_error(json_output, 3, &format!("bad stream line: {err}")),
        };
        if value.get("event").is_some() {
            print!("{}", line.trim_end_matches(['\r', '\n']));
            println!();
            line.clear();
            if io::stdout().flush().is_err() {
                return 3;
            }
            continue;
        }
        if value.get("id").and_then(Value::as_u64) != Some(REQUEST_ID) {
            line.clear();
            continue;
        }
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            line.clear();
            continue;
        }
        let error = value.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        return cli_error(json_output, 1, error);
    }
}

fn print_response(value: &Value, json_output: bool, print_human: PrintFn) -> i32 {
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = value.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        // Issue #98: a server-reported error under `--json` is the
        // envelope, not a bare stderr string (non-JSON output unchanged).
        return cli_error(json_output, 1, error);
    }
    let data = value.get("data").unwrap_or(&Value::Null);
    let mut stdout = io::stdout();
    let result = if json_output {
        serde_json::to_writer(&mut stdout, data)
            .and_then(|_| stdout.write_all(b"\n").map_err(serde_json::Error::io))
            .map_err(io::Error::other)
    } else {
        print_human(data, &mut stdout)
    };
    match result.and_then(|_| stdout.flush()) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("stdout error: {err}");
            3
        }
    }
}

fn build_no_args(flags: &FlagMap) -> Result<Value, UsageError> {
    flags.reject_remaining()?;
    Ok(json!({}))
}

fn build_surface(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "surface": flags.required_u64("surface")? }))
}

fn build_pane(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "pane": flags.required_u64("pane")? }))
}

fn build_screen(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "screen": flags.required_u64("screen")? }))
}

fn build_workspace(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "workspace": flags.required_u64("workspace")? }))
}

/// Issue #100: `--group` closes the workspace's worktree-child
/// workspaces too; without it they survive and the response reports
/// them. May be passed bare (`--group`) or with a value
/// (`--group 1` / `--group 0`).
fn build_close_workspace(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({ "workspace": flags.required_u64("workspace")? });
    if let Some(group) = flags.optional_bool("group") {
        value["group"] = json!(group);
    }
    Ok(value)
}

fn build_send(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({ "surface": flags.required_u64("surface")? });
    if let Some(text) = flags.optional("text") {
        value["text"] = json!(text);
    }
    if let Some(bytes) = flags.optional("bytes") {
        value["bytes"] = json!(bytes);
    }
    // `--send-cr` (boolean flag) appends a literal CR (0x0D) to the written bytes
    // so that fish (and other line-edited REPLs) submit their input buffer.
    // Default false. See `Command::Send::send_cr` in mux-core/src/server.rs.
    if let Some(send_cr) = flags.optional_bool("send-cr") {
        value["send_cr"] = json!(send_cr);
    }
    // `--shell` (issue #35): shell-aware input sanitisation. `raw` (the
    // default, matching pre-#35 passthrough) writes bytes verbatim; a
    // known shell prefixes a `\n` when the text could be mis-parsed;
    // `auto` resolves the pane's shell from /proc on Linux. See
    // `Command::Send::shell` in mux-core/src/server.rs.
    if let Some(shell) = flags.optional("shell") {
        if !matches!(shell.as_str(), "auto" | "fish" | "bash" | "zsh" | "sh" | "nu" | "raw") {
            return Err(UsageError(format!(
                "--shell must be one of auto, fish, bash, zsh, sh, nu, raw (got {shell:?})"
            )));
        }
        value["shell"] = json!(shell);
    }
    // Issue #88: confirmed (receipted) input is the CLI DEFAULT — the
    // command exits 0 only after the daemon observes the input consumed
    // (surface echo/advance or child exit within the timeout).
    // `--no-confirm` (or `--confirm=false`) preserves the pre-#88
    // fire-and-forget behavior; the two flags cannot disagree.
    let confirm_flag = flags.optional_bool("confirm");
    let no_confirm = flags.optional_bool("no-confirm").unwrap_or(false);
    if confirm_flag == Some(true) && no_confirm {
        return Err(UsageError("--confirm and --no-confirm are mutually exclusive".into()));
    }
    let confirm = !no_confirm && confirm_flag != Some(false);
    if confirm {
        value["confirm"] = json!(true);
        if let Some(raw) = flags.optional("timeout-ms") {
            let timeout_ms = parse_u64("timeout-ms", &raw)?;
            if timeout_ms == 0 {
                return Err(UsageError("--timeout-ms must be at least 1".into()));
            }
            value["timeout_ms"] = json!(timeout_ms);
        }
    }
    if value.get("text").is_none() && value.get("bytes").is_none() {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|err| UsageError(format!("failed to read stdin: {err}")))?;
        value["text"] = json!(text);
    }
    Ok(value)
}

fn build_new_tab(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_u64(&mut value, "pane")?;
    flags.insert_optional_string(&mut value, "cwd");
    flags.insert_optional_size(&mut value)?;
    // Issue #77 AC4: `--branch` creates a worktree and spawns the tab
    // inside it; `--prompt-file` reads the same keys from a leading
    // frontmatter block. The two are mutually exclusive so there is
    // never a precedence question.
    if let Some(path) = flags.optional("prompt-file") {
        if flags.optional("branch").is_some() || flags.optional("label").is_some() {
            return Err(UsageError(
                "--prompt-file frontmatter cannot be combined with --branch/--label".into(),
            ));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|err| UsageError(format!("failed to read --prompt-file {path:?}: {err}")))?;
        let (branch, label) = parse_prompt_frontmatter(&text)?;
        if let Some(branch) = branch {
            value["branch"] = json!(branch);
        }
        if let Some(label) = label {
            value["label"] = json!(label);
        }
    } else {
        flags.insert_optional_string(&mut value, "branch");
        flags.insert_optional_string(&mut value, "label");
    }
    // Issue #76: `--exec -- <argv...>` / `--env K=V,K2=V2` layer on top
    // of any worktree/frontmatter choices.
    insert_exec_env(flags, &mut value)?;
    Ok(value)
}

/// Issue #76: `--exec -- <argv...>` (verbatim argv passthrough) and
/// `--env K=V,K2=V2` (comma-separated pairs) → the socket command's
/// `command` / `env` fields.
fn insert_exec_env(flags: &FlagMap, value: &mut Value) -> Result<(), UsageError> {
    if let Some(argv) = &flags.exec {
        value["command"] = json!(argv);
    }
    if let Some(env) = flags.optional("env") {
        let mut map = serde_json::Map::new();
        for pair in env.split(',') {
            let Some((key, val)) = pair.split_once('=') else {
                return Err(UsageError(format!("--env entries must be K=V (got {pair:?})")));
            };
            if key.is_empty() {
                return Err(UsageError("--env keys cannot be empty".to_string()));
            }
            map.insert(key.to_string(), json!(val));
        }
        value["env"] = Value::Object(map);
    }
    Ok(())
}

/// Parse a leading `---` frontmatter block from an agent prompt file
/// (issue #77 AC4). A file that does not start with `---` has no
/// frontmatter and yields `(None, None)`. Strict, per the repo rule
/// that parse errors propagate instead of silently defaulting: the
/// block must close with a `---` line, only `branch`/`label` keys are
/// allowed, keys may not repeat, and values must be non-empty.
fn parse_prompt_frontmatter(text: &str) -> Result<(Option<String>, Option<String>), UsageError> {
    let mut lines = text.lines();
    if lines.next().map(|first| first.trim_end_matches('\r')) != Some("---") {
        return Ok((None, None));
    }
    let mut branch: Option<String> = None;
    let mut label: Option<String> = None;
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line == "---" {
            return Ok((branch, label));
        }
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(UsageError(format!(
                "malformed frontmatter line {line:?}: expected `key: value`"
            )));
        };
        let value = value.trim();
        if value.is_empty() {
            return Err(UsageError(format!("frontmatter key {key:?} needs a non-empty value")));
        }
        let slot = match key.trim() {
            "branch" => &mut branch,
            "label" => &mut label,
            other => {
                return Err(UsageError(format!(
                    "unknown frontmatter key {other:?} (want branch or label)"
                )));
            }
        };
        if slot.is_some() {
            return Err(UsageError(format!("duplicate frontmatter key {key:?}")));
        }
        *slot = Some(value.to_string());
    }
    Err(UsageError("unterminated frontmatter block: missing closing `---`".into()))
}

fn build_new_browser_tab(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({ "url": flags.required("url")? });
    flags.insert_optional_u64(&mut value, "pane")?;
    flags.insert_optional_size(&mut value)?;
    Ok(value)
}

fn build_new_workspace(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "name");
    flags.insert_optional_size(&mut value)?;
    Ok(value)
}

fn build_new_screen(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_u64(&mut value, "workspace")?;
    flags.insert_optional_size(&mut value)?;
    Ok(value)
}

fn build_split(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({ "pane": flags.required_u64("pane")?, "dir": flags.required_dir()? });
    flags.insert_optional_size(&mut value)?;
    // Issue #77 AC4: branch/label record a worktree on the new pane.
    flags.insert_optional_string(&mut value, "branch");
    flags.insert_optional_string(&mut value, "label");
    // Issue #76: --exec / --env layer on top.
    insert_exec_env(flags, &mut value)?;
    Ok(value)
}

fn build_set_ratio(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({
        "pane": flags.required_u64("pane")?,
        "dir": flags.required_dir()?,
        "ratio": flags.required_f32("ratio")?,
    }))
}

fn build_set_default_colors(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "fg");
    flags.insert_optional_string(&mut value, "bg");
    Ok(value)
}

fn build_rename_pane(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "pane": flags.required_u64("pane")?, "name": flags.required("name")? }))
}

fn build_rename_surface(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "surface": flags.required_u64("surface")?, "name": flags.required("name")? }))
}

fn build_rename_screen(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "screen": flags.required_u64("screen")?, "name": flags.required("name")? }))
}

fn build_rename_workspace(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "workspace": flags.required_u64("workspace")?, "name": flags.required("name")? }))
}

/// `pane-worktree-create` (issue #77): `--branch` names the branch to
/// create via `git worktree add -b`; `--label` is a display badge.
fn build_pane_worktree_create(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value =
        json!({ "pane": flags.required_u64("pane")?, "branch": flags.required("branch")? });
    flags.insert_optional_string(&mut value, "label");
    Ok(value)
}

fn build_pane_worktree_list(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "pane": flags.required_u64("pane")? }))
}

fn build_pane_worktree_remove(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({
        "pane": flags.required_u64("pane")?,
        "branch": flags.required("branch")?,
    }))
}

/// Rewrite the issue-#77 three-word verb form (`mtyx pane worktree
/// create ...`) into the canonical flat verb (`pane-worktree-create`)
/// at the first-command position, so the issue's documented invocation
/// works verbatim while the wire protocol keeps the flat kebab-case
/// shape every other verb uses (scout plan §2.8). Called from `main`
/// BEFORE `is_cli_invocation`, which otherwise would not recognise the
/// triple as a CLI invocation at all.
pub(crate) fn rewrite_pane_worktree_alias(args: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" | "--session" => i += 2,
            // Issue #98: `--flag=value` spellings of the global flags.
            arg if arg.starts_with("--socket=") || arg.starts_with("--session=") => i += 1,
            "--json" => i += 1,
            "pane"
                if args.get(i + 1).map(String::as_str) == Some("worktree")
                    && matches!(
                        args.get(i + 2).map(String::as_str),
                        Some("create" | "list" | "remove")
                    ) =>
            {
                let flat = format!("pane-worktree-{}", args[i + 2]);
                args.splice(i..i + 3, [flat]);
                return;
            }
            _ => return,
        }
    }
}

/// Issue #91: tmux-style verb shorthands, rewritten to the canonical
/// spelling at the verb position (the first token after the global
/// `--socket`/`--session`/`--json` flags) BEFORE dispatch. Exact
/// whole-word match only — never a prefix match — so `ls` cannot
/// shadow `list-sessions`, `new` cannot shadow `new-tab`, and `at`
/// cannot shadow `attach-surface`. Because the rewrite lands before
/// `is_cli_invocation`/`cli::run`, every handler downstream of dispatch
/// sees the long form: the socket request's `cmd` field — what the
/// server switches on — is byte-identical to the long-form invocation,
/// so the `--json` contract is unchanged. `at` targets the TUI
/// `attach` subcommand (not a control-socket verb), so `mtyx at`
/// dispatches exactly like `mtyx attach`. `send` is not aliased: it is
/// already the short form, no longer spelling exists.
const VERB_ALIASES: &[(&str, &str)] = &[
    ("ls", "list-workspaces"),
    ("new", "new-workspace"),
    ("at", "attach"),
    ("read", "read-screen"),
    ("shot", "screenshot"),
];

pub(crate) fn resolve_verb_alias(args: &mut [String]) {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" | "--session" => i += 2,
            // Issue #98: `--flag=value` spellings of the global flags.
            arg if arg.starts_with("--socket=") || arg.starts_with("--session=") => i += 1,
            "--json" => i += 1,
            arg => {
                if let Some((_, canonical)) = VERB_ALIASES.iter().find(|(a, _)| *a == arg) {
                    args[i] = (*canonical).to_string();
                }
                return;
            }
        }
    }
}

/// A colour value is required so an omitted flag never silently clears
/// the workspace colour. `--color` is primary; `--colour` remains an alias.
fn build_set_workspace_color(flags: &FlagMap) -> Result<Value, UsageError> {
    let workspace = flags.required_u64("workspace")?;
    let color = match (flags.optional("color"), flags.optional("colour")) {
        (Some(_), Some(_)) => return Err(UsageError("use only one of --color or --colour".into())),
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return Err(UsageError("missing --color".into())),
    };
    let colour = if color.is_empty() { Value::Null } else { json!(color) };
    Ok(json!({ "workspace": workspace, "colour": colour }))
}

fn build_set_status(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({ "icon": flags.required("icon")? });
    flags.insert_optional_u64(&mut value, "workspace")?;
    Ok(value)
}

fn build_workspace_color(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "name": flags.required("name")?, "color": flags.required("color")? }))
}

fn build_trigger_flash(flags: &FlagMap) -> Result<Value, UsageError> {
    let workspace = flags.required_u64("workspace")?;
    let mut value = json!({ "workspace": workspace });
    flags.insert_optional_u64(&mut value, "surface")?;
    Ok(value)
}

fn build_resize_surface(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({
        "surface": flags.required_u64("surface")?,
        "cols": flags.required_u16("cols")?,
        "rows": flags.required_u16("rows")?,
    }))
}

fn build_select_tab(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = selector_request(flags)?;
    flags.insert_optional_u64(&mut value, "pane")?;
    Ok(value)
}

fn build_select_screen(flags: &FlagMap) -> Result<Value, UsageError> {
    selector_request(flags)
}

fn build_select_workspace(flags: &FlagMap) -> Result<Value, UsageError> {
    selector_request(flags)
}

fn build_move_tab(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({
        "surface": flags.required_u64("surface")?,
        "pane": flags.required_u64("pane")?,
        "index": flags.required_usize("index")?,
    }))
}

fn build_move_workspace(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({
        "workspace": flags.required_u64("workspace")?,
        "index": flags.required_usize("index")?,
    }))
}

fn build_scroll_surface(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({
        "surface": flags.required_u64("surface")?,
        "delta": flags.required_isize("delta")?,
    }))
}

fn build_report_agent(flags: &FlagMap) -> Result<Value, UsageError> {
    // Issue #75 AC1: --surface defaults to $MTYX_MUX_SURFACE so a pane's
    // own child (hook or agent) can self-report without knowing its id.
    let surface = match flags.optional("surface") {
        Some(raw) => parse_u64("surface", &raw)?,
        None => match std::env::var("MTYX_MUX_SURFACE") {
            Ok(value) => parse_u64("MTYX_MUX_SURFACE", &value)?,
            Err(_) => {
                return Err(UsageError(
                    "--surface is required (or run inside a mtyx pane via $MTYX_MUX_SURFACE)"
                        .into(),
                ));
            }
        },
    };
    // --source defaults to "socket": an in-pane self-report keeps the
    // existing authority model where hook reports still override it.
    let mut value = json!({
        "surface": surface,
        "state": flags.required("state")?,
        "source": flags.optional("source").unwrap_or_else(|| "socket".into()),
    });
    if let Some(session) = flags.optional("agent-session") {
        value["session"] = json!(session);
    }
    if let Some(agent) = flags.optional("agent") {
        value["agent"] = json!(agent);
    }
    if let Some(message) = flags.optional("message") {
        value["message"] = json!(message);
    }
    Ok(value)
}

fn build_list_agents(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_u64(&mut value, "surface")?;
    flags.insert_optional_string(&mut value, "state");
    Ok(value)
}

/// Issue #78 AC4: `agent-pattern-add`. Patterns are substring/glob (`*`
/// wildcard), not regex — validated server-side against the same values.
fn build_agent_pattern_add(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({
        "name": flags.required("name")?,
        "pattern": flags.required("pattern")?,
    });
    flags.insert_optional_string(&mut value, "kind");
    flags.insert_optional_string(&mut value, "confidence");
    if let Some(ci) = flags.optional_bool("case-insensitive") {
        value["case_insensitive"] = json!(ci);
    }
    Ok(value)
}

fn build_agent_read(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({ "target": flags.required("target")? });
    if let Some(source) = flags.optional("source") {
        if !matches!(source.as_str(), "visible" | "recent" | "recent-unwrapped") {
            return Err(UsageError(format!(
                "--source must be one of visible, recent, recent-unwrapped (got {source:?})"
            )));
        }
        value["source"] = json!(source);
    }
    if let Some(lines) = flags.optional("lines") {
        value["lines"] = json!(parse_usize("lines", &lines)?);
    }
    Ok(value)
}

fn build_agent_pattern_remove(flags: &FlagMap) -> Result<Value, UsageError> {
    Ok(json!({ "name": flags.required("name")? }))
}

fn build_agent_send(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({
        "target": flags.required("target")?,
        "text": flags.required("text")?,
    });
    if let Some(shell) = flags.optional("shell") {
        if !matches!(shell.as_str(), "auto" | "fish" | "bash" | "zsh" | "sh" | "nu" | "raw") {
            return Err(UsageError(format!(
                "--shell must be one of auto, fish, bash, zsh, sh, nu, raw (got {shell:?})"
            )));
        }
        value["shell"] = json!(shell);
    }
    Ok(value)
}

fn build_wait_agent_status(flags: &FlagMap) -> Result<Value, UsageError> {
    // Issue #75 AC5: the issue's flag names (--status / --timeout in ms),
    // mapped onto the wire's state/timeout_ms.
    let status = flags.required("status")?;
    if !matches!(status.as_str(), "idle" | "working" | "blocked" | "done" | "unknown") {
        return Err(UsageError(format!(
            "--status must be one of idle, working, blocked, done, unknown (got {status:?})"
        )));
    }
    Ok(json!({
        "target": flags.required("target")?,
        "state": status,
        "timeout_ms": parse_u64("timeout", &flags.required("timeout")?)?,
    }))
}

fn build_kill_session(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "session");
    Ok(value)
}

/// Layout-verb parsers carry their flags; the runners below do the
/// required-flag checks, the file I/O, and their own exit-code maps
/// (issue #76).
fn build_layout_export(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "workspace");
    flags.insert_optional_string(&mut value, "output");
    Ok(value)
}

fn build_layout_apply(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "input");
    flags.insert_optional_string(&mut value, "workspace");
    Ok(value)
}

fn build_layout_export_all(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "output-dir");
    Ok(value)
}

/// Issue #84 screenshot parser: carries the flags (the positional <file>
/// already landed in `output` during parse); the runner does the
/// required checks, the file I/O, and the exit-code map — special-cased
/// in `run_command` like the layout verbs.
fn build_screenshot(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "surface");
    flags.insert_optional_string(&mut value, "output");
    Ok(value)
}

/// `rename-session` parser: carry the `--old`/`--new` flags. The real
/// connect/send (and CLI-side name validation + exit-code map) live in
/// `run_rename_session`, which is special-cased in `run_command` like
/// the other name-keyed verbs (list/kill-session/kill-stale).
fn build_rename_session(flags: &FlagMap) -> Result<Value, UsageError> {
    let mut value = json!({});
    flags.insert_optional_string(&mut value, "old");
    flags.insert_optional_string(&mut value, "new");
    Ok(value)
}

/// Runtime dir honoured by `global` for the name-keyed verbs
/// (`kill-session`, `rename-session`): the parent of an explicit
/// `--socket`, else the canonical `platform::runtime_dir()`.
/// Deliberately NOT legacy-aware — those verbs address one exact
/// directory; legacy probing belongs to [`discover_sessions`] and
/// `mux_core::server::client_socket_path`.
pub(crate) fn get_runtime_dir(global: &GlobalArgs) -> PathBuf {
    global
        .socket
        .as_ref()
        .and_then(|s| s.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(mux_core::platform::runtime_dir)
}

/// Directories [`discover_sessions`] scans, in precedence order
/// (issue #83).
///
/// An explicit `--socket <parent>/x.sock` pins discovery to exactly that
/// parent — verbatim, no legacy fallback: the caller asked for one
/// specific location and must not see unrelated roots. Otherwise the
/// canonical `mtyx-<uid>` root is scanned first, then the pre-rename
/// `cmux-<uid>` root, so sessions still served by a live legacy daemon
/// stay listable and attachable. The legacy root is probe-only: the
/// server never binds it.
fn discovery_roots(global: &GlobalArgs) -> Vec<PathBuf> {
    match global.socket.as_ref().and_then(|s| s.parent()) {
        Some(parent) => vec![parent.to_path_buf()],
        None => vec![mux_core::platform::runtime_dir(), mux_core::platform::legacy_runtime_dir()],
    }
}

pub(crate) fn read_pid_file(path: &std::path::Path) -> Option<u32> {
    if let Ok(content) = std::fs::read_to_string(path) {
        content.trim().parse::<u32>().ok()
    } else {
        None
    }
}

/// One discovered mtyx session (issue #63 L1).
///
/// `socket_path` is the exact path to reconnect to; `mtime` is for the
/// picker's newest-first sort (pid-file mtime preferred — the socket mtime
/// can shift on each connect — falling back to socket mtime, then `None`).
#[derive(Clone, Debug)]
pub(crate) struct DiscoveredSession {
    pub(crate) session: String,
    pub(crate) socket_path: PathBuf,
    pub(crate) pid: Option<u32>,
    pub(crate) live: bool,
    pub(crate) mtime: Option<std::time::SystemTime>,
}

/// Socket-centric discovery of mtyx sessions across the runtime roots
/// selected by [`discovery_roots`] (issue #83): the canonical `mtyx-<uid>`
/// dir AND, unless `--socket` pinned discovery to one parent, the legacy
/// `cmux-<uid>` dir. One row per `*.sock`: derive the pid via
/// `server::pid_path`, liveness via `server::is_session_socket_live`, and
/// an mtime for uptime sort. Shared by `run_list_sessions`,
/// `run_kill_stale`, `run_attach_session_list_json`, and the interactive
/// picker. Returned unsorted (read_dir order is filesystem-dependent);
/// callers sort as needed. Each row's `socket_path` names the root it was
/// found in, so attach reconnects to the right (possibly legacy) socket.
pub(crate) fn discover_sessions(global: &GlobalArgs) -> Vec<DiscoveredSession> {
    discover_sessions_in_roots(&discovery_roots(global))
}

/// [`discover_sessions`] with an explicit, ordered root list. Roots are
/// scanned in order and a session name already seen in an earlier root is
/// skipped, so on a same-named collision the canonical root (scanned
/// first) wins. Split out from `discover_sessions` so unit tests can drive
/// two synthetic roots without mutating process-global environment.
fn discover_sessions_in_roots(roots: &[PathBuf]) -> Vec<DiscoveredSession> {
    let mut out: Vec<DiscoveredSession> = Vec::new();
    for dir in roots {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if out.iter().any(|s| s.session == stem) {
                continue;
            }
            let pid_p = mux_core::server::pid_path(&path);
            let pid = read_pid_file(&pid_p);
            let live = mux_core::server::is_session_socket_live(&path);
            let mtime = std::fs::metadata(&pid_p)
                .and_then(|m| m.modified())
                .or_else(|_| std::fs::metadata(&path).and_then(|m| m.modified()))
                .ok();
            out.push(DiscoveredSession {
                session: stem.to_string(),
                socket_path: path,
                pid,
                live,
                mtime,
            });
        }
    }
    out
}

fn run_list_sessions(global: &GlobalArgs, _flags: &FlagMap) -> i32 {
    let mut sessions = discover_sessions(global);
    // Preserve the historical alphabetical order: the old impl built the
    // name set from a BTreeSet, and read_dir order is filesystem-dependent.
    sessions.sort_by(|a, b| a.session.cmp(&b.session));

    if global.json {
        let json_list: Vec<Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "session": s.session,
                    "name": s.session,
                    "pid": s.pid,
                    "status": if s.live { "live" } else { "stale" },
                })
            })
            .collect();
        let payload = json!({ "sessions": json_list });
        if serde_json::to_writer(io::stdout(), &payload).is_ok() {
            println!();
            0
        } else {
            3
        }
    } else {
        // Optional issue-#83 affordance: badge rows whose socket lives in
        // the legacy `cmux-<uid>` root so the origin is visible in human
        // output too (--json carries socket_path per entry already).
        let legacy_root = mux_core::platform::legacy_runtime_dir();
        for s in &sessions {
            let pid_str = s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
            let status = if s.live { "live" } else { "stale" };
            if s.socket_path.parent() == Some(legacy_root.as_path()) {
                println!("{} {} {} legacy", s.session, pid_str, status);
            } else {
                println!("{} {} {}", s.session, pid_str, status);
            }
        }
        0
    }
}

/// `mtyx attach --session-list --json` (issue #63 L1): non-interactive
/// discovery dump. Same shape as `run_list_sessions`'s JSON branch PLUS a
/// `socket_path` per entry, so a caller can reconnect to the exact socket
/// — important when discovery is scoped by `--socket <parent>/x.sock` and
/// `runtime_dir()` would resolve elsewhere, and since issue #83 also for
/// legacy `cmux-<uid>` sessions, whose `socket_path` names that root.
/// Exit 0; 3 on write error.
pub(crate) fn run_attach_session_list_json(global: &GlobalArgs) -> i32 {
    let mut sessions = discover_sessions(global);
    sessions.sort_by(|a, b| a.session.cmp(&b.session));
    let json_list: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "session": s.session,
                "name": s.session,
                "pid": s.pid,
                "status": if s.live { "live" } else { "stale" },
                "socket_path": s.socket_path.display().to_string(),
            })
        })
        .collect();
    let payload = json!({ "sessions": json_list });
    if serde_json::to_writer(io::stdout(), &payload).is_ok() {
        println!();
        0
    } else {
        3
    }
}

/// Signal a session daemon: SIGTERM/SIGKILL on unix; on Windows there
/// is no cross-console graceful signal (a headless daemon has no
/// console for GenerateConsoleCtrlEvent), so both spellings
/// hard-terminate via TerminateProcess — `kill-session` on Windows is
/// effectively always the escalated form. Documented degradation.
#[cfg(unix)]
fn signal_session_pid(pid: u32, kill: bool) {
    let sig = if kill { libc::SIGKILL } else { libc::SIGTERM };
    let _ = unsafe { libc::kill(pid as libc::pid_t, sig) };
}

#[cfg(windows)]
fn signal_session_pid(pid: u32, _kill: bool) {
    let _ = pid;
    // mux-core owns the OpenProcess/TerminateProcess plumbing.
    // Public re-export keeps mux-tui free of direct windows-sys use here.
    mux_core::win_terminate_pid(pid);
}

/// Kill the mtyx process owning `socket_path` (SIGTERM, escalate to SIGKILL
/// after 2s, reap up to 1s more) and remove its `.sock`/`.pid`. Shared by
/// `run_kill_session` and the picker's kill-focused (Claim 3). Returns true
/// if the pidfile named a live mtyx process that was signalled (regardless
/// of whether it died in time); false if there was no pid / no mtyx process.
/// The `.sock`/`.pid` are removed unconditionally, matching the historical
/// `run_kill_session` behaviour.
pub(crate) fn kill_session_at(socket_path: &std::path::Path, pid: Option<u32>) -> bool {
    if let Some(pid) = pid {
        if mux_core::server::is_cmux_process(pid) {
            signal_session_pid(pid, false);
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if !mux_core::server::is_process_alive(pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if mux_core::server::is_process_alive(pid) {
                signal_session_pid(pid, true);
                let deadline2 = std::time::Instant::now() + Duration::from_secs(1);
                while std::time::Instant::now() < deadline2 {
                    if !mux_core::server::is_process_alive(pid) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    let pid_p = mux_core::server::pid_path(socket_path);
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(&pid_p);
    pid.is_some()
}

/// Outcome of a `rename-session` RPC. Distinguishes a transport failure
/// (CLI exit 3) from a server-reported error (CLI exit 1) so the verb's
/// exit-code table (scout-plan Q5) can map them separately. Shared by the
/// CLI verb (`run_rename_session`) and the picker helper.
enum RenameOutcome {
    /// Server reported ok:true. Carries the new socket path and the
    /// (unchanged) daemon pid from the response.
    Ok { socket_path: PathBuf, pid: u64 },
    /// Server reported ok:false (rename refused/failed) or a malformed reply.
    ServerErr(String),
    /// Could not (re)establish the socket connection or the transport died.
    ConnectErr(String),
}

/// Connect to the daemon at `socket`, send `{"cmd":"rename-session",
/// "new_name":new_name}`, and read one response (skipping any pushed
/// events). Returns the parsed outcome. Used by both `run_rename_session`
/// (CLI verb) and `rename_session_at` (picker helper) so they share one
/// code path.
/// Outcome of a one-shot control-socket RPC (connect → write one request
/// line → read the first matching response, skipping pushed events).
/// Distinguishes a transport failure (`ConnectErr`) from a server-reported
/// error (`ServerErr`, also used for malformed replies) so callers like the
/// `rename-session` exit-code table can map them separately. Shared by
/// `rename_rpc`, the session-manager overlay's `list-workspaces` fetch, and
/// its `select-workspace` remote-focus one-shot (issue #63 L3).
pub(crate) enum OneShotOutcome {
    /// Server reported `ok:true`. Carries the full parsed response.
    Ok(Value),
    /// Server reported `ok:false`, sent a malformed reply, or the transport
    /// closed before a reply.
    ServerErr(String),
    /// Could not establish the connection or a write/read failed.
    ConnectErr(String),
}

/// Connect to `socket`, serialise `request` as one JSON line tagged with
/// `REQUEST_ID`, write it, and read the first non-event response. Bounded by
/// a 10s read timeout set on the fresh stream. No behaviour change versus
/// the inlined body `rename_rpc` previously had; the rename flow keeps its
/// own `RenameOutcome` so its exit-code table (server vs connect error) is
/// preserved, and now just maps from this generic outcome.
pub(crate) fn one_shot_rpc(socket: &std::path::Path, request: Value) -> OneShotOutcome {
    let mut stream = match transport::connect(socket) {
        Ok(stream) => stream,
        Err(err) => {
            return OneShotOutcome::ConnectErr(format!(
                "cannot connect to session socket {}: {err}",
                socket.display()
            ));
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = match serde_json::to_vec(&request) {
        Ok(line) => line,
        Err(err) => return OneShotOutcome::ServerErr(format!("failed to encode request: {err}")),
    };
    line.push(b'\n');
    if let Err(err) = stream.write_all(&line) {
        return OneShotOutcome::ConnectErr(format!("transport error: {err}"));
    }
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => return OneShotOutcome::ServerErr("transport closed before response".into()),
            Ok(_) => {}
            Err(err) => return OneShotOutcome::ConnectErr(format!("transport error: {err}")),
        }
        let value: Value = match serde_json::from_str(&buf) {
            Ok(value) => value,
            Err(err) => return OneShotOutcome::ServerErr(format!("bad response: {err}")),
        };
        if value.get("event").is_some() {
            continue;
        }
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            return OneShotOutcome::Ok(value);
        }
        let err = value.get("error").and_then(Value::as_str).unwrap_or("request failed");
        return OneShotOutcome::ServerErr(err.to_string());
    }
}

/// Send `rename-session` to the daemon at `socket` and parse the reply into
/// the rename-specific outcome. Delegates the connect/write/read-loop to
/// `one_shot_rpc` so the rename CLI verb, the picker helper, and the
/// session-manager overlay share one transport path.
fn rename_rpc(socket: &std::path::Path, new_name: &str) -> RenameOutcome {
    let request = json!({ "cmd": "rename-session", "new_name": new_name, "id": REQUEST_ID });
    match one_shot_rpc(socket, request) {
        OneShotOutcome::Ok(value) => {
            let data = value.get("data").unwrap_or(&Value::Null);
            let socket_path = data.get("socket_path").and_then(Value::as_str).map(PathBuf::from);
            let pid = data.get("pid").and_then(Value::as_u64);
            match (socket_path, pid) {
                (Some(p), Some(pid)) => RenameOutcome::Ok { socket_path: p, pid },
                _ => RenameOutcome::ServerErr("rename response missing socket_path/pid".into()),
            }
        }
        OneShotOutcome::ServerErr(e) => RenameOutcome::ServerErr(e),
        OneShotOutcome::ConnectErr(e) => RenameOutcome::ConnectErr(e),
    }
}

/// Send `rename-session` to the daemon bound at `socket_path` and return
/// the new socket path on success. Shared by the picker's `r` flow so the
/// TUI keybinding and the CLI verb exercise one code path (`rename_rpc`).
pub(crate) fn rename_session_at(
    socket_path: &std::path::Path,
    new_name: &str,
) -> Result<PathBuf, String> {
    match rename_rpc(socket_path, new_name) {
        RenameOutcome::Ok { socket_path, .. } => Ok(socket_path),
        RenameOutcome::ServerErr(e) | RenameOutcome::ConnectErr(e) => Err(e),
    }
}

/// Send `select-workspace` (by index) as a one-shot RPC to the daemon at
/// `socket` so a *different* session lands on workspace `index`. Used by the
/// in-TUI session manager overlay (issue #63 L3) to focus a workspace in
/// another session before the running TUI switches to it. Reuses
/// `one_shot_rpc`, the same path the rename flow rides. Best-effort: an
/// unreachable socket yields `Err` (the caller renders an `[unreachable]`
/// column rather than crashing).
pub(crate) fn select_workspace_remote(
    socket: &std::path::Path,
    index: usize,
) -> Result<(), String> {
    let request = json!({ "cmd": "select-workspace", "index": index, "id": REQUEST_ID });
    match one_shot_rpc(socket, request) {
        OneShotOutcome::Ok(_) => Ok(()),
        OneShotOutcome::ServerErr(e) | OneShotOutcome::ConnectErr(e) => Err(e),
    }
}

/// `kill-session` targets are matched EXACTLY, case-sensitively (issue
/// #98 AC3): the socket/pid paths are built verbatim from the given
/// name, so `mtyx kill-session --session Main` fails cleanly when only
/// `main` exists — there is deliberately no case-insensitive fallback.
fn run_kill_session(global: &GlobalArgs, flags: &FlagMap) -> i32 {
    let target_session = flags.optional("session").or_else(|| global.session.clone());
    let Some(session_name) = target_session else {
        return cli_error(global.json, 2, "mtyx: --session is required");
    };

    let dir = get_runtime_dir(global);
    let sock_path = dir.join(format!("{session_name}.sock"));
    let pid_p = dir.join(format!("{session_name}.pid"));

    if !sock_path.exists() && !pid_p.exists() {
        return cli_error(global.json, 1, &format!("mtyx: session {session_name:?} not found"));
    }

    let pid = read_pid_file(&pid_p);
    kill_session_at(&sock_path, pid);

    if global.json {
        println!("{}", json!({ "ok": true }));
    }
    0
}

fn run_kill_stale(global: &GlobalArgs, _flags: &FlagMap) -> i32 {
    let cleaned = kill_stale(global);
    if global.json {
        println!("{}", json!({ "ok": true, "cleaned": cleaned }));
    }
    0
}

/// Kill every stale (socket-not-connectable) session in the runtime dir
/// honoured by `global` and return how many were cleaned. Mirrors the
/// historical `run_kill_stale` semantics (remove `.sock` + `.pid` for each
/// `!live` row). Shared by the `kill-stale` CLI verb, the interactive
/// pre-attach picker (L1), and the in-TUI session manager (L3) so all three
/// exercise one code path.
pub(crate) fn kill_stale(global: &GlobalArgs) -> usize {
    let mut sessions = discover_sessions(global);
    sessions.sort_by(|a, b| a.session.cmp(&b.session));
    let mut cleaned = 0;
    for s in &sessions {
        if !s.live {
            let _ = std::fs::remove_file(&s.socket_path);
            let _ = std::fs::remove_file(mux_core::server::pid_path(&s.socket_path));
            cleaned += 1;
        }
    }
    cleaned
}

/// `mtyx rename-session --old <name> --new <name>` (issue #63). Resolves
/// the old session's socket the same way `kill-session` does (parent of
/// `--socket`, else `runtime_dir()`), pre-checks the target, then connects
/// and issues `rename-session`. Exit-code table (scout-plan Q5):
///   0 success · 1 old not found / server ok:false · 2 bad/missing flags,
///     invalid name, or target already live · 3 connect/transport failure.
fn run_rename_session(global: &GlobalArgs, flags: &FlagMap) -> i32 {
    // Parse --old/--new (UsageError -> exit 2).
    let old = match flags.required("old") {
        Ok(v) => v,
        Err(err) => {
            eprintln!("mtyx: {}", err.0);
            return 2;
        }
    };
    let new = match flags.required("new") {
        Ok(v) => v,
        Err(err) => {
            eprintln!("mtyx: {}", err.0);
            return 2;
        }
    };
    // CLI-side name validation (defence in depth; exit 2 before connecting).
    // The server re-validates as the security authority. Validate both the
    // source (`--old`) and destination (`--new`) so a bad `--old` yields a
    // clean "session name …" error instead of a cryptic "session not found".
    for name in [&old, &new] {
        if let Err(err) = mux_core::server::validate_session_name(name) {
            eprintln!("mtyx: {err}");
            return 2;
        }
    }

    let dir = get_runtime_dir(global);
    let old_sock = dir.join(format!("{old}.sock"));
    let old_pid = dir.join(format!("{old}.pid"));
    let new_sock = dir.join(format!("{new}.sock"));

    // Old session must be present (mirrors kill-session's not-found exit 1).
    if !old_sock.exists() && !old_pid.exists() {
        eprintln!("mtyx: session {old:?} not found");
        return 1;
    }
    // Criterion 5: refuse a LIVE target BEFORE connecting (exit 2). The
    // server re-checks inside the handler to cover direct API use and the
    // connect-vs-precheck race.
    if mux_core::server::is_session_socket_live(&new_sock) {
        eprintln!("mtyx: session {new:?} already exists");
        return 2;
    }

    match rename_rpc(&old_sock, &new) {
        RenameOutcome::Ok { socket_path, pid } => {
            if global.json {
                println!(
                    "{}",
                    json!({
                        "session": new,
                        "socket_path": socket_path.display().to_string(),
                        "pid": pid,
                    })
                );
            }
            // Plain mode is quiet, consistent with kill-session/rename-workspace.
            0
        }
        RenameOutcome::ServerErr(err) => {
            eprintln!("mtyx: {err}");
            1
        }
        RenameOutcome::ConnectErr(err) => {
            eprintln!("{err}");
            3
        }
    }
}

// -- issue #76: layout export/apply runners ------------------------------

/// `mtyx layout-export --workspace <name-or-id> --output <path>.json`.
/// The server produces the document; the CLIENT writes the file (tmp +
/// rename, refusing symlinked targets) so no daemon ever touches the
/// invoker's filesystem. Exit codes: 0 ok · 1 server/file error · 2 bad
/// flags · 3 transport.
fn run_layout_export(global: &GlobalArgs, flags: &FlagMap) -> i32 {
    let (workspace, output) = match (flags.required("workspace"), flags.required("output")) {
        (Ok(w), Ok(o)) => (w, o),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("mtyx: {}", e.0);
            return 2;
        }
    };
    let output = PathBuf::from(output);
    if let Err(e) = refuse_symlink(&output) {
        eprintln!("mtyx: {e}");
        return 1;
    }
    let request = json!({ "cmd": "layout-export", "workspace": workspace, "id": REQUEST_ID });
    match one_shot_rpc(&resolve_socket(global), request) {
        OneShotOutcome::Ok(value) => {
            let doc = value.get("data").cloned().unwrap_or(Value::Null);
            let pretty = match serde_json::to_string_pretty(&doc) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("mtyx: encoding layout document: {e}");
                    return 1;
                }
            };
            if let Err(e) = write_json_atomic(&output, &pretty) {
                eprintln!("mtyx: writing {}: {e}", output.display());
                return 1;
            }
            if global.json {
                println!("{}", json!({ "output": output.display().to_string() }));
            } else {
                println!("{}", output.display());
            }
            0
        }
        OneShotOutcome::ServerErr(e) => {
            eprintln!("mtyx: {e}");
            1
        }
        OneShotOutcome::ConnectErr(e) => {
            eprintln!("{e}");
            3
        }
    }
}

/// `mtyx layout-apply --input <path>.json --workspace <name>` (issue #76
/// AC2): replay a saved layout, creating the workspace if missing. The
/// file is parsed structurally here (parse errors propagate, exit 2);
/// the schema gate lives server-side so a version mismatch surfaces as
/// the daemon's loud error (exit 1).
fn run_layout_apply(global: &GlobalArgs, flags: &FlagMap) -> i32 {
    let (input, workspace) = match (flags.required("input"), flags.required("workspace")) {
        (Ok(i), Ok(w)) => (i, w),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("mtyx: {}", e.0);
            return 2;
        }
    };
    let contents = match std::fs::read_to_string(&input) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mtyx: reading layout {input:?}: {e}");
            return 2;
        }
    };
    let document: mux_core::LayoutDocument = match serde_json::from_str(&contents) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mtyx: parsing layout {input:?}: {e}");
            return 2;
        }
    };
    let request = json!({ "cmd": "layout-apply", "workspace": workspace, "document": document, "id": REQUEST_ID });
    match one_shot_rpc(&resolve_socket(global), request) {
        OneShotOutcome::Ok(value) => {
            if global.json {
                if let Some(data) = value.get("data") {
                    println!("{data}");
                }
            }
            0
        }
        OneShotOutcome::ServerErr(e) => {
            eprintln!("mtyx: {e}");
            1
        }
        OneShotOutcome::ConnectErr(e) => {
            eprintln!("{e}");
            3
        }
    }
}

/// `mtyx layout-export-all --output-dir <dir>` (issue #76 AC3): fetch one
/// document per workspace and fan them out as `<dir>/<sanitized>.json`.
fn run_layout_export_all(global: &GlobalArgs, flags: &FlagMap) -> i32 {
    let dir = match flags.required("output-dir") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mtyx: {}", e.0);
            return 2;
        }
    };
    let dir = PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("mtyx: creating {}: {e}", dir.display());
        return 2;
    }
    let request = json!({ "cmd": "layout-export-all", "id": REQUEST_ID });
    match one_shot_rpc(&resolve_socket(global), request) {
        OneShotOutcome::Ok(value) => {
            let files = value
                .get("data")
                .and_then(|d| d.get("files"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if files.is_empty() {
                eprintln!("mtyx: no workspaces to export");
                return 1;
            }
            let mut written = Vec::new();
            for file in &files {
                let Some(name) = file.get("filename").and_then(Value::as_str) else {
                    eprintln!("mtyx: export-all response entry missing filename");
                    return 1;
                };
                // The server sanitizes, but never trust a path component
                // off the wire: refuse anything that could escape --output-dir.
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.contains('/')
                    || name.contains('\\')
                {
                    eprintln!("mtyx: refusing unsafe export filename {name:?}");
                    return 1;
                }
                let path = dir.join(name);
                if let Err(e) = refuse_symlink(&path) {
                    eprintln!("mtyx: {e}");
                    return 1;
                }
                let pretty = match serde_json::to_string_pretty(
                    file.get("document").unwrap_or(&Value::Null),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("mtyx: encoding layout document: {e}");
                        return 1;
                    }
                };
                if let Err(e) = write_json_atomic(&path, &pretty) {
                    eprintln!("mtyx: writing {}: {e}", path.display());
                    return 1;
                }
                written.push(path.display().to_string());
            }
            if global.json {
                println!("{}", json!({ "files": written }));
            } else {
                for path in &written {
                    println!("{path}");
                }
            }
            0
        }
        OneShotOutcome::ServerErr(e) => {
            eprintln!("mtyx: {e}");
            1
        }
        OneShotOutcome::ConnectErr(e) => {
            eprintln!("{e}");
            3
        }
    }
}

/// `mtyx screenshot --surface <id> [--output] <file>` (issue #84):
/// capture a surface's visible text to a file. The request is the plain
/// `read-screen` command — the file's bytes are exactly what `mtyx
/// read-screen --surface <id>` prints to stdout — and the CLIENT writes
/// the file (tmp + rename, refusing symlinked targets) like
/// layout-export, so no daemon ever touches the invoker's filesystem.
/// Exit codes: 0 ok · 1 server/file error · 2 bad flags · 3 transport.
fn run_screenshot(global: &GlobalArgs, flags: &FlagMap) -> i32 {
    let surface = match flags.required_u64("surface") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mtyx: {}", e.0);
            return 2;
        }
    };
    let output = match flags.required("output") {
        Ok(o) => PathBuf::from(o),
        Err(e) => {
            eprintln!("mtyx: {}", e.0);
            return 2;
        }
    };
    if let Err(e) = refuse_symlink(&output) {
        eprintln!("mtyx: {e}");
        return 1;
    }
    let request = json!({ "cmd": "read-screen", "surface": surface, "id": REQUEST_ID });
    match one_shot_rpc(&resolve_socket(global), request) {
        OneShotOutcome::Ok(value) => {
            // print_read_screen writes data["text"] verbatim; the file
            // must hold those same bytes — no trailing newline added.
            let text =
                value.get("data").and_then(|d| d.get("text")).and_then(Value::as_str).unwrap_or("");
            if let Err(e) = write_text_atomic(&output, text) {
                eprintln!("mtyx: writing {}: {e}", output.display());
                return 1;
            }
            if global.json {
                println!("{}", json!({ "output": output.display().to_string() }));
            } else {
                println!("{}", output.display());
            }
            0
        }
        OneShotOutcome::ServerErr(e) => {
            eprintln!("mtyx: {e}");
            1
        }
        OneShotOutcome::ConnectErr(e) => {
            eprintln!("{e}");
            3
        }
    }
}

/// Atomic pretty-JSON write (write-to-temp then rename — the
/// `persist::SessionSnapshot::save` pattern) so a crash or a concurrent
/// reader never observes a truncated file.
fn write_json_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    write_atomic(path, "json.tmp", contents)
}

/// Issue #84: screenshot's plain-text write, same tmp+rename discipline.
fn write_text_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    write_atomic(path, "txt.tmp", contents)
}

/// Shared core of the atomic file writers: stage the contents in a
/// sibling tmp file, then rename into place.
fn write_atomic(path: &std::path::Path, tmp_ext: &str, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(tmp_ext);
    // A leftover tmp from a crashed run could itself be a symlink; the
    // rename must never write through one.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// `fs::write` on a symlink path overwrites the TARGET, not the link —
/// refuse symlinked output paths outright (AGENTS.md review checklist).
fn refuse_symlink(path: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(format!("refusing to write through symlink {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(_) => Ok(()), // nothing there yet — fine
    }
}

fn selector_request(flags: &FlagMap) -> Result<Value, UsageError> {
    match (flags.optional("index"), flags.optional("delta")) {
        (Some(_), Some(_)) => Err(UsageError("use only one of --index or --delta".to_string())),
        (Some(index), None) => Ok(json!({ "index": parse_usize("index", &index)? })),
        (None, Some(delta)) => Ok(json!({ "delta": parse_isize("delta", &delta)? })),
        (None, None) => Err(UsageError("one of --index or --delta is required".to_string())),
    }
}

impl FlagMap {
    fn reject_remaining(&self) -> Result<(), UsageError> {
        if let Some(name) = self.values.keys().next() {
            return Err(UsageError(format!("unexpected --{name}")));
        }
        Ok(())
    }

    fn optional(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    /// Boolean-flag reader. The flag may be passed as `--flag` (true),
    /// `--flag=1` (true), `--flag=0` (false), `--flag=true`, `--flag=false`.
    /// Returns None if the flag wasn't passed at all.
    fn optional_bool(&self, name: &str) -> Option<bool> {
        self.values.get(name).map(|v| {
            // Treat any non-"0"/"false"/"no" value as true; explicit
            // "0" / "false" / "no" as false. Matches the common CLI convention.
            !matches!(v.as_str(), "0" | "false" | "no" | "False" | "No" | "FALSE" | "NO")
        })
    }

    fn required(&self, name: &str) -> Result<String, UsageError> {
        self.optional(name).ok_or_else(|| UsageError(format!("--{name} is required")))
    }

    fn required_u64(&self, name: &str) -> Result<u64, UsageError> {
        parse_u64(name, &self.required(name)?)
    }

    fn required_u16(&self, name: &str) -> Result<u16, UsageError> {
        parse_u16(name, &self.required(name)?)
    }

    fn required_usize(&self, name: &str) -> Result<usize, UsageError> {
        parse_usize(name, &self.required(name)?)
    }

    fn required_isize(&self, name: &str) -> Result<isize, UsageError> {
        parse_isize(name, &self.required(name)?)
    }

    fn required_f32(&self, name: &str) -> Result<f32, UsageError> {
        self.required(name)?
            .parse::<f32>()
            .map_err(|_| UsageError(format!("--{name} must be a number")))
    }

    fn required_dir(&self) -> Result<String, UsageError> {
        let dir = self.required("dir")?;
        if dir == "right" || dir == "down" {
            Ok(dir)
        } else {
            Err(UsageError("--dir must be right or down".to_string()))
        }
    }

    fn insert_optional_string(&self, value: &mut Value, name: &str) {
        if let Some(text) = self.optional(name) {
            value[name] = json!(text);
        }
    }

    fn insert_optional_u64(&self, value: &mut Value, name: &str) -> Result<(), UsageError> {
        if let Some(raw) = self.optional(name) {
            value[name] = json!(parse_u64(name, &raw)?);
        }
        Ok(())
    }

    fn insert_optional_size(&self, value: &mut Value) -> Result<(), UsageError> {
        match (self.optional("cols"), self.optional("rows")) {
            (Some(cols), Some(rows)) => {
                value["cols"] = json!(parse_u16("cols", &cols)?);
                value["rows"] = json!(parse_u16("rows", &rows)?);
                Ok(())
            }
            (None, None) => Ok(()),
            _ => Err(UsageError("--cols and --rows must be supplied together".to_string())),
        }
    }
}

fn parse_u64(name: &str, value: &str) -> Result<u64, UsageError> {
    value.parse::<u64>().map_err(|_| UsageError(format!("--{name} must be a uint64")))
}

fn parse_u16(name: &str, value: &str) -> Result<u16, UsageError> {
    value.parse::<u16>().map_err(|_| UsageError(format!("--{name} must be a uint16")))
}

fn parse_usize(name: &str, value: &str) -> Result<usize, UsageError> {
    value.parse::<usize>().map_err(|_| UsageError(format!("--{name} must be a usize")))
}

fn parse_isize(name: &str, value: &str) -> Result<isize, UsageError> {
    value.parse::<isize>().map_err(|_| UsageError(format!("--{name} must be an isize")))
}

fn print_empty(_: &Value, _: &mut dyn Write) -> io::Result<()> {
    Ok(())
}

/// Issue #100: `close-workspace` output. A default close lists the
/// worktree-child workspaces that SURVIVED (flagging any with a running
/// agent) so orphans are never silent; a `--group` close (closed.len()
/// > 1) also lists every workspace that went with the parent.
fn print_close_workspace(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let survivors = data["survivors"].as_array().cloned().unwrap_or_default();
    for child in &survivors {
        let id = child["workspace"].as_u64().unwrap_or(0);
        let name = child["name"].as_str().unwrap_or("?");
        let path = child["worktree_path"].as_str().unwrap_or("?");
        let agent =
            if child["running_agent"].as_bool() == Some(true) { " [agent running]" } else { "" };
        match child["worktree_branch"].as_str() {
            Some(branch) => writeln!(
                out,
                "worktree child still open: workspace {id} ({name}) in {path} (branch {branch}){agent}"
            )?,
            None => writeln!(
                out,
                "worktree child still open: workspace {id} ({name}) in {path}{agent}"
            )?,
        }
    }
    let closed = data["closed"].as_array().cloned().unwrap_or_default();
    if closed.len() > 1 {
        for ws in &closed {
            let id = ws["id"].as_u64().unwrap_or(0);
            let name = ws["name"].as_str().unwrap_or("?");
            writeln!(out, "closed workspace {id} ({name})")?;
        }
    }
    Ok(())
}

fn print_agents(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let Some(agents) = data.get("agents").and_then(Value::as_array) else {
        return Ok(());
    };
    // Issue #75 AC2: the line ends with the agent name and last message
    // (`-` when absent), so the message (which may contain spaces) is
    // always the final, unambiguous column.
    for agent in agents {
        writeln!(
            out,
            "{} {} {} {} {} {}",
            agent.get("surface").and_then(Value::as_u64).unwrap_or(0),
            agent.get("state").and_then(Value::as_str).unwrap_or("unknown"),
            agent.get("source").and_then(Value::as_str).unwrap_or("?"),
            agent.get("session").and_then(Value::as_str).unwrap_or("-"),
            agent.get("agent").and_then(Value::as_str).unwrap_or("-"),
            agent.get("message").and_then(Value::as_str).unwrap_or("-"),
        )?;
    }
    Ok(())
}

/// Issue #78 AC1 human output: `<surface> <agent> <confidence> <evidence>`.
fn print_detect_agent(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "{} {} {} {}",
        data.get("surface").and_then(Value::as_u64).unwrap_or(0),
        data.get("agent").and_then(Value::as_str).unwrap_or("unknown"),
        data.get("confidence").and_then(Value::as_str).unwrap_or("none"),
        data.get("evidence").and_then(Value::as_str).unwrap_or(""),
    )
}

/// Issue #78 AC2 human output: `<surface> <agent>` rows, id-ordered.
fn print_detect_agents(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let Some(agents) = data.get("agents").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut rows: Vec<(u64, &str)> = agents
        .iter()
        .filter_map(|(id, agent)| Some((id.parse::<u64>().ok()?, agent.as_str()?)))
        .collect();
    rows.sort_unstable();
    for (id, agent) in rows {
        writeln!(out, "{id} {agent}")?;
    }
    Ok(())
}

/// Issue #78 AC4 human output: `<name> <kind> <confidence> <pattern>`.
fn print_agent_patterns(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let Some(patterns) = data.get("patterns").and_then(Value::as_array) else {
        return Ok(());
    };
    for pattern in patterns {
        writeln!(
            out,
            "{} {} {} {}",
            pattern.get("name").and_then(Value::as_str).unwrap_or("?"),
            pattern.get("kind").and_then(Value::as_str).unwrap_or("?"),
            pattern.get("confidence").and_then(Value::as_str).unwrap_or("?"),
            pattern.get("pattern").and_then(Value::as_str).unwrap_or(""),
        )?;
    }

    Ok(())
}

/// Human stdout for `pane-worktree-create` (issue #77): just the
/// worktree path — the thing a caller pipes into something else.
/// `--json` prints the full `{pane,branch,path}` object.
fn print_worktree_created(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "{}", data.get("path").and_then(Value::as_str).unwrap_or(""))
}

/// Human stdout for `pane-worktree-list` (issue #77): one line per
/// worktree, `branch path label`, in creation order.
fn print_worktrees(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let Some(worktrees) = data.get("worktrees").and_then(Value::as_array) else {
        return Ok(());
    };
    for worktree in worktrees {
        writeln!(
            out,
            "{} {} {}",
            worktree.get("branch").and_then(Value::as_str).unwrap_or("unknown"),
            worktree.get("path").and_then(Value::as_str).unwrap_or("-"),
            worktree.get("label").and_then(Value::as_str).unwrap_or("-"),
        )?;
    }
    Ok(())
}

/// Human stdout for `mtyx get-resolved-config`: pretty-print the
/// server's resolved chrome as JSON (matches the shape that
/// `Config::resolved_chrome_value` produces and `mtyx attach
/// --print-resolved-config` prints for the merged view). `--json`
/// mode prints the same object compact via `print_response`.
fn print_get_resolved_config(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let pretty = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());
    writeln!(out, "{pretty}")
}

fn print_identify(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "mtyx session={} protocol={} pid={}",
        data.get("session").and_then(Value::as_str).unwrap_or(""),
        data.get("protocol").and_then(Value::as_u64).unwrap_or(0),
        data.get("pid").and_then(Value::as_u64).unwrap_or(0)
    )
}

fn print_read_screen(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    write!(out, "{}", data.get("text").and_then(Value::as_str).unwrap_or(""))
}

fn print_vt_state(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "cols={} rows={} data={}",
        data.get("cols").and_then(Value::as_u64).unwrap_or(0),
        data.get("rows").and_then(Value::as_u64).unwrap_or(0),
        data.get("data").and_then(Value::as_str).unwrap_or("")
    )
}

fn print_surface(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "{}", data.get("surface").and_then(Value::as_u64).unwrap_or(0))
}

fn print_tree(data: &Value, out: &mut dyn Write) -> io::Result<()> {
    let Some(workspaces) = data.get("workspaces").and_then(Value::as_array) else {
        return Ok(());
    };
    for workspace in workspaces {
        let workspace_id = id_field(workspace, "id");
        writeln!(
            out,
            "workspace id={} name={} color={} active={}",
            workspace_id,
            atom(workspace.get("name")),
            atom(workspace.get("color")),
            bool_field(workspace, "active")
        )?;
        let Some(screens) = workspace.get("screens").and_then(Value::as_array) else {
            continue;
        };
        for screen in screens {
            let screen_id = id_field(screen, "id");
            writeln!(
                out,
                "screen id={} workspace={} name={} active={} active_pane={}",
                screen_id,
                workspace_id,
                atom(screen.get("name")),
                bool_field(screen, "active"),
                id_field(screen, "active_pane")
            )?;
            let Some(panes) = screen.get("panes").and_then(Value::as_array) else {
                continue;
            };
            for pane in panes {
                let pane_id = id_field(pane, "id");
                if bool_field(pane, "dead") {
                    writeln!(out, "pane id={} screen={} dead=true", pane_id, screen_id)?;
                    continue;
                }
                writeln!(
                    out,
                    "pane id={} screen={} name={} active_tab={}",
                    pane_id,
                    screen_id,
                    atom(pane.get("name")),
                    id_field(pane, "active_tab")
                )?;
                let Some(tabs) = pane.get("tabs").and_then(Value::as_array) else {
                    continue;
                };
                for tab in tabs {
                    let size = tab.get("size");
                    let (cols, rows) = match size {
                        Some(size) if size.is_object() => {
                            (id_field(size, "cols"), id_field(size, "rows"))
                        }
                        _ => (0, 0),
                    };
                    writeln!(
                        out,
                        "tab surface={} pane={} kind={} browser_source={} name={} title={} dead={} cols={} rows={}",
                        id_field(tab, "surface"),
                        pane_id,
                        tab.get("kind").and_then(Value::as_str).unwrap_or(""),
                        atom(tab.get("browser_source")),
                        atom(tab.get("name")),
                        atom(tab.get("title")),
                        bool_field(tab, "dead"),
                        cols,
                        rows
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn id_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn atom(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => serde_json::to_string(text).unwrap_or_default(),
        Some(Value::Null) | None => "null".to_string(),
        Some(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for `cli` internals that a bin-only crate cannot expose to its
    //! integration-test file (`tests/cli.rs` links only against `mux-core`
    //! + the `mtyx` binary, not `mux-tui`'s private modules). These unit
    //! tests can call `pub(crate)` helpers directly and drive an in-process
    //! `mux-core` server — no subprocess spawn needed.
    use super::*;
    use mux_core::{server, Mux, SurfaceOptions};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// AC7/picker (scout-plan T11): the non-TUI helper the picker's `r` flow
    /// uses (`rename_session_at`) renames a live session over a direct socket
    /// connection. Driven against an in-process `mux-core` server so no
    // `CARGO_BIN_EXE_mtyx` (unavailable to in-source unit tests of a bin
    // crate) is needed. The accept thread outlives the assertion but dies
    // with the test process; the temp socket is unique per run.
    // --- prompt-file frontmatter (issue #77 AC4) ---

    #[test]
    fn prompt_frontmatter_parses_branch_and_label() {
        let text = "---\nbranch: feat-auth\nlabel: auth pane\n---\nFix the login flow.\n";
        let (branch, label) = parse_prompt_frontmatter(text).unwrap();
        assert_eq!(branch.as_deref(), Some("feat-auth"));
        assert_eq!(label.as_deref(), Some("auth pane"));

        // Only one key, blank lines tolerated, CRLF line endings.
        let text = "---\r\nbranch: x\r\n\r\n---\r\nbody";
        let (branch, label) = parse_prompt_frontmatter(text).unwrap();
        assert_eq!(branch.as_deref(), Some("x"));
        assert_eq!(label, None);
    }

    #[test]
    fn prompt_frontmatter_absent_when_file_has_no_block() {
        let (branch, label) = parse_prompt_frontmatter("just a prompt body\n").unwrap();
        assert_eq!(branch, None);
        assert_eq!(label, None);
    }

    #[test]
    fn prompt_frontmatter_rejects_malformed_blocks() {
        // Unterminated block.
        assert!(parse_prompt_frontmatter("---\nbranch: x\nbody").is_err());
        // Unknown key.
        assert!(parse_prompt_frontmatter("---\ncommit: abc\n---\nb").is_err());
        // Duplicate key.
        assert!(parse_prompt_frontmatter("---\nbranch: a\nbranch: b\n---\nb").is_err());
        // Empty value.
        assert!(parse_prompt_frontmatter("---\nbranch:\n---\nb").is_err());
        // Not a key: value line.
        assert!(parse_prompt_frontmatter("---\nfeat-auth\n---\nb").is_err());
    }

    #[test]
    fn rename_session_at_renames_via_socket() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mtyx-t11-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old_sock = dir.join("old.sock");

        // In-process daemon on old.sock (session "old").
        let mux = Mux::new("old", SurfaceOptions::default());
        server::serve(mux, Some(old_sock.clone())).expect("serve should bind old.sock");

        let new_sock =
            rename_session_at(&old_sock, "bar").expect("rename_session_at should succeed");
        assert!(new_sock.exists(), "returned new socket path should exist");
        assert_eq!(
            new_sock.file_name().and_then(|n| n.to_str()),
            Some("bar.sock"),
            "helper should return the new socket path"
        );
        assert!(server::is_session_socket_live(&new_sock));
        assert!(!old_sock.exists(), "old socket should be gone after helper rename");

        // Best-effort cleanup of the (now-renamed) files; the leaked accept
        // thread is reaped when the test process exits.
        let _ = std::fs::remove_file(&new_sock);
        let _ = std::fs::remove_file(server::pid_path(&new_sock));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- issue #83: legacy cmux-<uid> runtime-root discovery ---

    /// Make `dir` and drop a zero-byte `<name>.sock` in it. A plain file is
    /// enough: `is_session_socket_live` cannot connect to it, so it reads
    /// as `stale` — discovery only cares that the row appears.
    fn mk_sock_root(dir: &std::path::Path, names: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for name in names {
            std::fs::write(dir.join(format!("{name}.sock")), b"").unwrap();
        }
    }

    fn unique_tmp(tag: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("mtyx-83-{tag}-{}-{stamp}", std::process::id()))
    }

    /// AC2: two synthetic roots (canonical + legacy) both contribute rows,
    /// each carrying the socket_path of the root it was found in — so a
    /// legacy row reconnects to the legacy socket.
    #[test]
    fn discover_sessions_in_roots_merges_canonical_and_legacy() {
        let base = unique_tmp("merge");
        let canonical = base.join("mtyx-1000");
        let legacy = base.join("cmux-1000");
        mk_sock_root(&canonical, &["alpha"]);
        mk_sock_root(&legacy, &["beta"]);

        let found = discover_sessions_in_roots(&[canonical.clone(), legacy.clone()]);
        let mut names: Vec<String> = found.iter().map(|s| s.session.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(
            found.iter().find(|s| s.session == "alpha").unwrap().socket_path,
            canonical.join("alpha.sock")
        );
        assert_eq!(
            found.iter().find(|s| s.session == "beta").unwrap().socket_path,
            legacy.join("beta.sock"),
            "legacy row must carry its legacy socket_path so attach connects there"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// AC2: a same-named session in both roots resolves to the canonical
    /// one (the legacy row is dropped, not merged).
    #[test]
    fn discover_sessions_in_roots_canonical_wins_on_collision() {
        let base = unique_tmp("collide");
        let canonical = base.join("mtyx-1000");
        let legacy = base.join("cmux-1000");
        mk_sock_root(&canonical, &["demo"]);
        mk_sock_root(&legacy, &["demo"]);

        let found = discover_sessions_in_roots(&[canonical.clone(), legacy]);
        assert_eq!(found.len(), 1, "collision must dedupe to one row");
        assert_eq!(found[0].session, "demo");
        assert_eq!(found[0].socket_path, canonical.join("demo.sock"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit `--socket <parent>/x.sock` pins discovery to that one
    /// parent, verbatim — no legacy root appended (issue #83: must not
    /// regress).
    #[test]
    fn discovery_roots_pins_explicit_socket_parent() {
        let global = GlobalArgs {
            session: None,
            socket: Some(PathBuf::from("/tmp/mtyx-explicit/x.sock")),
            json: false,
        };
        assert_eq!(discovery_roots(&global), vec![PathBuf::from("/tmp/mtyx-explicit")]);
    }

    /// AC2 (env variant): `XDG_RUNTIME_DIR` pointing at a temp base that
    /// holds both `mtyx-<uid>/` and `cmux-<uid>/` subdirs makes the public
    /// `discover_sessions` see both roots. The env mutation is restored
    /// before any assertion so a panic cannot leak it to sibling tests.
    #[test]
    fn discover_sessions_from_env_roots_includes_legacy() {
        let base = unique_tmp("env");
        std::fs::create_dir_all(&base).unwrap();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", &base);
        let canonical = mux_core::platform::runtime_dir();
        let legacy = mux_core::platform::legacy_runtime_dir();
        mk_sock_root(&canonical, &["newdemo"]);
        mk_sock_root(&legacy, &["olddemo"]);

        let global = GlobalArgs { session: None, socket: None, json: false };
        let found = discover_sessions(&global);

        match prev {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }

        let mut names: Vec<String> = found.iter().map(|s| s.session.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["newdemo", "olddemo"]);
        assert_eq!(
            found.iter().find(|s| s.session == "olddemo").unwrap().socket_path,
            legacy.join("olddemo.sock")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // --- issue #91: tmux-style shorthand aliases ---

    /// AC3: an alias must never collide with an existing verb name (a
    /// colliding alias is dead at best and silently changes an existing
    /// verb's meaning at worst), and every alias target must be a real
    /// command — a VerbSpec, or the TUI `attach` subcommand which
    /// main.rs dispatches before the verb table.
    #[test]
    fn verb_aliases_never_collide_with_existing_verbs() {
        for (alias, canonical) in VERB_ALIASES {
            assert!(
                verb_by_name(alias).is_none(),
                "alias {alias:?} collides with an existing verb name"
            );
            assert!(
                verb_by_name(canonical).is_some() || *canonical == "attach",
                "alias {alias:?} points at unknown command {canonical:?}"
            );
        }
    }

    /// AC2 + AC1: every alias rewrites to its canonical spelling, and
    /// for verb targets `parse` then resolves the SAME VerbSpec the
    /// long form resolves (pointer-identical handler table entry). The
    /// socket request carries the canonical `cmd`, so `--json` output
    /// is byte-identical to the long-form invocation.
    #[test]
    fn verb_aliases_resolve_to_the_same_verb_spec() {
        for (alias, canonical) in VERB_ALIASES {
            // Rewritten at the verb position after global flags.
            let mut args = vec!["--json".to_string(), alias.to_string()];
            resolve_verb_alias(&mut args);
            assert_eq!(args[0], "--json", "global flags must not be touched");
            assert_eq!(args[1], *canonical, "alias {alias:?} must rewrite to {canonical:?}");

            if *canonical == "attach" {
                // Not a VerbSpec: `mtyx at` must become exactly the argv
                // that `mtyx attach` feeds main.rs's TUI subcommand
                // parse, so both spellings dispatch identically.
                continue;
            }
            let mut long = vec!["--json".to_string(), canonical.to_string()];
            match (parse(&args), parse(&long)) {
                (Ok(Parsed::Command(short)), Ok(Parsed::Command(full))) => {
                    assert!(
                        std::ptr::eq(short.verb, full.verb),
                        "{alias:?} and {canonical:?} must resolve to the same VerbSpec"
                    );
                    assert_eq!(short.verb.name, *canonical);
                }
                _ => panic!("parse failed for alias {alias:?} / {canonical:?}"),
            }
        }
    }

    /// AC3: exact whole-word match only. Prefixed verbs (`list-sessions`
    /// vs `ls`, `new-tab`/`new-screen` vs `new`, `attach-surface` vs
    /// `at`, `read-screen` vs `read`, `screenshot` vs `shot`) are never
    /// rewritten, an alias-spelled flag VALUE (`--session ls`) is not
    /// the verb position, and a verb-less argv falls through unchanged.
    #[test]
    fn verb_aliases_match_exact_words_and_never_prefixes() {
        for verb in [
            "list-sessions",
            "list-workspaces",
            "new-tab",
            "new-screen",
            "new-workspace",
            "attach-surface",
            "read-screen",
            "screenshot",
        ] {
            assert!(verb_by_name(verb).is_some(), "test premise: {verb:?} is a verb");
            let mut args = vec![verb.to_string()];
            resolve_verb_alias(&mut args);
            assert_eq!(args[0], verb, "exact verb {verb:?} must not be rewritten");
        }
        // A flag value is never the verb position.
        let mut args = vec!["--session".to_string(), "ls".to_string()];
        resolve_verb_alias(&mut args);
        assert_eq!(args[1], "ls", "an alias-spelled --session value must not be rewritten");
        // No verb at all: unchanged (TUI launch flags pass through).
        let mut args = vec!["--headless".to_string()];
        resolve_verb_alias(&mut args);
        assert_eq!(args, vec!["--headless".to_string()]);
    }

    // --- issue #98: flag order, `=` forms, `--` passthrough, envelope ---

    fn parse_ok(args: &[&str]) -> CliArgs {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse(&owned) {
            Ok(Parsed::Command(args)) => args,
            _ => panic!("parse({args:?}) did not yield a command"),
        }
    }

    fn parse_err(args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse(&owned) {
            Err(UsageError(msg)) => msg,
            _ => panic!("parse({args:?}) should have errored"),
        }
    }

    /// AC2: `--flag value` and `--flag=value` land in the same FlagMap
    /// slot, for both verb flags and the global --socket/--session.
    #[test]
    fn parse_accepts_space_and_equals_flag_forms() {
        let spaced = parse_ok(&["read-screen", "--surface", "7"]);
        assert_eq!(spaced.flags.values.get("surface").map(String::as_str), Some("7"));

        let equals = parse_ok(&["read-screen", "--surface=7"]);
        assert_eq!(equals.flags.values.get("surface").map(String::as_str), Some("7"));

        // A value may itself contain '=' (the first '=' splits).
        let env = parse_ok(&["new-tab", "--env=A=B,C=D"]);
        assert_eq!(env.flags.values.get("env").map(String::as_str), Some("A=B,C=D"));

        // Global flags accept the equals form too, before or after the verb.
        let global = parse_ok(&["--socket=/tmp/x.sock", "identify"]);
        assert_eq!(global.global.socket, Some(PathBuf::from("/tmp/x.sock")));
        let after = parse_ok(&["identify", "--session=work"]);
        assert_eq!(after.global.session.as_deref(), Some("work"));

        // Bare-bool valued forms keep their value (`--group=0` is false).
        let group = parse_ok(&["close-workspace", "--workspace=1", "--group=0"]);
        assert_eq!(group.flags.optional_bool("group"), Some(false));
    }

    /// AC2: flags parse identically before and after the positional
    /// (screenshot's <file> is the one positional-taking verb).
    #[test]
    fn parse_flags_in_any_position_relative_to_positional() {
        for argv in [
            vec!["screenshot", "--surface", "3", "out.png"],
            vec!["screenshot", "out.png", "--surface", "3"],
            vec!["screenshot", "--surface=3", "out.png"],
            vec!["screenshot", "out.png", "--surface=3"],
        ] {
            let parsed = parse_ok(&argv);
            assert_eq!(
                parsed.flags.values.get("surface").map(String::as_str),
                Some("3"),
                "{argv:?}"
            );
            assert_eq!(
                parsed.flags.values.get("output").map(String::as_str),
                Some("out.png"),
                "{argv:?}"
            );
        }
        // Passing both the positional and --output is still an error,
        // whichever way round they appear.
        assert!(parse_err(&["screenshot", "out.png", "--output", "other.png"])
            .contains("duplicate flag --output"));
        assert!(parse_err(&["screenshot", "--output", "other.png", "out.png"]).contains("once"));
    }

    /// AC3: a bare `--` ends flag parsing — everything after it is a
    /// positional, even flag-shaped tokens, global-flag names, and
    /// `--exec` (which only means exec-argv in its `--exec --` form).
    #[test]
    fn parse_dashdash_makes_following_tokens_positional() {
        let parsed = parse_ok(&["screenshot", "--surface=1", "--", "--weird.png"]);
        assert_eq!(parsed.flags.values.get("output").map(String::as_str), Some("--weird.png"));

        // `--json` after `--` is a positional, not the global flag.
        let parsed = parse_ok(&["screenshot", "--", "--json"]);
        assert!(!parsed.global.json);
        assert_eq!(parsed.flags.values.get("output").map(String::as_str), Some("--json"));

        // A verb with no positional slot rejects the token as an
        // unexpected ARGUMENT (not an unknown FLAG).
        let err = parse_err(&["read-screen", "--surface=1", "--", "--weird"]);
        assert!(err.contains("unexpected argument"), "got {err:?}");

        // `--exec` after `--` is a positional too, never the exec form.
        let err = parse_err(&["new-tab", "--", "--exec", "ls"]);
        assert!(err.contains("unexpected argument"), "got {err:?}");

        // The `--exec -- <argv>` form is untouched (issue #76).
        let parsed = parse_ok(&["new-tab", "--exec", "--", "ls", "-la"]);
        assert_eq!(parsed.flags.exec.as_deref(), Some(&["ls".to_string(), "-la".to_string()][..]));
    }

    /// AC4: the --json error envelope carries ok:false plus the exit
    /// code and message.
    #[test]
    fn error_envelope_carries_code_and_message() {
        let envelope = error_envelope(2, "unknown flag --file for agent-read");
        assert_eq!(envelope["ok"].as_bool(), Some(false));
        assert_eq!(envelope["error"]["code"].as_i64(), Some(2));
        assert_eq!(
            envelope["error"]["message"].as_str(),
            Some("unknown flag --file for agent-read")
        );
    }
}
