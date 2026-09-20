//! Per-pane git worktrees (issue #77).
//!
//! Filesystem-only repository discovery (mirroring mux-tui's
//! `git_info.rs`, which mux-core cannot depend on) plus thin wrappers
//! around the `git` binary for the actual worktree operations. Every
//! subprocess is spawned from an argv array with no shell involvement
//! (see AGENTS.md "Security gotchas"); stderr is captured and
//! propagated, never swallowed.

use std::path::{Component, Path, PathBuf};

/// One worktree attached to a pane. A pane can accumulate several over
/// its lifetime (one per `pane-worktree-create`/`new-tab --branch`);
/// `pane worktree list` returns them in creation order. Records are
/// session-scoped: the on-disk worktrees outlive them and remain
/// visible to `git worktree list` after a daemon restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub branch: String,
    pub path: String,
    pub label: Option<String>,
    pub created_at_ms: u64,
}

/// Default worktree path pattern, relative to the repository root:
/// `<repo>/../<repo>.<branch>/` (issue #77 AC6).
pub const DEFAULT_PATTERN: &str = "../<repo>.<branch>";

/// Walk up from `start` to the enclosing repository root (the closest
/// ancestor directory holding a `.git` entry). Resolves worktree
/// `.git` files (which point at a private gitdir) only to detect the
/// repository; the returned root is the worktree's own directory in
/// that case, matching how `git` itself scopes a worktree.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    // The repository root is the ancestor whose `.git` entry lives in
    // it directly: for a linked worktree that is the worktree dir itself
    // (its `.git` is a file), matching git's own scoping.
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The main repository root for a path inside a linked worktree: reads
/// the worktree's `.git` file, then the private gitdir's `commondir`
/// (e.g. `../..`), then strips the `.git` component. Also works for a
/// plain checkout, where the gitdir IS the common dir.
pub fn main_repo_root(start: &Path) -> Option<PathBuf> {
    let gitdir = find_git_dir(start)?;
    // Worktree admin dirs carry a `commondir` file (`../..` pointing at
    // the main `.git`); a plain checkout has none and IS the common dir.
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
        .map(|rel| if rel.is_absolute() { rel } else { gitdir.join(rel) })
        .map(|joined| lexical_normalise(&joined))
        .unwrap_or(gitdir);
    // The common dir is `<root>/.git`; the root is its parent.
    common.parent().map(Path::to_path_buf)
}

/// Substitute `<repo>` and `<branch>` in `pattern` and resolve the
/// result against `repo_root`. `/` in the branch name becomes `-` in
/// the directory component (two branches `a/b` and `a-b` can therefore
/// collide in the default pattern; git's own error then propagates).
pub fn resolve_worktree_path(
    repo_root: &Path,
    pattern: &str,
    branch: &str,
) -> anyhow::Result<PathBuf> {
    let repo_name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("repository root {:?} has no name component", repo_root))?;
    let branch_component = sanitise_branch_component(branch)?;
    let substituted = pattern.replace("<repo>", repo_name).replace("<branch>", &branch_component);
    let raw = PathBuf::from(substituted);
    let path = if raw.is_absolute() { raw } else { repo_root.join(raw) };
    let path = lexical_normalise(&path);
    if path == repo_root {
        anyhow::bail!("worktree pattern resolves to the repository itself: {}", path.display());
    }
    Ok(path)
}

/// `/` in a branch name is legal git but a path separator, so it maps
/// to `-` in the directory component. Anything that would still produce
/// an empty/`.`/`..` component is rejected before git ever runs.
fn sanitise_branch_component(branch: &str) -> anyhow::Result<String> {
    let sanitised = branch.replace('/', "-");
    if sanitised.is_empty() || sanitised == "." || sanitised == ".." {
        anyhow::bail!("branch {branch:?} has no usable directory component");
    }
    Ok(sanitised)
}

/// `git worktree add -b <branch> <path>` run from `repo_root`.
/// Run `git <args>` in `cwd`, capturing stderr/stdout. Any failure
/// becomes an `Err` carrying git's own message so it can surface as the
/// verb's exit code (issue #77 AC7). Argv array only — never a shell
/// string (AGENTS.md hard rule).
fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn git: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() { stdout } else { stderr };
    anyhow::bail!("git {} failed: {}", args.join(" "), detail.trim());
}

