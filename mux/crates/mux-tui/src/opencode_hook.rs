use std::fs;
use std::path::PathBuf;

use crate::hook_merge;

/// The TypeScript plugin content that reports agent state to mtyx.
/// Installed at `.opencode/plugin/mtyx.ts` (project) or
/// `~/.config/opencode/plugin/mtyx.ts` (global). The `__MTYX_BIN__`
/// placeholder is replaced at install time with the resolved
/// [`hook_merge::hook_bin`] path as a JS string literal (issue #97).
/// A placeholder + `.replace()` (rather than `format!`) keeps the
/// TypeScript braces below unescaped.
const MTYX_PLUGIN_TEMPLATE: &str = r#"// MTYX-START
// mtyx agent-state reporting plugin for opencode
// Installed by: mtyx opencode install-hooks
// Removed by: mtyx opencode install-hooks --uninstall
import { execFile } from "node:child_process"

// Absolute path of the mtyx binary that installed this plugin
// (current_exe), so a stale or shadowed `mtyx` on $PATH can never
// hijack the report.
const MTYX_BIN = __MTYX_BIN__

function reportAgent(state: string) {
  const surface = process.env.MTYX_MUX_SURFACE
  if (!surface) return
  // execFile with an arg array (no shell): the surface id is never
  // passed through a shell parser, and the binary path needs no
  // quoting even when it contains spaces.
  execFile(MTYX_BIN, ["report-agent", "--surface", surface, "--state", state, "--source", "hook"], () => {})
}

export default async () => {
  return {
    "tool.execute.before": async () => {
      reportAgent("working")
    },
    "tool.execute.after": async () => {
      reportAgent("idle")
    },
  }
}
// MTYX-END
"#;

fn mtyx_plugin() -> String {
    MTYX_PLUGIN_TEMPLATE.replace("__MTYX_BIN__", &hook_merge::js_quote(&hook_merge::hook_bin()))
}

fn plugin_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir()
            .map(|h| h.join(".config").join("opencode").join("plugin").join("mtyx.ts"))
    } else {
        Some(PathBuf::from(".opencode").join("plugin").join("mtyx.ts"))
    }
}

fn skill_path(global: bool) -> Option<PathBuf> {
    let base = if global {
        mux_core::platform::home_dir()?.join(".config").join("opencode").join("skills")
    } else {
        PathBuf::from(".opencode").join("skills")
    };
    Some(base.join("mtyx-orchestration").join("SKILL.md"))
}

fn hotfix_skill_path(global: bool) -> Option<PathBuf> {
    let base = if global {
        mux_core::platform::home_dir()?.join(".config").join("opencode").join("skills")
    } else {
        PathBuf::from(".opencode").join("skills")
    };
    Some(base.join("mtyx-hotfix-race").join("SKILL.md"))
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
                "mtyx: usage: mtyx opencode <install-hooks|install-skill> [--uninstall] [--global]"
            );
            2
        }
    }
}

fn run_install(uninstall: bool, global: bool) -> i32 {
    let Some(path) = plugin_path(global) else {
        eprintln!("error: could not resolve home directory for global plugin");
        return 1;
    };

    if uninstall {
        // If the file contains only our MTYX block (whether written by
        // this binary or an older one — hence the strip fallback below),
        // remove it entirely. Otherwise, strip the MTYX-START..MTYX-END
        // block.
        if !path.exists() {
            println!("No opencode plugin found at {}", path.display());
            return 0;
        }
        match fs::read_to_string(&path) {
            Ok(content) => {
                if content.trim() == mtyx_plugin().trim() {
                    if let Err(e) = fs::remove_file(&path) {
                        eprintln!("error removing {}: {e}", path.display());
                        return 1;
                    }
                } else {
                    let stripped = hook_merge::strip_marked_block(
                        &content,
                        &hook_merge::Markers { start: "MTYX-START", end: "MTYX-END" },
                    );
                    if stripped.trim().is_empty() {
                        // The file held nothing but our block (installed
                        // by a different mtyx build, so the exact-content
                        // check above missed it) — delete it rather than
                        // leaving an empty husk behind.
                        if let Err(e) = fs::remove_file(&path) {
                            eprintln!("error removing {}: {e}", path.display());
                            return 1;
                        }
                    } else if let Err(e) = fs::write(&path, stripped) {
                        eprintln!("error writing {}: {e}", path.display());
                        return 1;
                    }
                }
                println!("Successfully removed mtyx plugin from {}", path.display());
            }
            Err(e) => {
                eprintln!("error reading {}: {e}", path.display());
                return 1;
            }
        }
        0
    } else {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                eprintln!(
                    "error: refusing to overwrite symlink at {}.
                     Remove it manually if you want to install the plugin.",
                    path.display()
                );
                return 1;
            }
        }
        if let Err(e) = fs::write(&path, mtyx_plugin()) {
            eprintln!("error writing {}: {e}", path.display());
            return 1;
        }
        println!("Successfully installed mtyx plugin into {}", path.display());
        0
    }
}

