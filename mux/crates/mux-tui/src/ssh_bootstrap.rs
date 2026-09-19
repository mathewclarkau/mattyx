//! `mtyx ssh <host>` — creates a workspace backed by a `cmuxd-remote`
//! session over SSH (see `mux_core::remote_pty`).
//!
//! This is where the Go-toolchain-and-repo-layout knowledge lives, kept
//! out of `mux-core` on purpose: cross-compile `daemon/remote/` for the
//! remote's OS/arch, cache the result locally, then ask the *running*
//! `mtyx` session (over the control socket, like every other verb) to
//! open a remote workspace with that binary. Uploading it to the remote
//! host and speaking `cmuxd-remote`'s wire protocol both happen
//! server-side in `mux_core::remote_pty` — this module only gets a local
//! binary path onto disk and makes one socket call.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use mux_core::platform::transport;
use serde_json::{json, Value};

const USAGE: &str =
    "usage: mtyx ssh <host> [--name <workspace-name>] [--session <mux-session>] [--socket <path>]";

pub fn run(args: &[String]) -> i32 {
    let mut host = None;
    let mut name = None;
    let mut session = None;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                name = args.get(i + 1).cloned();
                i += 2;
            }
            "--session" => {
                session = args.get(i + 1).cloned();
                i += 2;
            }
            "--socket" => {
                socket = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "-h" | "--help" => {
                eprintln!("mtyx: {USAGE}");
                return 0;
            }
            other if host.is_none() && !other.starts_with("--") => {
                host = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("mtyx: unknown argument {other:?}\n{USAGE}");
                return 2;
            }
        }
    }
    let Some(host) = host else {
        eprintln!("mtyx: {USAGE}");
        return 2;
    };

    match connect(&host, name, session.as_deref(), socket.as_deref()) {
        Ok(surface_id) => {
            println!("{surface_id}");
            0
        }
        Err(e) => {
            eprintln!("mtyx: {e}");
            1
        }
    }
}

fn connect(
    host: &str,
    name: Option<String>,
    session: Option<&str>,
    socket: Option<&Path>,
) -> anyhow::Result<u64> {
    let local_binary_path = ensure_remote_binary(host)?;
    let mut params = json!({
        "host": host,
        "slot": "mtyx",
        "session_id": mux_core::remote_pty::generate_session_id(),
        "local_binary_path": local_binary_path.to_string_lossy(),
    });
    if let Some(name) = name {
        params["name"] = json!(name);
    }
    let socket_path = resolve_socket(socket, session);
    let data = send_request(&socket_path, "new-remote-workspace", params)?;
    data.get("surface")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("new-remote-workspace response had no surface id"))
}

// ---------- build/cache the daemon binary ----------

fn ensure_remote_binary(host: &str) -> anyhow::Result<PathBuf> {
    let (os, arch) = detect_remote_platform(host)?;
    let cache_path = cache_dir()?.join(cache_binary_name(&os, &arch));
    let version = crate::VERSION;
    if cache_is_fresh(&cache_path, &stamp_path_for(&cache_path), version) {
        return Ok(cache_path);
    }
    // Missing binary, or one built by a different mtyx: (re)build and
    // stamp it. Issue #90: the daemon is stamped with mtyx's version via
    // -ldflags (below), so a cache entry from an older/newer mtyx must
    // not be reused — it would run a daemon whose wire protocol/config
    // expectations may not match the mtyx talking to it.
    eprintln!("mtyx: building cmuxd-remote for {os}/{arch} (version {version})...");
    build_mtyxd_remote(&os, &arch, &cache_path)?;
    write_stamp(&stamp_path_for(&cache_path), version)?;
    Ok(cache_path)
}

/// Cache filename for a cross-compiled daemon. Kept as a function so the
/// naming scheme has a single home (and a unit test).
fn cache_binary_name(os: &str, arch: &str) -> String {
    format!("cmuxd-remote-{os}-{arch}")
}

