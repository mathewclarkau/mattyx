//! A PTY backed by `cmuxd-remote` (vendored at `daemon/remote/`) running on
//! a remote host over SSH, instead of a local child process.
//!
//! The trick: `portable_pty::{MasterPty, SlavePty, Child, ChildKiller}` have
//! no method that fundamentally requires a real local file descriptor (the
//! Unix-only fd/tty-name/process-group accessors are all `Option` and fine
//! to return `None` from). Implementing them here means [`Surface::spawn`]
//! and its reader thread, `write_bytes`, `resize`, and the OSC-notification
//! watcher all work completely unchanged for a remote surface — they just
//! see a `Box<dyn MasterPty>`/`Box<dyn SlavePty>` and don't know or care
//! that bytes are actually flowing over `cmuxd-remote`'s NDJSON RPC
//! protocol through an SSH-exec'd child process instead of a kernel pty.
//!
//! Protocol (from `daemon/remote/cmd/cmuxd-remote/{main,ws_pty}.go`):
//! newline-delimited JSON, `{"id":...,"method":...,"params":{...}}` /
//! `{"id":...,"ok":bool,"result"|"error":...}`, plus pushed
//! `{"event":...}` lines with no `id`. Only the `pty.*` methods matter
//! here (`session.*` is separate multi-attachment resize bookkeeping we
//! don't need for one attachment per surface): `pty.attach` (creates or
//! reattaches to `session_id`, returns an `attachment_id`/
//! `attachment_token`), `pty.write`, `pty.resize`, `pty.detach` (drops the
//! attachment, leaves the remote shell running — this is what
//! [`ChildKiller::kill`] sends, since the entire point of a remote surface
//! is that closing it locally doesn't kill the remote session), and
//! `pty.close` (actually kills it — not currently exposed by any verb; a
//! stale remote session needs manual cleanup, e.g. deleting
//! `~/.cache/mattyx/cmuxd-remote` and `~/.mattyx/daemon/` on the remote).
//!
//! `cmuxd-remote serve --stdio --persistent --slot <slot>` is what makes
//! the remote shell outlive both an SSH disconnect and this local process
//! restarting: it spawns (or reconnects to) a detached background daemon
//! under `~/.mattyx/daemon/dev/<slot>/` on the remote and becomes a thin
//! byte-proxy to it, so a fresh `ssh` invocation reattaching to the same
//! `session_id` later replays scrollback and continues the same shell.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, SlavePty,
};
use serde_json::{json, Value};

use crate::persist::shell_quote;

/// Everything needed to open (or reattach to) one remote pty session.
#[derive(Debug, Clone)]
pub struct RemoteSpec {
    /// SSH destination, e.g. `"host"` or `"user@host"` — passed to `ssh`
    /// verbatim, so `~/.ssh/config` aliases and options work.
    pub host: String,
    /// `cmuxd-remote`'s persistent-daemon slot name. Sessions under
    /// different slots are independent; reuse one slot per host to let
    /// its daemon multiplex several `session_id`s.
    pub slot: String,
    /// Which remote pty session to create-or-reattach. Callers that want
    /// reconnect-after-restart must persist this and pass it back
    /// unchanged (see `persist.rs`'s `TabSnapshot`).
    pub session_id: String,
    /// Local path to a `cmuxd-remote` binary built for the *remote*
    /// host's OS/arch. Uploaded (once, hash-checked) to
    /// `~/.cache/mattyx/cmuxd-remote` on the remote if missing or
    /// stale. Building this is deliberately not this module's job — it
    /// needs a Go toolchain and knowledge of where the vendored source
    /// lives, both of which belong to the frontend, not this library.
    pub local_binary_path: std::path::PathBuf,
}

pub fn generate_session_id() -> String {
    format!("mtyx-{}", random_token())
}

fn random_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}-{:x}", std::process::id(), nanos, n)
}

