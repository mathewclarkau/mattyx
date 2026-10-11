# pifactory-fleet (mtyx plugin example)

Worked-example plugin for mattyx's plugin loader. Closes the last
remaining acceptance criterion of [issue #42][i42]: a real, installable
plugin that adapts the mtyx verbs `scripts/cmux-panel-lib.sh` from the
[pifactory][pifactory] repo uses, so a fleet operator can drive
multi-pane agent dispatch via `mtyx pifactory-fleet <verb>` instead of
sourcing the shell library by hand.

[i42]: https://github.com/mathewclarkau/mattyx/issues/42
[pifactory]: https://example.invalid/mathewclarkau/pifactory "local-only repo;
`scripts/cmux-panel-lib.sh` lives outside mattyx"

## What this plugin does

`cmux-panel-lib.sh` is the shell glue pifactory's lead agents call to
dispatch worker panes (`cmux_dispatch_worker_pane`,
`cmux_dispatch_worker_pane_interactive`). It is a thin wrapper around
the mtyx CLI verbs `split`, `rename-surface`, `send --text ...` (often
`--send-cr`), and `close-workspace`.

`pifactory-fleet` re-exposes the same operations as mtyx-plugin verbs:

| Verb       | Underlying mtyx verb(s) | Notes                              |
| ---------- | ----------------------- | ---------------------------------- |
| `ping`     | `identify`              | read-only smoke test               |
| `status`   | `list-workspaces`       | read-only snapshot                 |
| `deploy`   | `new-workspace`         | one mtyx_call per role             |
| `dispatch` | `new-workspace`         | one mtyx_call (anchor for workers) |
| `rollback` | `close-workspace`       | one mtyx_call per closed workspace |

All `mtyx_call` traffic is mediated by the wasmtime host
(`mux/crates/mux-tui/src/plugin_host.rs`): the plugin receives a
per-call auth token, and every forwarded request is checked against
the manifest's verb allowlist and `socket = "write"` capability
before reaching the control socket.

## Layout

```
mux/spec/plugins/pifactory-fleet/
├── mtyx-plugin.toml        Manifest read by `mtyx plugin install`.
├── README.md               This file.
├── Cargo.toml              Crate manifest for the WASM adapter source.
├── build.sh                Builds src/lib.rs to bin/fleet.wasm.
├── src/
│   └── lib.rs              WASM source: thin mtyx_call dispatcher.
├── bin/
│   ├── fleet.wasm          Compiled artifact (commit after running build.sh).
│   └── fleet.sh            Shell adapter (cmux-panel-lib idiom) for reference.
├── lib/
│   └── panel.sh            Glue stub (mirrors the mtyx verbs it wraps).
└── examples/
    └── team-spec.json      Example input for `deploy`.
```

`bin/fleet.sh` is **reference material only** — it documents the
adapter logic in the cmux-panel-lib.sh idiom (one bash function per
verb, each function shelling out to `mtyx <verb>`). The mtyx loader
itself only executes `bin/fleet.wasm`, which is built from
`src/lib.rs` and implements the same verb set in Rust.

## Build

The plugin's `entry` (`bin/fleet.wasm`) is a wasm32-unknown-unknown
artifact. Build it from this directory with:

```sh
./build.sh
```

`build.sh` runs `cargo build --release --target wasm32-unknown-unknown
--target-dir target` and copies the resulting `fleet.wasm` into
`bin/`. Requires:

- `cargo` 1.80+ (wasmtime 27 host crate in mattyx MSRVs at 1.80;
  the plugin's own crate MSRVs at 1.75 and builds with the same
  pinned toolchain as mattyx).
- The `wasm32-unknown-unknown` rustup target:
  `rustup target add wasm32-unknown-unknown`.

The build is hermetic (no network needed if the registry cache is
warm). It does not pull in any mattyx code; the plugin's WASM
talks to mtyx through the three host imports defined in
`mux/crates/mux-tui/src/plugin_host.rs`:

```rust
fn mtyx_token() -> u64;        // (ptr << 32) | len of token bytes
fn mtyx_call(req_ptr, req_len, out_ptr, out_cap) -> i32;
fn mtyx_log(level, ptr, len);  // 0=info, 1=warn, 2=error, other=debug
```

## Install

```sh
mtyx plugin install ./mtyx-plugin.toml
mtyx plugin list                 # should show pifactory-fleet enabled
```

That command copies this directory (minus the build artefacts) into
`$XDG_DATA_HOME/mattyx/plugins/pifactory-fleet/` and appends an entry
to `$XDG_DATA_HOME/mattyx/plugins.json`. The install path validates the
manifest, refuses symlinks (see `mux/crates/mux-tui/src/plugin.rs`
`cmd_install`), and registers the plugin as enabled by default.

## Invoke

```sh
mtyx pifactory-fleet ping
mtyx pifactory-fleet status
mtyx pifactory-fleet deploy   workpieces/p1
mtyx pifactory-fleet rollback workpieces/p1
```

Each invocation is one wasmtime instantiation: the loader mints a
fresh per-call auth token (see
`mux/crates/mux-tui/src/plugin_host.rs::mint_token`), validates the
requested verb against the manifest's `verbs` allowlist, runs the
plugin with the manifest's `fuel` + `max_runtime_ms` budgets, and
returns non-zero on any failure.

## Single-call-per-invocation limitation

The loader's `expected_request_id` is initialised to 0 and never
incremented after `mtyx_call` (see
`plugin_host.rs::define_host_imports`, which reads but does not
bump the counter). A second `mtyx_call` from the same plugin
invocation therefore fails with `StaleId { expected: 0, got: 1 }`.

`pifactory-fleet`'s verbs are designed around this constraint: each
verb makes at most one `mtyx_call`. The README of a future loader
fix that increments `expected_request_id` per call can lift this
restriction without changing the manifest schema. This is tracked
as a known issue — see `mattyx/AGENTS.md` follow-ups.

## Extending

To add a new verb:

1. Add it to `verbs` in `mtyx-plugin.toml` (so the loader accepts it
   on the argv path AND so it appears in the mtyx_call allowlist).
2. Add a `mtyx_call` branch in `src/lib.rs` that builds the
   `{"id":0,"verb":"<mtyx-verb>","args":{...}}` request.
3. Mirror the logic in `bin/fleet.sh` so the shell reference stays
   in sync.
4. Re-run `./build.sh` and re-install.

If the verb is mutating and the manifest declares `socket = "read"`,
the loader will reject it as `WriteBlocked`. Bump the manifest's
`[plugin.capabilities].socket` to `"write"` (this plugin already is).

## Tests

- `mux/crates/mux-tui/tests/cli.rs::plugin_install_list_uninstall_round_trip`
  is the closest existing analogue; it round-trips a hand-rolled
  `pifactory-fleet`-shaped manifest through `mtyx plugin
install/list/uninstall` and asserts the registry state.
- `mux/crates/mux-tui/src/plugin.rs::tests::example_pifactory_fleet_manifest_parses`
  (added with this plugin) parses the actual `mtyx-plugin.toml` from
  this directory and asserts the schema values match the contract
  documented above. It is a manifest-level test, not a runtime test
  — it does not require the `bin/fleet.wasm` to be built.

## Security

The plugin manifest sets `socket = "write"` so the plugin can
dispatch workers. Reduce to `"read"` if you only need the
read-only verbs (`ping`, `status`). The `filesystem = []` default
keeps the WASI preopen set to the plugin's install dir only; add
specific paths under `[plugin.capabilities].filesystem` to grant
more (paths that do not exist on disk are silently skipped by
`build_wasi`).

`network = "off"` blocks WASI preview2 sockets. The preview1 sockets
that wasmtime exposes by default are not wired up in
`build_wasi`, so network is effectively unavailable regardless.
