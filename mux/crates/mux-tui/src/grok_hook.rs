use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::hook_merge;

/// Legacy flat-array schema previously written to `.grok/hooks.json`.
/// Kept so install/uninstall can clean leftovers that Grok Build never loaded.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
struct LegacyGrokHooksConfig {
    #[serde(default)]
    hooks: Vec<LegacyGrokHook>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct LegacyGrokHook {
    event: String,
    command: String,
}

const HOOK_FILENAME: &str = "mtyx-agent-state.json";

/// Grok Build loads `$GROK_HOME/hooks/*.json` (and `<repo>/.grok/hooks/*.json`)
/// in the Claude-compatible object schema. The old installer wrote
/// `.grok/hooks.json` (wrong path) in a flat array (wrong shape) with
/// `--source grok` (rejected by `report-agent`, which only accepts
/// `socket` or `hook`).
pub(crate) fn config_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir().map(|h| h.join(".grok").join("hooks").join(HOOK_FILENAME))
    } else {
        Some(PathBuf::from(".grok").join("hooks").join(HOOK_FILENAME))
    }
}

fn legacy_config_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir().map(|h| h.join(".grok").join("hooks.json"))
    } else {
        Some(PathBuf::from(".grok").join("hooks.json"))
    }
}

fn report_command(state: &str) -> String {
    // Issue #97: invoke the *running* binary by absolute path (shell-
    // quoted — the resolved path can contain spaces), never a bare
    // `mtyx` that a rename or a stale $PATH entry could hijack.
    let bin = hook_merge::shell_quote(&hook_merge::hook_bin());
    format!(
        "test -n \"$MTYX_MUX_SURFACE\" && {bin} report-agent --surface \"$MTYX_MUX_SURFACE\" --state {state} --source hook || true"
    )
}

fn grok_native_hooks() -> Value {
    let command = |state: &str| {
        json!({
            "type": "command",
            "command": report_command(state),
            "timeout": 5
        })
    };
    let group = |state: &str| json!([{ "hooks": [command(state)] }]);
    json!({
        "hooks": {
            "SessionStart": group("working"),
            "PreToolUse": group("working"),
            "PostToolUse": group("idle"),
            "Notification": [{
                "matcher": "idle_prompt|permission_prompt",
                "hooks": [command("blocked")]
            }],
            "Stop": group("done"),
            "SubagentStart": group("working"),
            "SubagentStop": group("idle")
        }
    })
}

fn refuse_symlink(path: &Path) -> Option<i32> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            eprintln!(
                "error: refusing to overwrite symlink at {}. Remove it manually if you want to install the hooks.",
                path.display()
            );
            return Some(1);
        }
    }
    None
}

/// Strip leftover mtyx entries from the pre-fix `.grok/hooks.json`. Deletes
/// the file when nothing else remains. Leaves an unreadable or non-legacy
/// file alone so we never clobber a user config we don't understand.
fn clean_legacy(global: bool) -> bool {
    let Some(path) = legacy_config_path(global) else {
        return false;
    };
    match hook_merge::load_json::<LegacyGrokHooksConfig>(&path) {
        Ok(mut config) => {
            let before = config.hooks.len();
            // `report-agent` (not the fuller `mtyx report-agent`): also
            // removes entries carrying a quoted absolute binary path
            // installed since issue #97, alongside the bare legacy text.
            config.hooks.retain(|h| !h.command.contains("report-agent"));
            if config.hooks.is_empty() {
                let _ = fs::remove_file(&path);
                before > 0
            } else if config.hooks.len() != before {
                let _ = hook_merge::save_pretty(&path, &config);
                true
            } else {
                false
            }
        }
        Err(hook_merge::LoadError::NotFound) => false,
        Err(_) => false,
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut uninstall = false;
    let mut global = false;

    for arg in args.iter().skip(1) {
        if arg == "--uninstall" {
            uninstall = true;
        } else if arg == "--global" {
            global = true;
        }
    }

    match args.first().map(String::as_str) {
        Some("install-hooks") => run_install(uninstall, global),
        Some("install-skill") => run_install_skill(uninstall, global),
        _ => {
            eprintln!(
                "mtyx: usage: mtyx grok <install-hooks|install-skill> [--uninstall] [--global]"
            );
            2
        }
    }
}

fn run_install(uninstall: bool, global: bool) -> i32 {
    let Some(path) = config_path(global) else {
        eprintln!("error: could not resolve home directory for global hooks");
        return 1;
    };

    if uninstall {
        let mut removed = false;
        if path.exists() {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("error removing {}: {e}", path.display());
                return 1;
            }
            removed = true;
        }
        let cleaned_legacy = clean_legacy(global);
        if removed || cleaned_legacy {
            println!("Successfully removed mtyx hooks from {}", path.display());
        } else {
            println!("No Grok hooks file found at {}", path.display());
        }
        return 0;
    }

    if let Some(code) = refuse_symlink(&path) {
        return code;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("error creating {}: {e}", parent.display());
            return 1;
        }
    }

    if let Err(e) = hook_merge::save_pretty(&path, &grok_native_hooks()) {
        match e {
            hook_merge::SaveError::Serialize(e) => {
                eprintln!("error: failed to serialize config: {e}");
                return 1;
            }
            hook_merge::SaveError::Io(e) => {
                eprintln!("error writing {}: {e}", path.display());
                return 1;
            }
        }
    }
    clean_legacy(global);
    println!("Successfully installed mtyx hooks into {}", path.display());
    0
}

