//! `mtyx update` — self-update verb (issue #133).
//!
//! Reuses [`crate::update_check`]'s release check and semver comparator to
//! resolve the latest GitHub release tag, downloads the release binary and its
//! `<asset>.sha256` sidecar for the current `(OS, ARCH)`, verifies the SHA-256
//! digest and the staged binary's `--version` output, and atomically swaps the
//! target binary in place.
//!
//! Note: SSH/remote `mtyxd-remote` binaries on remote hosts are out of scope
//! for `mtyx update` — they re-upload automatically on the next
//! `mtyx ssh <host>` after the local binary upgrades.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::update_check::{
    curl_program, default_fetch_text, is_dev_build, is_newer_release, local_fixture_path,
    parse_release_tag, resolve_release_api_url, RELEASES_LATEST_URL, RELEASES_URL,
};

/// Map `(std::env::consts::OS, std::env::consts::ARCH)` to the release asset
/// filename published by `.github/workflows/release.yml`.
pub fn asset_name_for(os: &str, arch: &str) -> Result<&'static str, String> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("mtyx-linux-x86_64"),
        ("linux", "aarch64") => Ok("mtyx-linux-aarch64"),
        ("macos", "aarch64") => Ok("mtyx-macos-aarch64"),
        ("macos", "x86_64") => Ok("mtyx-macos-x86_64"),
        ("windows", "x86_64") => Ok("mtyx-windows-x86_64.exe"),
        _ => Err(format!(
            "no prebuilt release binary for {os}-{arch}; download or build manually from {RELEASES_URL}"
        )),
    }
}

/// Pure-Rust FIPS 180-4 SHA-256 digest, formatted as a 64-char lowercase hex
/// string. Avoids shelling out to platform-specific checksum utilities
/// (`sha256sum` vs `shasum` vs `certutil`) and adds zero crate dependencies.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 72) & !63);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    format!(
        "{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        state[0], state[1], state[2], state[3], state[4], state[5], state[6], state[7]
    )
}

/// Extract the expected 64-character hex SHA-256 digest from a `.sha256`
/// sidecar file (`<hex>  <basename>`).
pub fn parse_sha256_sidecar(sidecar_text: &str) -> Result<String, String> {
    let line = sidecar_text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| "empty .sha256 sidecar".to_string())?;
    let hex = line
        .split_whitespace()
        .next()
        .ok_or_else(|| "malformed .sha256 sidecar".to_string())?
        .to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid sha256 digest in sidecar: {hex:?}"));
    }
    Ok(hex)
}

