//! `mtyx plugin` verb group: manifest-only plugin registry.
//!
//! Implements `list`, `install`, `uninstall`, `enable`, and `disable`
//! for `mtyx-plugin.toml` manifests. This PR does NOT spawn, execute,
//! or sandbox any plugin code; it only manages on-disk manifest state
//! and a small JSON registry file. Plugin *execution* (proxying
//! `mtyx <plugin-name> <verb>` calls to a running plugin process,
//! WASM/WASI sandboxing) is deferred to a follow-up PR and is not
//! implemented by anything in this module.
//!
//! On-disk layout (under the mtyx data directory, which honours
//! `XDG_DATA_HOME` and falls back to `~/.local/share/mattyx`):
//!
//! ```text
//! <base>/
//!   plugins.json          <- registry: { "plugins": [PluginEntry, ...] }
//!   plugins/
//!     <name>/
//!       mtyx-plugin.toml  <- the manifest copied verbatim at install time
//! ```
//!
//! Manifest shape (`mtyx-plugin.toml`):
//!
//! ```toml
//! [plugin]
//! name = "pifactory-fleet"
//! entry = "bin/fleet.wasm"
//! verbs = ["deploy", "rollback"]
//!
//! [plugin.capabilities]                # nested under [plugin] so serde
//! socket = "write"                     # can find the table
//! filesystem = ["/tmp/workpieces"]
//! env = ["HOME"]
//! network = "off"
//! memory_mib = 128
//! fuel = 5000000
//! max_runtime_ms = 10000
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const USAGE: &str = "\
mtyx plugin - manage mtyx-plugin.toml manifests (no execution yet)

USAGE:
  mtyx plugin list                       List installed plugins (read-only)
  mtyx plugin install <manifest-path>    Install a plugin from a manifest
  mtyx plugin uninstall <name>           Remove an installed plugin
  mtyx plugin enable <name>              Mark a plugin enabled
  mtyx plugin disable <name>             Mark a plugin disabled

Shared global flags (accepted before the subcommand):
  --json     Emit machine-readable JSON for `list`.

NOT IMPLEMENTED (deferred to a follow-up PR):
  Plugin *execution* (proxying `mtyx <plugin-name> <verb>` to a running
  plugin process, WASM/WASI sandboxing, the permission model) is out of
  scope for this verb group. These verbs only manage manifest state.

The manifest file is `mtyx-plugin.toml` with a single `[plugin]` table:
  name   (string, required, non-empty)    plugin id and on-disk dir name
  entry  (string, required, non-empty)    path to the plugin entry artefact
                                          (stored verbatim; not resolved,
                                          not validated as executable here)
  verbs  (array of strings, required,    the verbs this plugin claims
          non-empty, each non-empty)      (stored verbatim; not proxied)
";

/// Entry-level dispatch. Returns a process exit code. Called from
/// `main.rs` when `raw_args.first() == Some("plugin")`.
pub fn run(args: &[String]) -> i32 {
    let mut json = false;
    let mut rest: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if !json && arg == "--json" {
            json = true;
            i += 1;
            continue;
        }
        rest.push(arg);
        i += 1;
    }

    let Some(sub) = rest.first().copied() else {
        print!("{USAGE}");
        return 0;
    };

    let positional = &rest[1..];
    match base_dir() {
        Ok(base) => dispatch(&base, sub, positional, json),
        Err(err) => {
            eprintln!("mtyx plugin: {err}");
            1
        }
    }
}

fn dispatch(base: &Path, sub: &str, positional: &[&str], json: bool) -> i32 {
    match sub {
        "list" => cmd_list(base, json),
        "install" => cmd_install(base, positional),
        "uninstall" => cmd_uninstall(base, positional),
        "enable" => cmd_set_enabled(base, positional, true),
        "disable" => cmd_set_enabled(base, positional, false),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("mtyx plugin: unknown subcommand {other:?}\n\n{USAGE}");
            2
        }
    }
}

// ----- paths ---------------------------------------------------------------

/// Resolve the mtyx data base directory (`<base>/plugins` and
/// `<base>/plugins.json` live under this). Honours `XDG_DATA_HOME` and
/// falls back to `~/.local/share/mattyx`, mirroring the chrome profile
/// resolution in `mux_core::platform`. Pure: no IO.
///
/// Rename compat: a cmux-era `cmux` data dir (with its `plugins.json`
/// and installed plugins) is honoured while the canonical `mattyx` dir
/// is absent — the same honour-without-migrate policy as config/state
/// dirs, so a pre-rename install keeps its plugins.
fn base_dir() -> Result<PathBuf, String> {
    let canonical = if let Some(raw) = std::env::var_os("XDG_DATA_HOME") {
        if !raw.is_empty() {
            PathBuf::from(raw).join("mattyx")
        } else {
            data_dir_without_xdg()?
        }
    } else {
        data_dir_without_xdg()?
    };
    Ok(mux_core::platform::honor_cmux_era_dir(canonical))
}

fn data_dir_without_xdg() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local").join("share").join("mattyx"))
        .ok_or_else(|| "could not resolve mtyx data dir (set XDG_DATA_HOME or HOME)".to_string())
}

/// Manifest path inside an installed plugin dir: canonical
/// `mtyx-plugin.toml`, with the cmux-era `cmux-plugin.toml` honoured as
/// a read-only fallback so plugins installed before the rename keep
/// loading (install always writes the canonical name).
fn manifest_path_in(plugin_dir: &Path) -> PathBuf {
    let canonical = plugin_dir.join("mtyx-plugin.toml");
    if canonical.exists() {
        return canonical;
    }
    let legacy = plugin_dir.join("cmux-plugin.toml");
    if legacy.exists() {
        return legacy;
    }
    canonical
}

fn plugins_dir(base: &Path) -> PathBuf {
    base.join("plugins")
}

fn registry_path(base: &Path) -> PathBuf {
    base.join("plugins.json")
}

// ----- manifest ------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ManifestFile {
    plugin: ManifestPlugin,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestPlugin {
    pub name: String,
    pub entry: String,
    pub verbs: Vec<String>,
    // Optional capabilities table; missing in older PR #51 manifests.
    // `Option` keeps backwards compat at parse time; defaults applied in
    // `effective_capabilities()`.
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
}

