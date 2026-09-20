//! Git branch lookup for the sidebar, from a pane's known cwd.
//!
//! Reads `.git/HEAD` directly instead of shelling out to `git`: HEAD's
//! `ref: refs/heads/<branch>` line names the branch regardless of whether
//! that ref is packed, so no subprocess or `packed-refs` parsing is
//! needed. Correct for worktrees, where `.git` is a file pointing at a
//! private `gitdir` with its own `HEAD`.
//!
//! Cwd here is always a local path: today's only frontends are the local
//! TUI and an attach client on the same machine (see
//! `session/remote.rs`). If a networked SSH remote is added later, this
//! lookup needs to move to wherever the PTY actually runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(2);

/// Walks up from `start` to find the enclosing repo's git dir, resolving
/// worktree/submodule `.git` files to their real `gitdir`.
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

/// Branch name from a git dir's `HEAD`, or a short sha when detached.
fn read_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        Some(branch.to_string())
    } else if head.len() >= 7 && head.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        Some(format!("@{}", &head[..7]))
    } else {
        None
    }
}

fn branch_for_path(cwd: &str) -> Option<String> {
    let git_dir = find_git_dir(Path::new(cwd))?;
    read_branch(&git_dir)
}

/// The common (shared) git dir for `git_dir`: linked worktrees keep a
/// `commondir` file (`../..` relative to the private admin dir) pointing
/// at the main repository's `.git`, where shared refs live. A plain
/// checkout has no such file and IS its own common dir.
fn read_commondir(git_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() { path } else { git_dir.join(path) })
}

/// A 7-char short sha, or None when `sha` is not a long-enough hex
/// string.
fn short_sha(sha: &str) -> Option<String> {
    let sha = sha.trim();
    (sha.len() >= 7 && sha.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .then(|| sha[..7].to_string())
}

/// Resolve a git dir's HEAD to a short sha (issue #77 sidebar badge).
/// A detached HEAD is its own sha; a branch ref resolves against the
/// COMMON dir (worktrees share refs with the main repository): the
/// loose `refs/heads/<branch>` file first, then `packed-refs`. Pure
/// filesystem reads — no subprocess, like the rest of this module.
fn head_short(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(refname) = head.strip_prefix("ref:") else {
        return short_sha(head);
    };
    let refname = refname.trim();
    let common = read_commondir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    if let Ok(loose) = std::fs::read_to_string(common.join(refname)) {
        if let Some(short) = short_sha(&loose) {
            return Some(short);
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, name) = line.split_once(' ')?;
        (name.trim() == refname).then(|| short_sha(sha)).flatten()
    })
}

fn head_short_for_path(cwd: &str) -> Option<String> {
    find_git_dir(Path::new(cwd)).and_then(|git_dir| head_short(&git_dir))
}

/// Per-cwd branch cache so the sidebar's per-frame redraw doesn't restat
/// `.git/HEAD` more than a couple of times a second.
#[derive(Default)]
pub struct GitInfoCache {
    entries: Mutex<HashMap<String, (Instant, Option<String>)>>,
    head_shorts: Mutex<HashMap<String, (Instant, Option<String>)>>,
}