/// Verify `bytes` against the expected digest in `sidecar_text`.
pub fn verify_sha256(bytes: &[u8], sidecar_text: &str) -> Result<String, String> {
    let expected = parse_sha256_sidecar(sidecar_text)?;
    let actual = sha256_hex(bytes);
    if actual != expected {
        return Err(format!(
            "sha256 checksum mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(actual)
}

fn permission_denied_message(target: &Path) -> String {
    format!(
        "cannot write to {} (permission denied) — if installed via a package manager (paru/yay), update with that instead, or download from {RELEASES_URL}",
        target.display()
    )
}

fn ensure_not_symlink(path: &Path) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to overwrite symlink {} — if installed from a source checkout (ln -sf), pull and rebuild there, or replace the symlink with a regular binary from {RELEASES_URL}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn ensure_target_writable(target: &Path) -> Result<(), String> {
    ensure_not_symlink(target)?;
    if let Ok(meta) = std::fs::symlink_metadata(target) {
        if meta.permissions().readonly() {
            return Err(permission_denied_message(target));
        }
    }
    let Some(parent) = target.parent() else {
        return Err(format!("target path {} has no parent directory", target.display()));
    };
    match std::fs::metadata(parent) {
        Ok(dir_meta) if dir_meta.permissions().readonly() => {
            return Err(permission_denied_message(target));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(permission_denied_message(target));
        }
        Err(e) => {
            return Err(format!(
                "cannot access parent directory {}: {e}",
                parent.display()
            ));
        }
    }
    Ok(())
}

/// Two-step Windows rename dance (`<target>` → `<target>.old`, then
/// `<staged>` → `<target>`), rolling back `<target>.old` if the second rename
/// fails. Kept platform-independent so unit tests on Unix can also exercise
/// the `.old` backup and rollback semantics directly.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn swap_with_old_backup(target: &Path, staged: &Path) -> Result<PathBuf, String> {
    ensure_not_symlink(target)?;
    ensure_not_symlink(staged)?;
    let mut old_os = target.as_os_str().to_os_string();
    old_os.push(".old");
    let old_path = PathBuf::from(old_os);
    ensure_not_symlink(&old_path)?;
    if old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }
    let target_existed = target.exists();
    if target_existed {
        std::fs::rename(target, &old_path).map_err(|e| {
            let _ = std::fs::remove_file(staged);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                permission_denied_message(target)
            } else {
                format!(
                    "failed to move {} to {}: {e}",
                    target.display(),
                    old_path.display()
                )
            }
        })?;
    }
    if let Err(e) = std::fs::rename(staged, target) {
        if target_existed {
            let _ = std::fs::rename(&old_path, target);
        }
        let _ = std::fs::remove_file(staged);
        return Err(if e.kind() == std::io::ErrorKind::PermissionDenied {
            permission_denied_message(target)
        } else {
            format!(
                "failed to move staged binary {} to {}: {e}",
                staged.display(),
                target.display()
            )
        });
    }
    Ok(old_path)
}

/// Best-effort removal of a stale `<target>.old` (`mtyx.exe.old`) left behind
/// by a previous Windows self-update.
pub fn cleanup_stale_old_binary(target: &Path) {
    let mut old_os = target.as_os_str().to_os_string();
    old_os.push(".old");
    let old_path = PathBuf::from(old_os);
    if let Ok(meta) = std::fs::symlink_metadata(&old_path) {
        if !meta.file_type().is_symlink() {
            let _ = std::fs::remove_file(&old_path);
        }
    }
}

/// Atomically replace `target` with `staged` (both must reside on the same
/// filesystem, typically `target.parent()`).
///
/// - Refuses if `target` (or `staged`) is a symlink.
/// - Refuses if `target` or its parent directory is read-only / permission-denied.
/// - On Unix: sets mode `0o755` on `staged` and `rename(staged, target)`.
/// - On Windows: renames `target` to `<target>.old`, then `staged` to `target`.
pub fn apply_update(target: &Path, staged: &Path) -> Result<(), String> {
    if let Err(e) = ensure_not_symlink(target) {
        let _ = std::fs::remove_file(staged);
        return Err(e);
    }
    if let Err(e) = ensure_not_symlink(staged) {
        return Err(e);
    }
    if let Err(e) = ensure_target_writable(target) {
        let _ = std::fs::remove_file(staged);
        return Err(e);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        if let Err(e) = std::fs::set_permissions(staged, perms) {
            let _ = std::fs::remove_file(staged);
            return Err(format!(
                "failed to chmod 0755 staged binary {}: {e}",
                staged.display()
            ));
        }
        if let Err(e) = std::fs::rename(staged, target) {
            let _ = std::fs::remove_file(staged);
            return Err(if e.kind() == std::io::ErrorKind::PermissionDenied {
                permission_denied_message(target)
            } else {
                format!(
                    "failed to replace {} with {}: {e}",
                    target.display(),
                    staged.display()
                )
            });
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        swap_with_old_backup(target, staged).map(|_| ())
    }
}

/// Run `<staged> --version` and require its output to equal
/// `mtyx <expected_version_without_v>`.
pub fn verify_staged_version(
    staged: &Path,
    expected_version_without_v: &str,
) -> Result<(), String> {
    let child = Command::new(staged)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run staged binary {} --version: {e}", staged.display()))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed waiting on staged binary --version: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "staged binary {} --version exited with {}",
            staged.display(),
            output.status
        ));
    }
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let expected = format!("mtyx {expected_version_without_v}");
    if reported != expected {
        return Err(format!(
            "staged binary version mismatch: expected {expected:?}, got {reported:?}"
        ));
    }
    Ok(())
}

