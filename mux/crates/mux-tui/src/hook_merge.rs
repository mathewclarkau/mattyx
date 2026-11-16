//! Shared helpers for the agent hook installers (`antigravity_hook`,
//! `codex_hook`, `grok_hook`, `pi_hook`, `opencode_hook`, `aider_hook`).
//!
//! ## What is and isn't migrated here
//!
//! `claude_hook.rs` keeps its own config logic: post-PR #7 it is already
//! minimal, and it is the precedent for "if the file is already small,
//! don't refactor it for the sake of it." It does share [`hook_bin`] —
//! it is where that resolution logic was born (issue #97 generalized it
//! to every installer).
//!
//! `aider_hook.rs` shares only [`hook_bin`]/[`shell_quote`]: it installs
//! a bash wrapper and has no JSON config and no marker-block logic, so
//! it consumes none of the load/save/marker helpers below.
//!
//! ## Behavior contracts preserved from the original inline code
//!
//! These are the two contracts the scout report flagged as load-bearing;
//! getting either wrong is a silent regression, not a refactor:
//!
//!   * **`load_or_default` fails loud on parse errors** and only
//!     substitutes `T::default()` on a *genuinely missing* file. A
//!     silent `unwrap_or_default()`-style swallow would overwrite a
//!     user's real, merely-schema-drifted config (the old inline code
//!     carried explicit comments warning about this at
//!     `antigravity_hook.rs:65-67` and `codex_hook.rs:67`).
//!
//!   * **`replace_marked_block` strips-then-appends**, not
//!     replace-in-place: any existing block is removed from wherever it
//!     sits and a fresh block is appended at the very end. This matches
//!     `pi_hook.rs`'s original install behavior. The trailing whitespace
//!     is trimmed before appending (the old install path did not trim,
//!     which left blank-line drift; the uninstall path did trim — this
//!     module makes the install path match the uninstall path).

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// The binary every installed hook command must invoke (issue #97).
///
/// Resolves to the absolute path of the *currently running* executable
/// so a hook never depends on a `$PATH` lookup at agent runtime — during
/// the cmux→mtyx rename a stale or shadowed `mtyx` earlier on `$PATH`
/// could silently hijack every installed hook. Falls back to the bare
/// name `"mtyx"` only when `current_exe()` fails (no meaningful path on
/// this platform), in which case behaviour matches the pre-#97 install.
/// This is the same resolution `claude_hook.rs` has always done; it lives
/// here now so every installer shares one copy of it.
pub(crate) fn hook_bin() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "mtyx".to_string())
}

/// Quote `word` as a single POSIX shell word (issue #97).
///
/// The resolved [`hook_bin`] path can contain spaces or any other shell
/// metacharacter; embedding it unquoted into the shell command strings
/// we write into agent configs would split into multiple words. Always
/// single-quotes: inside single quotes nothing is special, and an
/// embedded `'` is emitted as `'\''` (close, escaped quote, reopen).
/// The result is safe to interpolate into `sh -c` command strings.
pub(crate) fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Render `s` as a double-quoted JavaScript/TypeScript string literal
/// (issue #97). JSON string literals are valid JS string literals, and
/// serde_json escapes everything either language needs escaped
/// (`"`, `\\`, control characters), so installed `.ts` hook files can
/// embed the resolved [`hook_bin`] path — even one containing quotes or
/// backslashes — without breaking the file's syntax.
pub(crate) fn js_quote(s: &str) -> String {
    serde_json::to_string(s).expect("serializing a &str to a JSON string cannot fail")
}