fn skill_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir()
            .map(|h| h.join(".grok").join("skills").join("mtyx-orchestration").join("SKILL.md"))
    } else {
        Some(PathBuf::from(".agents").join("skills").join("mtyx-orchestration").join("SKILL.md"))
    }
}

fn run_install_skill(uninstall: bool, global: bool) -> i32 {
    let Some(path) = skill_path(global) else {
        eprintln!("error: could not resolve home directory");
        return 1;
    };

    if uninstall {
        if path.exists() {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("error removing {}: {e}", path.display());
                return 1;
            }
            if let Some(parent) = path.parent() {
                let _ = fs::remove_dir(parent);
                if let Some(grandparent) = parent.parent() {
                    let _ = fs::remove_dir(grandparent);
                }
            }
            println!("Successfully removed mtyx skill from {}", path.display());
        } else {
            println!("No mtyx skill found at {}", path.display());
        }
        0
    } else {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // Refuse to overwrite a symlink: same rationale as claude_hook.rs
        // (PR #18 / issue #10 hardening) and aider_hook.rs (PR #3) — fs::write
        // on a symlink path overwrites the symlink target, not the symlink
        // itself. An attacker-placed symlink in a user-writable target path
        // could redirect the write to an arbitrary file.
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                eprintln!(
                    "error: refusing to overwrite symlink at {}.                      Remove it manually if you want to install the skill.",
                    path.display()
                );
                return 1;
            }
        }
        if let Err(e) = fs::write(&path, crate::skill_content::ORCHESTRATION_SKILL) {
            eprintln!("error writing {}: {e}", path.display());
            return 1;
        }
        println!("Successfully installed mtyx skill into {}", path.display());
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_hooks_use_grok_object_schema_and_hook_source() {
        let hooks = grok_native_hooks();
        let pre = &hooks["hooks"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(pre["type"], "command");
        let command = pre["command"].as_str().expect("command is a string");
        assert!(command.contains("--source hook"), "{command}");
        assert!(!command.contains("--source grok"), "{command}");
        assert!(command.contains("test -n \"$MTYX_MUX_SURFACE\""), "{command}");
        assert_eq!(hooks["hooks"]["Notification"][0]["matcher"], "idle_prompt|permission_prompt");
    }

    /// Issue #97 AC1: every emitted command invokes the running binary by
    /// its current_exe() absolute path, never a bare `mtyx` PATH lookup.
    #[test]
    fn native_hooks_invoke_the_running_binary_absolute_path() {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap();
        // The hooks are compared as serde_json text, where backslashes in
        // Windows paths are escaped (`\\`). Compare against the escaped
        // form so the assert is platform-neutral.
        let exe_json = serde_json::to_string(&exe).unwrap().trim_matches('"').to_string();
        let quoted = hook_merge::shell_quote(&exe_json);
        let hooks = grok_native_hooks();
        let text = serde_json::to_string(&hooks).unwrap();
        assert!(text.contains(&quoted), "commands must name the running binary: {text}");
        assert!(
            !text.contains("mtyx report-agent"),
            "a bare-mtyx PATH lookup must not survive: {text}"
        );
        // The shell guard still wraps the invocation.
        let command = hooks["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.starts_with("test -n \"$MTYX_MUX_SURFACE\" && "), "{command}");
    }

    #[test]
    fn config_path_is_the_grok_hooks_directory() {
        let project = config_path(false).expect("project path");
        assert_eq!(project, PathBuf::from(".grok").join("hooks").join("mtyx-agent-state.json"));
    }

    #[test]
    fn test_run_unknown_subcommand() {
        let code = run(&["invalid".to_string()]);
        assert_eq!(code, 2);
    }

    /// Issue #97 AC3: uninstall still removes entries written by older
    /// versions (bare `mtyx report-agent …` command text) from the legacy
    /// flat `.grok/hooks.json`, keeps unrelated user entries, and drops
    /// the native hooks file wholesale.
    #[test]
    fn uninstall_removes_legacy_bare_mtyx_entries_and_keeps_user_hooks() {
        let _guard = crate::hook_merge::test_support::ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!(
            "grok-hook-test-legacy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(home.join(".grok").join("hooks")).unwrap();
        fs::write(
            home.join(".grok").join("hooks.json"),
            r#"{"hooks":[{"event":"PreToolUse","command":"mtyx report-agent --surface 1 --state working --source hook"},{"event":"Stop","command":"/opt/elsewhere/mtyx report-agent --surface 1 --state done --source hook"},{"event":"UserEvent","command":"echo keep me"}]}"#,
        )
        .unwrap();
        fs::write(home.join(".grok").join("hooks").join(HOOK_FILENAME), "{}").unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(true, true), 0);

        // Native hooks file removed wholesale.
        assert!(!home.join(".grok").join("hooks").join(HOOK_FILENAME).exists());
        // Legacy flat config: ours gone, the user's entry kept.
        let legacy: LegacyGrokHooksConfig = serde_json::from_str(
            &fs::read_to_string(home.join(".grok").join("hooks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(legacy.hooks.len(), 1);
        assert_eq!(legacy.hooks[0].command, "echo keep me");

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }
}