/// Resolve the download base URL for `v_tag` (`MTYX_UPDATE_DOWNLOAD_BASE`
/// override or `https://github.com/mathewclarkau/mattyx/releases/download/<v_tag>`).
pub fn resolve_download_base(v_tag: &str) -> String {
    if let Ok(custom) = std::env::var("MTYX_UPDATE_DOWNLOAD_BASE") {
        let trimmed = custom.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    format!("{RELEASES_URL}/download/{v_tag}")
}

/// Fetch binary bytes from `url` (or read directly from disk when `url` is a
/// `file://` URL or local filesystem path).
pub fn default_fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = local_fixture_path(url) {
        return std::fs::read(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()));
    }
    let user_agent = format!("User-Agent: mtyx/{}", mux_core::VERSION);
    let child = Command::new(curl_program())
        .args(["-fsSL", "--max-time", "60", "-H", &user_agent, "--", url])
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
    Ok(output.stdout)
}

/// Stage `asset_bytes` into `<target_dir>/.mtyx-update-<pid>.tmp`, verify its
/// SHA-256 against `sidecar_text`, optionally verify `--version`, and
/// atomically swap it onto `target`.
///
/// Any failure along the way deletes the staged file and leaves `target`
/// byte-for-byte untouched.
pub fn stage_verify_and_apply<W: Write>(
    target: &Path,
    asset_bytes: &[u8],
    sidecar_text: &str,
    expected_version_without_v: Option<&str>,
    out: &mut W,
) -> Result<PathBuf, String> {
    ensure_target_writable(target)?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("target path {} has no parent directory", target.display()))?;
    let staged = parent.join(format!(".mtyx-update-{}.tmp", std::process::id()));
    ensure_not_symlink(&staged)?;

    if let Err(e) = std::fs::write(&staged, asset_bytes) {
        let _ = std::fs::remove_file(&staged);
        return Err(if e.kind() == std::io::ErrorKind::PermissionDenied {
            permission_denied_message(target)
        } else {
            format!("failed to write staged binary {}: {e}", staged.display())
        });
    }

    let staged_bytes = match std::fs::read(&staged) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(format!(
                "failed to read back staged binary {}: {e}",
                staged.display()
            ));
        }
    };

    if let Err(e) = verify_sha256(&staged_bytes, sidecar_text) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    let _ = writeln!(out, "sha256 ok");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        if let Err(e) = std::fs::set_permissions(&staged, perms) {
            let _ = std::fs::remove_file(&staged);
            return Err(format!(
                "failed to chmod 0755 staged binary {}: {e}",
                staged.display()
            ));
        }
    }

    let _ = writeln!(out, "staged → {}", staged.display());

    if let Some(expected_ver) = expected_version_without_v {
        if let Err(e) = verify_staged_version(&staged, expected_ver) {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    }

    apply_update(target, &staged)?;
    Ok(staged)
}