/// Opens a `PtyPair` backed by a remote `cmuxd-remote` session. Uploads
/// the daemon binary first if the remote copy is missing or doesn't match
/// `spec.local_binary_path`'s hash.
pub fn open_remote_pty(spec: &RemoteSpec, size: PtySize) -> anyhow::Result<PtyPair> {
    let remote_bin = ensure_uploaded(&spec.host, &spec.local_binary_path)?;
    let conn = RemoteConn::spawn(&spec.host, &remote_bin, &spec.slot)?;
    let shared = Arc::new(RemoteShared {
        conn,
        session_id: spec.session_id.clone(),
        attachment_id: Mutex::new(None),
        attachment_token: Mutex::new(None),
        size: Mutex::new(size),
        reader_rx: Mutex::new(None),
    });
    Ok(PtyPair {
        slave: Box::new(RemoteSlavePty(shared.clone())),
        master: Box::new(RemoteMasterPty(shared)),
    })
}

/// Relative to `$HOME` — SSH's non-interactive exec, scp, and sftp all
/// resolve relative remote paths against the login directory by default,
/// so this needs no `~` at all. That matters: `~` only expands when
/// unquoted, and every use of this path in an exec'd command string goes
/// through [`shell_quote`] for safety against paths/slots with spaces or
/// quotes, which would otherwise silently defeat tilde expansion.
const REMOTE_BIN_PATH: &str = ".cache/mattyx/cmuxd-remote";

/// Uploads `local_binary_path` to `$HOME/.cache/mattyx/cmuxd-remote` on
/// `host` unless a copy with the same SHA-256 is already there. Returns
/// the remote path (relative to `$HOME`).
fn ensure_uploaded(host: &str, local_binary_path: &std::path::Path) -> anyhow::Result<String> {
    let local_hash = sha256_file(local_binary_path)?;
    let remote_hash = ssh_output(
        host,
        &format!("sha256sum {} 2>/dev/null | cut -d' ' -f1", shell_quote(REMOTE_BIN_PATH)),
    )
    .unwrap_or_default()
    .trim()
    .to_string();
    if remote_hash != local_hash {
        let remote_dir = std::path::Path::new(REMOTE_BIN_PATH).parent().unwrap().to_str().unwrap();
        run_checked(
            Command::new("ssh").arg(host).arg(format!("mkdir -p {}", shell_quote(remote_dir))),
            "mkdir remote cache dir",
        )?;
        run_checked(
            Command::new("scp").arg(local_binary_path).arg(format!("{host}:{REMOTE_BIN_PATH}")),
            "upload cmuxd-remote",
        )?;
        run_checked(
            Command::new("ssh").arg(host).arg(format!("chmod +x {}", shell_quote(REMOTE_BIN_PATH))),
            "chmod remote cmuxd-remote",
        )?;
    }
    Ok(REMOTE_BIN_PATH.to_string())
}

fn run_checked(cmd: &mut Command, what: &str) -> anyhow::Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("{what} failed: {status}");
    }
    Ok(())
}

fn ssh_output(host: &str, remote_command: &str) -> anyhow::Result<String> {
    let output = Command::new("ssh").arg(host).arg(remote_command).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn sha256_file(path: &std::path::Path) -> anyhow::Result<String> {
    let output = Command::new("sha256sum").arg(path).output()?;
    if !output.status.success() {
        anyhow::bail!("sha256sum {} failed", path.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.split_whitespace().next().unwrap_or_default().to_string())
}

// ---------- RPC connection ----------

type PendingResult = Result<Value, String>;

struct RemoteConn {
    child: Mutex<std::process::Child>,
    stdin: Mutex<std::process::ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, mpsc::Sender<PendingResult>>>,
    data_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    exited: Arc<AtomicBool>,
}

impl RemoteConn {
    fn spawn(host: &str, remote_bin: &str, slot: &str) -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new("ssh")
            .args(["-o", "BatchMode=yes"])
            .arg(host)
            .arg(format!(
                "{} serve --stdio --persistent --slot {}",
                shell_quote(remote_bin),
                shell_quote(slot)
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let conn = Arc::new(RemoteConn {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            data_tx: Mutex::new(None),
            exited: Arc::new(AtomicBool::new(false)),
        });

        let reader_conn = conn.clone();
        std::thread::Builder::new().name("remote-pty-rpc-reader".into()).spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    reader_conn.handle_line(value);
                }
            }
            reader_conn.mark_exited();
        })?;

        Ok(conn)
    }

    fn mark_exited(&self) {
        self.exited.store(true, Ordering::Release);
        if let Some(tx) = self.data_tx.lock().unwrap().take() {
            drop(tx);
        }
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err("remote connection closed".to_string()));
        }
    }

    fn handle_line(&self, value: Value) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            if let Some(tx) = self.pending.lock().unwrap().remove(id) {
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let sent = if ok {
                    tx.send(Ok(value.get("result").cloned().unwrap_or_else(|| json!({}))))
                } else {
                    let message = value
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("remote error")
                        .to_string();
                    tx.send(Err(message))
                };
                let _ = sent;
            }
            return;
        }
        match value.get("event").and_then(Value::as_str) {
            Some("pty.data") => {
                if let Some(bytes) = value
                    .get("data_base64")
                    .and_then(Value::as_str)
                    .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
                {
                    if let Some(tx) = self.data_tx.lock().unwrap().as_ref() {
                        let _ = tx.send(bytes);
                    }
                }
            }
            // Scoped to one attachment (detach or the remote command
            // genuinely exiting look identical on the wire) - either way
            // this attachment's stream is over, so close its data channel
            // to signal EOF to whoever is reading it.
            Some("pty.exit") => {
                if let Some(tx) = self.data_tx.lock().unwrap().take() {
                    drop(tx);
                }
            }
            _ => {}
        }
    }

    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.exited.load(Ordering::Acquire) {
            anyhow::bail!("remote connection already closed");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let line = serde_json::to_string(&json!({"id": id, "method": method, "params": params}))?;
        {
            let mut stdin = self.stdin.lock().unwrap();
            stdin.write_all(line.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
        }
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => anyhow::bail!("{message}"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                anyhow::bail!("remote request {method} timed out")
            }
        }
    }
}

