# TEST_READY: mattyx E2E Requirement Test Suite (R1 - R5)

**Status**: READY FOR VERIFICATION  
**Author**: E2E Test Writer  
**Specification Sources**: `ORIGINAL_REQUEST.md`, `PROJECT.md`, `TEST_INFRA.md`  
**Test File**: `mux/crates/mux-tui/tests/e2e_requirements.rs`

---

## 1. Test Suite Overview & Coverage Summary

The E2E test suite implements a 4-tier systematic verification architecture covering all five feature requirements (R1–R5) across nominal, boundary, combinatorial, and realistic workload dimensions.

| Feature | Scope | Tier 1 (Nominal) | Tier 2 (Boundary) | Total Feature Tests | Pre-Implementation Status |
|---|---|:---:|:---:|:---:|:---:|
| **R1: WASM Plugin Multi-Call Execution** | Sequence ID advance (`expected_request_id`), multi-call query lifecycle, wrapping, StaleId error handling | 5 | 5 | 10 | **10 Passed / 0 Failed** |
| **R2: Pane Zoom Toggle (`zoom-pane` / `Ctrl-b z`)** | Fullscreen layout toggle, geometry preservation, ratio restore, auto-unzoom, pane targeting, status JSON | 5 | 5 | 10 | **5 Passed / 5 Failed** (pending M2 CLI verb) |
| **R3: Screen Text Synchronization (`wait-for`)** | Text match, regex evaluation, multiline, `--timeout-ms` handling, exit code 0 vs 1, JSON payload | 5 | 5 | 10 | **2 Passed / 8 Failed** (pending M3 CLI verb) |
| **R4: Direct Process Spawning (`run`)** | Shell vs argv execution, cwd, naming, `--pane` vs `--new-workspace`, surface ID stdout, JSON format | 5 | 5 | 10 | **3 Passed / 7 Failed** (pending M3 CLI verb) |
| **R5: PR CI Pipeline Hardening** | `.github/workflows/pr-build.yml` syntax, matrix OS targets, `mux-core` tests, Go daemon tests, non-Windows guards | 5 | 5 | 10 | **10 Passed / 0 Failed** |
| **Tier 3: Pairwise Combinations** | Cross-feature interactions: (R3+R4), (R2+R4), (R2+R3), (R1+R3), (R2+R4+R3) | — | — | 5 | **1 Passed / 4 Failed** |
| **Tier 4: Real-World Workloads** | Automation workflow, interactive split navigation & zoom, plugin lifecycle, CI matrix, ANSI regex sync | — | — | 5 | **2 Passed / 3 Failed** |
| **TOTAL** | **Full Requirement Test Suite** | **25** | **25** | **60** | **33 Passed / 27 Failed** |

---

## 2. Test Execution Commands

All commands strictly follow the pinned repository toolchain (`ZIG` and Rust 1.97.0).

### Run the Complete E2E Suite (All 60 Tests)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements -- --test-threads=1
```

### Run Tests Per Milestone / Feature

#### Feature R1 (WASM Plugin Multi-Call Execution)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_r1
```

#### Feature R2 (Pane Zoom Toggle)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_r2
```

#### Feature R3 (Screen Text Synchronization `wait-for`)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_r3
```

#### Feature R4 (Direct Process Spawning `run`)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_r4
```

#### Feature R5 (PR CI Hardening)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_r5
```