fn run_install_skill(uninstall: bool, global: bool) -> i32 {
    let Some(path) = skill_path(global) else {
        eprintln!("error: could not resolve home directory");
        return 1;
    };
    let Some(hotfix_path) = hotfix_skill_path(global) else {
        eprintln!("error: could not resolve home directory");
        return 1;
    };

    if uninstall {
        let mut removed = 0;
        for p in [&path, &hotfix_path] {
            if p.exists() {
                if let Err(e) = fs::remove_file(p) {
                    eprintln!("error removing {}: {e}", p.display());
                    return 1;
                }
                if let Some(parent) = p.parent() {
                    let _ = fs::remove_dir(parent);
                    if let Some(grandparent) = parent.parent() {
                        let _ = fs::remove_dir(grandparent);
                    }
                }
                removed += 1;
            }
        }
        println!("Removed {removed} mtyx skill(s) from opencode");
        0
    } else {
        let mut installed = 0;
        for (p, content) in [
            (&path, crate::skill_content::ORCHESTRATION_SKILL),
            (&hotfix_path, crate::skill_content::HOTFIX_RACE_SKILL),
        ] {
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(meta) = fs::symlink_metadata(p) {
                if meta.file_type().is_symlink() {
                    eprintln!(
                        "error: refusing to overwrite symlink at {}.
                         Remove it manually if you want to install the skill.",
                        p.display()
                    );
                    continue;
                }
            }
            if let Err(e) = fs::write(p, content) {
                eprintln!("error writing {}: {e}", p.display());
                continue;
            }
            installed += 1;
        }
        println!("Successfully installed {installed} mtyx skill(s) into opencode");
        if installed > 0 {
            0
        } else {
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_unknown_subcommand() {
        let code = run(&["invalid".to_string()]);
        assert_eq!(code, 2);
    }

    #[test]
    fn plugin_content_has_markers() {
        assert!(mtyx_plugin().contains("MTYX-START"));
        assert!(mtyx_plugin().contains("MTYX-END"));
    }

    #[test]
    fn uninstall_strips_cmux_era_marker_block() {
        // Rename compat: a plugin file last written by a pre-rename
        // build carries a bare `// CMUX-START`..`// CMUX-END` block.
        // The uninstall path's strip must remove that block exactly
        // like a canonical one, so re-running the installer never
        // duplicates blocks (dual-parse lives in hook_merge::MarkerSet).
        let legacy =
            "// unrelated header\n// CMUX-START\nexec(\"mtyx report-agent …\")\n// CMUX-END\n";
        let markers = hook_merge::Markers { start: "MTYX-START", end: "MTYX-END" };
        assert_eq!(hook_merge::strip_marked_block(legacy, &markers), "// unrelated header");
        let replaced = hook_merge::replace_marked_block(legacy, &markers, "fresh body");
        assert_eq!(replaced, "// unrelated header\nMTYX-START\nfresh body\nMTYX-END\n");
    }

    #[test]
    fn plugin_content_reports_agent_state() {
        assert!(mtyx_plugin().contains("report-agent"));
        assert!(mtyx_plugin().contains("working"));
        assert!(mtyx_plugin().contains("idle"));
        // execFile arg-array form: the flag and its value are separate
        // array elements.
        assert!(mtyx_plugin().contains("\"--source\", \"hook\""));
    }

    /// Issue #97 AC1: the installed plugin invokes the running binary via
    /// its current_exe() absolute path as the execFile target, never a
    /// bare `mtyx` PATH lookup.
    #[test]
    fn plugin_invokes_the_running_binary_absolute_path() {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap();
        // The plugin embeds the path JSON-escaped (js_quote), so compare
        // against the escaped literal — raw Windows backslashes never
        // appear verbatim in the file.
        let exe_json = serde_json::to_string(&exe).unwrap();
        let plugin = mtyx_plugin();
        assert!(
            plugin.contains(&format!("const MTYX_BIN = {}", exe_json)),
            "execFile target must be the running binary's absolute path:\n{plugin}"
        );
        assert!(
            !plugin.contains("exec(`mtyx") && !plugin.contains("exec(\"mtyx\""),
            "a bare-mtyx exec/execFile target must not survive:\n{plugin}"
        );
    }

    /// Issue #97 AC2 (rename simulation): reinstalling over a plugin
    /// written by an older binary path replaces it wholesale (the plugin
    /// file is fully rewritten, never merged).
    #[test]
    fn install_replaces_a_stale_plugin_from_a_different_binary_path() {
        let _guard = crate::hook_merge::test_support::ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!(
            "opencode-hook-test-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let plugin = home.join(".config").join("opencode").join("plugin").join("mtyx.ts");
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(
            &plugin,
            "// MTYX-START\nconst MTYX_BIN = \"/old/deleted/path/mtyx\"\nexecFile(MTYX_BIN, [\"report-agent\"], () => {})\n// MTYX-END\n",
        )
        .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap();
        // The plugin embeds the path JSON-escaped (js_quote); on Windows the
        // raw backslashes never appear verbatim in the file.
        let exe_json = serde_json::to_string(&exe).unwrap().trim_matches('"').to_string();
        let content = fs::read_to_string(&plugin).unwrap();
        assert!(content.contains(&exe_json), "{content}");
        assert!(!content.contains("/old/deleted/path/"), "{content}");

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }

    /// Issue #97 AC3: uninstall removes a plugin written by an older
    /// build (bare `mtyx` exec command — content the exact-match check
    /// can no longer hit), deleting the file when our block was all it
    /// held, and keeps unrelated user content otherwise.
    #[test]
    fn uninstall_removes_a_legacy_bare_mtyx_plugin() {
        let _guard = crate::hook_merge::test_support::ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!(
            "opencode-hook-test-legacy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let plugin = home.join(".config").join("opencode").join("plugin").join("mtyx.ts");
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        // Legacy pre-#97 plugin text: bare `mtyx` exec, MTYX markers.
        fs::write(
            &plugin,
            "// MTYX-START\nimport { exec } from \"node:child_process\"\nexec(`mtyx report-agent --surface ${surface} --state ${state} --source hook`)\n// MTYX-END\n",
        )
        .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(true, true), 0);
        // The file held only our block — it is deleted, not emptied.
        assert!(!plugin.exists(), "legacy plugin-only file must be deleted");

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }
}