/// Dependency-injected self-update workflow used by `mtyx update` and unit
/// tests.
#[allow(clippy::too_many_arguments)]
pub fn run_update_with<FT, FB, W>(
    running_version: &str,
    os: &str,
    arch: &str,
    target: &Path,
    check_only: bool,
    release_api_url: &str,
    download_base_override: Option<&str>,
    mut fetch_text: FT,
    mut fetch_bytes: FB,
    verify_version_exec: bool,
    out: &mut W,
) -> Result<(), String>
where
    FT: FnMut(&str) -> Result<String, String>,
    FB: FnMut(&str) -> Result<Vec<u8>, String>,
    W: Write,
{
    cleanup_stale_old_binary(target);

    let release_json = fetch_text(release_api_url)?;
    let v_tag = parse_release_tag(&release_json)?;
    let new_ver = v_tag.strip_prefix('v').unwrap_or(&v_tag);
    let newer = is_newer_release(running_version, &v_tag);
    let dev = is_dev_build(running_version);

    if check_only {
        if newer {
            let _ = writeln!(
                out,
                "mtyx update available: {running_version} → {v_tag} ({RELEASES_LATEST_URL})"
            );
        } else if dev {
            let _ = writeln!(
                out,
                "mtyx running dev build {running_version} (latest release: {v_tag} — {RELEASES_LATEST_URL})"
            );
        } else {
            let _ = writeln!(
                out,
                "mtyx is up to date ({running_version}, latest {v_tag} — {RELEASES_LATEST_URL})"
            );
        }
        return Ok(());
    }

    if !newer && !dev {
        let _ = writeln!(out, "mtyx is up to date ({running_version})");
        return Ok(());
    }

    let asset = asset_name_for(os, arch)?;
    ensure_target_writable(target)?;

    if dev && !newer {
        let _ = writeln!(
            out,
            "running dev build {running_version}; replacing with release {v_tag}"
        );
    } else if dev {
        let _ = writeln!(
            out,
            "running dev build {running_version}; replacing with release {v_tag}"
        );
    }

    let _ = writeln!(out, "fetching {v_tag} for {os}-{arch}…");

    let base = match download_base_override.map(|s| s.trim().trim_end_matches('/')) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => resolve_download_base(&v_tag),
    };
    let asset_url = format!("{base}/{asset}");
    let sidecar_url = format!("{base}/{asset}.sha256");

    let sidecar_bytes = fetch_bytes(&sidecar_url)?;
    let sidecar_text = String::from_utf8(sidecar_bytes)
        .map_err(|e| format!("invalid UTF-8 in {sidecar_url}: {e}"))?;
    let asset_bytes = fetch_bytes(&asset_url)?;

    let expected_ver = if verify_version_exec { Some(new_ver) } else { None };
    stage_verify_and_apply(target, &asset_bytes, &sidecar_text, expected_ver, out)?;

    let _ = writeln!(
        out,
        "updated: {running_version} → {new_ver} (restart running sessions to pick it up)"
    );
    Ok(())
}

/// Resolve the path of the binary to update.
///
/// Checks `MTYX_UPDATE_TARGET_BIN` first (test override), then checks whether
/// `argv[0]` resolves to a symlink on disk (because on Linux
/// `std::env::current_exe()` reads `/proc/self/exe`, which kernel-resolves
/// symlinks and would otherwise bypass the symlink guard for a `~/.local/bin/mtyx`
/// symlink pointing into a cargo build directory), and finally falls back to
/// `std::env::current_exe()`.
pub fn resolve_target_binary() -> Result<PathBuf, String> {
    if let Ok(override_path) = std::env::var("MTYX_UPDATE_TARGET_BIN") {
        let trimmed = override_path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    if let Some(arg0) = std::env::args_os().next() {
        let candidate = PathBuf::from(&arg0);
        if candidate.components().count() > 1 {
            if std::fs::symlink_metadata(&candidate)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                return Ok(candidate);
            }
        } else if let Some(path_os) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_os) {
                let full = dir.join(&candidate);
                if let Ok(meta) = std::fs::symlink_metadata(&full) {
                    if meta.file_type().is_symlink() {
                        return Ok(full);
                    }
                    break;
                }
            }
        }
    }
    std::env::current_exe().map_err(|e| format!("failed to resolve current executable path: {e}"))
}