#### Tier 3 (Pairwise Cross-Feature Interactions)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_tier3
```

#### Tier 4 (Real-World Application Scenarios)
```bash
export ZIG="$PWD/.tools/zig-x86_64-linux-0.15.2/zig"
cd mux && cargo +1.97 test -p mux-tui --test e2e_requirements test_tier4
```

---

## 3. Individual Test Inventory

### R1: WASM Plugin Multi-Call Execution
- `test_r1_t1_sequential_call_request_ids_advance_correctly`: Validates counter advances from 0 -> 1 -> 2 across consecutive queries.
- `test_r1_t1_three_step_sequential_queries_succeed`: 3 sequential host queries in one invocation without failure.
- `test_r1_t1_sequential_read_queries_return_valid_responses`: Sequential read queries return `ok: true`.
- `test_r1_t1_response_payload_preserves_request_id_matching`: Host response `id` matches guest request `id`.
- `test_r1_t1_counter_wrapping_semantics`: Request ID counter cleanly wraps on `u64::MAX`.
- `test_r1_t2_skipped_request_id_fails_with_stale_id`: Missing intermediate ID returns `StaleId { expected: 1, got: 2 }`.
- `test_r1_t2_repeated_request_id_fails_with_stale_id`: Repeated ID returns `StaleId { expected: 1, got: 0 }`.
- `test_r1_t2_stale_id_error_preserves_expected_counter`: Stale ID rejection does not corrupt counter state.
- `test_r1_t2_disallowed_verb_on_step_two_rejected`: Disallowed verb on step 2 rejected without breaking host.
- `test_r1_t2_invalid_json_request_on_step_two_rejected`: Malformed request JSON rejected safely.

### R2: Pane Zoom Toggle (`zoom-pane` / `Ctrl-b z`)
- `test_r2_t1_zoom_pane_cli_zooms_active_split_pane`: `mtyx zoom-pane` expands split pane to full screen dimensions.
- `test_r2_t1_zoom_pane_cli_toggle_restores_split_geometry`: Toggling again restores underlying split tree.
- `test_r2_t1_zoom_pane_with_target_pane_flag`: `mtyx zoom-pane --pane <id>` zooms specific target pane.
- `test_r2_t1_zoom_state_reflected_in_workspace_json`: `list-workspaces --json` reflects `zoomed: Some(pane_id)`.
- `test_r2_t1_zoom_ipc_command_returns_zoomed_status`: Direct IPC `{"cmd": "zoom-pane"}` returns `{"zoomed": ...}`.
- `test_r2_t2_zoom_single_pane_is_noop`: Single-pane screens safely handle zoom without panicking.
- `test_r2_t2_zoom_nonexistent_pane_fails`: Targeting invalid pane ID returns nonzero exit code.
- `test_r2_t2_split_while_zoomed_auto_unzooms`: Splitting a new pane while zoomed auto-unzooms to reveal split.
- `test_r2_t2_close_zoomed_pane_auto_unzooms`: Closing zoomed pane clears zoom state.
- `test_r2_t2_asymmetric_split_ratios_preserved_after_unzoom`: Custom floating-point split ratios intact after zoom toggle.

### R3: Screen Text Synchronization (`wait-for`)
- `test_r3_t1_wait_for_exact_match_exits_zero`: Exact string match exits 0.
- `test_r3_t1_wait_for_regex_pattern_match_exits_zero`: Regex pattern match exits 0.
- `test_r3_t1_wait_for_timeout_exits_one`: Absent pattern exits 1 upon timeout expiration.
- `test_r3_t1_wait_for_json_reports_matched_and_elapsed`: `--json` outputs `{matched: true, elapsed_ms: ...}`.
- `test_r3_t1_wait_for_immediate_check_timeout_zero`: `--timeout-ms 0` succeeds immediately when pattern is present.
- `test_r3_t2_wait_for_immediate_check_timeout_zero_fails_if_absent`: `--timeout-ms 0` exits 1 immediately when pattern is absent.
- `test_r3_t2_wait_for_invalid_regex_syntax_exits_nonzero`: Malformed regex exits nonzero with diagnostic error.
- `test_r3_t2_wait_for_unknown_surface_exits_nonzero`: Invalid surface ID exits nonzero with unknown surface error.
- `test_r3_t2_wait_for_multiline_pattern_matching`: Multiline regex matching across terminal lines.
- `test_r3_t2_wait_for_short_timeout_bounded_duration`: Low timeout duration is strictly bounded without hangs.

### R4: Direct Process Spawning (`run`)
- `test_r4_t1_run_argv_spawns_process_and_prints_surface_id`: `mtyx run -- <argv...>` spawns executable and prints surface ID.
- `test_r4_t1_run_command_flag_spawns_via_shell`: `mtyx run --command <cmd>` spawns via shell `-lc`.
- `test_r4_t1_run_new_workspace_flag`: `--new-workspace` allocates command in fresh workspace.
- `test_r4_t1_run_cwd_flag_sets_working_directory`: `--cwd <dir>` sets child working directory.
- `test_r4_t1_run_name_flag_names_surface_or_workspace`: `--name <name>` sets surface title.
- `test_r4_t2_run_mutual_exclusion_argv_and_command`: Simultaneous `--command` and `-- <argv...>` rejected with usage error.
- `test_r4_t2_run_mutual_exclusion_pane_and_new_workspace`: Simultaneous `--pane` and `--new-workspace` rejected.
- `test_r4_t2_run_target_specific_pane`: `--pane <id>` adds tab to specific target pane.
- `test_r4_t2_run_json_output_format`: `--json` outputs `{surface, pane, screen, workspace}`.
- `test_r4_t2_run_missing_argv_and_command_fails`: Invocation without command or argv rejected with usage error.

### R5: PR CI Pipeline Hardening
- `test_r5_t1_ci_workflow_file_exists_and_readable`: Workflow file `.github/workflows/pr-build.yml` exists and readable.
- `test_r5_t1_ci_matrix_runs_mux_core_tests`: Workflow includes `cargo +1.97 test -p mux-core`.
- `test_r5_t1_ci_workflow_installs_go`: Workflow includes `actions/setup-go@v5`.
- `test_r5_t1_ci_workflow_runs_go_daemon_tests`: Workflow includes `go test ./...`.
- `test_r5_t1_ci_workflow_guards_go_tests_non_windows`: Go test step guarded by `runner.os != 'Windows'`.
- `test_r5_t2_ci_mux_core_working_directory_is_mux`: `working-directory: mux` set for `mux-core` tests.
- `test_r5_t2_ci_go_working_directory_is_daemon_remote`: `working-directory: daemon/remote` set for Go tests.
- `test_r5_t2_ci_rust_toolchain_pinned_to_1_97`: Toolchain pinned to Rust 1.97.0 per AGENTS.md.
- `test_r5_t2_ci_go_version_source_configuration`: Go version sourced from `daemon/remote/go.mod`.
- `test_r5_t2_ci_matrix_runner_operating_systems`: Matrix targets Ubuntu, macOS, and Windows runners.

### Tier 3: Pairwise Combinatorial Interactions
- `test_tier3_pairwise_run_and_wait_for`: `run` launches process, `wait-for` synchronizes on output token.
- `test_tier3_pairwise_zoom_and_run`: Spawning a process via `run` while a pane is zoomed.
- `test_tier3_pairwise_zoom_and_wait_for`: Verifying `wait-for` matching while a pane is zoomed.
- `test_tier3_pairwise_plugin_multicall_and_screen_inspection`: Sequential plugin queries concurrent with screen inspection.
- `test_tier3_pairwise_run_zoom_wait_unzoom_chain`: Full chain: `run` -> `zoom-pane` -> `wait-for` -> unzoom.

### Tier 4: Real-World Workload Scenarios
- `test_tier4_scenario_scripted_automation_workflow`: Scripted automation launches background service, synchronizes on ready banner, sends commands, and verifies responses.
- `test_tier4_scenario_interactive_split_navigation_and_zoom`: Complex IDE layout with multiple horizontal/vertical splits, active pane zoom, and split restoration.
- `test_tier4_scenario_multi_query_wasm_plugin_lifecycle`: End-to-end guest plugin lifecycle issuing multiple host queries in sequence.
- `test_tier4_scenario_ci_multi_target_verification`: Verifies multi-target CI configuration conforms to all platform and package requirements.
- `test_tier4_scenario_complex_regex_screen_sync_with_timeout`: Terminal output with ANSI escape sequences matched against complex regex pattern.

---

## 4. Current Implementation Bugs & Gaps (Escalation to Implementing Agents)

Running the test suite against the current codebase identifies the exact set of implementation gaps to be addressed in subsequent milestones:

1. **Milestone 2 Gap (R2 - Pane Zoom)**:
   - CLI verb `mtyx zoom-pane` is currently unrecognized (exits code 2).
   - IPC command `{"cmd": "zoom-pane"}` is not yet handled in `server.rs`.
   - `ScreenView` / `Screen` lacks `zoomed: Option<PaneId>` serialization.
   - *Target agent*: M2 Implementer (`mux-core/src/model.rs`, `layout.rs`, `mux.rs`, `server.rs`, `mux-tui/src/cli.rs`, `config.rs`).

2. **Milestone 3 Gap (R3 - `wait-for`)**:
   - CLI verb `mtyx wait-for` is currently unrecognized (exits code 2).
   - IPC command `{"cmd": "wait-for"}` is not yet handled in `server.rs`.
   - *Target agent*: M3 Implementer (`mux-core/src/server.rs`, `mux-tui/src/cli.rs`).

3. **Milestone 3 Gap (R4 - `run`)**:
   - CLI verb `mtyx run` is currently unrecognized (exits code 2).
   - IPC command `{"cmd": "run"}` is not yet handled in `server.rs`.
   - *Target agent*: M3 Implementer (`mux-core/src/server.rs`, `mux-tui/src/cli.rs`).
