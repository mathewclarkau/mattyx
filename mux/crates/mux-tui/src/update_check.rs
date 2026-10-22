//! Low-noise release update check on TUI startup (issue #132).
//!
//! Fetches the latest GitHub release metadata on a background thread at most
//! once per 24 hours and, when the published tag is strictly newer than
//! [`mux_core::VERSION`], sends a one-line toast message to the TUI event loop:
//!
//! ```text
//! mtyx v0.X.Y available — github.com/mathewclarkau/mattyx/releases/latest
//! ```
//!
//! Only the interactive TUI startup path (`app::run` / `run_tui`) spawns this
//! check. Headless daemons, control-socket CLI verbs, `mtyx ssh`, and agent
//! hook subcommands never invoke it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default GitHub Releases API endpoint for the latest published release.
pub const DEFAULT_RELEASE_API_URL: &str =
    "https://api.github.com/repos/mathewclarkau/mattyx/releases/latest";

/// Short display URL shown in the TUI update toast.
pub const RELEASES_LATEST_SHORT: &str = "github.com/mathewclarkau/mattyx/releases/latest";

/// Full HTTPS URL to the latest release page.
pub const RELEASES_LATEST_URL: &str = "https://github.com/mathewclarkau/mattyx/releases/latest";

/// Full HTTPS URL to the releases list page.
pub const RELEASES_URL: &str = "https://github.com/mathewclarkau/mattyx/releases";

/// Minimum interval between remote update checks (24 hours).
pub const CHECK_INTERVAL_SECS: u64 = 86_400;

/// On-disk state in `<config_dir>/update-check.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckCache {
    pub last_check: u64,
    pub latest_seen: String,
}

/// Parse a version or tag string (`"0.25.0"`, `"v0.25.0"`,
/// `"0.25.0-14-gabc1234"`, `"0.25.0-dirty"`) down to its `(major, minor, patch)`
/// semver triple.
///
/// Build/dev suffixes after `-` or `+` are ignored for triple extraction,
/// though a bare trailing delimiter (`"0.25.0-"`) is rejected. Returns `None`
/// on any unparseable input without panicking.
pub fn parse_semver_triple(version: &str) -> Option<(u64, u64, u64)> {
    let s = version.trim();
    let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let core = match s.find(['-', '+']) {
        Some(idx) => {
            let suffix = &s[idx + 1..];
            if suffix.is_empty() {
                return None;
            }
            &s[..idx]
        }
        None => s,
    };
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// True only when `latest_tag` parses to a semver triple strictly greater
/// than `running`'s semver triple.
///
/// An equal triple with a `-<N>-g<sha>` or `-dirty` suffix on `running`
/// means the user is running a post-tag development build of that release
/// (ahead, not behind) and returns `false`. Unparseable input on either
/// side returns `false`.
pub fn is_newer_release(running: &str, latest_tag: &str) -> bool {
    let (Some(running_triple), Some(latest_triple)) =
        (parse_semver_triple(running), parse_semver_triple(latest_tag))
    else {
        return false;
    };
    latest_triple > running_triple
}

/// True when `version` carries a pre-release / git-describe / dirty suffix
/// after its semver triple (e.g. `0.25.0-3-gabc1234`, `0.25.0-dirty`,
/// `0.0.1-test`).
pub fn is_dev_build(version: &str) -> bool {
    if parse_semver_triple(version).is_none() {
        return false;
    }
    let s = version.trim();
    let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
    s.contains('-')
}

/// Normalize a release tag to its canonical `vX.Y.Z` form (preserving any
/// valid tag suffix if present, with a single leading `v`). Returns `None`
/// if `tag` does not start with a valid semver triple.
pub fn normalize_v_tag(tag: &str) -> Option<String> {
    parse_semver_triple(tag)?;
    let trimmed = tag.trim();
    let without_v =
        trimmed.strip_prefix('v').or_else(|| trimmed.strip_prefix('V')).unwrap_or(trimmed);
    Some(format!("v{without_v}"))
}

/// Format the one-line TUI banner for `tag`.
pub fn banner_for_tag(tag: &str) -> Option<String> {
    let v_tag = normalize_v_tag(tag)?;
    Some(format!("mtyx {v_tag} available — {RELEASES_LATEST_SHORT}"))
}

/// Extract and validate `"tag_name"` from a GitHub Releases API JSON payload.
pub fn parse_release_tag(json_text: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(json_text).map_err(|e| format!("invalid release JSON: {e}"))?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| "release JSON missing string field \"tag_name\"".to_string())?;
    normalize_v_tag(tag).ok_or_else(|| format!("unparseable release tag_name {tag:?}"))
}

