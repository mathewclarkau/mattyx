//! Platform decisions for mattyx.

use std::path::{Path, PathBuf};

pub mod transport {
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::time::Duration;

    pub trait Stream: Read + Write + Send {
        fn try_clone_box(&self) -> io::Result<Box<dyn Stream>>;
        fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;

        /// Issue #86: the uid of the process at the other end of an
        /// accepted local stream, when the transport can report one.
        ///
        /// Unix: `getsockopt(SO_PEERCRED)` on the underlying fd. This is
        /// a *kernel-attested* credential — it cannot be spoofed by the
        /// client — which is why it is the right basis for a default-deny
        /// peer-auth check on the daemon control socket.
        ///
        /// The default is an `Unsupported` error, so a transport that
        /// has no peer-credential surface (the Windows `uds_windows`
        /// shim, or any future non-unix transport) is forced to declare
        /// itself explicitly rather than silently reporting uid 0. Callers
        /// MUST treat an `Err` as "cannot authenticate" (see
        /// [`is_peer_cred_unsupported`]) and never as a match.
        fn peer_uid(&self) -> io::Result<u32> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "peer credentials unsupported"))
        }
    }

    /// Issue #86: is this `peer_uid()` error the "transport has no
    /// peer-credential surface" case (Windows/unsupported) rather than a
    /// genuine lookup failure on a transport that *does* support creds?
    ///
    /// The distinction drives the enforcement policy in `server.rs`: an
    /// unsupported transport falls back to the filesystem-permissions
    /// boundary with a one-time warning, while a lookup error on a
    /// credential-capable transport is a hard reject.
    pub fn is_peer_cred_unsupported(err: &io::Error) -> bool {
        err.kind() == io::ErrorKind::Unsupported
    }

    pub struct Listener {
        inner: imp::Listener,
    }

    pub fn listen(path: &Path) -> io::Result<Listener> {
        imp::listen(path).map(|inner| Listener { inner })
    }

    pub fn connect(path: &Path) -> io::Result<Box<dyn Stream>> {
        imp::connect(path)
    }

    impl Listener {
        pub fn accept(&self) -> io::Result<Box<dyn Stream>> {
            self.inner.accept()
        }
    }

    #[cfg(unix)]
    mod imp {
        use std::io;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::Path;
        use std::time::Duration;

        use super::Stream;

        pub(super) struct Listener {
            inner: UnixListener,
        }

        pub(super) fn listen(path: &Path) -> io::Result<Listener> {
            UnixListener::bind(path).map(|inner| Listener { inner })
        }

        pub(super) fn connect(path: &Path) -> io::Result<Box<dyn Stream>> {
            Ok(Box::new(UnixStream::connect(path)?))
        }

        impl Listener {
            pub(super) fn accept(&self) -> io::Result<Box<dyn Stream>> {
                let (stream, _) = self.inner.accept()?;
                Ok(Box::new(stream))
            }
        }

        impl Stream for UnixStream {
            fn try_clone_box(&self) -> io::Result<Box<dyn Stream>> {
                Ok(Box::new(self.try_clone()?))
            }

            fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                UnixStream::set_read_timeout(self, timeout)
            }

            /// Issue #86: `getsockopt(SOL_SOCKET, SO_PEERCRED)` returns a
            /// `struct ucred` describing the peer at connect(2) time.
            /// Linux fills `pid`/`uid`/`gid`; `uid` is what we authenticate
            /// on. A lookup error is returned as-is (never coerced to 0) so
            /// the caller rejects rather than accepts.
            ///
            /// SO_PEERCRED is Linux-specific (other unix targets use
            /// `LOCAL_PEERCRED`/`getpeereid`), so this is gated to Linux;
            /// other unix hosts get the trait default (unsupported) and the
            /// documented filesystem-permissions fallback.
            #[cfg(target_os = "linux")]
            fn peer_uid(&self) -> io::Result<u32> {
                use std::os::unix::io::AsRawFd;

                let fd = self.as_raw_fd();
                let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                let rc = unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_PEERCRED,
                        &mut cred as *mut libc::ucred as *mut libc::c_void,
                        &mut len,
                    )
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(cred.uid)
            }

            #[cfg(not(target_os = "linux"))]
            fn peer_uid(&self) -> io::Result<u32> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SO_PEERCRED peer auth unavailable on this unix target",
                ))
            }
        }
    }

    #[cfg(windows)]
    mod imp {
        use std::io;
        use std::path::Path;
        use std::time::Duration;

        use super::Stream;
        use uds_windows::{UnixListener, UnixStream};

        pub(super) struct Listener {
            inner: UnixListener,
        }

        pub(super) fn listen(path: &Path) -> io::Result<Listener> {
            UnixListener::bind(path).map(|inner| Listener { inner })
        }

        pub(super) fn connect(path: &Path) -> io::Result<Box<dyn Stream>> {
            Ok(Box::new(UnixStream::connect(path)?))
        }

        impl Listener {
            pub(super) fn accept(&self) -> io::Result<Box<dyn Stream>> {
                let (stream, _) = self.inner.accept()?;
                Ok(Box::new(stream))
            }
        }

        impl Stream for UnixStream {
            fn try_clone_box(&self) -> io::Result<Box<dyn Stream>> {
                Ok(Box::new(self.try_clone()?))
            }

            fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                UnixStream::set_read_timeout(self, timeout)
            }

            /// Issue #86: DOCUMENTED GAP. The `uds_windows` shim wraps a
            /// named pipe, which has no `SO_PEERCRED` equivalent — the
            /// peer's uid is not knowable here. We deliberately do NOT
            /// attempt a named-pipe impersonation/ACL rewrite in this
            /// change. Returning `Unsupported` keeps the trait semantics
            /// and makes `server.rs` fall back to the
            /// filesystem-permissions boundary (the runtime dir is
            /// per-user) with a one-time log line. A real fix would query
            /// the pipe's client SID via `GetNamedPipeClientProcessId` +
            /// token lookup — a separate, larger change.
            fn peer_uid(&self) -> io::Result<u32> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "peer credentials unsupported on Windows named pipes",
                ))
            }
        }
    }
}

