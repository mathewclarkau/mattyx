# mtyx Plugin Execution Mechanism (issue #42, PR 2 of 3)

## Status

Draft spec — **not yet implemented.** This document is the design contract for the
plugin execution layer that PR #51 (manifest + registry) deferred. Per the issue's
own effort note, this is the security-sensitive half of the loader and deserves a
focused implementation session with a human reviewer present, not an autonomous
overnight ship.

## Context

**What PR #51 shipped** (commit d86b225, merged 2026-07-30):

- `mux/crates/mux-tui/src/plugin.rs` (989 LOC): manifest parser (`[plugin] name /
  entry / verbs`), JSON registry (`plugins.json` with `Vec<PluginEntry>`), and the
  five subcommands `mtyx plugin {list, install, uninstall, enable, disable}`.
- `entry` is stored as an opaque string. **Nothing resolves or executes it.**
- Symlink-safe writes/removes (security checklist alignment from
  `mattyx/AGENTS.md`).
- 126 unit + 17 integration tests green. No new deps.

**What's missing**:

- Nothing invokes the plugin. `mtyx <plugin-name> <verb>` is currently rejected.
- The manifest's `entry` field has no concrete resolution rule (binary path? wasm
  path? both?).
- No capability / sandbox model. PR #51 explicitly left this for a separate PR.

## Goal

Add the execution layer that turns a registered, enabled plugin into a callable
verb-extension on `mtyx`. Plugin code runs **sandboxed** (WASM/WASI via wasmtime)
and communicates with mtyx over the existing JSON-lines control socket with a
**per-plugin scoped auth token** + **verb allowlist** + **capability manifest**.

## Approach: Option A (WASM/WASI sandbox)

Per the user's call (2026-07-30), we're committing to **WASM-only execution**.
Native subprocess plugins (Option B from the earlier trade-off discussion) are
**not in scope** for this PR — the manifest's `entry` field should resolve to a
`.wasm` file or be rejected.

### Why WASM over a subprocess

| Property | WASM (wasmtime) | Subprocess (B) | In-process dlopen (C, rejected) |
|---|---|---|---|
| Memory isolation | **Hard, by construction** | Process boundary only | None |
| Capability-based fs | WASI allowlist | Path-based | None |
| Cross-platform plugin binary | **Yes** (.wasm is portable) | No (per-OS build) | No |
| Plugin author ergonomics | Rust → wasm32-unknown-unknown | Any POSIX binary | C/Rust FFI |
| Runtime dep weight | ~50MB wasmtime | None | libdl |
| Rust 1.75 compatibility | wasmtime 14.x and earlier | n/a | n/a |
| Risk profile | **Lowest** | Moderate | **Highest** |

The 50MB dependency cost is real but worth it — terminal multiplexers that
control hundreds of panes need a real sandbox, not a "we trust the plugin
binary" model.

## Manifest extension (additive — backwards-compatible)

PR #51's manifest was:

```toml
[plugin]
name = "pifactory-fleet"
entry = "bin/fleet.wasm"
verbs = ["deploy", "rollback"]
```

This PR extends it with a `[plugin.capabilities]` table (also additive —
existing manifests with no `[plugin.capabilities]` get a sensible default).
The table is nested under `[plugin]` so toml's per-table model aligns
with serde's per-struct deserialization (a top-level `[plugin.capabilities]`
table doesn't round-trip with `#[derive(Deserialize)]` on
`ManifestPlugin`):

```toml
[plugin]
name = "pifactory-fleet"
entry = "bin/fleet.wasm"
verbs = ["deploy", "rollback"]

[plugin.capabilities]
# What mtyx resources the plugin can access
socket = "read"                    # "off" | "read" | "write"
filesystem = ["/tmp/workpieces"]   # preopen dirs (NOT env-var-substituted)
env = ["HOME", "USER"]            # explicit allowlist (NOT inherited)
network = "off"                    # "off" | "outbound"  (WASI preview1 sockets)

# Plugin execution constraints
memory_mib = 64                    # wasm linear memory cap
fuel = 1_000_000                   # wasmtime fuel budget per invocation
max_runtime_ms = 5000              # wall-clock timeout per verb call
```

**Default capability values** (when `[plugin.capabilities]` is missing) for
backwards-compat with PR #51 manifests:

