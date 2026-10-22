//! Issue #132: release-available check, TUI startup only.
//!
//! `run_tui` spawns one pending check before entering the alternate
//! screen; the result (if a newer release exists) surfaces as a toast.
//! The same discipline as `desktop_notify` applies here: the network
//! fetch runs on a background thread (never the render loop), every
//! failure mode is fully silent (no toast, no log spam, no panic — a
//! missing curl or unreachable GitHub degrades to "no banner"), and the
//! curl child is always reaped via `Command::output()` (which waits);
//! a `spawn()` + drop would leak a zombie per check. The worker never
//! touches stdout/stderr of the TUI.

use std::path::Path;

/// GitHub's unauthenticated rate limit is 60 requests/hour per IP, so a
/// check that runs on every TUI start (including every `mtyx attach`)
/// must be throttled on disk. Fresh cache = no request at all.
const THROTTLE_SECS: u64 = 24 * 60 * 60;

const DEFAULT_URL: &str = "https://api.github.com/repos/mathewclarkau/mattyx/releases/latest";

/// Handle held by the App so the event loop can poll for the result
/// without ever blocking (or joining) the worker thread.
///
/// `Some(text)` on the channel = a newer release exists, `text` is the
/// banner; `None` = checked and nothing newer (or the check failed
/// silently — both look identical to the UI, by design).
pub struct UpdateCheckState {
    pub rx: std::sync::mpsc::Receiver<Option<String>>,
    /// Set once the banner has been shown; the notice is once per session
    /// even if the state lingers.
    pub noticed: bool,
}

/// Start the startup check if one is due. Returns `None` when opted out
/// (`MTYX_NO_UPDATE_CHECK=1` or `update_check = false` in the local
/// config), throttled by the on-disk cache, or there is no HOME to cache
/// in — every early exit is silent. The caller holds the returned state
/// and polls it from the event loop; the worker thread is detached and
/// self-cleaning.
pub fn spawn_pending() -> Option<UpdateCheckState> {
    if no_check_env() {
        return None;
    }
    // Read the local config fresh, never `App.config`: a thin-client
    // attach rebuilds that from server chrome and would silently lose a
    // local opt-out (issue #132).
    if !crate::config::update_check_enabled() {
        return None;
    }
    let url = url_from_env();
    let cache_path = mux_core::platform::config_dir()?.join("update-check.json");
    let now = unix_now();
    // Fresh cache (checked <24h ago) = no thread, no request.
    if !should_check(read_cache(&cache_path).and_then(|c| c.last_check), now, THROTTLE_SECS) {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // check_once re-reads the cache and writes the throttle timestamp
        // itself, so a racing second start is still throttled. The send
        // error (App already dropped) is deliberately ignored.
        let result = check_once(&url, mux_core::VERSION, &cache_path, now, curl_fetch);
        let _ = tx.send(result);
    });
    Some(UpdateCheckState { rx, noticed: false })
}

/// `MTYX_NO_UPDATE_CHECK=1` (exact string "1") disables the check.
fn no_check_env() -> bool {
    std::env::var("MTYX_NO_UPDATE_CHECK").ok().as_deref() == Some("1")
}