/// Path to `<config_dir>/update-check.json`.
pub fn default_cache_path() -> Option<PathBuf> {
    mux_core::platform::config_dir().map(|dir| dir.join("update-check.json"))
}

/// Read the cached check state from `path`. Missing or corrupt cache files
/// return `None` so the next successful fetch overwrites them cleanly.
pub fn load_cache(path: &Path) -> Option<UpdateCheckCache> {
    let text = std::fs::read_to_string(path).ok()?;
    let cache: UpdateCheckCache = serde_json::from_str(&text).ok()?;
    parse_semver_triple(&cache.latest_seen)?;
    Some(cache)
}

/// Best-effort atomic write of `cache` to `path`. Refuses to write through
/// symlinks and silently ignores unwritable directories.
pub fn save_cache(path: &Path, cache: &UpdateCheckCache) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return;
        }
    }
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(serialized) = serde_json::to_string_pretty(cache) else { return };
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if let Ok(meta) = std::fs::symlink_metadata(&tmp) {
        if meta.file_type().is_symlink() {
            return;
        }
    }
    if std::fs::write(&tmp, serialized).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Pure predicate for `MTYX_NO_UPDATE_CHECK`: any non-empty value other than
/// `"0"` or `"false"` opts out.
pub fn env_opts_out_value(val: Option<&str>) -> bool {
    match val.map(str::trim) {
        None | Some("") | Some("0") => false,
        Some(v) if v.eq_ignore_ascii_case("false") => false,
        Some(_) => true,
    }
}

/// Whether `MTYX_NO_UPDATE_CHECK` is set in the process environment.
pub fn env_opts_out() -> bool {
    env_opts_out_value(std::env::var("MTYX_NO_UPDATE_CHECK").ok().as_deref())
}

/// Read the `update_check` boolean from a `mux.json` or `mux.toml` file.
///
/// - Missing file → `Ok(true)` (default enabled).
/// - Foreign/unknown keys in `mux.json` or `mux.toml` are tolerated here and
///   the file is never written to.
/// - Malformed JSON/TOML propagates `Err(...)` per `AGENTS.md` (never silently
///   `unwrap_or_default()` on a broken user config).
pub fn read_config_update_check(path: &Path) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let is_toml = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
    let is_json = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        || text.trim_start_matches('\u{feff}').trim_start().starts_with('{');
    if is_json && !is_toml {
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
        match value.get("update_check") {
            None => Ok(true),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(format!(
                "invalid update_check={other} in {}: expected boolean",
                path.display()
            )),
        }
    } else {
        let value: toml::Value = toml::from_str(&text)
            .map_err(|e| format!("invalid TOML in {}: {e}", path.display()))?;
        match value.get("update_check") {
            None => Ok(true),
            Some(toml::Value::Boolean(b)) => Ok(*b),
            Some(other) => Err(format!(
                "invalid update_check={other} in {}: expected boolean",
                path.display()
            )),
        }
    }
}

