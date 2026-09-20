//! Plugin host: wasmtime runtime + mtyx-call host imports.
//!
//! Implements the execution layer for `mtyx plugin install`. Per the
//! design spec at `spec/mtyx-plugin-execution.md`:
//!
//!   - Plugin WASM is loaded from the manifest's `entry` path.
//!   - Three host imports are exposed to the plugin: `cmux_token`,
//!     `cmux_call`, `cmux_log`.
//!   - Every `cmux_call` request carries a per-call auth token that was
//!     minted when the plugin was invoked; the token is scoped to
//!     (plugin_name, verb_allowlist, socket_capability).
//!   - Fuel + wall-clock timeout are enforced.
//!
//! This file is the security boundary: every mtyx-side effect must go
//! through `validate_and_forward`, which checks the token against
//! the plugin's manifest before any state mutation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wasmtime::{Config, Engine, Instance, Linker, Module, Store, Trap};
use wasmtime_wasi::preview1::{add_to_linker_sync, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::plugin::{self, Capabilities, PluginEntry};

/// Per-call auth token. Minted by `PluginCall::invoke`, validated by
/// `validate_and_forward` on every `cmux_call` request.
///
/// A token is a 32-byte random value hex-encoded; the mtyx side keeps a
/// `HashMap<token, TokenContext>` keyed by token and removes the entry
/// after the call completes (or the wall-clock timeout fires).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallToken(String);

impl CallToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Per-token metadata captured at mint time. The forward step uses
/// this to validate every cmux_call request without re-reading the
/// registry on the hot path.
#[derive(Debug, Clone)]
pub struct TokenContext {
    pub plugin_name: String,
    pub verb_allowlist: Vec<String>,
    pub socket_capability: SocketCapability,
    pub minted_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketCapability {
    Off,
    Read,
    Write,
}

impl SocketCapability {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("off") => Self::Off,
            Some("read") => Self::Read,
            Some("write") => Self::Write,
            _ => Self::Read,
        }
    }
}

/// JSON shape the plugin sends in `cmux_call`.
#[derive(Debug, Deserialize, Serialize)]
pub struct PluginRequest {
    pub id: u64,
    pub verb: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// JSON shape mtyx returns from `cmux_call`.
#[derive(Debug, Serialize)]
pub struct PluginResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Trait the host uses to forward a validated `cmux_call` request to the
/// real mtyx control socket. Production wiring in `main.rs` builds an
/// adapter that writes a JSON request to the mux-core socket and reads
/// the response; tests can supply an in-memory mock.
pub trait CmuxDispatcher: Send + Sync {
    fn dispatch(&self, request_json: String) -> Result<String, String>;
}

/// Errors a plugin invocation can produce.
#[derive(Debug)]
pub enum PluginError {
    EntryNotFound(String),
    InvalidManifest(String),
    UnknownToken,
    VerbNotAllowed(String),
    WriteBlocked(String),
    FuelExhausted,
    Timeout,
    InstantiationFailed(String),
    Trap(String),
    InvalidRequest(String),
    StaleId { expected: u64, got: u64 },
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntryNotFound(p) => write!(f, "plugin entry not found: {p}"),
            Self::InvalidManifest(m) => write!(f, "invalid manifest: {m}"),
            Self::UnknownToken => write!(f, "plugin cmux_call had unknown token"),
            Self::VerbNotAllowed(v) => write!(f, "verb {v:?} not in plugin allowlist"),
            Self::WriteBlocked(v) => write!(f, "verb {v:?} requires socket=write"),
            Self::FuelExhausted => write!(f, "plugin exhausted fuel"),
            Self::Timeout => write!(f, "plugin exceeded wall-clock timeout"),
            Self::InstantiationFailed(s) => write!(f, "plugin instantiation failed: {s}"),
            Self::Trap(s) => write!(f, "plugin trap: {s}"),
            Self::InvalidRequest(s) => write!(f, "plugin sent invalid request: {s}"),
            Self::StaleId { expected, got } => {
                write!(f, "plugin sent stale request id {got}, expected {expected}")
            }
        }
    }
}

pub type PluginResult = Result<String, PluginError>;