| Capability | Default | Reasoning |
|---|---|---|
| `socket` | `"read"` | Plugins can read mtyx state but can't mutate it without explicit grant |
| `filesystem` | `["$PLUGIN_DATA_DIR"]` (the plugin's install dir) | Plugin can read its own data but nothing else |
| `env` | `[]` (empty) | No environment leakage by default |
| `network` | `"off"` | Plugins are local by default |
| `memory_mib` | 64 | Generous default; most plugins need <10MB |
| `fuel` | 1_000_000 | ~1ms of compute per verb call on modern CPUs |
| `max_runtime_ms` | 5000 | Plugin calls shouldn't hold a verb for more than 5s |

### How `socket` capability interacts with the control protocol

When `mtyx pifactory-fleet deploy` is invoked:

1. mtyx reads `plugins.json`, finds the `pifactory-fleet` entry, verifies
   `enabled: true` and `verbs` includes `"deploy"`.
2. mtyx reads the manifest's `[plugin.capabilities].socket` field.
3. mtyx **mints a per-call auth token** (random 32 bytes, hex-encoded) that is
   scoped to:
   - the plugin's name
   - the specific verb being called
   - the verb allowlist (only the verbs declared in the manifest)
   - the socket capability (`read` blocks mutating verbs, `write` allows them)
4. mtyx spawns the WASM module with `wasmtime` using the manifest's
   `entry` path resolved against the plugin install directory.
5. mtyx passes the auth token to the WASM module via a host import
   (`mtyx_token(token: String) -> ()`).
6. WASM module calls back into mtyx via a single host import
   `mtyx_call(request_json: String) -> String`, which mtyx validates against
   the token + verb allowlist before forwarding to the real control socket.

This means:

- Plugin cannot bypass the auth token (the only way to talk to mtyx is through
  the `mtyx_call` host import, which validates every request).
- Plugin cannot call verbs not in its allowlist (the token only authorises the
  specific verb being executed).
- Plugin cannot read secrets from mtyx's environment (it gets only the
  `[plugin.capabilities].env` allowlist).