/// Pure, dependency-injected update check used by both the TUI background
/// worker and unit tests (zero network access in tests).
///
/// - Suppresses the fetch (call count 0) if `!config_enabled`, `env_opt_out`,
///   or `cache_path` holds a valid `last_check` within `CHECK_INTERVAL_SECS`
///   of `now_secs`.
/// - On a successful fetch + valid `tag_name`, writes `update-check.json`
///   and returns `Some(banner)` iff `is_newer_release(running_version, &tag)`.
pub fn check_with<F>(
    running_version: &str,
    config_enabled: bool,
    env_opt_out: bool,
    cache_path: Option<&Path>,
    now_secs: u64,
    url: &str,
    mut fetch: F,
) -> Option<String>
where
    F: FnMut(&str) -> Result<String, String>,
{
    if !config_enabled || env_opt_out {
        return None;
    }
    if let Some(path) = cache_path {
        if let Some(cached) = load_cache(path) {
            if now_secs >= cached.last_check
                && now_secs.saturating_sub(cached.last_check) < CHECK_INTERVAL_SECS
            {
                return None;
            }
        }
    }
    let body = fetch(url).ok()?;
    let v_tag = parse_release_tag(&body).ok()?;
    if let Some(path) = cache_path {
        save_cache(
            path,
            &UpdateCheckCache {
                last_check: now_secs,
                latest_seen: v_tag.clone(),
            },
        );
    }
    if is_newer_release(running_version, &v_tag) {
        banner_for_tag(&v_tag)
    } else {
        None
    }
}

/// Resolve the release metadata URL (`MTYX_UPDATE_CHECK_URL` override or
/// [`DEFAULT_RELEASE_API_URL`]).
pub fn resolve_release_api_url() -> String {
    std::env::var("MTYX_UPDATE_CHECK_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_API_URL.to_string())
}

/// If `url_or_path` is a `file://` URL or does not start with `http://` /
/// `https://`, return the corresponding local filesystem path.
pub(crate) fn local_fixture_path(url_or_path: &str) -> Option<PathBuf> {
    if let Some(rest) = url_or_path.strip_prefix("file://") {
        return Some(PathBuf::from(rest));
    }
    if !url_or_path.starts_with("http://") && !url_or_path.starts_with("https://") {
        return Some(PathBuf::from(url_or_path));
    }
    None
}

pub(crate) fn curl_program() -> &'static str {
    if cfg!(windows) {
        "curl.exe"
    } else {
        "curl"
    }
}