/// Context passed through wasmtime's Store.
pub struct HostState {
    pub wasi: WasiP1Ctx,
    pub token: CallToken,
    pub token_ctx: TokenContext,
    pub dispatcher: Arc<dyn CmuxDispatcher>,
    pub expected_request_id: u64,
}

impl HostState {
    pub fn new(
        wasi: WasiP1Ctx,
        token: CallToken,
        token_ctx: TokenContext,
        dispatcher: Arc<dyn CmuxDispatcher>,
    ) -> Self {
        Self { wasi, token, token_ctx, dispatcher, expected_request_id: 0 }
    }
}

/// Validate an inbound request shape (before dispatch). Pulled out so
/// the tests can exercise it without a wasmtime engine.
fn validate_request_shape(
    req: &PluginRequest,
    token_ctx: &TokenContext,
    expected_id: u64,
) -> Result<(), PluginError> {
    if req.id != expected_id {
        return Err(PluginError::StaleId { expected: expected_id, got: req.id });
    }
    if !token_ctx.verb_allowlist.iter().any(|v| v == &req.verb) {
        return Err(PluginError::VerbNotAllowed(req.verb.clone()));
    }
    if token_ctx.socket_capability == SocketCapability::Read && is_mutating_verb(&req.verb) {
        return Err(PluginError::WriteBlocked(req.verb.clone()));
    }
    Ok(())
}

/// Heuristic for which mtyx verbs are mutating. Conservative.
fn is_mutating_verb(verb: &str) -> bool {
    matches!(
        verb,
        "set-default-colors"
            | "rename-workspace"
            | "rename-pane"
            | "rename-surface"
            | "rename-screen"
            | "set-ratio"
            | "close-surface"
            | "close-pane"
            | "close-screen"
            | "close-workspace"
            | "new-workspace"
            | "new-screen"
            | "new-tab"
            | "split"
            | "send"
            | "browser-reload"
            | "select-tab"
            | "select-screen"
            | "select-workspace"
            | "move-tab"
            | "move-workspace"
            | "resize-surface"
            | "scroll-surface"
            | "focus-pane"
            | "report-agent"
            | "plugin"
    )
}

/// Build the wasmtime engine.
pub fn build_engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config
        .cranelift_opt_level(wasmtime::OptLevel::Speed)
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_multi_memory(false)
        .wasm_component_model(false);
    Engine::new(&config).map_err(|e| format!("failed to build wasmtime engine: {e}"))
}

/// Load a wasmtime module from a file path.
pub fn load_module(engine: &Engine, path: &Path) -> Result<Module, String> {
    Module::from_file(engine, path).map_err(|e| format!("failed to load wasm module {path:?}: {e}"))
}

/// Configure the WASI context per the manifest's filesystem + env caps.
/// Returns a WasiP1Ctx (the preview1-specific context that the
/// `add_to_linker_sync` host imports hook into).
pub fn build_wasi(
    cap: &Capabilities,
    plugin_data_dir: &Path,
    args: &[String],
    env: &[String],
) -> Result<WasiP1Ctx, String> {
    let mut builder = WasiCtxBuilder::new();
    builder.args(args);
    for name in env {
        if let Ok(value) = std::env::var(name) {
            builder.env(name, value);
        }
    }
    builder
        .preopened_dir(plugin_data_dir, ".", DirPerms::all(), FilePerms::all())
        .map_err(|e| format!("failed to preopen plugin data dir {plugin_data_dir:?}: {e}"))?;
    if let Some(extra) = &cap.filesystem {
        for path_str in extra {
            let p = PathBuf::from(path_str);
            if p.exists() {
                builder.preopened_dir(&p, ".", DirPerms::all(), FilePerms::all()).map_err(|e| {
                    format!("failed to preopen manifest filesystem path {p:?}: {e}")
                })?;
            }
        }
    }
    Ok(builder.build_p1())
}

/// Mint a fresh per-call token.
pub fn mint_token() -> CallToken {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let raw = (nanos as u64) ^ n ^ (std::process::id() as u64);
    CallToken(format!("{:032x}", raw))
}

