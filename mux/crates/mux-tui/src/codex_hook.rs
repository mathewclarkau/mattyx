use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::hook_merge;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CodexHook {
    pub command: String,
    #[serde(rename = "statusMessage", default)]
    pub status_message: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct CodexHooksConfig {
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<CodexHook>>,
}

fn paths(global: bool) -> Option<(PathBuf, PathBuf)> {
    let home = mux_core::platform::home_dir()?;
    if global {
        let codex_dir = home.join(".codex");
        Some((codex_dir.join("hooks.json"), codex_dir.join("config.toml")))
    } else {
        let codex_dir = PathBuf::from(".codex");
        Some((codex_dir.join("hooks.json"), codex_dir.join("config.toml")))
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
                "mtyx: usage: mtyx codex <install-hooks|install-skill> [--uninstall] [--global]"
            );
            2
        }
    }
}

fn run_install(uninstall: bool, global: bool) -> i32 {
    let Some((hooks_path, config_path)) = paths(global) else {
        eprintln!("error: could not resolve home directory for global hooks");
        return 1;
    };

    if uninstall {
        let mut config: CodexHooksConfig = match hook_merge::load_json(&hooks_path) {
            Ok(c) => c,
            // Codex uninstall is silent on a missing hooks file (matches
            // the original `if hooks_path.exists() { .. } 0` fall-through).
            Err(hook_merge::LoadError::NotFound) => return 0,
            // Fail-loud on malformed config: silent `unwrap_or_default()`
            // would overwrite the user's real config on schema drift.
            Err(hook_merge::LoadError::Parse(e)) => {
                eprintln!("error: malformed Codex hooks config at {}: {e}", hooks_path.display());
                return 1;
            }
            Err(hook_merge::LoadError::Io(e)) => {
                eprintln!("error reading {}: {e}", hooks_path.display());
                return 1;
            }
        };
        for hooks_list in config.hooks.values_mut() {
            // Match on the bare `report-agent` verb, not on a fuller
            // command string: entries installed since issue #97 carry a
            // quoted absolute path (`'/…/mtyx' report-agent …`) while
            // entries from older builds carry the bare `mtyx report-agent`
            // — both must be removed, and a rebuilt/moved binary must
            // still match its own previous entries.
            hooks_list.retain(|h| !h.command.contains("report-agent"));
        }
        // Retain only events that still have hooks
        config.hooks.retain(|_, v| !v.is_empty());

        if let Err(e) = hook_merge::save_pretty(&hooks_path, &config) {
            match e {
                hook_merge::SaveError::Serialize(e) => {
                    eprintln!("error: failed to serialize config: {e}");
                    return 1;
                }
                hook_merge::SaveError::Io(e) => {
                    eprintln!("error writing {}: {e}", hooks_path.display());
                    return 1;
                }
            }
        }
        println!("Successfully removed mtyx hooks from {}", hooks_path.display());
        0
    } else {
        if let Some(parent) = hooks_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        // 1. Setup config.toml features. Only add `codex_hooks = true` to
        // an actual `[features]` section header — never to a substring
        // match that might be inside a comment or a longer key like
        // `docs.codex_hooks`. Done by walking lines.
        let mut config_content = if config_path.exists() {
            match fs::read_to_string(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: could not read {}: {e}", config_path.display());
                    String::new()
                }
            }
        } else {
            String::new()
        };

        let already_featured = config_content.lines().any(|l| l.trim() == "codex_hooks = true");
        if !already_featured {
            if let Some(features_idx) =
                config_content.lines().position(|l| l.trim() == "[features]")
            {
                // Find the next blank line or section header after [features],
                // insert after that. Default to appending at the section.
                let mut new_lines: Vec<String> = config_content.lines().map(String::from).collect();
                let insert_at = features_idx + 1;
                new_lines.insert(insert_at, "codex_hooks = true".to_string());
                config_content = new_lines.join("\n");
                if !config_content.ends_with('\n') {
                    config_content.push('\n');
                }
            } else {
                if !config_content.ends_with('\n') && !config_content.is_empty() {
                    config_content.push('\n');
                }
                config_content.push_str("\n[features]\ncodex_hooks = true\n");
            }
            if let Err(e) = fs::write(&config_path, &config_content) {
                eprintln!("error: could not update {}: {e}", config_path.display());
                return 1;
            }
        }

        // 2. Setup hooks.json — fail-loud on malformed config.
        let mut config = match hook_merge::load_or_default::<CodexHooksConfig>(&hooks_path) {
            Ok(c) => c,
            // Fail-loud on malformed config: silent `unwrap_or_default()`
            // would overwrite the user's real config on schema drift.
            Err(hook_merge::LoadError::Parse(e)) => {
                eprintln!("error: malformed Codex hooks config at {}: {e}", hooks_path.display());
                return 1;
            }
            Err(hook_merge::LoadError::Io(e)) => {
                eprintln!("error reading {}: {e}", hooks_path.display());
                return 1;
            }
            // load_or_default converts NotFound -> Ok(default) internally,
            // so this arm is unreachable in practice; kept for exhaustiveness.
            Err(hook_merge::LoadError::NotFound) => CodexHooksConfig::default(),
        };

        // Clear existing mtyx hooks (however the binary path was spelled
        // when they were installed — see the uninstall retain above).
        for hooks_list in config.hooks.values_mut() {
            hooks_list.retain(|h| !h.command.contains("report-agent"));
        }

        // Issue #97: invoke the *running* binary by absolute path (shell-
        // quoted, since the resolved path can contain spaces), never a
        // bare `mtyx` that a rename or a stale $PATH entry could hijack.
        let bin = hook_merge::shell_quote(&hook_merge::hook_bin());
        let report = |state: &str| {
            format!(
                "{bin} report-agent --surface \"$MTYX_MUX_SURFACE\" --state {state} --source codex"
            )
        };
        let new_hooks = vec![
            ("PreToolUse", report("working")),
            ("PostToolUse", report("idle")),
            ("Stop", report("done")),
        ];

        for (event, command) in new_hooks {
            config.hooks.entry(event.to_string()).or_insert_with(Vec::new).push(CodexHook {
                command: command.to_string(),
                status_message: Some("Reporting state to mtyx".to_string()),
            });
        }

        if let Err(e) = hook_merge::save_pretty(&hooks_path, &config) {
            match e {
                hook_merge::SaveError::Serialize(e) => {
                    eprintln!("error: failed to serialize config: {e}");
                    return 1;
                }
                hook_merge::SaveError::Io(e) => {
                    eprintln!("error writing {}: {e}", hooks_path.display());
                    return 1;
                }
            }
        }
        println!("Successfully installed mtyx hooks into {}", hooks_path.display());
        0
    }
}

