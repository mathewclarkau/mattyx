# Project: mattyx

## Architecture
mattyx is a high-performance terminal multiplexer ported to Linux with a modular architecture:
- `mux-core`: Foundational multiplexer models (`model.rs`), binary split tree geometry & layout (`layout.rs`), surface abstraction over PTY & VT (`surface.rs`), core engine (`mux.rs`), and Unix domain socket IPC server (`server.rs`).
- `mux-tui`: Terminal user interface front-end, screen rendering (`ui/`), keyboard event loop and prefix handling (`app.rs`, `config.rs`), client socket CLI subcommands (`cli.rs`, `main.rs`), and WASM plugin host runtime (`plugin_host.rs`).
- `ghostty-vt-sys` / `ghostty-vt`: Vendored Ghostty virtual terminal emulator engine compiled via Zig 0.15.2.
- `daemon/remote`: Standalone Go daemon (`mtyxd-remote`) providing remote headless PTY/SSH management.
- `.github/workflows/pr-build.yml`: Pull request CI pipeline validating multi-platform builds and test suites.

## Feature Inventory
| # | Feature | Description | Milestone | Source |
|---|---|---|---|---|
| 1 | R1: WASM Plugin Multi-Call Counter | Advance `expected_request_id` in `plugin_host.rs` on `mtyx_call` so sequential host calls succeed without `StaleId` | M1 | ORIGINAL_REQUEST § R1, Survey 1 |
| 2 | R1: WASM Plugin Multi-Call Tests | Add unit and integration tests verifying sequential guest `mtyx_call` invocations | M1 | ORIGINAL_REQUEST § R1, Survey 1 |
| 3 | R2: Pane Zoom State Tracking | Add `zoomed: Option<PaneId>` to `Screen` and `ScreenView` in `mux-core/src/model.rs` | M2 | ORIGINAL_REQUEST § R2, Survey 2 |
| 4 | R2: Non-Destructive Layout Engine | Implement `layout_screen_with_zoom` in `mux-core/src/layout.rs` preserving split tree geometry & ratios | M2 | ORIGINAL_REQUEST § R2, Survey 2 |
| 5 | R2: Multiplexer Zoom Toggle Logic | Implement `toggle_zoom` in `mux-core/src/mux.rs` with auto-unzoom on split/close | M2 | ORIGINAL_REQUEST § R2, Survey 2 |
| 6 | R2: Zoom Keybinding & UI Indicators | Bind `Ctrl-b z` in `config.rs`, dispatch `ToggleZoom`, and render `[Z]` indicators in status bar and pane border | M2 | ORIGINAL_REQUEST § R2, Survey 2 |
| 7 | R2: `zoom-pane` IPC & CLI Verb | Add `zoom-pane` command to `server.rs` and `cli.rs` with serialized `zoomed` field in `screen_json` | M2 | ORIGINAL_REQUEST § R2, Survey 2 |
| 8 | R3: `wait-for` Socket Command | Implement `wait-for` in `server.rs` polling VT plain text with regex matching and timeout | M3 | ORIGINAL_REQUEST § R3, Survey 3 |
| 9 | R3: `wait-for` CLI Verb | Implement `mtyx wait-for --surface <id> --pattern <regex> --timeout-ms <n>` with exit code 0/1 | M3 | ORIGINAL_REQUEST § R3, Survey 3 |
| 10 | R4: `run` Socket Command | Implement `run` in `server.rs` with direct exec `argv`, shell `-lc` command, cwd, and pane/workspace targets | M3 | ORIGINAL_REQUEST § R4, Survey 3 |
| 11 | R4: `run` CLI Verb | Implement `mtyx run [--pane <id> | --new-workspace] [--cwd <dir>] [--name <name>] -- <argv...>` / `--command <cmd>` outputting surface ID | M3 | ORIGINAL_REQUEST § R4, Survey 3 |
| 12 | R5: CI Pipeline Hardening `mux-core` | Expand `pr-build.yml` matrix jobs to execute `cargo +1.97 test -p mux-core` | M4 | ORIGINAL_REQUEST § R5, Survey 1 |
| 13 | R5: CI Pipeline Hardening Go Remote Daemon | Expand `pr-build.yml` with `actions/setup-go@v5` and `go test ./...` in `daemon/remote` for non-Windows runners | M4 | ORIGINAL_REQUEST § R5, Survey 1 |
| 14 | Final Verification & PR Submission | Pass 100% of test suites across workspace and Go daemon, and open a clean PR against `main` | M5 | ORIGINAL_REQUEST Acceptance Criteria |