pub fn git_worktree_add(repo_root: &Path, branch: &str, path: &Path) -> anyhow::Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worktree path {:?} is not valid UTF-8", path))?;
    run_git(repo_root, &["worktree", "add", "-b", branch, path])
}

/// `git worktree remove <path>` run from `ctx` (the main repo root, or
/// the worktree itself — git accepts both).
pub fn git_worktree_remove(ctx: &Path, path: &Path) -> anyhow::Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worktree path {:?} is not valid UTF-8", path))?;
    run_git(ctx, &["worktree", "remove", path])
}

/// `git worktree prune` run from `repo_root`.
pub fn git_worktree_prune(repo_root: &Path) -> anyhow::Result<()> {
    run_git(repo_root, &["worktree", "prune"])
}

/// Resolve a `.git` entry (dir for a plain checkout, `gitdir:` file for
/// a worktree/submodule) by walking up from `start`.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let contents = std::fs::read_to_string(&candidate).ok()?;
            let rest = contents.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = PathBuf::from(rest);
            return Some(if gitdir.is_absolute() { gitdir } else { dir.join(gitdir) });
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Lexically remove `.` and `..` components from an absolute path
/// (std has no normalise; `canonicalise` would hit the filesystem,
/// which must stay untouched — the target does not exist yet).
fn lexical_normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mtyx-wt-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn worktree_default_pattern_resolves_repo_dot_branch() {
        let base = temp_dir("default-pattern");
        let repo = base.join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let path = resolve_worktree_path(&repo, DEFAULT_PATTERN, "feat-auth").unwrap();
        assert_eq!(path, base.join("proj.feat-auth"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn worktree_pattern_sanitises_branch_slashes() {
        let base = temp_dir("slash-branch");
        let repo = base.join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let path = resolve_worktree_path(&repo, DEFAULT_PATTERN, "feat/auth").unwrap();
        assert_eq!(path, base.join("proj.feat-auth"));

        // A branch with no usable directory component is rejected.
        assert!(resolve_worktree_path(&repo, DEFAULT_PATTERN, "..").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn worktree_custom_pattern_substitutes_placeholders() {
        let base = temp_dir("custom-pattern");
        let repo = base.join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let path = resolve_worktree_path(&repo, "../wt/<branch>", "feat-auth").unwrap();
        assert_eq!(path, base.join("wt").join("feat-auth"));

        // Absolute patterns are used verbatim after substitution.
        let abs = resolve_worktree_path(&repo, "/var/tmp/mtyx-wt/<repo>-<branch>", "x").unwrap();
        assert_eq!(abs, Path::new("/var/tmp/mtyx-wt/proj-x"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn repo_root_walks_up_and_resolves_gitfile_indirection() {
        // Mirrors the real git layout from git_info.rs's test: the
        // worktree's `.git` file points at a private admin dir under
        // the main repo's `.git/worktrees/`, which carries `commondir`.
        let base = temp_dir("walk-up");
        let main = base.join("main-repo");
        let admin = main.join(".git").join("worktrees").join("feature");
        let worktree = base.join("worktree");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();

        // From a nested dir inside the worktree, the repo root is the
        // worktree itself (git scopes a worktree as its own root).
        assert_eq!(find_repo_root(&worktree.join("src")), Some(worktree.clone()));
        // The MAIN repo root resolves through the commondir pointer.
        assert_eq!(main_repo_root(&worktree), Some(main.clone()));
        // A plain checkout: main_repo_root == the checkout root.
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert_eq!(main_repo_root(&main), Some(main.clone()));
        // No .git anywhere in a fresh temp dir.
        let bare = temp_dir("no-git");
        assert_eq!(find_repo_root(&bare), None);
        std::fs::remove_dir_all(&base).unwrap();
        std::fs::remove_dir_all(&bare).unwrap();
    }

    #[test]
    fn git_worktree_failure_propagates_stderr() {
        // git must be present for worktree verbs (AGENTS.md toolchain);
        // skip rather than fail on hosts without it.
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let base = temp_dir("git-failure");
        let not_a_repo = base.join("not-a-repo");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        let err = git_worktree_add(&not_a_repo, "feat-auth", &base.join("wt")).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("not a git repository"),
            "expected git's stderr in the error, got: {message}"
        );
        // Nothing was created and no record-worthy side effects remain.
        assert!(!base.join("wt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