fn skill_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir()
            .map(|h| h.join(".codex").join("skills").join("mtyx-orchestration").join("SKILL.md"))
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
            "codex-hook-test-{label}-{}-{}",
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
        home.join(".codex").join("hooks.json")
    }

    fn read_config(home: &PathBuf) -> CodexHooksConfig {
        serde_json::from_str(&fs::read_to_string(hooks_path(home)).unwrap()).unwrap()
    }

    /// Issue #97 AC1: the installed command invokes the running binary by
    /// its current_exe() absolute path, never a bare `mtyx` PATH lookup.
    #[test]
    fn install_emits_current_exe_absolute_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("exe");
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = current_exe_str();
        let config = read_config(&home);
        for (event, state) in [("PreToolUse", "working"), ("PostToolUse", "idle"), ("Stop", "done")]
        {
            let list = config.hooks.get(event).expect(event);
            assert_eq!(list.len(), 1, "{event} must have exactly our hook");
            let command = &list[0].command;
            assert!(
                command.contains(&exe),
                "{event} command must name the running binary: {command}"
            );
            assert!(
                command.contains(&format!("--state {state} --source codex")),
                "{event}: {command}"
            );
            assert!(
                !command.contains("mtyx report-agent"),
                "a bare-mtyx PATH lookup must not survive: {command}"
            );
        }

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }

    /// Issue #97 AC2 (rename simulation, mirrors claude_hook's
    /// install_hooks_replaces_a_stale_entry_from_a_different_binary_path):
    /// entries installed from a since-moved binary path are replaced in
    /// place, not accumulated alongside the new absolute path.
    #[test]
    fn install_replaces_entries_from_a_stale_binary_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("stale");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            hooks_path(&home),
            r#"{"hooks":{"PreToolUse":[{"command":"/old/deleted/path/mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state working --source codex","statusMessage":"x"}]}}"#,
        )
        .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = current_exe_str();
        let config = read_config(&home);
        let list = config.hooks.get("PreToolUse").unwrap();
        assert_eq!(list.len(), 1, "stale entry must be replaced, not duplicated");
        assert!(list[0].command.contains(&exe));
        assert!(!list[0].command.contains("/old/deleted/path/"));

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }

    /// Issue #97 AC3: uninstall still removes entries written by older
    /// versions whose command text was the bare `mtyx report-agent …`,
    /// while unrelated user hooks survive untouched.
    #[test]
    fn uninstall_removes_legacy_bare_mtyx_entries_and_keeps_user_hooks() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("legacy-uninstall");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            hooks_path(&home),
            r#"{"hooks":{"PreToolUse":[{"command":"mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state working --source codex"}],"Stop":[{"command":"/opt/elsewhere/mtyx report-agent --surface \"$MTYX_MUX_SURFACE\" --state done --source codex"}],"UserEvent":[{"command":"echo keep me"}]}}"#,
        )
        .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(true, true), 0);

        let config = read_config(&home);
        assert!(config.hooks.get("PreToolUse").is_none(), "legacy bare-mtyx entry must be removed");
        assert!(config.hooks.get("Stop").is_none(), "old absolute-path entry must be removed");
        let user = config.hooks.get("UserEvent").expect("user hooks survive");
        assert_eq!(user[0].command, "echo keep me");

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }
}