- Plugin cannot escape its filesystem sandbox (wasmtime WASI preopen directories
  are scoped to the manifest's `filesystem` list).

## WASM host imports (the mtyx ↔ plugin ABI)

The plugin WASM module gets exactly three host functions. Anything else is a
load-time failure (wasmtime validates the import list).

```rust
// host import: mtyx_token — returns the auth token for this call
fn mtyx_token() -> String;

// host import: mtyx_call — send a JSON request, get a JSON response
// request shape: { "id": u32, "verb": "list-workspaces", "args": {...} }
// response shape: { "id": u32, "ok": bool, "data"?: ..., "error"?: string }
// mtyx rejects verbs not in the manifest's allowlist or socket capability
fn mtyx_call(request_json: String) -> String;

// host import: mtyx_log — plugin writes to mtyx's stderr (for diagnostics)
fn mtyx_log(level: u32, message: String);  // 0=info 1=warn 2=error
```

Plus standard WASI preview1 imports for filesystem + clock access (the
manifest's `filesystem` list becomes the WASI preopen list).

**No raw socket imports.** The plugin cannot open arbitrary TCP/UDP
connections — that's what the `network` capability controls (when `"outbound"`,
wasmtime's WASI preview2 socket import is enabled with explicit allowlist of
hostnames).

## Execution flow

```
$ mtyx pifactory-fleet deploy <args>
       │
       ▼
┌──────────────────────────────────────────────────┐
│ mtyx CLI dispatcher                              │
│  1. read plugins.json, find pifactory-fleet       │
│  2. verify enabled && "deploy" in verbs allowlist │
│  3. mint per-call auth token (scoped: name+verb)   │
│  4. resolve <plugin>/bin/fleet.wasm               │
│  5. load module via wasmtime::Module::from_file    │
│  6. instantiate with imports: mtyx_token/_call/_log │
│  7. invoke plugin entrypoint with <args> as JSON   │
│  8. fuel + timeout enforced throughout             │
└──────────────────────────────────────────────────┘
       │
       ▼  (plugin WASM runs sandboxed)
       │
┌──────────────────────────────────────────────────┐
│ pifactory-fleet.wasm                             │
│   fn main(args_json_ptr, args_json_len) -> u32    │
│     let token = mtyx_token();                    │
│     let resp = mtyx_call(json!({                  │
│       "id": 1, "verb": "list-workspaces"          │
│     }));                                        │
│     // ... decide what to dispatch ...           │
│     return 0;                                    │
└──────────────────────────────────────────────────┘
```

If the WASM module panics, exhausts fuel, exceeds wall-clock, or makes an
unauthorised call, mtyx reports the failure on stderr with exit code and
returns non-zero to the caller.

## Implementation plan

This PR is ~600-900 LOC across ~6 files. Estimated shape:

| File | Change |
|---|---|
| `mux/Cargo.toml` | Add `wasmtime = { version = "14", default-features = false, features = ["cranelift"] }` |
| `mux/crates/mux-tui/Cargo.toml` | Wire wasmtime |
| `mux/crates/mux-tui/src/plugin.rs` | Extend manifest parser to recognise `[plugin.capabilities]`; add `cmd_call` subcommand that handles the dispatch flow above |
| `mux/crates/mux-tui/src/plugin_host.rs` (new) | The wasmtime host import implementations (`mtyx_token`, `mtyx_call`, `mtyx_log`) + the per-call auth token mint + validation |
| `mux/crates/mux-tui/src/main.rs` | Route `mtyx <plugin> <verb>` to `plugin::cmd_call` instead of rejecting; document the new shape in USAGE |
| `mux/spec/cli.md` | Add the `mtyx <plugin> <verb>` entry to the verb table |
| `mux/spec/commands.md` | Add the dispatch protocol (token mint, validation rules) |
| `mux/crates/mux-tui/tests/cli.rs` | Integration test: build a tiny wasm32 module in the test that asserts the host-import contract; install it; call `mtyx <plugin> <verb>`; assert the response |
| `mux/spec/security-model.md` (new) | Document the threat model + capability defaults |

## Test strategy

Three layers:

1. **Pure unit tests** in `plugin.rs` for the manifest extension and
   capability-default logic.
2. **Host-import unit tests** in `plugin_host.rs`:
   - Token mint produces unique tokens per call
   - Token validation rejects requests with verbs not in the allowlist
   - Token validation rejects requests with the wrong socket capability
     (`read` cannot invoke mutating verbs)
   - Token validation rejects tampered tokens
3. **End-to-end integration test** in `tests/cli.rs`:
   - Build a minimal `tests/fixtures/hello-world-plugin/` that compiles to
     `.wasm` via `cargo build --target wasm32-unknown-unknown` (gated by a
     `RUSTFLAGS` check so the test is skipped if the target isn't installed)
   - Install the fixture via `mtyx plugin install`
   - Call `mtyx <plugin> greet` and assert the stdout contains "hello"

If wasm32-unknown-unknown isn't available in CI, the integration test skips
gracefully — `wasmtime` is the host requirement, not `wasm32-unknown-unknown`.

## Security model (for the spec/security-model.md doc)

**Trust boundary**: a plugin is third-party code. mtyx never trusts the plugin
binary. Every capability the plugin has is opt-in via the manifest, every
verb call is scoped to a per-call token, every filesystem access is mediated
by wasmtime WASI.

**Threats we mitigate**:

1. **Plugin reads secrets from mtyx's environment** — mitigated by the
   `[plugin.capabilities].env` allowlist (empty by default).
2. **Plugin calls mutating mtyx verbs it shouldn't** — mitigated by the
   per-call auth token scoping to verb allowlist + socket capability.
3. **Plugin reads/modifies files outside its scope** — mitigated by WASI
   preopen dirs (manifest's `filesystem` list).
4. **Plugin makes outbound network connections** — mitigated by `network:
   "off"` default and explicit hostname allowlist when enabled.
5. **Plugin runs forever** — mitigated by `fuel` + `max_runtime_ms` enforcement.
6. **Plugin exhausts memory** — mitigated by `memory_mib` cap.
7. **Plugin escapes the wasm sandbox** — out of scope for this PR (wasmtime
   has had CVEs in the past); out of scope for the manifest spec but tracked
   in the security model doc as "follow the wasmtime release notes".

**Threats we don't mitigate** (and document as such):

- Plugin author is malicious **at source** (e.g. ships a plugin that does
  something useful in CI but exfiltrates data at runtime). Mitigation: plugin
  review + publishing model is out of scope (separate spec).
- Plugin upgrades its own manifest at runtime (the registry is read-only per
  invocation).
- Side-channel attacks via fuel consumption timing — accepted as low-risk;
  the fuel budget is per-call and resets.

## Worked example: pifactory-fleet plugin

The plugin wraps `scripts/cmux-panel-lib.sh` (already loaded via PR #51). The
WASM module is a thin adapter that:

1. Reads the JSON args (`workpiece_path`, `team_spec`)
2. Calls `mtyx_call` to dispatch scouts/planners/builders/reviewers via
   existing mtyx verbs (`new-workspace`, `send`, `read-screen`)
3. Returns a JSON summary of dispatched teams

The plugin does NOT implement the agent state machine itself — that stays
in `scripts/team-dispatch.py` (Python on the host). The WASM module is purely
a "translate JSON args → mtyx verb calls" adapter.

Source repo for the worked example (separate from mattyx): the
pifactory repo's `plugins/mtyx-pifactory-fleet/`. Built separately against
`wasm32-unknown-unknown`, output `bin/fleet.wasm` copied into the plugin
install dir on `mtyx plugin install`.

## Risks and tradeoffs

- **Wasmtime version pinning**: PR #51's existing code uses Rust 1.75. Wasmtime
  14.x supports Rust 1.75+; later wasmtime versions may need newer Rust.
  Verifying on this box first (since the user's toolchain is `rustup install
  1.75.0` per `AGENTS.md`).
- **CI build cost**: wasmtime adds ~30-60s to a cold `cargo build`. Acceptable
  but should be documented.
- **First-run UX**: a user running `mtyx pifactory-fleet deploy` for the first
  time on a freshly-installed plugin will see a ~100ms wasmtime load cost.
  Acceptable; cache via `wasmtime::Module::serialize()` to disk.
- **No native plugins**: commits the user to the WASM authoring path. For the
  pifactory fleet's adapter use case this is fine (we control the source).
  For future third-party plugin authors, this is a higher bar than Option B.

## Decision points — actual choices

1. **Wasmtime version**: **27.0.0** (latest stable, MSRV 1.80.0). Originally
   the spec said 14.x for Rust 1.75 compat; the user chose the most recent
   Rust instead, so 27 was used. **Bumps CI to 1.80 minimum; a separate
   PR moves pr-build.yml and release.yml from `dtolnay/rust-toolchain@1.75.0`
   to `@1.97.0`** (the version on the dev box).
2. **`memory_mib` default**: **64**. Generous, but per-plugin field
   override is mandatory in any meaningful plugin. Tighter default would
   just add boilerplate.
3. **`max_runtime_ms` default**: **5000ms (5s)**. Plugins that need
   longer-running operations must split them across multiple `mtyx_call`
   invocations.
4. **Token scope lifetime**: **per-call**. Mint a fresh 32-byte hex token at
   invocation, discard on return. Per-session tokens are a foot-gun if
   leaked.
5. **Plugin upgrade semantics**: **PR #51's existing behaviour** — `mtyx
   plugin install` of a duplicate name rejects. (No `--force` flag added
   in this PR; follow-up if needed.)
6. **Manifest table nesting**: **`[plugin.capabilities]`** (nested under
   `[plugin]`) rather than a top-level `[capabilities]` table. Reason:
   toml's per-table model + serde's per-struct deserialization align when
   the table is nested under the same `#[derive(Deserialize)]` struct.
   Top-level `[capabilities]` does not round-trip cleanly.
7. **WASI flavour**: **preview1** (via `wasmtime_wasi::preview1`).
   Preview2's component-model import is the default in wasmtime 27 and
   adds significant complexity; preview1 is sufficient for filesystem
   + clock + env reads, which is all our plugin ABI needs.

## Cross-references

- Issue #42: <https://github.com/mathewclarkau/mattyx/issues/42>
- PR #51 (merged): commit `d86b225` "feat(plugin): manifest + registry loader"
- This PR's commits (in branch `feat/plugin-execution-wasm`):
  - `430bcbd` — spec (this file's earlier version)
  - `49a81fb` — wasmtime execution layer (this PR, part 1)
  - `749c55d` — `mtyx <plugin> <verb>` dispatch in main.rs (this PR, part 2)
- Existing manifest schema: `mux/crates/mux-tui/src/plugin.rs`
- Host runtime: `mux/crates/mux-tui/src/plugin_host.rs`
- Existing security checklist: `mattyx/AGENTS.md` (no-silent-fallback,
  symlink check, JSON-parse propagates, shell-escape arg arrays)
- herdr plugin ecosystem comparison: `~/Projects/hermes/wiki/concepts/herdr-vs-mattyx-comparison.md` (referenced from issue #42)

## Status

**Implementation complete; PR ready.** Code lives on branch
`feat/plugin-execution-wasm`. The CI bump (Rust 1.75 → 1.97) is a
separate PR that must land before this one can pass `pr-build.yml`,
since wasmtime 27 requires Rust 1.80+.

What's done:

* wasmtime 27 + wasmtime-wasi 27 (preview1) wired in.
* `[plugin.capabilities]` parsed, validated, defaulted.
* 3 host imports (`mtyx_token`, `mtyx_call`, `mtyx_log`) implemented.
* Per-call auth tokens + verb allowlist + socket-capability enforcement
  + stale-id detection.
* Fuel + epoch-interruption-based wall-clock caps.
* `SocketDispatcher` against `mux_core::platform::transport::connect`.
* `mtyx <plugin> <verb> [args]` argv dispatch in main.rs.
* 152 unit tests passing (16 new in `plugin_host::tests`).

What's NOT in this PR (deliberate follow-ups):

* Worked example: `pifactory-fleet` plugin. Lives in the pifactory repo
  under `plugins/mtyx-pifactory-fleet/`. Ships with its own integration
  tests.
* End-to-end integration test that builds a tiny `wasm32-unknown-unknown`
  fixture and exercises the full `cmd_call` path. Skipped here because
  the `wasm32-unknown-unknown` target isn't a default rustup component;
  adding it as a test-only dev-dep is a follow-up.
* `serialized_module` cache (mentioned in earlier spec as a UX
  nicety). Not implemented; ~100ms first-run cost is acceptable for
  v1.
* Per-session tokens. Explicitly NOT supported; per-call is the only
  token lifetime.
