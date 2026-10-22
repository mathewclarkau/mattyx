//! Issue #129: argv for tests that only need a quiet, long-lived child.
//!
//! Unix keeps `/bin/cat` so Linux coverage is unchanged. Windows cannot
//! `CreateProcessW` a `/bin/...` path (windows-latest CI, os error 3).
//! PowerShell `Start-Sleep` is on that image and does not exit immediately.
//! POSIX shell scripts (`/bin/sh -c`) stay `#[cfg(unix)]` at the call site:
//! they are not a Windows command, and gating them does not drop the Linux run.

/// Quiet long-lived child argv. Empty is never returned.
pub fn idle_child_argv() -> Vec<String> {
    #[cfg(unix)]
    {
        vec!["/bin/cat".to_string()]
    }
    #[cfg(windows)]
    {
        windows_idle_child_argv()
    }
    #[cfg(not(any(unix, windows)))]
    {
        vec!["cat".to_string()]
    }
}

/// Windows idle child. Always compiled so the #129 regression test can
/// reject a `/bin/...` argv without a Windows host.
pub fn windows_idle_child_argv() -> Vec<String> {
    vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 3600".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::idle_child_argv;

    #[test]
    fn idle_child_argv_is_host_spawnable() {
        let argv = idle_child_argv();
        assert!(!argv.is_empty(), "a test child needs an executable");
        assert!(
            !argv[0].starts_with("/bin/") || cfg!(unix),
            "issue #129: a /bin path is not CreateProcessW-able on Windows, got {argv:?}"
        );
        #[cfg(unix)]
        assert_eq!(argv, vec!["/bin/cat".to_string()], "Linux coverage must keep /bin/cat");
        #[cfg(windows)]
        {
            assert_eq!(argv, super::windows_idle_child_argv());
        }
    }

    #[test]
    fn windows_idle_child_is_not_a_unix_absolute_path() {
        let argv = super::windows_idle_child_argv();
        assert!(
            !argv[0].starts_with('/'),
            "issue #129: CreateProcessW cannot spawn a unix absolute path, got {argv:?}"
        );
        assert!(argv[0].to_ascii_lowercase().contains("powershell"));
    }
}