/// Runtime socket/pidfile directory for the current user.
///
/// Canonical since the mattyx rename. Servers built from this tree
/// bind here only; clients additionally probe [`legacy_runtime_dir`]
/// via [`pick_runtime_socket`] before giving up on a connect.
pub fn runtime_dir() -> PathBuf {
    runtime_base_dir().join(format!("mtyx-{}", user_id_component()))
}

/// cmux-era runtime socket/pidfile directory (`cmux-<uid>`).
///
/// Never bound by this build's server. Exists purely so a client can
/// fall back to a LIVE legacy socket (probe only, never create) while
/// a pre-rename `cmux` server is still running — the intended
/// transition story is "talk to the old daemon until you restart it
/// under the new name", not a destructive migration.
pub fn legacy_runtime_dir() -> PathBuf {
    runtime_base_dir().join(format!("cmux-{}", user_id_component()))
}

/// Pure client-side socket decision (rename compat): prefer the
/// canonical `mtyx-<uid>` socket; fall back to the legacy `cmux-<uid>`
/// socket only when the canonical one is not live and the legacy one
/// is. When neither is live the canonical path is returned so the
/// resulting connect error names the canonical location.
///
/// `canonical_live` / `legacy_live` are supplied by the caller
/// (connect-probe results, see `server::client_socket_path`) so this
/// decision is unit-testable without touching the filesystem.
pub fn pick_runtime_socket(
    canonical: PathBuf,
    legacy: PathBuf,
    canonical_live: bool,
    legacy_live: bool,
) -> PathBuf {
    if !canonical_live && legacy_live {
        legacy
    } else {
        canonical
    }
}

