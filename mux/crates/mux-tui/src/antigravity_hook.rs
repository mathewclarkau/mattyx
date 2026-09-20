use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::hook_merge;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AntigravityHook {
    pub event: String,
    pub command: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct AntigravityHooksConfig {
    #[serde(default)]
    pub hooks: Vec<AntigravityHook>,
}

fn config_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir().map(|h| h.join(".gemini").join("config").join("hooks.json"))
    } else {
        Some(PathBuf::from(".agents").join("hooks.json"))
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
            eprintln!("mtyx: usage: mtyx antigravity <install-hooks|install-skill> [--uninstall] [--global]");
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
        let mut config: AntigravityHooksConfig = match hook_merge::load_json(&path) {
            Ok(c) => c,
            Err(hook_merge::LoadError::NotFound) => {
                println!("No Antigravity hooks file found at {}", path.display());
                return 0;
            }
            // Fail-loud on malformed config: silent `unwrap_or_default()`
            // would overwrite the user's real config on schema drift.
            Err(hook_merge::LoadError::Parse(e)) => {
                eprintln!("error: malformed Antigravity config at {}: {e}", path.display());
                return 1;
            }
            Err(hook_merge::LoadError::Io(e)) => {
                eprintln!("error reading {}: {e}", path.display());
                return 1;
            }
        };
        // Match on the bare `report-agent` verb, not a fuller command
        // string: entries installed since issue #97 carry a quoted
        // absolute path (`'/…/mtyx' report-agent …`) while entries from
        // older builds carry the bare `mtyx report-agent` — both must be
        // removed (here and in the install-path retain below).
        config.hooks.retain(|h| !h.command.contains("report-agent"));

        if let Err(e) = hook_merge::save_pretty(&path, &config) {
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
        println!("Successfully removed mtyx hooks from {}", path.display());
        0
    } else {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut config = match hook_merge::load_or_default::<AntigravityHooksConfig>(&path) {
            Ok(c) => c,
            // Fail-loud on malformed config: silent `unwrap_or_default()`
            // would overwrite the user's real config on schema drift.
            Err(hook_merge::LoadError::Parse(e)) => {
                eprintln!("error: malformed Antigravity config at {}: {e}", path.display());
                return 1;
            }
            Err(hook_merge::LoadError::Io(e)) => {
                eprintln!("error reading {}: {e}", path.display());
                return 1;
            }
            // load_or_default converts NotFound -> Ok(default) internally,
            // so this arm is unreachable in practice; kept for exhaustiveness.
            Err(hook_merge::LoadError::NotFound) => AntigravityHooksConfig::default(),
        };

        // Remove any existing mtyx hooks to avoid duplicates, however
        // the binary path was spelled when they were installed.
        config.hooks.retain(|h| !h.command.contains("report-agent"));

        // Issue #97: invoke the *running* binary by absolute path (shell-
        // quoted — the resolved path can contain spaces), never a bare
        // `mtyx` that a rename or a stale $PATH entry could hijack.
        let bin = hook_merge::shell_quote(&hook_merge::hook_bin());
        let report = |event: &str, state: &str| {
            let command = format!(
                "{bin} report-agent --surface \"$MTYX_MUX_SURFACE\" --state {state} --source antigravity"
            );
            AntigravityHook { event: event.to_string(), command }
        };
        config.hooks.push(report("PreToolUse", "working"));
        config.hooks.push(report("PostToolUse", "idle"));
        config.hooks.push(report("Stop", "done"));

        if let Err(e) = hook_merge::save_pretty(&path, &config) {
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
        println!("Successfully installed mtyx hooks into {}", path.display());
        0
    }
}

fn skill_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir().map(|h| {
            h.join(".gemini")
                .join("antigravity-cli")
                .join("skills")
                .join("mtyx-orchestration")
                .join("SKILL.md")
        })
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
    use crate::hook_merge::test_support::ENV_LOCK;

    fn temp_home(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "antigravity-hook-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn current_exe_str() -> String {
        std::env::current_exe().map(|p| p.display().to_string()).unwrap()
    }

    fn hooks_path(home: &PathBuf) -> PathBuf {
        home.join(".gemini").join("config").join("hooks.json")
    }

    fn read_config(home: &PathBuf) -> AntigravityHooksConfig {
        serde_json::from_str(&fs::read_to_string(hooks_path(home)).unwrap()).unwrap()
    }

    /// Issue #97 AC1: the installed commands invoke the running binary by
    /// its current_exe() absolute path, never a bare `mtyx` PATH lookup.
    #[test]
    fn install_emits_current_exe_absolute_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("exe");
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = current_exe_str();
        let config = read_config(&home);
        let ours: Vec<&AntigravityHook> =
            config.hooks.iter().filter(|h| h.command.contains(&exe)).collect();
        assert_eq!(ours.len(), 3, "three hook commands must name the running binary");
        for hook in &ours {
            assert!(hook.command.contains("--source antigravity"), "{}", hook.command);
            assert!(
                !hook.command.contains("mtyx report-agent"),
                "a bare-mtyx PATH lookup must not survive: {}",
                hook.command
            );
        }

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }

    /// Issue #97 AC2 (rename simulation): a hook installed from a since-
    /// moved binary path is replaced in place, not accumulated.
    #[test]
    fn install_replaces_entries_from_a_stale_binary_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("stale");
        fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
        fs::write(
            hooks_path(&home),
            r#"{"hooks":[{"event":"PreToolUse","command":"/old/deleted/path/mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state working --source antigravity"},{"event":"Stop","command":"/old/deleted/path/mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state done --source antigravity"}]}"#,
        )
        .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = current_exe_str();
        let config = read_config(&home);
        assert_eq!(config.hooks.len(), 3, "stale entries must be replaced, not kept alongside");
        assert!(
            config.hooks.iter().all(|h| h.command.contains(&exe)),
            "every surviving command must name the running binary"
        );
        assert!(config.hooks.iter().all(|h| !h.command.contains("/old/deleted/path/")));

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }

    /// Issue #97 AC3: uninstall still removes entries written by older
    /// versions (bare `mtyx report-agent …` command text) and keeps
    /// unrelated user hooks.
    #[test]
    fn uninstall_removes_legacy_bare_mtyx_entries_and_keeps_user_hooks() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("legacy-uninstall");
        fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
        fs::write(
            hooks_path(&home),
            r#"{"hooks":[{"event":"PreToolUse","command":"mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state working --source antigravity"},{"event":"Stop","command":"/opt/elsewhere/mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state done --source antigravity"},{"event":"UserEvent","command":"echo keep me"}]}"#,
        )
        .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(true, true), 0);

        let config = read_config(&home);
        assert_eq!(config.hooks.len(), 1, "only the unrelated user hook survives");
        assert_eq!(config.hooks[0].command, "echo keep me");
        assert_eq!(config.hooks[0].event, "UserEvent");

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }
}