/// The endpoint to query: `MTYX_UPDATE_CHECK_URL` overrides (tests and
/// self-hosted mirrors), else the GitHub releases API for this repo.
fn url_from_env() -> String {
    std::env::var("MTYX_UPDATE_CHECK_URL").unwrap_or_else(|_| DEFAULT_URL.to_string())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse `major.minor.patch` from a version string. One optional leading
/// `v`; everything from the first `-` on is a dev suffix and ignored
/// (`-14-g<sha>`, `-dirty`, or both combined). Anything that is not
/// exactly three numeric components → `None`.
fn parse_triple(version: &str) -> Option<(u64, u64, u64)> {
    let body = version.strip_prefix('v').unwrap_or(version);
    let base = body.split('-').next().unwrap_or("");
    let mut parts = base.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Show a banner only when the release tag is a *strictly* newer semver
/// triple. An equal triple is never newer — a dev build of the current
/// release (`0.25.1-14-g<sha>` vs `v0.25.1`) must not nag — and an
/// unparseable version on either side fails closed (no banner).
fn is_newer_release(current: &str, release_tag: &str) -> bool {
    match (parse_triple(current), parse_triple(release_tag)) {
        (Some(cur), Some(rel)) => rel > cur,
        _ => false,
    }
}

/// `None` (never checked) or a timestamp older than `interval` → check
/// again. A *future* timestamp counts as fresh (clock skew / changed
/// system clock), saturating to "just checked", not "check now".
fn should_check(last_check: Option<u64>, now: u64, interval: u64) -> bool {
    match last_check {
        None => true,
        Some(t) => now.saturating_sub(t) >= interval,
    }
}

/// On-disk throttle state next to the config file. Unknown/missing
/// fields deserialize to `None`, so a cache written by a newer mtyx is
/// still readable rather than "corrupt".
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Cache {
    #[serde(default)]
    last_check: Option<u64>,
    #[serde(default)]
    latest_seen: Option<String>,
}

/// Missing, corrupt, or foreign-schema cache → `None` (never panics);
/// `None` simply means "treat as never checked".
fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Atomic write (temp file in the same dir, then rename), the persist.rs
/// snapshot discipline — a crash mid-write must not corrupt the previous
/// cache. Unlike snapshots this file holds nothing sensitive, so the
/// config dir is left at its existing mode. Callers ignore errors: a
/// read-only HOME just means an unthrottled (but still silent) check.
fn write_cache(path: &Path, cache: &Cache) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(cache)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// `tag_name` out of a GitHub `/releases/latest` response body.
fn parse_tag_name(json_body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(json_body)
        .ok()?
        .get("tag_name")?
        .as_str()
        .map(str::to_string)
}

/// Prepend `v` when the tag lacks one, then render the exact banner.
fn banner_text(tag: &str) -> String {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    format!("mtyx v{tag} available — github.com/mathewclarkau/mattyx/releases/latest")
}

/// The checkable core, parameterised over the fetch so tests inject
/// fixtures instead of the network. Every failure is a silent `None`.
///
/// The throttle timestamp is written *before* the fetch, at check time,
/// not at success time: a failing fetch (offline, GitHub down, rate
/// limited) must not re-request on every TUI start within the window —
/// otherwise a machine that boots mtyx hourly burns through GitHub's
/// 60 req/h unauthenticated limit in an hour while showing nothing.
fn check_once(
    url: &str,
    current: &str,
    cache_path: &Path,
    now: u64,
    fetch: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let cache = read_cache(cache_path);
    if !should_check(cache.as_ref().and_then(|c| c.last_check), now, THROTTLE_SECS) {
        return None;
    }
    // Throttle immediately, carrying over the last seen tag; write errors
    // are silent (read-only HOME degrades to an unthrottled check).
    let mut carried = Cache { last_check: Some(now), latest_seen: None };
    if let Some(old) = cache.as_ref() {
        carried.latest_seen = old.latest_seen.clone();
    }
    let _ = write_cache(cache_path, &carried);
    let body = fetch(url)?;
    let tag = parse_tag_name(&body)?;
    let _ = write_cache(
        cache_path,
        &Cache { last_check: Some(now), latest_seen: Some(tag.clone()) },
    );
    if is_newer_release(current, &tag) {
        Some(banner_text(&tag))
    } else {
        None
    }
}

/// Fetch `url` with curl. Argv array only — no shell anywhere between us
/// and the URL. `output()` waits for and reaps the child; a missing curl
/// or any non-success exit is `None` (silent skip, the same degradation
/// class as any fetch failure). Runs off the render loop (see
/// `spawn_pending`) and never touches the TUI's stdout.
fn curl_fetch(url: &str) -> Option<String> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "--fail", "--max-time", "3", "--connect-timeout", "2", "--location"])
        .arg("-H")
        .arg(format!("User-Agent: mtyx/{}", mux_core::VERSION))
        .arg("-H")
        .arg("Accept: application/vnd.github+json")
        .arg(url)
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: a console window must not flash on the TUI's
        // startup path (session/daemon.rs precedent).
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;

    /// Unique-per-test temp dir under the system temp dir (no tempfile
    /// dependency), removed on drop so repeated runs don't accumulate.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(suffix: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("mtyx-update-check-test-{}-{suffix}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn cache_path(&self) -> PathBuf {
            self.0.join("update-check.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const FIXTURE_LATEST: &str = r##"{"tag_name":"v0.26.0","html_url":"https://github.com/mathewclarkau/mattyx/releases/tag/v0.26.0"}"##;

    #[test]
    fn parse_triple_accepts_release_and_dev_forms() {
        for v in ["0.25.1", "v0.25.1", "0.25.1-14-gabc1234", "0.25.1-dirty", "0.25.1-14-gabc1234-dirty"] {
            assert_eq!(parse_triple(v), Some((0, 25, 1)), "{v}");
        }
    }

    #[test]
    fn parse_triple_rejects_non_triples() {
        for v in ["", "abc", "0.25", "0.25.1.2", "0.25.x", "v"] {
            assert_eq!(parse_triple(v), None, "{v}");
        }
    }

    #[test]
    fn is_newer_release_requires_strictly_greater_triple() {
        assert!(is_newer_release("0.25.1", "v0.26.0"));
        assert!(is_newer_release("0.25.1", "0.26.0"));
        assert!(!is_newer_release("0.25.1", "v0.25.1"));
        // Dev build of the current release is not nagged (issue #132 AC).
        assert!(!is_newer_release("0.25.1-14-gabc1234", "v0.25.1"));
        assert!(!is_newer_release("0.25.1-dirty", "v0.25.1"));
        assert!(!is_newer_release("0.25.1", "0.24.9"));
        assert!(!is_newer_release("garbage", "v0.26.0"));
        assert!(!is_newer_release("0.25.1", "not-a-tag"));
    }

    #[test]
    fn should_check_throttles_within_interval() {
        let interval = THROTTLE_SECS;
        let now = 1_000_000u64;
        assert!(should_check(None, now, interval));
        assert!(!should_check(Some(now - 100), now, interval));
        assert!(should_check(Some(now - 25 * 60 * 60), now, interval));
        // Future timestamp (clock skew) counts as fresh, not overdue.
        assert!(!should_check(Some(now + 1000), now, interval));
    }

    #[test]
    fn cache_roundtrip_and_corruption() {
        let dir = TempDir::new("cache-roundtrip");
        let path = dir.cache_path();
        assert!(read_cache(&path).is_none(), "missing file");
        write_cache(&path, &Cache { last_check: Some(42), latest_seen: Some("v9.9.9".into()) })
            .expect("write cache");
        let read = read_cache(&path).expect("read cache");
        assert_eq!(read.last_check, Some(42));
        assert_eq!(read.latest_seen.as_deref(), Some("v9.9.9"));
        fs::write(&path, "{not json").expect("corrupt cache");
        assert!(read_cache(&path).is_none(), "corrupt file");
    }

    #[test]
    fn parse_tag_name_from_github_body() {
        assert_eq!(
            parse_tag_name(FIXTURE_LATEST).as_deref(),
            Some("v0.26.0")
        );
        assert_eq!(parse_tag_name(r##"{"html_url":"x"}"##), None);
        assert_eq!(parse_tag_name("not json"), None);
    }

    #[test]
    fn banner_text_is_exact() {
        let expected = "mtyx v0.26.0 available — github.com/mathewclarkau/mattyx/releases/latest";
        assert_eq!(banner_text("v0.26.0"), expected);
        assert_eq!(banner_text("0.26.0"), expected);
    }

    #[test]
    fn check_once_fetches_and_persists_throttle() {
        let dir = TempDir::new("check-once-success");
        let path = dir.cache_path();
        let now = 1_000_000u64;
        let calls = Cell::new(0u32);
        let fetch = |_: &str| -> Option<String> {
            calls.set(calls.get() + 1);
            Some(FIXTURE_LATEST.to_string())
        };
        assert_eq!(
            check_once("https://example.test/latest", "0.25.1", &path, now, fetch),
            Some("mtyx v0.26.0 available — github.com/mathewclarkau/mattyx/releases/latest".to_string())
        );
        assert_eq!(calls.get(), 1);
        let cache = read_cache(&path).expect("cache written");
        assert_eq!(cache.last_check, Some(now));
        assert_eq!(cache.latest_seen.as_deref(), Some("v0.26.0"));
        // Immediate second start within the window: no second HTTP
        // request (issue #132 AC: at most one per 24h).
        assert_eq!(check_once("https://example.test/latest", "0.25.1", &path, now, fetch), None);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn check_once_failure_still_writes_throttle() {
        let dir = TempDir::new("check-once-failure");
        let path = dir.cache_path();
        let now = 1_000_000u64;
        let calls = Cell::new(0u32);
        let fetch = |_: &str| -> Option<String> {
            calls.set(calls.get() + 1);
            None // offline / curl missing / non-200
        };
        assert_eq!(check_once("https://example.test/latest", "0.25.1", &path, now, fetch), None);
        assert_eq!(calls.get(), 1);
        // The timestamp was written at check time, not success time, so
        // the retry is throttled rather than hammering a dead endpoint.
        assert_eq!(read_cache(&path).expect("throttle written").last_check, Some(now));
        assert_eq!(check_once("https://example.test/latest", "0.25.1", &path, now, fetch), None);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn check_once_treats_corrupt_cache_as_stale() {
        let dir = TempDir::new("check-once-corrupt");
        let path = dir.cache_path();
        fs::write(&path, "garbage{").expect("corrupt cache");
        let now = 1_000_000u64;
        let calls = Cell::new(0u32);
        let fetch = |_: &str| -> Option<String> {
            calls.set(calls.get() + 1);
            Some(FIXTURE_LATEST.to_string())
        };
        assert!(check_once("https://example.test/latest", "0.25.1", &path, now, fetch).is_some());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn check_once_equal_version_is_silent() {
        let dir = TempDir::new("check-once-equal");
        let path = dir.cache_path();
        let now = 1_000_000u64;
        let fetch = |_: &str| -> Option<String> {
            Some(r##"{"tag_name":"v0.25.1"}"##.to_string())
        };
        assert_eq!(check_once("https://example.test/latest", "0.25.1", &path, now, fetch), None);
    }

    /// Env vars are process-global and tests run in parallel threads, so
    /// ALL env mutation lives in this one test (save + restore).
    #[test]
    fn spawn_pending_honors_env() {
        let saved_no_check = std::env::var("MTYX_NO_UPDATE_CHECK").ok();
        let saved_url = std::env::var("MTYX_UPDATE_CHECK_URL").ok();
        std::env::set_var("MTYX_NO_UPDATE_CHECK", "1");
        assert!(spawn_pending().is_none(), "MTYX_NO_UPDATE_CHECK=1 must short-circuit");
        std::env::remove_var("MTYX_NO_UPDATE_CHECK");

        std::env::remove_var("MTYX_UPDATE_CHECK_URL");
        assert_eq!(url_from_env(), DEFAULT_URL);
        std::env::set_var("MTYX_UPDATE_CHECK_URL", "https://mirror.example.test/latest");
        assert_eq!(url_from_env(), "https://mirror.example.test/latest");

        // Restore.
        std::env::remove_var("MTYX_UPDATE_CHECK_URL");
        std::env::remove_var("MTYX_NO_UPDATE_CHECK");
        if let Some(v) = saved_no_check {
            std::env::set_var("MTYX_NO_UPDATE_CHECK", v);
        }
        if let Some(v) = saved_url {
            std::env::set_var("MTYX_UPDATE_CHECK_URL", v);
        }
    }
}