#[cfg(test)]
pub(crate) mod test_support {
    /// `$HOME` (and any other process-global env var) is mutated by hook
    /// installer tests in *several* modules (`claude_hook`, `codex_hook`,
    /// `antigravity_hook`, `grok_hook`, `pi_hook`, `opencode_hook`). Cargo
    /// runs tests in one process, so every test that sets or restores
    /// `$HOME` must hold this lock — a module-local mutex would still
    /// race against the other modules' tests.
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

/// Why a config load failed.
///
/// `NotFound` is *recoverable*: the caller may legitimately substitute
/// `T::default()` (install path) or skip (uninstall path). `Io` and
/// `Parse` are **not** recoverable — surfacing them silently would risk
/// overwriting a user's real, merely-schema-drifted config, so they are
/// propagated and the caller is expected to exit non-zero.
#[derive(Debug)]
pub(crate) enum LoadError {
    /// The file does not exist. Recoverable.
    NotFound,
    /// Reading the file failed for a reason other than "missing".
    Io(std::io::Error),
    /// The file's contents are not valid JSON for `T`.
    Parse(serde_json::Error),
}

/// Why a config save failed.
#[derive(Debug)]
pub(crate) enum SaveError {
    /// `value` could not be serialized to JSON.
    Serialize(serde_json::Error),
    /// Writing the serialized JSON to `path` failed.
    Io(std::io::Error),
}

/// A delimited mtyx-managed block. `start` / `end` are the literal
/// substrings searched for on each line (e.g. the HTML-comment markers
/// `<!-- MTYX-START -->` / `<!-- MTYX-END -->`). Carried as a pair so a
/// caller can never pass half a pair.
///
/// Rename compat: every block function in this module ALSO matches the
/// cmux-era spelling of the same pair (each `MTYX-` token in the marker
/// literals additionally matches `CMUX-`), so re-running any installer
/// against a file last touched by a pre-rename build replaces the old
/// block instead of duplicating it. Only the canonical spelling is ever
/// written back.
pub(crate) struct Markers {
    pub start: &'static str,
    pub end: &'static str,
}

/// The cmux-era spelling of a marker token, if it has one: `MTYX-`
/// inside the token additionally matches `CMUX-`. Owned (not `&str`)
/// because it is derived at runtime from the canonical literals.
fn legacy_token(token: &str) -> Option<String> {
    if token.contains("MTYX-") {
        Some(token.replace("MTYX-", "CMUX-"))
    } else {
        None
    }
}

/// The set of literal spellings a line is matched against: the
/// canonical pair plus its cmux-era sibling when one exists.
struct MarkerSet {
    starts: Vec<String>,
    ends: Vec<String>,
}

impl MarkerSet {
    fn of(markers: &Markers) -> Self {
        let mut starts = vec![markers.start.to_string()];
        let mut ends = vec![markers.end.to_string()];
        if let (Some(start), Some(end)) = (legacy_token(markers.start), legacy_token(markers.end)) {
            starts.push(start);
            ends.push(end);
        }
        Self { starts, ends }
    }

    fn opens(&self, line: &str) -> bool {
        self.starts.iter().any(|s| line.contains(s))
    }

    fn closes(&self, line: &str) -> bool {
        self.ends.iter().any(|s| line.contains(s))
    }
}

/// The real mtyx marker tokens used by `pi_hook.rs`'s `APPEND_SYSTEM.md`
/// rewriter. **HTML-comment style** — NOT the `<<<MTYX-START>>>` form
/// mentioned in the issue brief. Using the wrong tokens here would break
/// existing installs (the markers already written into users'
/// `APPEND_SYSTEM.md` files are these HTML-comment ones).
pub(crate) const MTYX_MARKERS: Markers =
    Markers { start: "<!-- MTYX-START -->", end: "<!-- MTYX-END -->" };

/// Read and parse a JSON file at `path`.
///
/// Returns `Err(LoadError::NotFound)` only when the file is genuinely
/// missing; other read errors become `Err(LoadError::Io(..))` and a
/// failed parse becomes `Err(LoadError::Parse(..))`. None of those are
/// silently swallowed — see the module docs for why.
pub(crate) fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T, LoadError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::NotFound),
        Err(e) => return Err(LoadError::Io(e)),
    };
    serde_json::from_str::<T>(&text).map_err(LoadError::Parse)
}