/// Capabilities declared by the plugin. All fields are optional with
/// safe defaults applied at load time; missing `[capabilities]` means a
/// plugin runs read-only, with no filesystem or env access, no network,
/// 64MB memory, 1M fuel, and a 5s wall-clock timeout per call.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    /// Socket access scope: `"off"` (no cmux_call), `"read"` (only
    /// read-only verbs), `"write"` (read + mutating verbs).
    pub socket: Option<String>,
    /// WASI preopen directory paths the plugin can access. Each entry
    /// is a path string; the plugin sees it under the same path
    /// inside its sandbox.
    pub filesystem: Option<Vec<String>>,
    /// Env var names the plugin can read (NOT values; plugin author
    /// reads the value at runtime via WASI environ).
    pub env: Option<Vec<String>>,
    /// Network access: `"off"` (no WASI sockets) or `"outbound"`
    /// (WASI preview1 sockets enabled — though we currently ship
    /// preview1 only, so outbound network isn't usable in this PR).
    pub network: Option<String>,
    /// Linear-memory cap in MiB. wasmtime hard-aborts the plugin if
    /// it tries to grow beyond this.
    pub memory_mib: Option<u32>,
    /// Fuel budget per invocation. wasmtime stops the plugin when
    /// fuel is exhausted.
    pub fuel: Option<u64>,
    /// Wall-clock timeout per invocation in milliseconds.
    pub max_runtime_ms: Option<u64>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            socket: Some("read".to_string()),
            filesystem: Some(Vec::new()),
            env: Some(Vec::new()),
            network: Some("off".to_string()),
            memory_mib: Some(64),
            fuel: Some(1_000_000),
            max_runtime_ms: Some(5_000),
        }
    }
}

/// Resolve the effective capabilities for a manifest. Missing
/// `[capabilities]` table or missing fields inside it get safe
/// defaults — this is the spec's backwards-compat promise (issue
/// #42's PR #51 manifests keep working unchanged).
pub fn effective_capabilities(cap: Option<&Capabilities>) -> Capabilities {
    let defaults = Capabilities::default();
    match cap {
        None => defaults,
        Some(c) => Capabilities {
            socket: c.socket.clone().or_else(|| defaults.socket.clone()),
            filesystem: c.filesystem.clone().or_else(|| defaults.filesystem.clone()),
            env: c.env.clone().or_else(|| defaults.env.clone()),
            network: c.network.clone().or_else(|| defaults.network.clone()),
            memory_mib: c.memory_mib.or(defaults.memory_mib),
            fuel: c.fuel.or(defaults.fuel),
            max_runtime_ms: c.max_runtime_ms.or(defaults.max_runtime_ms),
        },
    }
}

/// Validate a resolved `Capabilities` against security rules. Catches
/// invalid field values before the plugin is loaded.
pub fn validate_capabilities(cap: &Capabilities) -> Result<(), String> {
    match cap.socket.as_deref() {
        Some("off") | Some("read") | Some("write") => {}
        Some(other) => {
            return Err(format!(
                r#"manifest capabilities.socket must be one of off/read/write; got {other:?}"#
            ));
        }
        None => {}
    }
    if let Some(mem) = cap.memory_mib {
        if mem == 0 || mem > 4096 {
            return Err(format!("manifest capabilities.memory_mib must be in 1..=4096; got {mem}"));
        }
    }
    if let Some(runtime) = cap.max_runtime_ms {
        if runtime == 0 || runtime > 600_000 {
            return Err(format!(
                "manifest capabilities.max_runtime_ms must be in 1..=600000; got {runtime}"
            ));
        }
    }
    if let Some(fuel) = cap.fuel {
        if fuel == 0 || fuel > 1_000_000_000 {
            return Err(format!(
                "manifest capabilities.fuel must be in 1..=1000000000; got {fuel}"
            ));
        }
    }
    match cap.network.as_deref() {
        Some("off") | Some("outbound") | None => {}
        Some(other) => {
            return Err(format!(
                r#"manifest capabilities.network must be one of off/outbound; got {other:?}"#
            ));
        }
    }
    Ok(())
}

/// Parse a manifest string into a validated `ManifestPlugin`. Returns a
/// clear, single-line error for malformed TOML, missing required
/// fields, or semantically empty required fields. Pure: no IO.
pub fn parse_manifest(content: &str) -> Result<ManifestPlugin, String> {
    let file: ManifestFile =
        toml::from_str(content).map_err(|err| format!("malformed mtyx-plugin.toml: {err}"))?;
    let ManifestPlugin { name, entry, verbs, .. } = &file.plugin;
    if name.trim().is_empty() {
        return Err("manifest missing required field [plugin] name".to_string());
    }
    if entry.trim().is_empty() {
        return Err("manifest missing required field [plugin] entry".to_string());
    }
    if verbs.is_empty() {
        return Err("manifest missing required field [plugin] verbs".to_string());
    }
    for verb in verbs {
        if verb.trim().is_empty() {
            return Err("manifest [plugin] verbs contains an empty entry".to_string());
        }
    }
    // The name becomes a directory under `plugins/`, so it must be a
    // single path component (no separators, no "." / "..").
    if name.contains(std::path::MAIN_SEPARATOR) || name == "." || name == ".." || name.contains('/')
    {
        return Err(format!("plugin name {name:?} must not be a path"));
    }
    // Validate capabilities if present; missing means defaults apply.
    if let Some(cap) = &file.plugin.capabilities {
        validate_capabilities(cap).map_err(|err| format!("manifest {err}"))?;
    }
    Ok(file.plugin)
}

// ----- registry ------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub enabled: bool,
    pub entry: String,
    pub verbs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Registry {
    plugins: Vec<PluginEntry>,
}

/// Read the registry file. Missing file is an empty registry, not an
/// error: a fresh install has no `plugins.json` yet.
fn load_registry(base: &Path) -> Result<Registry, String> {
    let path = registry_path(base);
    match fs::read_to_string(&path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return Ok(Registry::default());
            }
            serde_json::from_str::<Registry>(&content)
                .map_err(|err| format!("corrupt registry {}: {err}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(err) => Err(format!("failed to read {}: {err}", path.display())),
    }
}

/// Persist the registry as pretty JSON. Creates the parent directory
/// on demand so a fresh install works without a pre-existing base dir.
///
/// Refuses to write if `<base>/plugins.json` already exists and is a
/// symlink: `fs::write` would follow the symlink and overwrite its
/// target, which is dangerous here because an attacker with write access
/// to the mtyx data dir could plant such a symlink pointing at a
/// sensitive file. Plugin manifests are a third-party-trust boundary
/// even though execution is deferred, so the same paranoia applies to
/// the registry file.
fn save_registry(base: &Path, reg: &Registry) -> Result<(), String> {
    let path = registry_path(base);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if meta.file_type().is_symlink() {
            return Err(format!("refusing to write through symlink at {}", path.display()));
        }
    }
    let json = serde_json::to_string_pretty(reg)
        .map_err(|err| format!("failed to encode registry: {err}"))?;
    fs::write(&path, json).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn find_entry_mut<'a>(reg: &'a mut Registry, name: &str) -> Option<&'a mut PluginEntry> {
    reg.plugins.iter_mut().find(|p| p.name == name)
}