## Milestones
| # | Name | Scope | Dependencies | Status |
|---|---|---|---|---|
| M0 | E2E Test Track | Design independent opaque-box tests covering R1–R5 specifications and publish `TEST_READY.md` | none | DONE |
| M1 | WASM Multi-Call (R1) | Fix `expected_request_id` advancement in `plugin_host.rs` and add multi-call unit/integration tests | none | DONE |
| M2 | Pane Zoom Toggle (R2) | Implement `zoomed` state, layout preservation, `Ctrl-b z` keybinding, `[Z]` indicators, and `zoom-pane` CLI verb | none | DONE |
| M3 | IPC & CLI Verbs (R3 & R4) | Implement `wait-for` text synchronization verb and `run` direct process spawning verb | none | DONE |
| M4 | CI Hardening (R5) | Expand `.github/workflows/pr-build.yml` to run `mux-core` and Go daemon test suites | none | DONE |
| M5 | Final Milestone & PR | Validate 100% test passes across Rust workspace & Go daemon, harden coverage, and create GitHub PR | M1, M2, M3, M4 | DONE |

## Interface Contracts

### M1 (WASM Plugin Host)
- Function: In `define_host_imports`, the closure for `"call"`:
  ```rust
  caller.data_mut().expected_request_id = expected_id.wrapping_add(1);
  ```
  Mutated after guest response is written and borrow released, ensuring sequential guest requests succeed.

### M2 (Pane Zoom Toggle)
- Model: `mux_core::model::Screen` has `pub zoomed: Option<PaneId>`.
- Layout: `mux_core::layout::layout_screen_with_zoom(root: &Node, area: Rect, zoomed: Option<PaneId>) -> LayoutResult`. If `zoomed` is `Some(id)` and `id` exists in `root`, returns single pane covering `area`.
- Engine: `mux_core::mux::Mux::toggle_zoom(&self, pane: Option<PaneId>) -> anyhow::Result<bool>`.
- IPC: `Command::ZoomPane { pane: Option<PaneId> }` -> `Ok(json!({ "zoomed": Option<PaneId> }))`.
- CLI: `mtyx zoom-pane [--pane <id>]`.
- Keybinding: `Ctrl-b z` triggers `Action::ToggleZoom`.

### M3 (Verbs: `wait-for` & `run`)
- IPC `wait-for`:
  - Request: `{"cmd": "wait-for", "surface": IdRef, "pattern": String, "timeout_ms": u64}`
  - Response on match: `{"matched": true, "text": String, "elapsed_ms": u64}`
  - Response on timeout: error `"timeout waiting for pattern"` (resulting in exit code 1 in CLI)
- CLI `wait-for`: `mtyx wait-for --surface <id> --pattern <regex> --timeout-ms <n>`.
- IPC `run`:
  - Request: `{"cmd": "run", "argv": Option<Vec<String>>, "command": Option<String>, "cwd": Option<String>, "pane": Option<IdRef>, "new_workspace": Option<bool>, "name": Option<String>}`
  - Response: `{"surface": Id, "pane": Id, "screen": Id, "workspace": Id}`
- CLI `run`: `mtyx run [--pane <id> | --new-workspace] [--cwd <dir>] [--name <name>] -- <argv...>` or `--command <cmd>`. Plain stdout outputs surface ID.

### M4 (CI Workflow)
- In `.github/workflows/pr-build.yml`:
  - Step: `cargo +1.97 test -p mux-core` in `working-directory: mux` across matrix.
  - Steps: `actions/setup-go@v5` + `go test ./...` in `working-directory: daemon/remote` with `if: runner.os != 'Windows'`.

## Code Layout
- `mux/Cargo.toml`: Workspace configuration & workspace dependencies (`regex = "1"`).
- `mux/crates/mux-core/`: Core data structures (`model.rs`), layout calculations (`layout.rs`), multiplexer controller (`mux.rs`), socket server (`server.rs`).
- `mux/crates/mux-tui/`: TUI app state (`app.rs`), keybindings (`config.rs`), UI rendering (`ui/mod.rs`, `ui/pane.rs`), CLI parsing (`cli.rs`, `main.rs`), WASM host (`plugin_host.rs`), tests (`tests/cli.rs`).
- `daemon/remote/`: Go daemon module (`go.mod`, `cmd/mtyxd-remote`).
- `.github/workflows/pr-build.yml`: PR CI workflow.