impl Drop for RemoteConn {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

// ---------- portable_pty trait impls ----------

struct RemoteShared {
    conn: Arc<RemoteConn>,
    session_id: String,
    attachment_id: Mutex<Option<String>>,
    attachment_token: Mutex<Option<String>>,
    size: Mutex<PtySize>,
    reader_rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
}

impl RemoteShared {
    fn attachment(&self) -> (String, String) {
        (
            self.attachment_id.lock().unwrap().clone().unwrap_or_default(),
            self.attachment_token.lock().unwrap().clone().unwrap_or_default(),
        )
    }
}

pub struct RemoteMasterPty(Arc<RemoteShared>);

impl MasterPty for RemoteMasterPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        *self.0.size.lock().unwrap() = size;
        let (attachment_id, attachment_token) = self.0.attachment();
        // Best-effort: a resize that arrives before attach or after the
        // remote session ended shouldn't be a hard error for the caller.
        let _ = self.0.conn.request(
            "pty.resize",
            json!({
                "session_id": self.0.session_id,
                "attachment_id": attachment_id,
                "client_attachment_token": attachment_token,
                "cols": size.cols,
                "rows": size.rows,
            }),
        );
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        Ok(*self.0.size.lock().unwrap())
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        let rx = self.0.reader_rx.lock().unwrap().take().ok_or_else(|| {
            anyhow::anyhow!("remote pty reader already taken or not yet attached")
        })?;
        Ok(Box::new(RemotePtyReader { rx, buf: Vec::new(), pos: 0 }))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        Ok(Box::new(RemotePtyWriter { shared: self.0.clone() }))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub struct RemoteSlavePty(Arc<RemoteShared>);

impl SlavePty for RemoteSlavePty {
    fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<Box<dyn Child + Send + Sync>> {
        let command = remote_command_line(&cmd);
        let (cols, rows) = {
            let size = *self.0.size.lock().unwrap();
            (size.cols, size.rows)
        };
        let attachment_token = format!("tok-{}", random_token());
        let mut params = json!({
            "session_id": self.0.session_id,
            "client_attachment_token": attachment_token,
            "cols": cols,
            "rows": rows,
        });
        if let Some(command) = command {
            params["command"] = json!(command);
        }
        let result = self.0.conn.request("pty.attach", params)?;
        let attachment_id =
            result.get("attachment_id").and_then(Value::as_str).unwrap_or_default().to_string();

        let (tx, rx) = mpsc::channel();
        *self.0.conn.data_tx.lock().unwrap() = Some(tx);
        *self.0.attachment_id.lock().unwrap() = Some(attachment_id);
        *self.0.attachment_token.lock().unwrap() = Some(attachment_token);
        *self.0.reader_rx.lock().unwrap() = Some(rx);

        Ok(Box::new(RemoteChild { shared: self.0.clone() }))
    }
}

/// `cmuxd-remote`'s `pty.attach.command` replaces the whole shell
/// invocation (`/bin/sh -c <command>`) and, unlike a local
/// `CommandBuilder`, has no separate cwd concept — so a requested cwd is
/// folded into the command as a leading `cd`.
fn remote_command_line(cmd: &CommandBuilder) -> Option<String> {
    let cwd = cmd.get_cwd().map(|c| c.to_string_lossy().into_owned());
    let cd_prefix = cwd.map(|c| format!("cd {} 2>/dev/null; ", shell_quote(&c)));

    if cmd.is_default_prog() {
        return cd_prefix.map(|prefix| format!("{prefix}exec \"$SHELL\" -l"));
    }
    let argv: Vec<String> =
        cmd.get_argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if argv.is_empty() {
        return cd_prefix.map(|prefix| format!("{prefix}exec \"$SHELL\" -l"));
    }
    let joined = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    Some(format!("{}exec {joined}", cd_prefix.unwrap_or_default()))
}

pub struct RemoteChild {
    shared: Arc<RemoteShared>,
}

impl std::fmt::Debug for RemoteChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteChild").field("session_id", &self.shared.session_id).finish()
    }
}