/// Fetch text from `url` with a hard 3-second timeout (or read directly from
/// disk when `url` is a `file://` URL or local path for hermetic testing).
///
/// Uses `Command::new(curl)` with an explicit argument array (never shell
/// interpolation) and reaps the child via `wait_with_output()`.
pub fn default_fetch_text(url: &str) -> Result<String, String> {
    if let Some(path) = local_fixture_path(url) {
        return std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()));
    }
    let user_agent = format!("User-Agent: mtyx/{}", mux_core::VERSION);
    let child = Command::new(curl_program())
        .args([
            "-fsSL",
            "--max-time",
            "3",
            "-H",
            &user_agent,
            "-H",
            "Accept: application/vnd.github+json",
            "--",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn curl: {e}"))?;
    let output =
        child.wait_with_output().map_err(|e| format!("failed waiting on curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "curl exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 from {url}: {e}"))
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Non-blocking handle owned by the TUI [`crate::app::App`] to poll the
/// one-shot background update check.
pub struct UpdateCheckHandle {
    rx: Option<Receiver<Option<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl UpdateCheckHandle {
    pub fn disabled() -> Self {
        Self { rx: None, handle: None }
    }

    /// Non-blockingly poll the worker thread. Once the worker sends its
    /// single result (or disconnects), the channel is dropped and the worker
    /// thread is joined immediately.
    pub fn poll(&mut self) -> Option<String> {
        let rx = self.rx.as_ref()?;
        match rx.try_recv() {
            Ok(msg) => {
                self.rx = None;
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
                msg
            }
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
                None
            }
            Err(TryRecvError::Empty) => None,
        }
    }
}

/// Spawn the background update check for a TUI session.
///
/// Checks both `config_enabled` (from the resolved `Config`) and the local
/// config file's `update_check` setting (so a thin-client attach whose base
/// config came from an existing daemon still honours a local
/// `update_check = false`), plus `MTYX_NO_UPDATE_CHECK`.
pub fn spawn(config_enabled: bool) -> UpdateCheckHandle {
    if !config_enabled || env_opts_out() {
        return UpdateCheckHandle::disabled();
    }
    if let Some(cfg_path) = mux_core::platform::config_path() {
        match read_config_update_check(&cfg_path) {
            Ok(true) => {}
            Ok(false) | Err(_) => return UpdateCheckHandle::disabled(),
        }
    }
    let cache_path = default_cache_path();
    let url = resolve_release_api_url();
    let now_secs = unix_now_secs();
    let (tx, rx) = channel();
    let handle = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let banner = check_with(
                mux_core::VERSION,
                true,
                false,
                cache_path.as_deref(),
                now_secs,
                &url,
                default_fetch_text,
            );
            let _ = tx.send(banner);
        })
        .ok();
    match handle {
        Some(h) => UpdateCheckHandle { rx: Some(rx), handle: Some(h) },
        None => UpdateCheckHandle::disabled(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mtyx-update-check-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn semver_parser_handles_tags_dev_suffixes_dirty_and_garbage() {
        assert_eq!(parse_semver_triple("0.24.0"), Some((0, 24, 0)));
        assert_eq!(parse_semver_triple("v0.25.0"), Some((0, 25, 0)));
        assert_eq!(parse_semver_triple("V1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver_triple("0.25.0-3-gabc1234"), Some((0, 25, 0)));
        assert_eq!(parse_semver_triple("0.25.0-dirty"), Some((0, 25, 0)));
        assert_eq!(parse_semver_triple("0.25.0-14-gabc1234-dirty"), Some((0, 25, 0)));
        assert_eq!(parse_semver_triple("0.0.1-test"), Some((0, 0, 1)));

        // Garbage / malformed inputs never panic and return None.
        for bad in ["", "v", "0.25", "0.25.0.1", "0.25.0-", "abc", "v1.x.0", "-1.0.0"] {
            assert_eq!(parse_semver_triple(bad), None, "expected None for {bad:?}");
        }
    }

    #[test]
    fn older_running_version_triggers_banner() {
        assert!(is_newer_release("0.24.0", "v0.25.0"));
        let cache_dir = unique_tmp_dir("older");
        let cache_path = cache_dir.join("update-check.json");
        let calls = Cell::new(0usize);
        let banner = check_with(
            "0.24.0",
            true,
            false,
            Some(&cache_path),
            1_700_000_000,
            "https://example.invalid/releases/latest",
            |_| {
                calls.set(calls.get() + 1);
                Ok(r#"{"tag_name":"v0.25.0"}"#.to_string())
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            banner.as_deref(),
            Some("mtyx v0.25.0 available — github.com/mathewclarkau/mattyx/releases/latest")
        );
        let saved = load_cache(&cache_path).expect("cache should be written");
        assert_eq!(saved.last_check, 1_700_000_000);
        assert_eq!(saved.latest_seen, "v0.25.0");
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn post_release_dev_build_does_not_trigger_banner() {
        // 0.25.0-3-gabc1234 running vs v0.25.0 latest → no banner (ahead of release).
        assert!(!is_newer_release("0.25.0-3-gabc1234", "v0.25.0"));
        assert!(is_dev_build("0.25.0-3-gabc1234"));
        let banner = check_with(
            "0.25.0-3-gabc1234",
            true,
            false,
            None,
            1_700_000_000,
            "https://example.invalid/releases/latest",
            |_| Ok(r#"{"tag_name":"v0.25.0"}"#.to_string()),
        );
        assert_eq!(banner, None);
    }

    #[test]
    fn equal_release_version_does_not_trigger_banner() {
        // 0.25.0 running vs v0.25.0 latest → no banner.
        assert!(!is_newer_release("0.25.0", "v0.25.0"));
        assert!(!is_dev_build("0.25.0"));
        let banner = check_with(
            "0.25.0",
            true,
            false,
            None,
            1_700_000_000,
            "https://example.invalid/releases/latest",
            |_| Ok(r#"{"tag_name":"v0.25.0"}"#.to_string()),
        );
        assert_eq!(banner, None);
    }

    #[test]
    fn fresh_cache_suppresses_second_fetch_within_24h() {
        let cache_dir = unique_tmp_dir("throttle");
        let cache_path = cache_dir.join("update-check.json");
        save_cache(
            &cache_path,
            &UpdateCheckCache {
                last_check: 1_700_000_000,
                latest_seen: "v0.25.0".to_string(),
            },
        );

        let calls = Cell::new(0usize);
        let banner = check_with(
            "0.24.0",
            true,
            false,
            Some(&cache_path),
            1_700_000_000 + CHECK_INTERVAL_SECS - 1,
            "https://example.invalid/releases/latest",
            |_| {
                calls.set(calls.get() + 1);
                Ok(r#"{"tag_name":"v0.25.0"}"#.to_string())
            },
        );
        assert_eq!(calls.get(), 0, "fresh cache must suppress fetch within 24h");
        assert_eq!(banner, None, "must not prompt again within 24h");

        // Once 24h has elapsed, the next check fetches again.
        let banner_after = check_with(
            "0.24.0",
            true,
            false,
            Some(&cache_path),
            1_700_000_000 + CHECK_INTERVAL_SECS,
            "https://example.invalid/releases/latest",
            |_| {
                calls.set(calls.get() + 1);
                Ok(r#"{"tag_name":"v0.25.0"}"#.to_string())
            },
        );
        assert_eq!(calls.get(), 1);
        assert!(banner_after.is_some());
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn env_and_config_opt_out_both_suppress_fetch() {
        let calls = Cell::new(0usize);
        assert!(env_opts_out_value(Some("1")));
        assert!(env_opts_out_value(Some("true")));
        assert!(!env_opts_out_value(None));
        assert!(!env_opts_out_value(Some("")));
        assert!(!env_opts_out_value(Some("0")));

        // MTYX_NO_UPDATE_CHECK=1 (env_opt_out = true) suppresses fetch.
        let res_env = check_with(
            "0.24.0",
            true,
            env_opts_out_value(Some("1")),
            None,
            1_700_000_000,
            "https://example.invalid/releases/latest",
            |_| {
                calls.set(calls.get() + 1);
                Ok(r#"{"tag_name":"v0.25.0"}"#.to_string())
            },
        );
        assert_eq!(res_env, None);
        assert_eq!(calls.get(), 0);

        // update_check: false in config (config_enabled = false) suppresses fetch.
        let res_cfg = check_with(
            "0.24.0",
            false,
            false,
            None,
            1_700_000_000,
            "https://example.invalid/releases/latest",
            |_| {
                calls.set(calls.get() + 1);
                Ok(r#"{"tag_name":"v0.25.0"}"#.to_string())
            },
        );
        assert_eq!(res_cfg, None);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn corrupt_cache_file_is_ignored_and_overwritten_while_mux_json_propagates_errors() {
        let dir = unique_tmp_dir("corrupt");
        let cache_path = dir.join("update-check.json");
        std::fs::write(&cache_path, "{not valid json").unwrap();
        assert_eq!(load_cache(&cache_path), None);

        let calls = Cell::new(0usize);
        let banner = check_with(
            "0.24.0",
            true,
            false,
            Some(&cache_path),
            1_700_000_500,
            "https://example.invalid/releases/latest",
            |_| {
                calls.set(calls.get() + 1);
                Ok(r#"{"tag_name":"v0.25.1"}"#.to_string())
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            banner.as_deref(),
            Some("mtyx v0.25.1 available — github.com/mathewclarkau/mattyx/releases/latest")
        );
        let overwritten = load_cache(&cache_path).expect("corrupt cache must be overwritten");
        assert_eq!(overwritten.last_check, 1_700_000_500);
        assert_eq!(overwritten.latest_seen, "v0.25.1");

        // Conversely, mux.json must propagate parse errors (never silently default)
        // and must never be modified when it contains unknown/existing user keys.
        let mux_json = dir.join("mux.json");
        std::fs::write(&mux_json, "{corrupt mux.json").unwrap();
        assert!(
            read_config_update_check(&mux_json).is_err(),
            "mux.json parse errors must propagate"
        );

        let user_mux_json = r#"{"update_check":false,"custom_foreign_key":{"keep":123}}"#;
        std::fs::write(&mux_json, user_mux_json).unwrap();
        assert_eq!(read_config_update_check(&mux_json).unwrap(), false);
        assert_eq!(
            std::fs::read_to_string(&mux_json).unwrap(),
            user_mux_json,
            "mux.json must never be modified by the update checker"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