/// The version stamp sibling of a cached daemon binary, e.g.
/// `cmuxd-remote-linux-amd64.version`. A sidecar file rather than a
/// version-suffixed binary name so a stale entry is overwritten in place
/// instead of accumulating one never-reused binary per mtyx release.
fn stamp_path_for(binary: &Path) -> PathBuf {
    let mut name = binary.file_name().unwrap_or_default().to_os_string();
    name.push(".version");
    binary.with_file_name(name)
}

/// A cached daemon is fresh only if the binary exists *and* its sidecar
/// stamp names the version of the running mtyx. A missing stamp (cache
/// written by a pre-#90 mtyx) counts as stale, so the first run after an
/// upgrade rebuilds rather than trusting an unstamped binary.
fn cache_is_fresh(binary: &Path, stamp: &Path, version: &str) -> bool {
    if !binary.exists() {
        return false;
    }
    match std::fs::read_to_string(stamp) {
        Ok(stamped) => stamped.trim() == version,
        Err(_) => false,
    }
}

fn write_stamp(stamp: &Path, version: &str) -> anyhow::Result<()> {
    if let Some(dir) = stamp.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(stamp, format!("{version}\n"))?;
    Ok(())
}

fn detect_remote_platform(host: &str) -> anyhow::Result<(String, String)> {
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes"])
        .arg(host)
        .arg("uname -s; uname -m")
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "couldn't reach {host} over ssh to detect its platform: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let os = match lines.next().unwrap_or_default().trim() {
        "Linux" => "linux",
        "Darwin" => "darwin",
        other => anyhow::bail!("unsupported remote OS {other:?}"),
    };
    let arch = match lines.next().unwrap_or_default().trim() {
        "x86_64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        other => anyhow::bail!("unsupported remote architecture {other:?}"),
    };
    Ok((os.to_string(), arch.to_string()))
}

fn build_mtyxd_remote(os: &str, arch: &str, out: &Path) -> anyhow::Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let source_dir = daemon_remote_source_dir();
    if !source_dir.join("go.mod").exists() {
        anyhow::bail!(
            "vendored daemon source not found at {} (expected daemon/remote/go.mod)",
            source_dir.display()
        );
    }
    let status = Command::new("go")
        .current_dir(&source_dir)
        .env("GOOS", os)
        .env("GOARCH", arch)
        .env("CGO_ENABLED", "0")
        .arg("build")
        // Issue #71: stamp the daemon with the version of the mtyx that
        // built it. `main.go` declares `var version = "dev"` purely as
        // the fallback for a bare `go build`; nothing else ever set it,
        // so `cmuxd-remote version` reported "dev" on every host. The
        // daemon is vendored in this repo and built from this checkout,
        // so mtyx's own version is its correct identity.
        .arg(format!("-ldflags=-X main.version={}", crate::VERSION))
        .arg("-o")
        .arg(out)
        .arg("./cmd/cmuxd-remote")
        .status()
        .map_err(|e| {
            anyhow::anyhow!("failed to run `go build` (is Go installed and on PATH?): {e}")
        })?;
    if !status.success() {
        anyhow::bail!("go build failed: {status}");
    }
    Ok(())
}

/// `mux-tui`'s crate lives at `<repo>/mux/crates/mux-tui`; the vendored
/// Go daemon lives at `<repo>/daemon/remote`. `CARGO_MANIFEST_DIR` is
/// baked in at compile time, matching this project's build-in-place,
/// symlink-the-binary-onto-PATH deployment (see `scripts/bootstrap.sh`) —
/// not meant to resolve correctly for a binary copied somewhere else.
fn daemon_remote_source_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../daemon/remote")
}

fn cache_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| mux_core::platform::home_dir().map(|h| h.join(".cache")))
        .ok_or_else(|| anyhow::anyhow!("could not resolve a cache directory ($HOME unset?)"))?;
    Ok(base.join("mattyx"))
}