/// Issue #86: the uid of the daemon process, used as the reference for
/// the peer-auth check. `None` on transports/hosts with no uid concept
/// (Windows).
#[cfg(unix)]
pub fn daemon_uid() -> Option<u32> {
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
pub fn daemon_uid() -> Option<u32> {
    None
}

/// Issue #86: outcome of the peer-authentication decision for one
/// accepted connection. Pure data so the whole matrix is unit-testable
/// without a second uid, and so `server.rs` merely executes a decision
/// it did not compute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAuthDecision {
    /// The peer's uid matches the daemon's (or is root). Forward normally.
    Accept,
    /// The peer authenticated as a DIFFERENT uid. Deny with the
    /// structured response naming the offending uid.
    RejectForeign { uid: u32 },
    /// A uid lookup failed on a transport that DOES support peer creds
    /// (Linux). A failed lookup must never be treated as a match, so this
    /// is a hard deny — but there is no uid to name in the response.
    RejectLookupError,
    /// The transport has no peer-credential surface at all (Windows named
    /// pipes, non-Linux unix). The peer is not authenticated here; the
    /// caller falls back to the filesystem-permissions boundary and logs
    /// once. This is the documented interim, not a silent accept.
    Unsupported,
}

/// Issue #86: decide whether an accepted connection may speak the
/// control protocol, BEFORE any `Request` is parsed.
///
/// - `peer`: the `peer_uid()` result for the connection.
/// - `daemon_uid`: this process's uid (`None` only on hosts with no uid,
///   i.e. Windows, where `transport_supports_creds` is false anyway).
/// - `transport_supports_creds`: whether this transport is expected to be
///   able to report a peer uid (true on Linux, false on Windows/other
///   unix). Distinguishes "lookup failed" from "cannot look up at all".
///
/// Policy: default-deny. Accept only an explicit uid match (or root,
/// uid 0, which is this process's superuser and is permitted — documented
/// here, not implicit). Every error path denies or falls back; none of
/// them accept.
pub fn peer_auth_decision(
    peer: Result<u32, &std::io::Error>,
    daemon_uid: Option<u32>,
    transport_supports_creds: bool,
) -> PeerAuthDecision {
    if !transport_supports_creds {
        return PeerAuthDecision::Unsupported;
    }
    match peer {
        // A mismatched uid is the whole point of this check: deny.
        Ok(uid) => match daemon_uid {
            Some(mine) if uid == mine || uid == 0 => PeerAuthDecision::Accept,
            Some(_) => PeerAuthDecision::RejectForeign { uid },
            // A transport that supports creds but a host with no uid
            // concept is contradictory; fail closed.
            None => PeerAuthDecision::RejectLookupError,
        },
        // Lookup error on a credential-capable transport: never a match.
        Err(_) => PeerAuthDecision::RejectLookupError,
    }
}

/// Issue #86: does this transport report peer credentials on this host?
/// True only on Linux, where `Stream::peer_uid` is implemented via
/// `SO_PEERCRED`; everywhere else the daemon uses the documented
/// filesystem-permissions fallback (see `PeerAuthDecision::Unsupported`).
pub const fn transport_supports_peer_creds() -> bool {
    cfg!(target_os = "linux")
}