impl GitInfoCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn branch_for(&self, cwd: &str) -> Option<String> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        if let Some((checked_at, branch)) = entries.get(cwd) {
            if now.duration_since(*checked_at) < TTL {
                return branch.clone();
            }
        }
        let branch = branch_for_path(cwd);
        entries.insert(cwd.to_string(), (now, branch.clone()));
        branch
    }

    /// Short HEAD sha for the repo at `cwd` (issue #77 sidebar badge),
    /// cached with the same TTL as [`Self::branch_for`]. For a linked
    /// worktree this reads the worktree's own HEAD, so the badge shows
    /// the worktree branch's tip.
    pub fn head_short_for(&self, cwd: &str) -> Option<String> {
        let now = Instant::now();
        let mut entries = self.head_shorts.lock().unwrap();
        if let Some((checked_at, short)) = entries.get(cwd) {
            if now.duration_since(*checked_at) < TTL {
                return short.clone();
            }
        }
        let short = head_short_for_path(cwd);
        entries.insert(cwd.to_string(), (now, short.clone()));
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branch_from_head() {
        let dir = std::env::temp_dir().join(format!("git-info-test-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/thing\n").unwrap();

        assert_eq!(branch_for_path(dir.to_str().unwrap()), Some("feature/thing".to_string()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let dir =
            std::env::temp_dir().join(format!("git-info-test-detached-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "a78fe53efaaea56b80d47569d85e0d7b76512aa7\n").unwrap();

        assert_eq!(branch_for_path(dir.to_str().unwrap()), Some("@a78fe53".to_string()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn worktree_gitdir_pointer_resolves() {
        // Mirrors real git layout: the worktree's `.git` file points at a
        // private admin dir (normally `<main>/.git/worktrees/<name>`) that
        // holds its own HEAD, separate from the main repo's.
        let base = std::env::temp_dir().join(format!("git-info-test-wt-{}", std::process::id()));
        let worktree_admin_dir = base.join("main-repo-git").join("worktrees").join("feature");
        let worktree_dir = base.join("worktree");
        std::fs::create_dir_all(&worktree_admin_dir).unwrap();
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(worktree_admin_dir.join("HEAD"), "ref: refs/heads/worktree-branch\n")
            .unwrap();
        std::fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", worktree_admin_dir.display()),
        )
        .unwrap();

        assert_eq!(
            branch_for_path(worktree_dir.to_str().unwrap()),
            Some("worktree-branch".to_string())
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn non_git_directory_returns_none() {
        assert_eq!(branch_for_path(std::env::temp_dir().to_str().unwrap()), None);
    }

    // --- worktree HEAD short-sha for the sidebar badge (issue #77) ---

    #[test]
    fn head_short_resolves_loose_ref_and_detached() {
        let dir = std::env::temp_dir().join(format!("git-info-sha-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads/feat")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feat/auth\n").unwrap();
        std::fs::write(
            git_dir.join("refs/heads/feat/auth"),
            "a78fe53efaaea56b80d47569d85e0d7b76512aa7\n",
        )
        .unwrap();
        assert_eq!(head_short_for_path(dir.to_str().unwrap()), Some("a78fe53".to_string()));

        // Detached HEAD: HEAD itself is the sha.
        std::fs::write(git_dir.join("HEAD"), "deadbee000000000000000000000000000000000\n").unwrap();
        assert_eq!(head_short_for_path(dir.to_str().unwrap()), Some("deadbee".to_string()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn head_short_falls_back_to_packed_refs() {
        let dir = std::env::temp_dir().join(format!("git-info-packed-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/old\n").unwrap();
        std::fs::write(
            git_dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \n1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/other\n2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/old\n",
        )
        .unwrap();
        assert_eq!(head_short_for_path(dir.to_str().unwrap()), Some("2222222".to_string()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn head_short_follows_commondir_from_worktree() {
        // Real linked-worktree layout: the worktree's `.git` file points
        // at an admin dir whose HEAD names the branch, while the sha
        // lives in the MAIN repo's refs (via the admin dir's
        // `commondir` pointer).
        let base = std::env::temp_dir().join(format!("git-info-wt-sha-{}", std::process::id()));
        let main_git = base.join("main").join(".git");
        let admin = main_git.join("worktrees").join("feat");
        let worktree = base.join("worktree");
        std::fs::create_dir_all(admin.join("refs/heads")).unwrap(); // must NOT be used
        std::fs::create_dir_all(main_git.join("refs/heads/feat")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat/auth\n").unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            main_git.join("refs/heads/feat/auth"),
            "9998887776665554443332221110009998887776\n",
        )
        .unwrap();
        assert_eq!(head_short_for_path(worktree.to_str().unwrap()), Some("9998887".to_string()));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn head_short_rejects_non_hex_heads() {
        let dir = std::env::temp_dir().join(format!("git-info-badsha-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "not-a-sha\n").unwrap();
        assert_eq!(head_short_for_path(dir.to_str().unwrap()), None);
        assert_eq!(short_sha("abc1234"), Some("abc1234".to_string()));
        assert_eq!(short_sha("abc"), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