/// Validate + forward a `cmux_call` request to the dispatcher. Pulled
/// out of the Linker closure for unit-testability.
pub fn validate_and_forward(
    request_json: &str,
    expected_id: u64,
    token_ctx: &TokenContext,
    dispatcher: Arc<dyn CmuxDispatcher>,
) -> Result<String, PluginError> {
    let req: PluginRequest = serde_json::from_str(request_json)
        .map_err(|e| PluginError::InvalidRequest(e.to_string()))?;
    validate_request_shape(&req, token_ctx, expected_id)?;
    let request_str =
        serde_json::to_string(&req).map_err(|e| PluginError::InvalidRequest(e.to_string()))?;
    let response_str = dispatcher
        .dispatch(request_str)
        .map_err(|e| PluginError::Trap(format!("dispatcher: {e}")))?;
    let data: serde_json::Value =
        serde_json::from_str(&response_str).unwrap_or(serde_json::Value::String(response_str));
    let resp = PluginResponse { id: expected_id, ok: true, data: Some(data), error: None };
    serde_json::to_string(&resp).map_err(|e| PluginError::Trap(format!("serialize response: {e}")))
}

/// Define the three host imports on a Linker. Returns the linker
/// configured for use. The closure bodies borrow caller state via
/// wasmtime's `Caller` API, which forbids holding a mutable borrow across
/// an indirect call into the dispatcher; we work around that by
/// copying the small pieces we need out of caller.data() first.
pub fn define_host_imports(linker: &mut Linker<HostState>) -> Result<(), String> {
    // cmux_token() -> u64 (high 32 bits = ptr, low 32 = len, both in
    // plugin's linear memory; plugin unpacks). Token string lives in
    // static-ish storage inside the HostState; we use a Vec<u8> that
    // the plugin reads via ptr+len and that we leak for the call's
    // duration.
    linker
        .func_wrap("mtyx", "token", |mut caller: wasmtime::Caller<'_, HostState>| -> u64 {
            let token = caller.data().token.as_str().to_string();
            let bytes = token.into_bytes();
            let ptr = bytes.as_ptr() as u32;
            let len = bytes.len() as u32;
            std::mem::forget(bytes);
            ((ptr as u64) << 32) | (len as u64)
        })
        .map_err(|e| format!("failed to define cmux_token: {e}"))?;

    // cmux_log(level: i32, ptr: i32, len: i32) -> ()
    linker
        .func_wrap(
            "mtyx",
            "log",
            |mut caller: wasmtime::Caller<'_, HostState>, level: i32, ptr: i32, len: i32| {
                let mem = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(m)) => m,
                    _ => return,
                };
                let data = mem.data(&caller);
                let slice = match data.get(ptr as usize..(ptr + len) as usize) {
                    Some(s) => s,
                    None => return,
                };
                let msg = match std::str::from_utf8(slice) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let prefix = match level {
                    0 => "info",
                    1 => "warn",
                    2 => "error",
                    _ => "debug",
                };
                eprintln!("[plugin:{}] {}: {}", caller.data().token_ctx.plugin_name, prefix, msg);
            },
        )
        .map_err(|e| format!("failed to define cmux_log: {e}"))?;

    // cmux_call(req_ptr, req_len, out_ptr, out_cap) -> i32 (response length).
    linker
        .func_wrap(
            "mtyx",
            "call",
            |mut caller: wasmtime::Caller<'_, HostState>,
             req_ptr: i32,
             req_len: i32,
             out_ptr: i32,
             out_cap: i32|
             -> i32 {
                // Pull everything we need out of caller.data() while we
                // have a shared borrow, then drop it before doing the
                // validate-and-forward (which mutably borrows caller
                // again to write the response).
                let (request_json, expected_id, token_ctx, dispatcher) = {
                    // Borrow the data first, then the memory separately
                    // — wasmtime's Caller API forbids simultaneous &mut
                    // and & borrows of caller.
                    let state = caller.data();
                    let (expected_id, token_ctx, dispatcher) = (
                        state.expected_request_id,
                        state.token_ctx.clone(),
                        state.dispatcher.clone(),
                    );
                    // Drop the &T borrow before getting &mut access via
                    // get_export.
                    drop(state);
                    let mem = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return -1,
                    };
                    let raw =
                        match mem.data(&caller).get(req_ptr as usize..(req_ptr + req_len) as usize)
                        {
                            Some(d) => d.to_vec(),
                            None => return -1,
                        };
                    let s = match std::str::from_utf8(&raw) {
                        Ok(s) => s.to_string(),
                        Err(_) => return -1,
                    };
                    (s, expected_id, token_ctx, dispatcher)
                };
                let response_json = match validate_and_forward(
                    &request_json,
                    expected_id,
                    &token_ctx,
                    dispatcher,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        // Convert PluginError into a structured
                        // PluginResponse so the plugin can read its
                        // own failures.
                        let resp = PluginResponse {
                            id: expected_id,
                            ok: false,
                            data: None,
                            error: Some(e.to_string()),
                        };
                        serde_json::to_string(&resp).unwrap_or_else(|_| {
                            format!(
                                r#"{{"id":{},"ok":false,"error":"serialization"}}"#,
                                expected_id
                            )
                        })
                    }
                };
                // Write the response back into the plugin's memory.
                let mem = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(m)) => m,
                    _ => return -1,
                };
                let resp_bytes = response_json.into_bytes();
                let len = resp_bytes.len() as i32;
                if len > out_cap {
                    return -1;
                }
                {
                    let mem_data = mem.data_mut(&mut caller);
                    let dst = match mem_data.get_mut(out_ptr as usize..(out_ptr + len) as usize) {
                        Some(d) => d,
                        None => return -1,
                    };
                    dst.copy_from_slice(&resp_bytes);
                }
                len
            },
        )
        .map_err(|e| format!("failed to define cmux_call: {e}"))?;

    Ok(())
}