impl Child for RemoteChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if self.shared.conn.exited.load(Ordering::Acquire) {
            Ok(Some(ExitStatus::with_exit_code(0)))
        } else {
            Ok(None)
        }
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        while !self.shared.conn.exited.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    // portable-pty's Child trait requires this on Windows; a remote
    // surface owns no local process handle.
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

impl ChildKiller for RemoteChild {
    fn kill(&mut self) -> std::io::Result<()> {
        // Detach, not close: the whole point of a remote surface is that
        // the shell survives closing the local tab. A genuinely stale
        // remote session needs manual cleanup on the remote host today
        // (no `pty.close` verb is wired up - see the module doc).
        let (attachment_id, attachment_token) = self.shared.attachment();
        let _ = self.shared.conn.request(
            "pty.detach",
            json!({
                "session_id": self.shared.session_id,
                "attachment_id": attachment_id,
                "client_attachment_token": attachment_token,
            }),
        );
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(RemoteChild { shared: self.shared.clone() })
    }
}

struct RemotePtyReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for RemotePtyReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(bytes) => {
                    self.buf = bytes;
                    self.pos = 0;
                }
                // Attachment ended (detach, remote exit, or connection
                // drop) - report EOF, exactly like a local pty closing.
                Err(_) => return Ok(0),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct RemotePtyWriter {
    shared: Arc<RemoteShared>,
}

impl Write for RemotePtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(buf);
        let (attachment_id, attachment_token) = self.shared.attachment();
        self.shared
            .conn
            .request(
                "pty.write",
                json!({
                    "session_id": self.shared.session_id,
                    "attachment_id": attachment_id,
                    "client_attachment_token": attachment_token,
                    "data_base64": data_base64,
                }),
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder(argv: &[&str]) -> CommandBuilder {
        if argv.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            let mut b = CommandBuilder::new(argv[0]);
            b.args(&argv[1..]);
            b
        }
    }

    #[test]
    fn default_shell_with_no_cwd_has_no_command_override() {
        let cmd = builder(&[]);
        assert_eq!(remote_command_line(&cmd), None);
    }

    #[test]
    fn default_shell_with_cwd_cds_then_execs_login_shell() {
        let mut cmd = builder(&[]);
        cmd.cwd("/tmp/my project");
        assert_eq!(
            remote_command_line(&cmd),
            Some("cd '/tmp/my project' 2>/dev/null; exec \"$SHELL\" -l".to_string())
        );
    }

    #[test]
    fn custom_command_is_shell_quoted_and_execed() {
        let cmd = builder(&["claude", "--resume", "abc"]);
        assert_eq!(remote_command_line(&cmd), Some("exec 'claude' '--resume' 'abc'".to_string()));
    }

    #[test]
    fn custom_command_with_cwd_and_special_chars() {
        let mut cmd = builder(&["echo", "it's fine"]);
        cmd.cwd("/tmp/a b");
        assert_eq!(
            remote_command_line(&cmd),
            Some("cd '/tmp/a b' 2>/dev/null; exec 'echo' 'it'\\''s fine'".to_string())
        );
    }
}