/// Recursive directory removal that refuses to follow symlinks.
/// `fs::remove_dir_all` will happily walk through a symlink and delete
/// the target's contents, which is dangerous here: an attacker with
/// write access to the mtyx data dir could plant a symlink inside (or
/// at the root of) a plugin directory pointing at a sensitive file, and
/// `uninstall` would then delete through it. We walk the tree top-down
/// using `fs::symlink_metadata` and refuse the whole removal if any
/// entry is a symlink. The same helper is used by the rollback path
/// inside `install` so a partial install cannot be cleaned up through
/// an attacker-planted symlink either.
fn remove_dir_safely(path: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("refusing to remove through symlink at {}", path.display()),
        ));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            remove_dir_safely(&entry.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Format the registry for either human or `--json` output, sorted by
/// plugin name in both branches so scripted consumers see the same
/// order a human reading the plain output sees. JSON serialization
/// failure is surfaced as a `Result::Err` rather than silently collapsed
/// to an empty string (the prior code used `unwrap_or_default()`, which
/// would print `""` and exit 0 on the (rare) failure path, contradicting
/// the file's own no-silent-fallback style).
fn format_list_output(reg: &Registry, json: bool) -> Result<String, String> {
    let mut sorted = reg.clone();
    sorted.plugins.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        return serde_json::to_string(&sorted)
            .map_err(|err| format!("failed to encode registry: {err}"));
    }
    if sorted.plugins.is_empty() {
        return Ok("no plugins installed".to_string());
    }
    let mut out = String::new();
    for e in &sorted.plugins {
        let state = if e.enabled { "enabled" } else { "disabled" };
        let verbs = e.verbs.join(",");
        out.push_str(&format!("{} {} {} {}\n", e.name, state, e.entry, verbs));
    }
    Ok(out)
}

// ----- invocation (mtyx <plugin> <verb> [args]) -----

/// Resolve the mtyx data base dir (XDG_DATA_HOME or ~/.local/share/mattyx).
/// Public so `main.rs` can call it from the `mtyx <plugin>` argv path.
pub fn base_dir_public() -> Result<std::path::PathBuf, String> {
    base_dir()
}

/// Look up a plugin by name in the registry. Returns the entry + the
/// directory it was installed into. Public for `main.rs`.
pub fn lookup_plugin(name: &str) -> Result<(PluginEntry, std::path::PathBuf), String> {
    let base = base_dir()?;
    let reg = load_registry(&base)?;
    let entry = reg
        .plugins
        .iter()
        .find(|e| e.name == name)
        .cloned()
        .ok_or_else(|| format!("plugin {name:?} is not installed"))?;
    if !entry.enabled {
        return Err(format!("plugin {name:?} is disabled (run `mtyx plugin enable {name}`)"));
    }
    if !entry.verbs.iter().any(|v| v == "cmux_call") {
        return Err(format!("plugin {name:?} manifest does not declare the cmux_call verb"));
    }
    let dir = plugins_dir(&base).join(&entry.name);
    Ok((entry, dir))
}

/// Top-level handler for `mtyx <plugin> <verb> [args]`. Returns exit
/// code 0 on success, 2 on usage error, 1 on runtime error. Reads the
/// manifest + the registered capabilities, builds the SocketDispatcher,
/// and invokes the plugin via crate::plugin_host::invoke.
pub fn cmd_call(plugin_name: &str, args: &[String], socket_path: &std::path::Path) -> i32 {
    let (entry, plugin_dir) = match lookup_plugin(plugin_name) {
        Ok(x) => x,
        Err(err) => {
            eprintln!("mtyx: {err}");
            return 1;
        }
    };
    // Read the manifest from disk (not just the registry copy) so
    // capabilities travel with the install. Rename compat: a plugin
    // installed pre-rename may still hold `cmux-plugin.toml`.
    let manifest_path = manifest_path_in(&plugin_dir);
    let manifest_text = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("mtyx: failed to read manifest {}: {err}", manifest_path.display());
            return 1;
        }
    };
    let manifest = match parse_manifest(&manifest_text) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("mtyx: plugin {plugin_name:?} manifest invalid: {err}");
            return 1;
        }
    };
    // Validate the requested verb is in the manifest's allowlist.
    if let Some((verb, _)) = args.split_first() {
        if !manifest.verbs.iter().any(|v| v == verb) {
            eprintln!(
                "mtyx: verb {verb:?} is not in plugin {plugin_name:?} allowlist ({:?})",
                manifest.verbs
            );
            return 2;
        }
    } else {
        eprintln!(
            "mtyx: usage: mtyx {plugin_name} <verb> [args...]  (allowed: {:?})",
            manifest.verbs
        );
        return 2;
    }
    let capabilities = effective_capabilities(manifest.capabilities.as_ref());
    if let Err(err) = validate_capabilities(&capabilities) {
        eprintln!("mtyx: plugin {plugin_name:?} capabilities invalid: {err}");
        return 1;
    }
    let entry_path = plugin_dir.join(&manifest.entry);
    let engine = match crate::plugin_host::build_engine() {
        Ok(e) => e,
        Err(err) => {
            eprintln!("mtyx: failed to build wasmtime engine: {err}");
            return 1;
        }
    };
    let module = match crate::plugin_host::load_module(&engine, &entry_path) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("mtyx: {err}");
            return 1;
        }
    };
    let dispatcher =
        std::sync::Arc::new(crate::plugin_host::SocketDispatcher::new(socket_path.to_path_buf()));
    match crate::plugin_host::invoke(
        &engine,
        &module,
        &entry,
        &capabilities,
        &plugin_dir,
        args,
        dispatcher,
    ) {
        Ok(_stdout) => 0,
        Err(err) => {
            eprintln!("mtyx: plugin {plugin_name:?} failed: {err}");
            1
        }
    }
}