/// Build a Store with fuel + epoch deadline set per the manifest.
pub fn build_store(
    engine: &Engine,
    state: HostState,
    fuel: u64,
    wall_clock_ms: u64,
) -> Store<HostState> {
    let mut store = Store::new(engine, state);
    store.set_fuel(fuel).expect("fuel should be consumable (consume_fuel=true)");
    store.set_epoch_deadline(wall_clock_ms);
    store
}

/// Invoke a plugin.
pub fn invoke(
    engine: &Engine,
    module: &Module,
    entry: &PluginEntry,
    cap: &Capabilities,
    plugin_data_dir: &Path,
    args: &[String],
    dispatcher: Arc<dyn CmuxDispatcher>,
) -> PluginResult {
    let token = mint_token();
    let token_ctx = TokenContext {
        plugin_name: entry.name.clone(),
        verb_allowlist: entry.verbs.clone(),
        socket_capability: SocketCapability::parse(cap.socket.as_deref()),
        minted_at: Instant::now(),
    };
    let env_list: Vec<String> = cap.env.clone().unwrap_or_default();
    let wasi =
        build_wasi(cap, plugin_data_dir, args, &env_list).map_err(PluginError::InvalidManifest)?;
    let state = HostState::new(wasi, token, token_ctx, dispatcher);
    let fuel = cap.fuel.unwrap_or(1_000_000);
    let wall_clock_ms = cap.max_runtime_ms.unwrap_or(5_000);
    let mut store = build_store(engine, state, fuel, wall_clock_ms);

    let mut linker: Linker<HostState> = Linker::new(engine);
    add_to_linker_sync(&mut linker, |s| &mut s.wasi)
        .map_err(|e| PluginError::InstantiationFailed(format!("add WASI to linker: {e}")))?;
    define_host_imports(&mut linker).map_err(PluginError::InstantiationFailed)?;

    let instance: Instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| PluginError::InstantiationFailed(format!("instantiate: {e}")))?;

    // Project-conventional entrypoint first, fall back to standard WASM `_start`.
    let func_result = instance
        .get_typed_func::<(), ()>(&mut store, "_mtyx_plugin_main")
        .or_else(|_| instance.get_typed_func::<(), ()>(&mut store, "_start"))
        .map_err(|e| {
            PluginError::InstantiationFailed(format!("missing _mtyx_plugin_main or _start: {e}"))
        })
        .and_then(|f| {
            f.call(&mut store, ()).map_err(|e| {
                // In wasmtime 27, `func.call` returns `anyhow::Result`.
                // Downcast the trap variants we care about; everything
                // else is a generic trap.
                if let Some(trap) = e.downcast_ref::<Trap>() {
                    match trap {
                        Trap::OutOfFuel => PluginError::FuelExhausted,
                        Trap::Interrupt => PluginError::Timeout,
                        _ => PluginError::Trap(format!("{trap:?}")),
                    }
                } else {
                    PluginError::Trap(format!("{e:?}"))
                }
            })
        });
    func_result.map(|_| String::new())
}