// ---------- socket client ----------

fn resolve_socket(explicit: Option<&Path>, session: Option<&str>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = std::env::var_os("MTYX_MUX_SOCKET") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    // Rename compat: fall back to a LIVE cmux-era socket when the
    // canonical mtyx one is not up (probe only; never creates).
    mux_core::server::client_socket_path(session.unwrap_or("main"))
}

fn send_request(socket_path: &Path, cmd: &str, mut params: Value) -> anyhow::Result<Value> {
    let stream = transport::connect(socket_path).map_err(|e| {
        anyhow::anyhow!("cannot connect to session socket {}: {e}", socket_path.display())
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    params["cmd"] = json!(cmd);
    params["id"] = json!(1);
    let mut line = serde_json::to_vec(&params)?;
    line.push(b'\n');
    let mut writer = stream.try_clone_box()?;
    writer.write_all(&line)?;
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;
    let response: Value = serde_json::from_str(&response_line)?;
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(response.get("data").cloned().unwrap_or_else(|| json!({})))
    } else {
        let message = response.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        anyhow::bail!("{message}")
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal unique scratch dir; no tempfile dependency in this crate.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("mtyx-ssh-bootstrap-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn cache_binary_name_is_platform_scoped() {
        assert_eq!(cache_binary_name("linux", "amd64"), "cmuxd-remote-linux-amd64");
        assert_eq!(cache_binary_name("darwin", "arm64"), "cmuxd-remote-darwin-arm64");
    }

    #[test]
    fn stamp_path_is_a_sibling_of_the_binary() {
        let binary = Path::new("/tmp/cache/cmuxd-remote-linux-amd64");
        assert_eq!(
            stamp_path_for(binary),
            PathBuf::from("/tmp/cache/cmuxd-remote-linux-amd64.version")
        );
    }

    #[test]
    fn missing_binary_is_stale() {
        let dir = scratch("missing-binary");
        let binary = dir.join(cache_binary_name("linux", "amd64"));
        // Stamp alone must not be enough: the binary is what gets executed.
        write_stamp(&stamp_path_for(&binary), "0.17.2").unwrap();
        assert!(!cache_is_fresh(&binary, &stamp_path_for(&binary), "0.17.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unstamped_binary_is_stale() {
        // Pre-#90 caches shipped a binary with no sidecar; they must be
        // rebuilt, not trusted.
        let dir = scratch("unstamped");
        let binary = dir.join(cache_binary_name("darwin", "arm64"));
        std::fs::write(&binary, b"\x7fELF-fake").unwrap();
        assert!(!cache_is_fresh(&binary, &stamp_path_for(&binary), "0.17.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_version_cache_is_not_fresh_and_rebuild_refreshes_it() {
        let dir = scratch("stale-version");
        let binary = dir.join(cache_binary_name("linux", "amd64"));
        let stamp = stamp_path_for(&binary);
        std::fs::write(&binary, b"\x7fELF-fake").unwrap();
        write_stamp(&stamp, "0.17.1").unwrap();

        // Different mtyx built the cached daemon: stale, so
        // ensure_remote_binary takes the rebuild path and re-stamps.
        assert!(!cache_is_fresh(&binary, &stamp, "0.17.2"));
        write_stamp(&stamp, "0.17.2").unwrap();
        assert!(cache_is_fresh(&binary, &stamp, "0.17.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_tolerates_trailing_whitespace() {
        let dir = scratch("whitespace");
        let binary = dir.join(cache_binary_name("linux", "arm64"));
        let stamp = stamp_path_for(&binary);
        std::fs::write(&binary, b"\x7fELF-fake").unwrap();
        // A stamp written with a blank line / CRLF still matches.
        write_stamp(&stamp, "0.17.2\r").unwrap();
        assert!(cache_is_fresh(&binary, &stamp, "0.17.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