/// Issue #87: the effective uid of this process, used as the reference
/// for the restored-snapshot ownership check. `None` on hosts with no
/// uid concept (Windows).
#[cfg(unix)]
pub fn euid() -> Option<u32> {
    Some(unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
pub fn euid() -> Option<u32> {
    None
}

/// Issue #87: the owning uid of a file, when the platform can report
/// one. `None` on non-unix hosts.
#[cfg(unix)]
pub fn file_uid(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.uid())
}

#[cfg(not(unix))]
pub fn file_uid(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Issue #87: outcome of the trust decision for a persisted session
/// snapshot at the restore boundary. Pure data so the decision is
/// unit-testable (including the foreign-owner case, which needs no
/// second uid) and `Mux::restore_session` merely executes a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotTrustDecision {
    /// The snapshot is owned by this euid and is not group/other
    /// accessible. Restore from it.
    Accept,
    /// No snapshot exists (`ENOENT`). This is the ordinary first-run
    /// case, not a failure: restore nothing, launch nothing, stay quiet.
    Absent,
    /// The file is owned by a DIFFERENT uid. Refuse and launch nothing.
    RejectForeignOwner { uid: u32 },
    /// `stat` failed for a reason other than "not found" (and, on unix,
    /// the euid itself could not be resolved). A credential lookup error
    /// is NEVER a match: refuse and launch nothing.
    RejectLookupError,
    /// The file's permission bits are more permissive than 0600 (some
    /// group or other bit is set). Refuse — do NOT chmod-and-proceed;
    /// this is default-deny for a possible tamper/info-leak.
    RejectWorldReadable { mode: u32 },
}

/// Issue #87: decide whether a persisted snapshot may be replayed, given
/// only its `stat` result and this process's euid. Pure, so the whole
/// matrix is testable without a second uid or a real file.
///
/// Default-deny policy:
/// - a missing file is `Absent` (nothing to do), never an error;
/// - any other `stat` failure is `RejectLookupError`;
/// - a file not owned by this euid is `RejectForeignOwner`;
/// - mode bits outside 0600 are `RejectWorldReadable`;
/// - only an owned, 0600-or-stricter file is `Accept`.
///
/// On non-unix hosts there is no uid/mode to check (`file_uid` returns
/// `None`); the filesystem-permissions boundary is the documented
/// interim there, so an existing file is accepted.
pub fn snapshot_trust_decision(
    meta: std::io::Result<std::fs::Metadata>,
    process_euid: Option<u32>,
) -> SnapshotTrustDecision {
    let meta = match meta {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SnapshotTrustDecision::Absent,
        Err(_) => return SnapshotTrustDecision::RejectLookupError,
    };
    match (file_uid(&meta), process_euid, file_mode(&meta)) {
        // Windows/other: no uid to enforce. An existing file is trusted
        // (documented fallback) — keep the old behaviour there.
        (None, _, _) => SnapshotTrustDecision::Accept,
        // Contradictory: a uid-bearing file with no process euid to
        // compare against. Fail closed.
        (Some(_), None, _) => SnapshotTrustDecision::RejectLookupError,
        (Some(uid), Some(mine), _mode) if uid != mine => {
            SnapshotTrustDecision::RejectForeignOwner { uid }
        }
        // Owned by us: now the permission check. `mode` is a mask of the
        // permission bits; any group/other bit is "more permissive than
        // 0600". A non-unix file reports mode 0o600 (see `file_mode`).
        (Some(_), Some(_), Some(mode)) if mode & 0o077 != 0 => {
            SnapshotTrustDecision::RejectWorldReadable { mode }
        }
        _ => SnapshotTrustDecision::Accept,
    }
}

/// Issue #87: the permission bits of a file as a `u32`, or `None` on
/// non-unix hosts.
#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Where a session's persisted tree snapshot lives, honoring the XDG
/// override order. Not a runtime dir (`$XDG_RUNTIME_DIR` is wiped on
/// logout/reboot — exactly when this needs to survive).
///
/// Rename compat: a cmux-era `cmux` state dir is honoured (used as-is)
/// while the canonical `mattyx` dir is absent; no migration is
/// performed. See [`honor_cmux_era_dir`].
pub fn session_snapshot_path(session: &str) -> PathBuf {
    let base = env_path("XDG_STATE_HOME")
        .or_else(|| home_dir().map(|home| home.join(".local").join("state")))
        .unwrap_or_else(std::env::temp_dir);
    let dir = honor_cmux_era_dir(base.join("mattyx"));
    dir.join("sessions").join(format!("{session}.json"))
}

/// User config directory, honoring the XDG override order. The config
/// file itself is `mux.json` or `mux.toml` inside this directory.
///
/// Rename compat: a cmux-era `cmux` config dir is honoured while the
/// canonical `mattyx` dir is absent; no migration. See
/// [`honor_cmux_era_dir`].
pub fn config_dir() -> Option<PathBuf> {
    let canonical = if let Some(config_home) = env_path("XDG_CONFIG_HOME") {
        config_home.join("mattyx")
    } else {
        platform_config_dir()?
    };
    Some(honor_cmux_era_dir(canonical))
}

/// User config file path, honoring the XDG override order. The legacy
/// `mux.json` wins when both JSON and TOML exist (it is the explicit
/// override); otherwise `mux.toml` is loaded when present.
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = env_path("MTYX_MUX_CONFIG") {
        return Some(path);
    }
    let dir = config_dir()?;
    let json_path = dir.join("mux.json");
    if json_path.exists() {
        return Some(json_path);
    }
    let toml_path = dir.join("mux.toml");
    if toml_path.exists() {
        return Some(toml_path);
    }
    Some(json_path)
}