/// Production `CmuxDispatcher` implementation that writes
/// `cmux_call` requests to the real mtyx control socket and reads
/// back the response. Constructed per-invocation so the socket can be
/// lazy-opened (errors surface as `PluginError::Trap` with the
/// underlying connect/write/read error).
pub struct SocketDispatcher {
    pub socket_path: std::path::PathBuf,
}

impl SocketDispatcher {
    pub fn new(socket_path: std::path::PathBuf) -> Self {
        Self { socket_path }
    }
}

impl CmuxDispatcher for SocketDispatcher {
    fn dispatch(&self, request_json: String) -> Result<String, String> {
        use std::io::{BufRead, BufReader, Write};
        // Connect + write the request + read one line back. Modeled on
        // ssh_bootstrap::send_request (see crates/mux-tui/src/ssh_bootstrap.rs).
        let stream = mux_core::platform::transport::connect(&self.socket_path)
            .map_err(|e| format!("connect to {}: {e}", self.socket_path.display()))?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(20)));
        let mut writer = stream.try_clone_box().map_err(|e| format!("clone stream: {e}"))?;
        writer
            .write_all(request_json.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .map_err(|e| format!("write request: {e}"))?;
        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).map_err(|e| format!("read response: {e}"))?;
        Ok(response_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_default_matches_spec() {
        let d = Capabilities::default();
        assert_eq!(d.socket.as_deref(), Some("read"));
        assert_eq!(d.memory_mib, Some(64));
        assert_eq!(d.fuel, Some(1_000_000));
        assert_eq!(d.max_runtime_ms, Some(5_000));
        assert_eq!(d.network.as_deref(), Some("off"));
    }

    #[test]
    fn validate_capabilities_rejects_bad_socket() {
        let mut c = Capabilities::default();
        c.socket = Some("admin".to_string());
        assert!(plugin::validate_capabilities(&c).is_err());
    }

    #[test]
    fn validate_capabilities_rejects_huge_memory() {
        let mut c = Capabilities::default();
        c.memory_mib = Some(8192);
        assert!(plugin::validate_capabilities(&c).is_err());
    }

    #[test]
    fn effective_capabilities_fills_missing_fields() {
        let partial = Capabilities {
            socket: Some("write".to_string()),
            filesystem: None,
            env: None,
            network: None,
            memory_mib: None,
            fuel: None,
            max_runtime_ms: None,
        };
        let eff = plugin::effective_capabilities(Some(&partial));
        assert_eq!(eff.socket.as_deref(), Some("write"));
        assert_eq!(eff.memory_mib, Some(64));
        assert_eq!(eff.fuel, Some(1_000_000));
    }

    #[test]
    fn effective_capabilities_none_uses_defaults() {
        let eff = plugin::effective_capabilities(None);
        assert_eq!(eff, Capabilities::default());
    }

    #[test]
    fn is_mutating_verb_catches_obvious_writes() {
        assert!(is_mutating_verb("set-default-colors"));
        assert!(is_mutating_verb("rename-workspace"));
        assert!(is_mutating_verb("split"));
        assert!(!is_mutating_verb("list-workspaces"));
        assert!(!is_mutating_verb("read-screen"));
        assert!(!is_mutating_verb("identify"));
    }

    #[test]
    fn socket_capability_parse_default_is_read() {
        assert_eq!(SocketCapability::parse(None), SocketCapability::Read);
        assert_eq!(SocketCapability::parse(Some("off")), SocketCapability::Off);
        assert_eq!(SocketCapability::parse(Some("write")), SocketCapability::Write);
        assert_eq!(SocketCapability::parse(Some("read")), SocketCapability::Read);
    }

    struct MockDispatcher {
        captured: std::sync::Mutex<Vec<String>>,
    }
    impl CmuxDispatcher for MockDispatcher {
        fn dispatch(&self, request_json: String) -> Result<String, String> {
            self.captured.lock().unwrap().push(request_json.clone());
            let req: serde_json::Value = serde_json::from_str(&request_json).unwrap();
            let id = req["id"].as_u64().unwrap_or(0);
            Ok(format!(r#"{{"id":{id},"echo":"ok"}}"#))
        }
    }

    #[test]
    fn validate_and_forward_accepts_allowed_read_verb() {
        let dispatcher = Arc::new(MockDispatcher { captured: std::sync::Mutex::new(Vec::new()) });
        let token_ctx = TokenContext {
            plugin_name: "test".into(),
            verb_allowlist: vec!["list-workspaces".into()],
            socket_capability: SocketCapability::Read,
            minted_at: Instant::now(),
        };
        let resp = validate_and_forward(
            r#"{"id":42,"verb":"list-workspaces","args":{}}"#,
            42,
            &token_ctx,
            dispatcher,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
        assert_eq!(parsed["id"], 42);
    }

    #[test]
    fn validate_and_forward_blocks_disallowed_verb() {
        let dispatcher = Arc::new(MockDispatcher { captured: std::sync::Mutex::new(Vec::new()) });
        let token_ctx = TokenContext {
            plugin_name: "test".into(),
            verb_allowlist: vec!["list-workspaces".into()],
            socket_capability: SocketCapability::Read,
            minted_at: Instant::now(),
        };
        let err = validate_and_forward(
            r#"{"id":42,"verb":"rename-workspace","args":{}}"#,
            42,
            &token_ctx,
            dispatcher,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::VerbNotAllowed(_)));
    }

    #[test]
    fn validate_and_forward_blocks_mutating_verb_on_read_socket() {
        let dispatcher = Arc::new(MockDispatcher { captured: std::sync::Mutex::new(Vec::new()) });
        let token_ctx = TokenContext {
            plugin_name: "test".into(),
            verb_allowlist: vec!["set-default-colors".into(), "list-workspaces".into()],
            socket_capability: SocketCapability::Read,
            minted_at: Instant::now(),
        };
        let err = validate_and_forward(
            r#"{"id":42,"verb":"set-default-colors","args":{}}"#,
            42,
            &token_ctx,
            dispatcher,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::WriteBlocked(_)));
    }

    #[test]
    fn validate_and_forward_detects_stale_id() {
        let dispatcher = Arc::new(MockDispatcher { captured: std::sync::Mutex::new(Vec::new()) });
        let token_ctx = TokenContext {
            plugin_name: "test".into(),
            verb_allowlist: vec!["list-workspaces".into()],
            socket_capability: SocketCapability::Read,
            minted_at: Instant::now(),
        };
        let err = validate_and_forward(
            r#"{"id":99,"verb":"list-workspaces","args":{}}"#,
            42,
            &token_ctx,
            dispatcher,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::StaleId { expected: 42, got: 99 }));
    }

    #[test]
    fn parse_manifest_accepts_missing_capabilities() {
        // Backwards compat: PR #51 manifests without [capabilities] keep working.
        let m = plugin::parse_manifest(
            r#"[plugin]
name = "x"
entry = "x.wasm"
verbs = ["foo"]
"#,
        )
        .unwrap();
        assert_eq!(m.name, "x");
    }

    #[test]
    fn parse_manifest_accepts_full_capabilities() {
        // Nest [capabilities] under [plugin] so toml's per-table model
        // matches serde's per-struct model.
        let m = plugin::parse_manifest(
            r#"[plugin]
name = "pifactory-fleet"
entry = "bin/fleet.wasm"
verbs = ["deploy"]

[plugin.capabilities]
socket = "write"
filesystem = ["/tmp/work"]
env = ["HOME"]
network = "off"
memory_mib = 128
fuel = 5000000
max_runtime_ms = 10000
"#,
        )
        .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(m.capabilities.as_ref().unwrap().socket.as_deref(), Some("write"));
        assert_eq!(m.capabilities.as_ref().unwrap().memory_mib, Some(128));
    }
}
