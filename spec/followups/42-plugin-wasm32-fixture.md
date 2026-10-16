# Follow-up: wasm32-unknown-unknown fixture for plugin integration test

Status: **NOT IMPLEMENTED** (deliberate follow-up from PR 2 of issue #42).

## What this PR needs

PR 2 (commits `49a81fb`, `749c55d`, `82a8392` on
`feat/plugin-execution-wasm`) ships the wasmtime execution layer but
does not include an end-to-end integration test that actually loads a
real `.wasm` module. The host implementation has 16 unit tests
(manifest parsing, validation, request shape, allowlist, stale-id
detection, etc.) plus the dispatcher trait — but the actual wasmtime
`Module::from_file` -> `Store::new` -> linker -> invoke path is
covered only by the existing test suite (no real plugin WASM).

## What needs to happen

1. Install the `wasm32-unknown-unknown` target on a dev box and CI:
   ```bash
   rustup target add wasm32-unknown-unknown
   ```
2. Create a tiny test fixture in `mux/crates/mux-tui/tests/fixtures/`:
   - A `Cargo.toml` with a `cdylib` target
   - A `src/lib.rs` that exports a `_mtyx_plugin_main` function
   - The function does one `mtyx_call("list-workspaces", ...)` and
     returns
3. Add a `build.rs` (or `tests/cli.rs` build script) that compiles
   the fixture to `target/wasm32-unknown-unknown/debug/fixture.wasm`
   before the integration test runs.
4. Add an integration test in `mux/crates/mux-tui/tests/cli.rs`:
   - `mtyx plugin install <path to fixture manifest>` against a
     temp data dir
   - `mtyx <plugin-name> <verb>` to invoke
   - assert the output is correct
5. (Optionally) Add `wasm32-unknown-unknown` as a target dep in
   `mux/crates/mux-tui/Cargo.toml` so the test fixture builds
   automatically.

## Estimated effort

~1-2 hours. The fixture is trivial (10-20 lines of Rust calling one
`mtyx_call`); the build.rs wiring is the only fiddly bit.

## Why it's deferred

Adding the `wasm32-unknown-unknown` target as a CI dependency pulls
in ~200MB of additional build dependencies and pushes CI runtime up
by 2-3 minutes. Not worth doing in the same PR as the core
execution layer — better to land #42's first half cleanly, then
add the test in a focused follow-up.

## Tracking

This is a known follow-up; not currently tracked as a separate
GitHub issue. Recommend opening an issue once PR 2 lands, with this
file as the design doc.