#[cfg(not(windows))]
fn platform_config_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".config").join("mattyx"))
}

#[cfg(windows)]
fn platform_config_dir() -> Option<PathBuf> {
    env_path("APPDATA").map(|appdata| appdata.join("mattyx"))
}

/// Default interactive shell for spawned PTY surfaces.
#[cfg(not(windows))]
pub fn default_shell() -> String {
    if let Some(shell) = env_string("SHELL") {
        return shell;
    }

    if Path::new("/bin/bash").is_file() {
        "/bin/bash".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

/// Default interactive shell for spawned PTY surfaces.
#[cfg(windows)]
pub fn default_shell() -> String {
    find_on_path(&["pwsh.exe", "powershell.exe", "cmd.exe"])
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "cmd.exe".to_string())
}

/// Candidate Chrome/Chromium-family binaries in platform discovery order.
pub fn chrome_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    #[cfg(target_os = "macos")]
    {
        push_unique(
            &mut candidates,
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        );
        push_unique(&mut candidates, "/Applications/Chromium.app/Contents/MacOS/Chromium".into());
        push_unique(
            &mut candidates,
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".into(),
        );
        push_unique(
            &mut candidates,
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
        );
        push_path_candidates(
            &mut candidates,
            &[
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
                "brave-browser",
                "microsoft-edge",
            ],
        );
    }

    #[cfg(target_os = "linux")]
    {
        push_path_candidates(
            &mut candidates,
            &["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"],
        );
        for path in [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
            "/opt/google/chrome/chrome",
            "/opt/chromium.org/chromium/chromium",
        ] {
            push_unique(&mut candidates, path.into());
        }
    }

    #[cfg(windows)]
    {
        push_path_candidates(
            &mut candidates,
            &["chrome.exe", "google-chrome.exe", "chromium.exe", "msedge.exe", "brave.exe"],
        );
        for base in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(dir) = env_path(base) {
                for path in [
                    dir.join("Google").join("Chrome").join("Application").join("chrome.exe"),
                    dir.join("Chromium").join("Application").join("chrome.exe"),
                    dir.join("BraveSoftware")
                        .join("Brave-Browser")
                        .join("Application")
                        .join("brave.exe"),
                    dir.join("Microsoft").join("Edge").join("Application").join("msedge.exe"),
                ] {
                    push_unique(&mut candidates, path);
                }
            }
        }
    }

    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    {
        push_path_candidates(
            &mut candidates,
            &["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"],
        );
    }

    candidates
}

/// Candidate Ghostty config files used to seed selection colors.
pub fn ghostty_config_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(config_home) = env_path("XDG_CONFIG_HOME") {
        push_unique(&mut candidates, config_home.join("ghostty").join("config"));
    }
    if let Some(home) = home_dir() {
        push_unique(&mut candidates, home.join(".config").join("ghostty").join("config"));
        #[cfg(target_os = "macos")]
        push_unique(
            &mut candidates,
            home.join("Library")
                .join("Application Support")
                .join("com.mitchellh.ghostty")
                .join("config"),
        );
    }
    candidates
}

/// Persistent profile directory for launched Chrome/Chromium sessions.
///
/// Rename compat: the `mattyx` data dir is honoured-without-migrating
/// against its cmux-era `cmux` sibling (see [`honor_cmux_era_dir`]);
/// the chrome-profile leaf rides on whichever base wins.
pub fn chrome_user_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let dir =
            home_dir().map(|home| home.join("Library").join("Application Support").join("mattyx"));
        dir.map(honor_cmux_era_dir).map(|d| d.join("chrome-profile"))
    }

    #[cfg(target_os = "linux")]
    {
        let dir = env_path("XDG_DATA_HOME")
            .map(|data_home| data_home.join("mattyx"))
            .or_else(|| home_dir().map(|home| home.join(".local").join("share").join("mattyx")));
        dir.map(honor_cmux_era_dir).map(|d| d.join("chrome-profile"))
    }

    #[cfg(windows)]
    {
        let dir = env_path("LOCALAPPDATA").map(|d| d.join("mattyx"));
        dir.map(honor_cmux_era_dir).map(|d| d.join("chrome-profile"))
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "linux"), not(windows)))]
    {
        let dir = env_path("XDG_DATA_HOME")
            .map(|d| d.join("mattyx"))
            .or_else(|| home_dir().map(|home| home.join(".local").join("share").join("mattyx")));
        dir.map(honor_cmux_era_dir).map(|d| d.join("chrome-profile"))
    }
}