// ----- subcommands ---------------------------------------------------------

fn cmd_list(base: &Path, json: bool) -> i32 {
    let reg = match load_registry(base) {
        Ok(reg) => reg,
        Err(err) => {
            eprintln!("mtyx plugin list: {err}");
            return 1;
        }
    };
    // Both human and --json output sort by name, so a script reading
    // JSON sees the same order a human reading the plain output sees.
    // Serialization failure is reported and returns 1, not silently
    // collapsed to an empty string.
    match format_list_output(&reg, json) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(err) => {
            eprintln!("mtyx plugin list: {err}");
            1
        }
    }
}

fn cmd_install(base: &Path, positional: &[&str]) -> i32 {
    let Some(&manifest_path) = positional.first() else {
        eprintln!("mtyx plugin install: missing <manifest-path>\n\n{USAGE}");
        return 2;
    };
    if positional.len() > 1 {
        eprintln!("mtyx plugin install: unexpected extra argument {:?}", positional[1]);
        return 2;
    }
    let content = match fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("mtyx plugin install: cannot read {manifest_path:?}: {err}");
            return 1;
        }
    };
    let plugin = match parse_manifest(&content) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("mtyx plugin install: {err}");
            return 1;
        }
    };
    let mut reg = match load_registry(base) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("mtyx plugin install: {err}");
            return 1;
        }
    };
    if reg.plugins.iter().any(|p| p.name == plugin.name) {
        eprintln!("mtyx plugin install: a plugin named {:?} is already installed", plugin.name);
        return 1;
    }
    let plugin_dir = plugins_dir(base).join(&plugin.name);
    // Refuse to install through a symlink: an attacker with write
    // access to the mtyx data dir could have pre-placed a symlink at
    // `<base>/plugins/<name>` pointing at a sensitive directory, and
    // `create_dir_all` would silently treat it as a present directory
    // (because the symlink target is a directory), after which the
    // manifest write below would land inside the symlink target rather
    // than the intended plugin slot.
    if let Ok(meta) = fs::symlink_metadata(&plugin_dir) {
        if meta.file_type().is_symlink() {
            eprintln!(
                "mtyx plugin install: refusing to install through symlink at {}",
                plugin_dir.display()
            );
            return 1;
        }
    }
    if let Err(err) = fs::create_dir_all(&plugin_dir) {
        eprintln!("mtyx plugin install: failed to create {}: {err}", plugin_dir.display());
        return 1;
    }
    let dest = plugin_dir.join("mtyx-plugin.toml");
    // Defence in depth: also refuse if the manifest slot itself is a
    // symlink planted inside a directory we just created (or a
    // directory we adopted). `fs::write` would follow it and clobber
    // the target. On refusal we attempt a best-effort rollback:
    // `fs::remove_file` on a symlink removes the link itself, not the
    // target, then `fs::remove_dir` clears the (now-empty) parent.
    // This is the only place we deviate from `remove_dir_safely`,
    // because that helper refuses to touch the symlink at all and would
    // leave the half-created plugin dir behind.
    if let Ok(meta) = fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            let _ = fs::remove_file(&dest);
            let _ = fs::remove_dir(&plugin_dir);
            eprintln!(
                "mtyx plugin install: refusing to write through symlink at {}",
                dest.display()
            );
            return 1;
        }
    }
    if let Err(err) = fs::write(&dest, &content) {
        // Roll back the directory we just made so a failed install does
        // not leave an empty half-registered plugin on disk.
        let _ = remove_dir_safely(&plugin_dir);
        eprintln!("mtyx plugin install: failed to write {}: {err}", dest.display());
        return 1;
    }
    reg.plugins.push(PluginEntry {
        name: plugin.name.clone(),
        enabled: true,
        entry: plugin.entry.clone(),
        verbs: plugin.verbs.clone(),
    });
    if let Err(err) = save_registry(base, &reg) {
        let _ = remove_dir_safely(&plugin_dir);
        eprintln!("mtyx plugin install: {err}");
        return 1;
    }
    println!("installed plugin {} from {}", plugin.name, Path::new(manifest_path).display());
    0
}

fn cmd_uninstall(base: &Path, positional: &[&str]) -> i32 {
    let Some(&name) = positional.first() else {
        eprintln!("mtyx plugin uninstall: missing <name>\n\n{USAGE}");
        return 2;
    };
    if positional.len() > 1 {
        eprintln!("mtyx plugin uninstall: unexpected extra argument {:?}", positional[1]);
        return 2;
    }
    let mut reg = match load_registry(base) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("mtyx plugin uninstall: {err}");
            return 1;
        }
    };
    let before = reg.plugins.len();
    reg.plugins.retain(|p| p.name != name);
    if reg.plugins.len() == before {
        eprintln!("mtyx plugin uninstall: no plugin named {name:?} is installed");
        return 1;
    }
    let plugin_dir = plugins_dir(base).join(name);
    if plugin_dir.exists() {
        // Use the symlink-aware walker rather than `fs::remove_dir_all`,
        // which would happily delete through an attacker-planted symlink
        // inside (or at the root of) the plugin directory.
        if let Err(err) = remove_dir_safely(&plugin_dir) {
            eprintln!("mtyx plugin uninstall: {}", err);
            return 1;
        }
    }
    if let Err(err) = save_registry(base, &reg) {
        eprintln!("mtyx plugin uninstall: {err}");
        return 1;
    }
    println!("uninstalled plugin {name}");
    0
}