/// CLI entry point for `mtyx update [--check]`.
pub fn run(check_only: bool) -> i32 {
    let target = match resolve_target_binary() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mtyx: {e}");
            return 1;
        }
    };
    let api_url = resolve_release_api_url();
    let base_override = std::env::var("MTYX_UPDATE_DOWNLOAD_BASE").ok();
    let mut stdout = std::io::stdout();
    match run_update_with(
        mux_core::VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        &target,
        check_only,
        &api_url,
        base_override.as_deref(),
        default_fetch_text,
        default_fetch_bytes,
        true,
        &mut stdout,
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mtyx: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mtyx-self-update-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn asset_name_mapping_covers_supported_and_rejects_unsupported() {
        assert_eq!(asset_name_for("linux", "x86_64").unwrap(), "mtyx-linux-x86_64");
        assert_eq!(asset_name_for("linux", "aarch64").unwrap(), "mtyx-linux-aarch64");
        assert_eq!(asset_name_for("macos", "aarch64").unwrap(), "mtyx-macos-aarch64");
        assert_eq!(asset_name_for("macos", "x86_64").unwrap(), "mtyx-macos-x86_64");
        assert_eq!(
            asset_name_for("windows", "x86_64").unwrap(),
            "mtyx-windows-x86_64.exe"
        );

        let intel_mac_legacy = asset_name_for("macos", "i686").unwrap_err();
        assert!(intel_mac_legacy.contains("macos-i686"), "got: {intel_mac_legacy}");
        assert!(intel_mac_legacy.contains(RELEASES_URL), "got: {intel_mac_legacy}");

        let unknown = asset_name_for("freebsd", "riscv64").unwrap_err();
        assert!(unknown.contains("freebsd-riscv64"), "got: {unknown}");
        assert!(unknown.contains(RELEASES_URL), "got: {unknown}");
    }

    #[test]
    fn sha256_mismatch_aborts_deletes_staged_and_leaves_target_untouched() {
        let dir = unique_tmp_dir("mismatch");
        let target = dir.join("mtyx");
        let original_bytes = b"original-mtyx-binary-bytes";
        std::fs::write(&target, original_bytes).unwrap();

        let new_bytes = b"corrupted-download-payload";
        let wrong_sidecar =
            "0000000000000000000000000000000000000000000000000000000000000000  mtyx-linux-x86_64\n";
        let mut out = Vec::new();
        let err = stage_verify_and_apply(&target, new_bytes, wrong_sidecar, None, &mut out)
            .expect_err("sha256 mismatch must fail");
        assert!(err.contains("sha256 checksum mismatch"), "got: {err}");

        // Target binary is byte-for-byte identical.
        assert_eq!(std::fs::read(&target).unwrap(), original_bytes);

        // No staged temp files left behind in dir.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["mtyx".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn happy_path_stage_verify_and_atomic_rename_replaces_target() {
        let dir = unique_tmp_dir("happy");
        let target = dir.join("mtyx");
        std::fs::write(&target, b"old-binary").unwrap();

        let new_bytes = b"#!/bin/sh\necho \"mtyx 0.25.1\"\n";
        let digest = sha256_hex(new_bytes);
        let sidecar = format!("{digest}  mtyx-linux-x86_64\n");
        let mut out = Vec::new();

        run_update_with(
            "0.24.0",
            "linux",
            "x86_64",
            &target,
            false,
            "https://example.invalid/releases/latest",
            Some("https://example.invalid/download"),
            |_| Ok(r#"{"tag_name":"v0.25.1"}"#.to_string()),
            |url| {
                if url.ends_with(".sha256") {
                    Ok(sidecar.as_bytes().to_vec())
                } else {
                    Ok(new_bytes.to_vec())
                }
            },
            cfg!(unix),
            &mut out,
        )
        .expect("happy-path update should succeed");

        assert_eq!(std::fs::read(&target).unwrap(), new_bytes);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("fetching v0.25.1 for linux-x86_64…"), "log: {log}");
        assert!(log.contains("sha256 ok"), "log: {log}");
        assert!(log.contains("staged →"), "log: {log}");
        assert!(
            log.contains("updated: 0.24.0 → 0.25.1 (restart running sessions to pick it up)"),
            "log: {log}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "updated binary must have mode 0755 on Unix");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_is_refused_leaving_symlink_and_referent_untouched() {
        let dir = unique_tmp_dir("symlink");
        let real_bin = dir.join("real-mtyx");
        let link_bin = dir.join("mtyx-link");
        let referent_bytes = b"real-build-tree-binary";
        std::fs::write(&real_bin, referent_bytes).unwrap();
        std::os::unix::fs::symlink(&real_bin, &link_bin).unwrap();

        let staged = dir.join(".mtyx-update-staged.tmp");
        std::fs::write(&staged, b"new-release-binary").unwrap();

        let err = apply_update(&link_bin, &staged).expect_err("symlink target must be refused");
        assert!(err.contains("symlink"), "got: {err}");

        // Symlink still a symlink, and referent is untouched.
        assert!(
            std::fs::symlink_metadata(&link_bin)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&real_bin).unwrap(), referent_bytes);
        assert!(!staged.exists(), "staged temp file must be cleaned up");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readonly_target_exits_with_manual_install_url_and_leaves_no_temp_files() {
        let dir = unique_tmp_dir("readonly");
        let target = dir.join("mtyx");
        std::fs::write(&target, b"package-managed-binary").unwrap();

        // Mark target read-only (simulating a package-manager-owned binary).
        let mut perms = std::fs::metadata(&target).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&target, perms).unwrap();

        let new_bytes = b"new-binary";
        let sidecar = format!("{}  mtyx-linux-x86_64\n", sha256_hex(new_bytes));
        let mut out = Vec::new();
        let err = stage_verify_and_apply(&target, new_bytes, &sidecar, None, &mut out)
            .expect_err("read-only target must fail");
        assert!(
            err.contains(RELEASES_URL),
            "error must name manual install URL: {err}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"package-managed-binary");

        // Ensure no temp files remain.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["mtyx".to_string()]);

        // Restore write permission so remove_dir_all succeeds on Windows too.
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut p = std::fs::metadata(&target).unwrap().permissions();
            p.set_readonly(false);
            let _ = std::fs::set_permissions(&target, p);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn already_up_to_date_exits_0_without_hitting_download_url() {
        let dir = unique_tmp_dir("uptodate");
        let target = dir.join("mtyx");
        std::fs::write(&target, b"current-binary").unwrap();

        let download_calls = Cell::new(0usize);
        let mut out = Vec::new();
        run_update_with(
            "0.25.1",
            "linux",
            "x86_64",
            &target,
            false,
            "https://example.invalid/releases/latest",
            Some("https://example.invalid/download"),
            |_| Ok(r#"{"tag_name":"v0.25.1"}"#.to_string()),
            |_| {
                download_calls.set(download_calls.get() + 1);
                Err("download must not be called when already up to date".to_string())
            },
            false,
            &mut out,
        )
        .expect("already-up-to-date must succeed");

        assert_eq!(download_calls.get(), 0, "download URL must not be hit");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.trim(), "mtyx is up to date (0.25.1)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_two_step_old_backup_swap_and_cleanup_work() {
        let dir = unique_tmp_dir("win-swap");
        let target = dir.join("mtyx.exe");
        let staged = dir.join(".mtyx-update-1.tmp");
        std::fs::write(&target, b"old-exe").unwrap();
        std::fs::write(&staged, b"new-exe").unwrap();

        let old_path = swap_with_old_backup(&target, &staged).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-exe");
        assert_eq!(std::fs::read(&old_path).unwrap(), b"old-exe");

        cleanup_stale_old_binary(&target);
        assert!(!old_path.exists(), "stale .old file should be removed");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