/// Load a JSON config, falling back to `T::default()` only when the file
/// is *missing*.
///
/// This is the install-path convenience: "if the user has no config yet,
/// start from a default; otherwise parse what they have." Parse and IO
/// errors are still propagated as `Err(..)` — they are **not** defaulted
/// — because silently defaulting on a parse error would overwrite a
/// user's real config on schema drift. The uninstall path should use
/// [`load_json`] instead, since it must *skip* on missing (not default
/// and then write a freshly-created file).
pub(crate) fn load_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T, LoadError> {
    match load_json::<T>(path) {
        Ok(v) => Ok(v),
        Err(LoadError::NotFound) => Ok(T::default()),
        Err(e) => Err(e),
    }
}

/// Serialize `value` as pretty JSON (2-space indent, matching
/// `serde_json::to_string_pretty`) and write it to `path`, creating or
/// truncating the file.
///
/// Does **not** create parent directories. The contract is that callers
/// `fs::create_dir_all(parent)` first — every hook installer already
/// does this separately, and keeping the mkdir out of this helper means
/// a missing-parent error stays a real `Err(Io)` instead of being
/// papered over.
pub(crate) fn save_pretty<T: Serialize>(path: &Path, value: &T) -> Result<(), SaveError> {
    let s = serde_json::to_string_pretty(value).map_err(SaveError::Serialize)?;
    std::fs::write(path, s).map_err(SaveError::Io)
}