/// The cmux-era sibling of a canonical mattyx path: the same path with
/// the last `mattyx` component renamed `cmux`. `None` when the path
/// holds no such component (nothing to fall back to).
fn cmux_era_sibling(canonical: &Path) -> Option<PathBuf> {
    let components: Vec<_> = canonical.components().collect();
    let idx = components.iter().rposition(|c| c.as_os_str() == std::ffi::OsStr::new("mattyx"))?;
    let mut out = PathBuf::new();
    for component in &components[..idx] {
        out.push(component.as_os_str());
    }
    out.push("cmux");
    for component in &components[idx + 1..] {
        out.push(component.as_os_str());
    }
    Some(out)
}

/// Honour-without-migrating (rename compat): use the canonical `mattyx`
/// dir; if it does not exist yet but a cmux-era `cmux` dir does, keep
/// using the old one so the rename never orphans a user's existing
/// config/state/profile. Once the canonical dir appears (any write
/// under the new name creates it), it wins and the old dir is left
/// untouched on disk. Never copies, moves, or deletes anything.
///
/// Public so mux-tui can apply the same policy to its plugin data dir.
pub fn honor_cmux_era_dir(canonical: PathBuf) -> PathBuf {
    match cmux_era_sibling(&canonical) {
        Some(legacy) if !canonical.exists() && legacy.is_dir() => legacy,
        _ => canonical,
    }
}

pub fn restrict_directory(path: &Path) -> std::io::Result<()> {
    restrict_permissions(path, 0o700)
}

pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    restrict_permissions(path, 0o600)
}

pub fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(not(windows))]
fn runtime_base_dir() -> PathBuf {
    env_path("XDG_RUNTIME_DIR")
        .or_else(|| env_path("TMPDIR"))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(windows)]
fn runtime_base_dir() -> PathBuf {
    env_path("TEMP").or_else(|| env_path("TMP")).unwrap_or_else(std::env::temp_dir)
}

#[cfg(not(windows))]
pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
}

#[cfg(windows)]
pub fn home_dir() -> Option<PathBuf> {
    // An explicit $HOME wins first: Git Bash / MSYS shells set it, CI
    // and the hook tests override it, and unix already treats it as
    // authoritative — honouring it here gives one override mechanism
    // everywhere (and stops the hook tests from writing the real
    // %USERPROFILE%\.claude when they point HOME at a temp dir).
    // Fall through to the native resolution when it is unset/empty.
    env_path("HOME").or_else(|| env_path("USERPROFILE")).or_else(|| {
        let drive = std::env::var_os("HOMEDRIVE")?;
        let path = std::env::var_os("HOMEPATH")?;
        let mut home = PathBuf::from(drive);
        home.push(path);
        Some(home)
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

#[cfg(not(windows))]
fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(unix)]
fn user_id_component() -> String {
    unsafe { libc::getuid() }.to_string()
}

#[cfg(windows)]
fn user_id_component() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string())
}