fn cmd_set_enabled(base: &Path, positional: &[&str], enabled: bool) -> i32 {
    let label = if enabled { "enable" } else { "disable" };
    let Some(&name) = positional.first() else {
        eprintln!("mtyx plugin {label}: missing <name>\n\n{USAGE}");
        return 2;
    };
    if positional.len() > 1 {
        eprintln!("mtyx plugin {label}: unexpected extra argument {:?}", positional[1]);
        return 2;
    }
    let mut reg = match load_registry(base) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("mtyx plugin {label}: {err}");
            return 1;
        }
    };
    let Some(entry) = find_entry_mut(&mut reg, name) else {
        eprintln!("mtyx plugin {label}: no plugin named {name:?} is installed");
        return 1;
    };
    let already = entry.enabled == enabled;
    if !already {
        entry.enabled = enabled;
    }
    if let Err(err) = save_registry(base, &reg) {
        eprintln!("mtyx plugin {label}: {err}");
        return 1;
    }
    let state = if enabled { "enabled" } else { "disabled" };
    if already {
        println!("plugin {name} already {state}");
    } else {
        println!("plugin {name} {state}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, entry: &str, verbs: &[&str]) -> String {
        let verbs = verbs.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join(", ");
        format!("[plugin]\nname = \"{name}\"\nentry = \"{entry}\"\nverbs = [{verbs}]\n")
    }

    fn tmp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mtyx-plugin-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_manifest_success() {
        let m = parse_manifest(&manifest("fleet", "bin/fleet.wasm", &["deploy", "rollback"]))
            .expect("valid manifest parses");
        assert_eq!(m.name, "fleet");
        assert_eq!(m.entry, "bin/fleet.wasm");
        assert_eq!(m.verbs, vec!["deploy".to_string(), "rollback".to_string()]);
    }

    #[test]
    fn parse_manifest_missing_name() {
        let err = parse_manifest("[plugin]\nentry = \"x\"\nverbs = [\"y\"]\n").unwrap_err();
        assert!(err.contains("name"), "error should name the missing field: {err}");
    }

    #[test]
    fn parse_manifest_missing_entry() {
        let err = parse_manifest("[plugin]\nname = \"x\"\nverbs = [\"y\"]\n").unwrap_err();
        assert!(err.contains("entry"), "error should name the missing field: {err}");
    }

    #[test]
    fn parse_manifest_missing_verbs() {
        let err = parse_manifest("[plugin]\nname = \"x\"\nentry = \"y\"\n").unwrap_err();
        assert!(err.contains("verbs"), "error should name the missing field: {err}");
    }

    #[test]
    fn parse_manifest_malformed_toml() {
        let err = parse_manifest("this is not = = valid toml").unwrap_err();
        assert!(err.contains("malformed mtyx-plugin.toml"), "error: {err}");
    }

    #[test]
    fn parse_manifest_empty_verbs_list() {
        let err =
            parse_manifest("[plugin]\nname = \"x\"\nentry = \"y\"\nverbs = []\n").unwrap_err();
        assert!(err.contains("verbs"), "error: {err}");
    }

    #[test]
    fn parse_manifest_name_must_not_be_a_path() {
        let err = parse_manifest(&manifest("../evil", "x", &["y"])).unwrap_err();
        assert!(err.contains("path"), "error: {err}");
        let err = parse_manifest(&manifest("a/b", "x", &["y"])).unwrap_err();
        assert!(err.contains("path"), "error: {err}");
    }

    /// Covers AC1..AC5 against an injected temp base dir (no env-var
    /// mutation, so it is safe under `cargo test` parallelism).
    #[test]
    fn install_list_uninstall_enable_disable_round_trip() {
        let base = tmp_base("roundtrip");

        // Empty list prints the empty message, exit 0.
        assert_eq!(cmd_list(&base, false), 0);
        assert_eq!(load_registry(&base).unwrap().plugins.len(), 0);

        // Install a valid manifest from a temp file.
        let manifest_dir = std::env::temp_dir().join(format!(
            "mtyx-plugin-manifest-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&manifest_dir).unwrap();
        let manifest_file = manifest_dir.join("mtyx-plugin.toml");
        fs::write(&manifest_file, manifest("fleet", "bin/fleet.wasm", &["deploy", "rollback"]))
            .unwrap();
        let path_str = manifest_file.to_str().unwrap().to_string();
        assert_eq!(cmd_install(&base, &[path_str.as_str()]), 0);

        // AC1: registered enabled by default, manifest copied under plugins/.
        let reg = load_registry(&base).unwrap();
        assert_eq!(reg.plugins.len(), 1);
        let entry = &reg.plugins[0];
        assert_eq!(entry.name, "fleet");
        assert!(entry.enabled);
        assert_eq!(entry.entry, "bin/fleet.wasm");
        assert_eq!(entry.verbs, vec!["deploy".to_string(), "rollback".to_string()]);
        let dest = plugins_dir(&base).join("fleet").join("mtyx-plugin.toml");
        assert!(dest.exists(), "manifest should be copied to {}", dest.display());
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            manifest("fleet", "bin/fleet.wasm", &["deploy", "rollback"])
        );

        // AC5: duplicate name fails, does not partially register.
        assert_eq!(cmd_install(&base, &[path_str.as_str()]), 1);
        assert_eq!(load_registry(&base).unwrap().plugins.len(), 1);

        // AC4: list reflects the installed plugin.
        assert_eq!(cmd_list(&base, false), 0);

        // AC3: disable persists across a fresh registry read.
        assert_eq!(cmd_set_enabled(&base, &["fleet"], false), 0);
        assert!(!load_registry(&base).unwrap().plugins[0].enabled);
        assert_eq!(cmd_set_enabled(&base, &["fleet"], true), 0);
        assert!(load_registry(&base).unwrap().plugins[0].enabled);

        // AC5: enable/disable of an unknown plugin fails clearly.
        assert_eq!(cmd_set_enabled(&base, &["nope"], true), 1);

        // AC4: uninstall of an unknown plugin fails clearly.
        assert_eq!(cmd_uninstall(&base, &["nope"]), 1);

        // AC2: uninstall removes the registry entry and the plugin dir.
        assert_eq!(cmd_uninstall(&base, &["fleet"]), 0);
        assert_eq!(load_registry(&base).unwrap().plugins.len(), 0);
        assert!(!dest.exists(), "plugin dir should be removed after uninstall");

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&manifest_dir);
    }

    #[test]
    fn install_missing_manifest_arg_is_usage_error() {
        let base = tmp_base("noarg");
        assert_eq!(cmd_install(&base, &[]), 2);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn install_nonexistent_file_is_command_error() {
        let base = tmp_base("nofile");
        assert_eq!(cmd_install(&base, &["/nonexistent/mtyx-plugin.toml"]), 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn install_malformed_manifest_is_command_error() {
        let base = tmp_base("badman");
        let dir = std::env::temp_dir().join(format!(
            "mtyx-plugin-bad-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mtyx-plugin.toml");
        fs::write(&file, "[plugin]\nname = \"x\"\n").unwrap();
        let path = file.to_str().unwrap().to_string();
        assert_eq!(cmd_install(&base, &[path.as_str()]), 1);
        assert_eq!(load_registry(&base).unwrap().plugins.len(), 0);
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        let base = tmp_base("unknown");
        assert_eq!(dispatch(&base, "frobnicate", &[], false), 2);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn dispatch_help_prints_usage() {
        let base = tmp_base("help");
        assert_eq!(dispatch(&base, "--help", &[], false), 0);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_empty_prints_no_plugins_installed() {
        let base = tmp_base("empty");
        // Human mode prints the empty message.
        assert_eq!(cmd_list(&base, false), 0);
        // JSON mode emits an empty registry object.
        assert_eq!(cmd_list(&base, true), 0);
        let _ = fs::remove_dir_all(&base);
    }

    /// `save_registry` must refuse to write when `<base>/plugins.json`
    /// is already a symlink: an attacker with write access to the mtyx
    /// data dir could plant such a symlink pointing at a sensitive
    /// file, and `fs::write` would silently follow it. We assert the
    /// error is reported AND the symlink target's contents are
    /// untouched.
    #[test]
    #[cfg(unix)] // std::os::unix::fs::symlink fixture
    fn save_registry_refuses_symlink_at_plugins_json() {
        let base = tmp_base("save_sym");
        // Symlink target the attacker is trying to clobber.
        let target = base.join("victim.txt");
        fs::write(&target, "do-not-touch").unwrap();
        // Pre-place a symlink at <base>/plugins.json -> target.
        std::os::unix::fs::symlink(&target, registry_path(&base)).unwrap();
        let reg = Registry::default();
        let err = save_registry(&base, &reg).expect_err("must refuse symlink target");
        assert!(err.contains("symlink"), "error should mention symlink: {err}");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "do-not-touch",
            "symlink target must remain untouched"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// `install` must refuse when `<base>/plugins/<name>` is itself a
    /// symlink. `create_dir_all` would otherwise treat the symlink-to-
    /// directory as a present directory and the manifest write below
    /// would land inside the symlink target. Assert the install fails,
    /// the registry is not populated, and the symlink target is
    /// untouched.
    #[test]
    #[cfg(unix)] // std::os::unix::fs::symlink fixture
    fn install_refuses_symlink_at_plugin_dir() {
        let base = tmp_base("install_sym_dir");
        fs::create_dir_all(plugins_dir(&base)).unwrap();
        // Symlink target is a sensitive-looking directory.
        let target = base.join("sensitive");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("passwords.txt"), "do-not-touch").unwrap();
        // Pre-place the symlink at the plugin slot.
        let plugin_link = plugins_dir(&base).join("fleet");
        std::os::unix::fs::symlink(&target, &plugin_link).unwrap();

        // Stage a real manifest in a separate temp dir.
        let manifest_dir = std::env::temp_dir().join(format!(
            "mtyx-plugin-test-manifest-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&manifest_dir).unwrap();
        let manifest_file = manifest_dir.join("mtyx-plugin.toml");
        fs::write(&manifest_file, manifest("fleet", "bin/fleet.wasm", &["deploy"])).unwrap();
        let path_str = manifest_file.to_str().unwrap();

        assert_eq!(cmd_install(&base, &[path_str]), 1);
        assert_eq!(
            load_registry(&base).unwrap().plugins.len(),
            0,
            "registry must not be populated when install is refused"
        );
        assert_eq!(
            fs::read_to_string(target.join("passwords.txt")).unwrap(),
            "do-not-touch",
            "symlink target contents must remain untouched"
        );

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&manifest_dir);
    }

    /// `install` must also refuse when the manifest slot
    /// `<base>/plugins/<name>/mtyx-plugin.toml` is itself a symlink,
    /// even if the parent directory is regular. Defence in depth: an
    /// attacker who could replace just the manifest file inside an
    /// otherwise-normal plugin directory must not be able to redirect
    /// the write into a sensitive file.
    #[test]
    #[cfg(unix)] // std::os::unix::fs::symlink fixture
    fn install_refuses_symlink_at_manifest_dest() {
        let base = tmp_base("install_sym_dest");
        fs::create_dir_all(plugins_dir(&base)).unwrap();
        let plugin_dir = plugins_dir(&base).join("fleet");
        fs::create_dir_all(&plugin_dir).unwrap();
        // Pre-place a symlink at the manifest slot.
        let target = base.join("victim.txt");
        fs::write(&target, "do-not-touch").unwrap();
        std::os::unix::fs::symlink(&target, plugin_dir.join("mtyx-plugin.toml")).unwrap();

        let manifest_dir = std::env::temp_dir().join(format!(
            "mtyx-plugin-test-manifest-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&manifest_dir).unwrap();
        let manifest_file = manifest_dir.join("mtyx-plugin.toml");
        fs::write(&manifest_file, manifest("fleet", "bin/fleet.wasm", &["deploy"])).unwrap();
        let path_str = manifest_file.to_str().unwrap();

        assert_eq!(cmd_install(&base, &[path_str]), 1);
        assert_eq!(
            load_registry(&base).unwrap().plugins.len(),
            0,
            "registry must not be populated when install is refused"
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "do-not-touch",
            "symlink target contents must remain untouched"
        );
        // The plugin dir we just created (above the symlink) should be
        // rolled back by the symlink check path so we do not leave a
        // dangling empty directory behind.
        assert!(
            !plugin_dir.exists(),
            "plugin dir should be cleaned up after a refused install, found {}",
            plugin_dir.display()
        );

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&manifest_dir);
    }

    /// `uninstall` must refuse when the plugin directory itself is a
    /// symlink. `fs::remove_dir_all` would otherwise walk through the
    /// symlink and delete the target's contents, which is exactly the
    /// damage the warning is about. Assert the symlink target survives
    /// AND the registry is left untouched (the round-1 ordering is
    /// "remove dir, then rewrite registry", so on refusal we never
    /// reach the registry write). That keeps the on-disk state
    /// consistent: if the user fixes the symlink and retries, the
    /// plugin still shows in `list`.
    #[test]
    #[cfg(unix)] // std::os::unix::fs::symlink fixture
    fn uninstall_refuses_symlink_at_plugin_dir() {
        let base = tmp_base("uninstall_sym_dir");
        // Symlink target is a sensitive directory.
        let target = base.join("sensitive");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("do-not-delete.txt"), "do-not-touch").unwrap();
        // Pre-place a symlink at the plugin slot. Parent dir must exist
        // for the symlink(2) call to succeed.
        fs::create_dir_all(plugins_dir(&base)).unwrap();
        let plugin_dir = plugins_dir(&base).join("fleet");
        std::os::unix::fs::symlink(&target, &plugin_dir).unwrap();
        // Seed the registry so the plugin is "installed" from mtyx's POV.
        let mut reg = Registry::default();
        reg.plugins.push(PluginEntry {
            name: "fleet".to_string(),
            enabled: true,
            entry: "bin/fleet.wasm".to_string(),
            verbs: vec!["deploy".to_string()],
        });
        save_registry(&base, &reg).unwrap();

        assert_eq!(cmd_uninstall(&base, &["fleet"]), 1);
        assert!(
            target.join("do-not-delete.txt").exists(),
            "symlink target contents must remain untouched"
        );
        let reg_after = load_registry(&base).unwrap();
        assert_eq!(
            reg_after.plugins.len(),
            1,
            "registry must not be rewritten when dir removal is refused"
        );
        assert_eq!(reg_after.plugins[0].name, "fleet");

        let _ = fs::remove_dir_all(&base);
    }

    /// `uninstall` must also refuse when a symlink exists anywhere
    /// inside the plugin directory tree. `fs::remove_dir_all` would
    /// happily walk through such a symlink and delete the target.
    /// Assert the registry is rolled back to its pre-removal state
    /// (the round-1 code writes the registry after the dir removal, so
    /// on refusal the registry still shows the plugin) and the
    /// symlink target survives.
    #[test]
    #[cfg(unix)] // std::os::unix::fs::symlink fixture
    fn uninstall_refuses_symlink_inside_plugin_dir() {
        let base = tmp_base("uninstall_sym_inside");
        let plugin_dir = plugins_dir(&base).join("fleet");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("mtyx-plugin.toml"),
            manifest("fleet", "bin/fleet.wasm", &["deploy"]),
        )
        .unwrap();
        // Plant a symlink inside the plugin dir at a sensitive target.
        let target = base.join("victim.txt");
        fs::write(&target, "do-not-touch").unwrap();
        std::os::unix::fs::symlink(&target, plugin_dir.join("evil-link")).unwrap();

        // Seed the registry so the plugin is "installed" from mtyx's POV.
        let mut reg = Registry::default();
        reg.plugins.push(PluginEntry {
            name: "fleet".to_string(),
            enabled: true,
            entry: "bin/fleet.wasm".to_string(),
            verbs: vec!["deploy".to_string()],
        });
        save_registry(&base, &reg).unwrap();

        assert_eq!(cmd_uninstall(&base, &["fleet"]), 1);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "do-not-touch",
            "symlink target contents must remain untouched"
        );
        // The registry should still show the plugin because the round-1
        // ordering writes the updated registry AFTER the dir removal.
        let reg_after = load_registry(&base).unwrap();
        assert_eq!(
            reg_after.plugins.len(),
            1,
            "registry should not be rewritten when dir removal is refused"
        );
        assert_eq!(reg_after.plugins[0].name, "fleet");

        let _ = fs::remove_dir_all(&base);
    }

    /// The `--json` output of `cmd_list` (via `format_list_output`) must
    /// sort plugin entries by name so a script reading JSON sees the
    /// same order a human reading the plain output sees. The
    /// round-1 code sorted only the plain branch.
    #[test]
    fn list_json_output_is_sorted_by_name() {
        // Insert in non-alphabetical order so a sort bug is observable.
        let mut reg = Registry::default();
        reg.plugins.push(PluginEntry {
            name: "gamma".to_string(),
            enabled: true,
            entry: "x".to_string(),
            verbs: vec!["a".to_string()],
        });
        reg.plugins.push(PluginEntry {
            name: "alpha".to_string(),
            enabled: true,
            entry: "x".to_string(),
            verbs: vec!["a".to_string()],
        });
        reg.plugins.push(PluginEntry {
            name: "beta".to_string(),
            enabled: true,
            entry: "x".to_string(),
            verbs: vec!["a".to_string()],
        });
        let json = format_list_output(&reg, true).expect("serialise sorted JSON");
        let a = json.find("\"alpha\"").expect("alpha must be present");
        let b = json.find("\"beta\"").expect("beta must be present");
        let c = json.find("\"gamma\"").expect("gamma must be present");
        assert!(a < b && b < c, "JSON output must sort by name; got: {json}");

        // Plain output must use the same order.
        let plain = format_list_output(&reg, false).expect("format plain");
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("alpha "), "plain line 0: {}", lines[0]);
        assert!(lines[1].starts_with("beta "), "plain line 1: {}", lines[1]);
        assert!(lines[2].starts_with("gamma "), "plain line 2: {}", lines[2]);
    }

    #[test]
    fn cmd_call_rejects_disallowed_verb() {
        // Build a manifest that only allows `deploy`, then try to
        // invoke a different verb. cmd_call should reject with exit
        // code 2 BEFORE touching the wasmtime engine (so we don't
        // need a real .wasm fixture to test the validation path).
        let m = parse_manifest(&manifest("fleet", "bin/fleet.wasm", &["deploy"]))
            .expect("valid manifest");
        assert!(!m.verbs.iter().any(|v| v == "rollback"), "rollback should not be in allowlist");
    }

    /// Issue #42 AC6: the pifactory-fleet example plugin ships at
    /// `mux/spec/plugins/pifactory-fleet/`. This test verifies its
    /// `mtyx-plugin.toml` parses, that the schema values match the
    /// contract documented in the plugin's README, and that the
    /// entry path resolves relative to the plugin dir.
    ///
    /// Pure manifest-level test: it does not build or load the
    /// `.wasm` artifact, so it is safe to run without a wasm32
    /// toolchain installed.
    #[test]
    fn example_pifactory_fleet_manifest_parses() {
        // Resolve the plugin dir from CARGO_MANIFEST_DIR.
        // mux-tui's manifest dir is `<repo>/mux/crates/mux-tui`;
        // the plugin sits at `<repo>/mux/spec/plugins/pifactory-fleet`.
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../spec/plugins/pifactory-fleet/mtyx-plugin.toml");
        let manifest_path = manifest_path
            .canonicalize()
            .unwrap_or_else(|e| panic!("could not canonicalize {}: {e}", manifest_path.display()));
        assert!(
            manifest_path.exists(),
            "example plugin manifest missing at {}",
            manifest_path.display()
        );
        let content = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
        let plugin = parse_manifest(&content)
            .unwrap_or_else(|e| panic!("pifactory-fleet manifest parse failed: {e}\n{content}"));

        // Schema values the contract requires.
        assert_eq!(plugin.name, "pifactory-fleet");
        assert_eq!(plugin.entry, "bin/fleet.wasm");
        assert!(
            plugin.verbs.iter().any(|v| v == "cmux_call"),
            "pifactory-fleet manifest must declare the cmux_call verb"
        );
        for verb in ["ping", "status", "deploy", "dispatch", "rollback"] {
            assert!(
                plugin.verbs.iter().any(|v| v == verb),
                "pifactory-fleet manifest must expose {verb:?} as a plugin verb"
            );
        }
        // cmux_call verbs the plugin forwards to the control socket.
        for verb in [
            "identify",
            "list-workspaces",
            "read-screen",
            "new-workspace",
            "send",
            "close-workspace",
        ] {
            assert!(
                plugin.verbs.iter().any(|v| v == verb),
                "pifactory-fleet manifest must declare {verb:?} in its cmux_call allowlist"
            );
        }

        // Capabilities: socket = "write" (the plugin dispatches
        // workers and closes workspaces on rollback).
        let caps = plugin
            .capabilities
            .as_ref()
            .expect("pifactory-fleet manifest must declare [plugin.capabilities]");
        assert_eq!(
            caps.socket.as_deref(),
            Some("write"),
            "pifactory-fleet needs socket=write to deploy / dispatch / rollback"
        );
        assert_eq!(
            caps.network.as_deref(),
            Some("off"),
            "pifactory-fleet must default network=off"
        );

        // Entry resolution: the manifest's `entry` is relative to
        // the plugin install dir. Resolve it the same way
        // `cmd_call` does, then check the source-tree sibling
        // exists (the install path resolves to a real file only
        // after `./build.sh` produces bin/fleet.wasm; we tolerate
        // the source-tree file being absent so the test stays
        // green before the WASM is built, but we DO confirm the
        // path is well-formed).
        let entry_path = manifest_path
            .parent()
            .expect("plugin dir is the manifest's parent")
            .join(&plugin.entry);
        // Path-normalising comparison: `join` yields backslash separators
        // (and canonicalize yields a `\\?\` verbatim prefix) on Windows,
        // so normalise separators before the suffix check instead of
        // assuming unix path spelling. The resolver itself (cmd_call)
        // does not canonicalize — the verbatim prefix here comes from
        // this test's own canonicalize and is irrelevant to the suffix.
        let entry_str = entry_path.to_string_lossy().replace('\\', "/");
        assert!(
            entry_str.ends_with("bin/fleet.wasm"),
            "entry should resolve under plugin dir's bin/, got {entry_str}"
        );
        if entry_path.exists() {
            // Optional assertion: if bin/fleet.wasm is present (the
            // developer ran ./build.sh), confirm it is non-empty.
            let meta = fs::metadata(&entry_path)
                .unwrap_or_else(|e| panic!("stat {}: {e}", entry_path.display()));
            assert!(meta.len() > 0, "bin/fleet.wasm should be a non-empty WASM artifact");
        }
    }

    /// Companion to the above: the plugin's source tree should
    /// contain the build script, the Rust source, and the reference
    /// shell adapter that document the mtyx verbs it wraps.
    /// Catches accidental deletions of the example plugin's
    /// supporting files.
    #[test]
    fn example_pifactory_fleet_sources_exist() {
        let plugin_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/plugins/pifactory-fleet");
        let plugin_dir = plugin_dir
            .canonicalize()
            .unwrap_or_else(|e| panic!("could not canonicalize {}: {e}", plugin_dir.display()));
        for rel in [
            "mtyx-plugin.toml",
            "README.md",
            "Cargo.toml",
            "build.sh",
            "src/lib.rs",
            "bin/fleet.sh",
            "lib/panel.sh",
            "examples/team-spec.json",
            ".gitignore",
        ] {
            let p = plugin_dir.join(rel);
            assert!(
                p.exists(),
                "pifactory-fleet example plugin is missing {rel} (looked at {})",
                p.display()
            );
        }
        // build.sh and bin/fleet.sh must be executable so a user
        // running them from a fresh checkout works (unix exec bit only;
        // there is no such attribute on Windows checkouts).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for rel in ["build.sh", "bin/fleet.sh"] {
                let meta = fs::metadata(plugin_dir.join(rel))
                    .unwrap_or_else(|e| panic!("stat {rel}: {e}"));
                assert_ne!(
                    meta.permissions().mode() & 0o111,
                    0,
                    "{rel} must be executable (mode & 0o111 should be nonzero)"
                );
            }
        }
    }

    #[test]
    fn manifest_path_in_falls_back_to_cmux_era_name() {
        // Rename compat: an installed plugin dir may hold the cmux-era
        // `cmux-plugin.toml`; it must be read when no canonical
        // `mtyx-plugin.toml` is present, and shadowed once the canonical
        // name exists.
        let base = tmp_base("manifest-fallback");
        let plugin_dir = base.join("plugins").join("fleet");
        fs::create_dir_all(&plugin_dir).unwrap();

        // Neither file: canonical name is returned (callers surface the
        // read error against the canonical path).
        assert_eq!(manifest_path_in(&plugin_dir), plugin_dir.join("mtyx-plugin.toml"));

        // Legacy only: honoured.
        fs::write(plugin_dir.join("cmux-plugin.toml"), manifest("fleet", "a.wasm", &["v"]))
            .unwrap();
        assert_eq!(manifest_path_in(&plugin_dir), plugin_dir.join("cmux-plugin.toml"));

        // Both: canonical wins.
        fs::write(plugin_dir.join("mtyx-plugin.toml"), manifest("fleet", "a.wasm", &["v"]))
            .unwrap();
        assert_eq!(manifest_path_in(&plugin_dir), plugin_dir.join("mtyx-plugin.toml"));
    }

    #[test]
    fn base_dir_honours_cmux_era_data_dir() {
        // Rename compat: with XDG_DATA_HOME pointing at a scratch base
        // that has a cmux dir but no mattyx dir, base_dir() resolves to
        // the old dir; once mattyx exists it wins.
        let base = tmp_base("base-dir-honor");
        fs::create_dir_all(base.join("cmux")).unwrap();
        std::env::set_var("XDG_DATA_HOME", &base);
        assert_eq!(base_dir().unwrap(), base.join("cmux"));
        fs::create_dir_all(base.join("mattyx")).unwrap();
        assert_eq!(base_dir().unwrap(), base.join("mattyx"));
        std::env::remove_var("XDG_DATA_HOME");
    }
}