/// Strip every `start..end`-delimited block from `content` and return
/// the surviving lines (marker lines and inter-block content are
/// dropped; everything outside the block(s) is kept), with no trailing
/// newline.
///
/// An unclosed `start` swallows everything from there to EOF — matching
/// the original `pi_hook.rs` strip loops (the `skipping` flag, once set
/// by a `start` line with no matching `end`, never clears).
///
/// This is the core of [`replace_marked_block`] (which strips any
/// existing block and appends a fresh block). It is `pub(crate)` rather
/// than private because `pi_hook`'s uninstall path needs a pure strip
/// (remove the block; if the file ends up empty, delete it) that
/// `replace_marked_block` does not provide on its own.
pub(crate) fn strip_marked_block(content: &str, markers: &Markers) -> String {
    let set = MarkerSet::of(markers);
    let mut out = String::new();
    let mut skipping = false;
    for line in content.lines() {
        if set.opens(line) {
            skipping = true;
            continue;
        }
        if set.closes(line) {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    // The loop appends a trailing '\n' after every kept line; trim it so
    // callers (replace_marked_block, the uninstall path) control the
    // exact tail and we avoid blank-line drift.
    out.trim_end().to_string()
}

/// Replace the mtyx-managed block in `content` with a fresh block
/// wrapping `replacement_inner`, preserving the original
/// "strip-any-existing-block-from-anywhere, then append a fresh block
/// at the end" behavior of `pi_hook.rs`'s install path (NOT
/// replace-in-place).
///
/// `replacement_inner` is the text that goes *between* the markers; it
/// must not include the marker lines themselves. Trailing whitespace on
/// the stripped content is trimmed before the fresh block is appended
/// (the old install path did not trim, leaving blank-line drift; this
/// makes it match the uninstall path's `trim_end()`).
///
/// The appended block is exactly `\n{start}\n{inner}\n{end}\n`, so an
/// empty `content` yields just that block with a leading newline.
pub(crate) fn replace_marked_block(
    content: &str,
    markers: &Markers,
    replacement_inner: &str,
) -> String {
    let stripped = strip_marked_block(content, markers);
    let mut out = stripped;
    out.push('\n');
    out.push_str(markers.start);
    out.push('\n');
    out.push_str(replacement_inner);
    out.push('\n');
    out.push_str(markers.end);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    // A small JSON config type standing in for the real
    // AntigravityHooksConfig / CodexHooksConfig — the helpers are
    // generic, so a toy type exercises them without pulling in the
    // hook modules.
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Cfg {
        #[serde(default)]
        hooks: Vec<String>,
    }

    // Each file-I/O test gets its own unique scratch directory under the
    // system temp dir. We don't have `tempfile` as a dev-dep, so this
    // rolls our own: a per-process, per-call counter ensures parallel
    // tests and repeated runs never collide. Leaking the dirs is fine
    // (the OS reaps /tmp).
    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir(label: &str) -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mtyx_hook_merge_test_{}_{}_{}",
            std::process::id(),
            n,
            label
        ));
        // Start from a clean slate in case a prior run left it behind.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // ---- hook_bin / shell_quote / js_quote ----

    #[test]
    fn hook_bin_resolves_the_running_executable() {
        // In a test binary current_exe() is the test harness itself, so
        // hook_bin() must return exactly that path (the "mtyx" fallback
        // is only reachable on platforms where current_exe() fails).
        let expected = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "mtyx".to_string());
        assert_eq!(hook_bin(), expected);
        // Platform-neutral absolute check: Windows current_exe() is `D:\...`,
        // so a leading-slash assert can never pass there.
        assert!(
            std::path::Path::new(hook_bin().as_str()).is_absolute(),
            "expected an absolute path, got {}",
            hook_bin()
        );
    }

    #[test]
    fn shell_quote_quotes_spaces_and_embedded_single_quotes() {
        // Paths with spaces stay one shell word; embedded single quotes
        // are closed-escaped-reopened; plain paths are still quoted
        // (quoting is unconditional so no caller can forget it).
        assert_eq!(shell_quote("/usr/local/bin/mtyx"), "'/usr/local/bin/mtyx'");
        assert_eq!(shell_quote("/opt/my mtyx/mtyx"), "'/opt/my mtyx/mtyx'");
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn js_quote_emits_a_valid_string_literal() {
        assert_eq!(js_quote("/usr/local/bin/mtyx"), "\"/usr/local/bin/mtyx\"");
        assert_eq!(js_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    // ---- load_or_default ----

    #[test]
    fn load_or_default_missing_file_returns_default() {
        // Happy path: a genuinely missing file yields Ok(T::default()),
        // and must NOT create the file or print an error.
        let dir = scratch_dir("load_missing");
        let path = dir.join("hooks.json");
        assert!(!path.exists());
        let got: Cfg = load_or_default(&path).expect("missing file should Ok(default)");
        assert_eq!(got, Cfg::default());
        // Contract: no file is created by a load.
        assert!(!path.exists());
    }

    #[test]
    fn load_or_default_malformed_json_returns_parse_err() {
        // Error/edge path: a file that exists but is not valid JSON
        // returns Err(LoadError::Parse). This is the case the old
        // inline comments explicitly protected — defaulting here would
        // silently clobber a user's real, schema-drifted config.
        let dir = scratch_dir("load_malformed");
        let path = dir.join("hooks.json");
        std::fs::write(&path, "{not json").unwrap();
        match load_or_default::<Cfg>(&path) {
            Err(LoadError::Parse(_)) => {}
            other => panic!("expected Err(Parse), got {other:?}"),
        }
    }

    // ---- save_pretty ----

    #[test]
    fn save_pretty_round_trips_through_load_json() {
        // Happy path: save then load yields an equal value, and the
        // on-disk JSON uses 2-space pretty indentation.
        let dir = scratch_dir("save_roundtrip");
        let path = dir.join("hooks.json");
        let cfg = Cfg { hooks: vec!["mtyx report-agent".to_string(), "other".to_string()] };
        save_pretty(&path, &cfg).expect("save should succeed");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("\n  "),
            "expected 2-space pretty indent on disk, got:\n{on_disk}"
        );
        let back: Cfg = load_json(&path).expect("reload should succeed");
        assert_eq!(back, cfg);
    }

    #[test]
    fn save_pretty_missing_parent_returns_io_err() {
        // Error/edge path: the helper does NOT auto-mkdir, so writing
        // into a path whose parent does not exist is Err(SaveError::Io)
        // — preserving the contract that callers create the parent.
        let dir = scratch_dir("save_no_parent");
        let path = dir.join("does_not_exist").join("hooks.json");
        assert!(dir.join("does_not_exist").exists() == false);
        match save_pretty(&path, &Cfg::default()) {
            Err(SaveError::Io(_)) => {}
            other => panic!("expected Err(Io) for missing parent, got {other:?}"),
        }
        // No file was created.
        assert!(!path.exists());
    }

    // ---- replace_marked_block ----

    #[test]
    fn replace_marked_block_strips_existing_and_appends_fresh_at_end() {
        // Happy path: an existing block in the middle is removed from
        // its old position, and a fresh block is appended at the very
        // end. Text before and after the old block is preserved and
        // concatenated; trailing whitespace is trimmed so there is a
        // single newline before the new block (no blank-line drift).
        let content = "before\n\
            <!-- MTYX-START -->\n\
            stale old skill\n\
            <!-- MTYX-END -->\n\
            after\n";
        let got = replace_marked_block(content, &MTYX_MARKERS, "fresh skill");
        assert_eq!(
            got,
            "before\nafter\n\
             <!-- MTYX-START -->\n\
             fresh skill\n\
             <!-- MTYX-END -->\n"
        );
    }

    #[test]
    fn replace_marked_block_empty_content_yields_just_the_block() {
        // Error/edge path: empty content -> exactly the fresh block with
        // a leading newline (the append-to-empty case). Pins the leading
        // newline so installers don't accidentally drop it.
        let got = replace_marked_block("", &MTYX_MARKERS, "skill");
        assert_eq!(got, "\n<!-- MTYX-START -->\nskill\n<!-- MTYX-END -->\n");
    }

    // ---- rename compat: cmux-era marker spelling ----

    #[test]
    fn replace_marked_block_replaces_legacy_cmux_block_not_duplicates() {
        // Rename compat: a file last managed by a pre-rename build holds
        // a `<!-- CMUX-START/END -->` block. Re-running the installer must
        // strip the OLD block and append the fresh canonical one — never
        // leave two blocks behind.
        let content =
            "preamble\n\n<!-- CMUX-START -->\nold cmux-era skill\n<!-- CMUX-END -->\n\ntail\n";
        let got = replace_marked_block(content, &MTYX_MARKERS, "new skill");
        assert_eq!(got, "preamble\n\n\ntail\n<!-- MTYX-START -->\nnew skill\n<!-- MTYX-END -->\n");
        assert!(!got.contains("CMUX-START"));
        assert!(!got.contains("old cmux-era skill"));
    }

    #[test]
    fn replace_marked_block_handles_mixed_and_unclosed_legacy_blocks() {
        // A block opened with the legacy spelling but closed with the
        // canonical one (a rename-era install interrupted mid-write) is
        // still one block to the strip logic; and an unclosed legacy
        // start swallows to EOF, matching the canonical behaviour.
        let mixed = "a\n<!-- CMUX-START -->\nstale\n<!-- MTYX-END -->\nb\n";
        let got = replace_marked_block(mixed, &MTYX_MARKERS, "fresh");
        assert_eq!(got, "a\nb\n<!-- MTYX-START -->\nfresh\n<!-- MTYX-END -->\n");

        let unclosed = "a\n<!-- CMUX-START -->\nnever closed\nrest swallowed\n";
        let got = replace_marked_block(unclosed, &MTYX_MARKERS, "fresh");
        assert_eq!(got, "a\n<!-- MTYX-START -->\nfresh\n<!-- MTYX-END -->\n");
    }

    #[test]
    fn strip_matches_legacy_cmux_blocks() {
        let content = "keep\n<!-- CMUX-START -->\ninner line\n<!-- CMUX-END -->\nalso keep\n";
        assert_eq!(strip_marked_block(content, &MTYX_MARKERS), "keep\nalso keep");
    }

    #[test]
    fn bare_markers_also_match_legacy_spelling() {
        // opencode's plugin uses the bare `MTYX-START`/`MTYX-END`
        // spelling (no HTML comment); its cmux-era sibling `CMUX-START`
        // must be honoured identically.
        let bare = Markers { start: "MTYX-START", end: "MTYX-END" };
        let content = "// header\n// CMUX-START\nold plugin body\n// CMUX-END\n";
        assert_eq!(strip_marked_block(content, &bare), "// header");
        let got = replace_marked_block(content, &bare, "new body");
        assert_eq!(got, "// header\nMTYX-START\nnew body\nMTYX-END\n");
    }
}