fn push_path_candidates(candidates: &mut Vec<PathBuf>, names: &[&str]) {
    for name in names {
        if let Some(candidate) = find_on_path(&[*name]) {
            push_unique(candidates, candidate);
        }
    }
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for name in names {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn push_unique(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if !candidates.iter().any(|candidate| candidate == &path) {
        candidates.push(path);
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // No tempfile dev-dep in mux-core; per-process counters keep
    // parallel runs from colliding, same technique as mux-tui's tests.
    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir(label: &str) -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mtyx_platform_test_{}_{}_{}",
            std::process::id(),
            n,
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn pick_runtime_socket_prefers_canonical_when_live() {
        let canonical = PathBuf::from("/run/user/1000/mtyx-1000/main.sock");
        let legacy = PathBuf::from("/run/user/1000/cmux-1000/main.sock");
        assert_eq!(pick_runtime_socket(canonical.clone(), legacy.clone(), true, true), canonical);
    }

    #[test]
    fn pick_runtime_socket_falls_back_to_live_legacy() {
        // The transition case: no mtyx server yet, a cmux-era server is
        // still running. The client must talk to the old daemon.
        let canonical = PathBuf::from("/run/user/1000/mtyx-1000/main.sock");
        let legacy = PathBuf::from("/run/user/1000/cmux-1000/main.sock");
        assert_eq!(pick_runtime_socket(canonical.clone(), legacy.clone(), false, true), legacy);
    }

    #[test]
    fn pick_runtime_socket_returns_canonical_when_neither_live() {
        // Nothing anywhere: return the canonical path so the connect
        // error names where a NEW server is expected to appear, not the
        // legacy location.
        let canonical = PathBuf::from("/run/user/1000/mtyx-1000/main.sock");
        let legacy = PathBuf::from("/run/user/1000/cmux-1000/main.sock");
        assert_eq!(pick_runtime_socket(canonical.clone(), legacy.clone(), false, false), canonical);
    }

    #[test]
    fn pick_runtime_socket_ignores_dead_legacy_when_canonical_live() {
        let canonical = PathBuf::from("/run/user/1000/mtyx-1000/main.sock");
        let legacy = PathBuf::from("/run/user/1000/cmux-1000/main.sock");
        assert_eq!(pick_runtime_socket(canonical.clone(), legacy.clone(), true, false), canonical);
    }

    #[test]
    fn cmux_era_sibling_renames_last_mattyx_component() {
        assert_eq!(
            cmux_era_sibling(Path::new("/home/u/.config/mattyx")).as_deref(),
            Some(Path::new("/home/u/.config/cmux"))
        );
        // Mid-path component: the chrome-profile base maps too.
        assert_eq!(
            cmux_era_sibling(Path::new("/data/mattyx/chrome-profile")).as_deref(),
            Some(Path::new("/data/cmux/chrome-profile"))
        );
        // No mattyx component: nothing to fall back to.
        assert_eq!(cmux_era_sibling(Path::new("/etc/other")), None);
    }

    #[test]
    fn honor_cmux_era_dir_uses_legacy_only_while_canonical_absent() {
        let dir = scratch_dir("honor_dir");
        let base = dir.join("xdg");
        std::fs::create_dir_all(base.join("cmux")).unwrap();

        // Canonical absent, legacy present -> honour the old dir.
        assert_eq!(honor_cmux_era_dir(base.join("mattyx")), base.join("cmux"));

        // Canonical appears -> it wins from then on; the legacy dir is
        // left untouched (no deletion, no merge).
        std::fs::create_dir_all(base.join("mattyx")).unwrap();
        assert_eq!(honor_cmux_era_dir(base.join("mattyx")), base.join("mattyx"));
        assert!(base.join("cmux").is_dir());
    }

    #[test]
    fn honor_cmux_era_dir_ignores_legacy_files_and_missing_both() {
        let dir = scratch_dir("honor_edge");
        // A legacy FILE at the sibling path is not a directory to adopt.
        let base = dir.join("xdg2");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("cmux"), b"not a dir").unwrap();
        assert_eq!(honor_cmux_era_dir(base.join("mattyx")), base.join("mattyx"));
        // Neither exists: canonical (a fresh install has no legacy dir).
        let base2 = dir.join("xdg3");
        std::fs::create_dir_all(&base2).unwrap();
        assert_eq!(honor_cmux_era_dir(base2.join("mattyx")), base2.join("mattyx"));
    }

    // ---- Issue #86: peer-auth decision matrix (default-deny) ----

    #[test]
    fn peer_auth_same_uid_accepted() {
        assert_eq!(peer_auth_decision(Ok(1000), Some(1000), true), PeerAuthDecision::Accept);
        // Root (uid 0) is this process's superuser and is permitted by
        // policy (documented in peer_auth_decision).
        assert_eq!(peer_auth_decision(Ok(0), Some(1000), true), PeerAuthDecision::Accept);
    }

    #[test]
    fn peer_auth_foreign_uid_rejected() {
        assert_eq!(
            peer_auth_decision(Ok(0xdead), Some(1000), true),
            PeerAuthDecision::RejectForeign { uid: 0xdead }
        );
        // uid 0 daemon with a non-root peer: still a mismatch, still denied.
        assert_eq!(
            peer_auth_decision(Ok(1000), Some(0), true),
            PeerAuthDecision::RejectForeign { uid: 1000 }
        );
    }

    #[test]
    fn peer_auth_lookup_error_is_rejected_not_matched() {
        // A lookup error on a credential-capable transport must NEVER be
        // treated as a match.
        let err = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        assert_eq!(
            peer_auth_decision(Err(&err), Some(1000), true),
            PeerAuthDecision::RejectLookupError
        );
        // Contradictory combination (creds supported, host has no uid)
        // fails closed rather than accepting.
        assert_eq!(peer_auth_decision(Ok(1000), None, true), PeerAuthDecision::RejectLookupError);
    }

    #[test]
    fn peer_auth_unsupported_transport_falls_back() {
        // Windows named pipes / non-Linux unix: no peer-cred surface, so
        // the decision is the documented filesystem-permissions fallback
        // regardless of what (if anything) the lookup returned.
        let err = std::io::Error::new(std::io::ErrorKind::Unsupported, "nope");
        assert_eq!(peer_auth_decision(Err(&err), None, false), PeerAuthDecision::Unsupported);
        assert_eq!(peer_auth_decision(Ok(1000), Some(1000), false), PeerAuthDecision::Unsupported);
        assert_eq!(
            peer_auth_decision(Ok(0xbeef), Some(1000), false),
            PeerAuthDecision::Unsupported
        );
    }

    // ---- Issue #87: restored-daemon snapshot trust matrix ----

    fn meta_for(mode: u32) -> std::fs::Metadata {
        let dir = scratch_dir("snapshot_meta");
        let path = dir.join("snap.json");
        std::fs::write(&path, b"{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::metadata(&path).unwrap()
    }

    #[test]
    fn snapshot_trust_accepts_owned_0600_file() {
        let meta = meta_for(0o600);
        assert_eq!(
            snapshot_trust_decision(Ok(meta), file_uid(&meta_for(0o600))),
            SnapshotTrustDecision::Accept
        );
    }

    #[test]
    fn snapshot_trust_absent_is_not_an_error() {
        // The ordinary first-run case: no snapshot at all.
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        assert_eq!(snapshot_trust_decision(Err(err), Some(1000)), SnapshotTrustDecision::Absent);
    }

    #[test]
    fn snapshot_trust_lookup_error_is_rejected() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "boom");
        assert_eq!(
            snapshot_trust_decision(Err(err), Some(1000)),
            SnapshotTrustDecision::RejectLookupError
        );
        // A uid-bearing file with no process euid to compare fails closed.
        let meta = meta_for(0o600);
        assert_eq!(
            snapshot_trust_decision(Ok(meta), None),
            SnapshotTrustDecision::RejectLookupError
        );
    }

    #[test]
    fn snapshot_trust_rejects_foreign_owner() {
        // Simulated via the decision function: the real uid of the file
        // is irrelevant, only the mismatch matters.
        let meta = meta_for(0o600);
        let owner = file_uid(&meta).unwrap();
        assert_eq!(
            snapshot_trust_decision(Ok(meta), Some(owner.wrapping_add(1))),
            SnapshotTrustDecision::RejectForeignOwner { uid: owner }
        );
    }

    #[test]
    fn snapshot_trust_rejects_group_and_other_bits() {
        for mode in [0o640, 0o644, 0o660, 0o666, 0o777] {
            let meta = meta_for(mode);
            let owner = file_uid(&meta).unwrap();
            assert_eq!(
                snapshot_trust_decision(Ok(meta), Some(owner)),
                SnapshotTrustDecision::RejectWorldReadable { mode },
                "mode {mode:o} must be refused"
            );
        }
        // 0600 and stricter pass; 0700 has no group/other bits, so it
        // leaks nothing to another user and is accepted.
        for mode in [0o600, 0o400, 0o700] {
            let meta = meta_for(mode);
            let owner = file_uid(&meta).unwrap();
            assert_eq!(
                snapshot_trust_decision(Ok(meta), Some(owner)),
                SnapshotTrustDecision::Accept
            );
        }
    }
}
