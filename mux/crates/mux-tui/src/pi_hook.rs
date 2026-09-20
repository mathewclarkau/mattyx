use std::fs;
use std::path::PathBuf;

use crate::hook_merge;

fn extension_path(global: bool) -> Option<PathBuf> {
    if global {
        mux_core::platform::home_dir()
            .map(|h| h.join(".pi").join("agent").join("extensions").join("mtyx.ts"))
    } else {
        Some(PathBuf::from(".pi").join("extensions").join("mtyx.ts"))
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
                "mtyx: usage: mtyx pi <install-hooks|install-skill> [--uninstall] [--global]"
            );
            2
        }
    }
}

fn run_install(uninstall: bool, global: bool) -> i32 {
    let Some(path) = extension_path(global) else {
        eprintln!("error: could not resolve home directory for global extensions");
        return 1;
    };

    if uninstall {
        if path.exists() {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("error removing {}: {e}", path.display());
                return 1;
            }
            println!("Successfully removed mtyx extension from {}", path.display());
        } else {
            println!("No mtyx extension found at {}", path.display());
        }
        0
    } else {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let code = extension_code();

        if let Err(e) = fs::write(&path, code) {
            eprintln!("error writing {}: {e}", path.display());
            return 1;
        }
        println!("Successfully installed mtyx extension into {}", path.display());
        0
    }
}

/// The pi extension body installed by `install-hooks`. The
/// `__MTYX_BIN__` placeholder is replaced at install time with the
/// resolved [`hook_merge::hook_bin`] path as a JS string literal
/// (issue #97). A placeholder + `.replace()` (rather than `format!`)
/// keeps the TypeScript braces below unescaped.
const EXTENSION_TEMPLATE: &str = r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { execFile } from "child_process";

export default function cmuxExtension(pi: ExtensionAPI) {
  const report = (state: string) => {
    const surface = process.env.MTYX_MUX_SURFACE;
    if (surface) {
      // Use execFile (arg array) instead of exec (shell string) so the
      // surface id is never passed through a shell parser. Defends
      // against future callers that may set MTYX_MUX_SURFACE from
      // untrusted input. The target is the absolute path of the mtyx
      // binary that installed this extension (current_exe), so a stale
      // or shadowed `mtyx` on $PATH can never hijack the report — and
      // execFile spawns without a shell, so a path containing spaces
      // needs no quoting either.
      execFile(
        __MTYX_BIN__,
        ["report-agent", "--surface", surface, "--state", state, "--source", "pi"],
        (err) => {
          // Silent error
        }
      );
    }
  };

  pi.on("tool_call", () => {
    report("working");
  });

  pi.on("session_shutdown", () => {
    report("done");
  });
}
"#;

fn extension_code() -> String {
    EXTENSION_TEMPLATE.replace("__MTYX_BIN__", &hook_merge::js_quote(&hook_merge::hook_bin()))
}

fn skill_path(global: bool) -> Option<std::path::PathBuf> {
    if global {
        mux_core::platform::home_dir().map(|h| h.join(".pi").join("agent").join("APPEND_SYSTEM.md"))
    } else {
        Some(std::path::PathBuf::from(".pi").join("APPEND_SYSTEM.md"))
    }
}

fn run_install_skill(uninstall: bool, global: bool) -> i32 {
    let Some(path) = skill_path(global) else {
        eprintln!("error: could not resolve home directory");
        return 1;
    };

    if uninstall {
        if !path.exists() {
            println!("No APPEND_SYSTEM.md found at {}", path.display());
            return 0;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error reading {}: {e}", path.display());
                return 1;
            }
        };

        // Strip the mtyx-managed block (marker lines and inter-block
        // content dropped, everything outside kept). strip_marked_block
        // already trim_end()s, matching the old
        // `new_content.trim_end().to_string() + "\n"` exactly.
        let new_content =
            hook_merge::strip_marked_block(&content, &hook_merge::MTYX_MARKERS) + "\n";
        if new_content == "\n" {
            let _ = fs::remove_file(&path);
            println!("Removed empty APPEND_SYSTEM.md at {}", path.display());
        } else {
            if let Err(e) = fs::write(&path, new_content) {
                eprintln!("error writing {}: {e}", path.display());
                return 1;
            }
            println!("Successfully removed mtyx skill from {}", path.display());
        }
        0
    } else {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let content = if path.exists() {
            fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };

        // Strip any existing mtyx block from anywhere in the file, then
        // append a fresh block at the end (the original strip-then-append
        // behavior, NOT replace-in-place). Trailing whitespace is trimmed
        // before appending so there is a single newline before the block —
        // the old install path left a blank line here (it did not trim,
        // while the uninstall path did); this matches the uninstall path
        // and avoids blank-line drift on repeated installs.
        let cleaned = hook_merge::replace_marked_block(
            &content,
            &hook_merge::MTYX_MARKERS,
            crate::skill_content::ORCHESTRATION_SKILL,
        );

        if let Err(e) = fs::write(&path, cleaned) {
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

    fn temp_home(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-hook-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn current_exe_str() -> String {
        // The .ts extension embeds the path JSON-escaped via js_quote, so the
        // comparison value must be escaped too — raw Windows backslashes
        // (`D:\a\...`) never appear verbatim in the emitted file.
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap();
        serde_json::to_string(&exe).unwrap().trim_matches('"').to_string()
    }

    /// Issue #97 AC1: the installed extension invokes the running binary
    /// via its current_exe() absolute path as the execFile target, never
    /// a bare `mtyx` PATH lookup.
    #[test]
    fn install_emits_current_exe_absolute_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("exe");
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = current_exe_str();
        let code =
            fs::read_to_string(home.join(".pi").join("agent").join("extensions").join("mtyx.ts"))
                .unwrap();
        assert!(
            code.contains(&format!("execFile(\n        \"{}\"", exe)),
            "execFile target must be the running binary's absolute path:\n{code}"
        );
        assert!(
            !code.contains("\"mtyx\","),
            "a bare-mtyx execFile target must not survive:\n{code}"
        );
        assert!(code.contains("\"report-agent\""), "{code}");

        // Uninstall still removes the file (issue #97 AC3: the pi
        // uninstall path is a plain delete — no command matching needed).
        assert_eq!(run_install(true, true), 0);
        assert!(!home.join(".pi").join("agent").join("extensions").join("mtyx.ts").exists());

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }

    /// Issue #97 AC2 (rename simulation): reinstalling replaces the file
    /// wholesale, so an extension written by an older binary path leaves
    /// only the current one behind (the file is fully rewritten from the
    /// template, never merged).
    #[test]
    fn reinstall_leaves_only_the_current_binary_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_home("stale");
        let ext = home.join(".pi").join("agent").join("extensions").join("mtyx.ts");
        fs::create_dir_all(ext.parent().unwrap()).unwrap();
        fs::write(&ext, "execFile(\"/old/deleted/path/mtyx\", [\"report-agent\"], () => {})")
            .unwrap();
        std::env::set_var("HOME", &home);

        assert_eq!(run_install(false, true), 0);

        let exe = current_exe_str();
        let code = fs::read_to_string(&ext).unwrap();
        assert!(code.contains(&exe), "{code}");
        assert!(!code.contains("/old/deleted/path/"), "{code}");

        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&home);
    }
}
