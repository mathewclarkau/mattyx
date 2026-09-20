use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const HOOK_VERSION: &str = "v0.2.0";

type Runner = fn(&[String]) -> i32;
type PathResolver = fn(bool) -> Option<PathBuf>;

struct AgentSpec {
    name: &'static str,
    runner: Runner,
    path: PathResolver,
}

const REGISTRY: &[AgentSpec] = &[
    AgentSpec { name: "claude", runner: crate::claude_hook::run, path: claude_path },
    AgentSpec { name: "antigravity", runner: crate::antigravity_hook::run, path: antigravity_path },
    AgentSpec { name: "codex", runner: crate::codex_hook::run, path: codex_path },
    AgentSpec { name: "aider", runner: crate::aider_hook::run, path: aider_path },
    AgentSpec { name: "pi", runner: crate::pi_hook::run, path: pi_path },
    AgentSpec { name: "grok", runner: crate::grok_hook::run, path: grok_path },
    AgentSpec { name: "opencode", runner: crate::opencode_hook::run, path: opencode_path },
];

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => run_list(args),
        Some("install") => run_install_command(REGISTRY, args),
        _ => {
            eprintln!(
                "mtyx: usage: mtyx agents <list|install --all|install --only <agent>> [--uninstall] [--global]"
            );
            2
        }
    }
}

fn run_install_command(registry: &[AgentSpec], args: &[String]) -> i32 {
    let mut only = None;
    let mut all = false;
    let mut global = false;
    let mut uninstall = false;
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--all" => all = true,
            "--global" => global = true,
            "--uninstall" => uninstall = true,
            "--only" => {
                index += 1;
                only = args.get(index).map(String::as_str);
                if only.is_none() {
                    eprintln!("mtyx: --only needs an agent name");
                    return 2;
                }
            }
            other => {
                eprintln!("mtyx: unknown agents install argument {other:?}");
                return 2;
            }
        }
        index += 1;
    }
    if all == only.is_some() {
        eprintln!("mtyx: agents install requires exactly one of --all or --only <agent>");
        return 2;
    }
    if let Some(name) = only {
        if !is_registered(registry, name) {
            eprintln!("mtyx: unknown agent {name:?}");
            return 2;
        }
    }
    if install_selected(registry, only, global, uninstall) {
        0
    } else {
        1
    }
}

fn install_selected(
    registry: &[AgentSpec],
    only: Option<&str>,
    global: bool,
    uninstall: bool,
) -> bool {
    let mut success = true;
    for agent in registry.iter().filter(|agent| only.map_or(true, |name| name == agent.name)) {
        let mut args = vec!["install-hooks".to_string()];
        if uninstall {
            args.push("--uninstall".to_string());
        }
        if global {
            args.push("--global".to_string());
        }
        let code = (agent.runner)(&args);
        if code == 0 {
            println!("agents: {}: {}", agent.name, if uninstall { "removed" } else { "installed" });
        } else {
            eprintln!("agents: {}: failed (exit code {code})", agent.name);
            success = false;
        }
    }
    success
}

fn is_registered(registry: &[AgentSpec], name: &str) -> bool {
    registry.iter().any(|agent| agent.name == name)
}

fn run_list(args: &[String]) -> i32 {
    let mut global = false;
    for arg in args.iter().skip(1) {
        if arg == "--global" {
            global = true;
        } else {
            eprintln!("mtyx: unknown agents list argument {arg:?}");
            return 2;
        }
    }
    println!("agent\tstatus\tversion\tlast-installed\tinstall-path");
    for agent in REGISTRY {
        let path = (agent.path)(global);
        let installed = path.as_deref().is_some_and(Path::exists);
        let timestamp = path
            .as_deref()
            .and_then(|p| p.metadata().ok())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{}\t{}\t{}\t{}\t{}",
            agent.name,
            if installed { "installed" } else { "not-installed" },
            if installed { HOOK_VERSION } else { "-" },
            if installed { timestamp } else { "-".to_string() },
            path.map_or_else(|| "-".to_string(), |p| p.display().to_string())
        );
    }
    0
}

fn home_join(parts: &[&str]) -> Option<PathBuf> {
    let mut path = mux_core::platform::home_dir()?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

fn claude_path(_: bool) -> Option<PathBuf> {
    home_join(&[".claude", "settings.json"])
}
fn antigravity_path(global: bool) -> Option<PathBuf> {
    if global {
        home_join(&[".gemini", "config", "hooks.json"])
    } else {
        Some(PathBuf::from(".agents/hooks.json"))
    }
}
fn codex_path(global: bool) -> Option<PathBuf> {
    if global {
        home_join(&[".codex", "hooks.json"])
    } else {
        Some(PathBuf::from(".codex/hooks.json"))
    }
}
fn aider_path(global: bool) -> Option<PathBuf> {
    if global {
        home_join(&[".local", "bin", "aider"])
    } else {
        Some(PathBuf::from(".bin/aider"))
    }
}
fn pi_path(global: bool) -> Option<PathBuf> {
    if global {
        home_join(&[".pi", "agent", "extensions", "mtyx.ts"])
    } else {
        Some(PathBuf::from(".pi/extensions/mtyx.ts"))
    }
}
fn grok_path(global: bool) -> Option<PathBuf> {
    crate::grok_hook::config_path(global)
}
fn opencode_path(global: bool) -> Option<PathBuf> {
    if global {
        home_join(&[".config", "opencode", "plugin", "mtyx.ts"])
    } else {
        Some(PathBuf::from(".opencode").join("plugin").join("mtyx.ts"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn succeeds(_: &[String]) -> i32 {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        0
    }
    fn fails(_: &[String]) -> i32 {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        1
    }
    fn records_args(args: &[String]) -> i32 {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        assert_eq!(args, &["install-hooks", "--uninstall", "--global"]);
        0
    }
    fn test_path(_: bool) -> Option<PathBuf> {
        None
    }

    #[test]
    fn install_all_continues_after_an_agent_failure() {
        let _lock = TEST_LOCK.lock().unwrap();
        ATTEMPTS.store(0, Ordering::SeqCst);
        let registry = [
            AgentSpec { name: "one", runner: succeeds, path: test_path },
            AgentSpec { name: "two", runner: fails, path: test_path },
            AgentSpec { name: "three", runner: succeeds, path: test_path },
        ];
        let args = vec!["install".to_string(), "--all".to_string()];
        assert_eq!(run_install_command(&registry, &args), 1);
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn only_selects_one_registered_agent() {
        let _lock = TEST_LOCK.lock().unwrap();
        ATTEMPTS.store(0, Ordering::SeqCst);
        let registry = [
            AgentSpec { name: "one", runner: succeeds, path: test_path },
            AgentSpec { name: "two", runner: records_args, path: test_path },
        ];
        assert!(install_selected(&registry, Some("two"), true, true));
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unknown_only_agent_is_rejected() {
        let registry = [AgentSpec { name: "one", runner: succeeds, path: test_path }];
        assert!(!is_registered(&registry, "missing"));
    }
}
