# E2E Test Infra: mattyx

## Test Philosophy
- Opaque-box, requirement-driven test verification derived directly from `ORIGINAL_REQUEST.md` and user-facing protocol specifications (`mux/spec/commands.md`, `mux/spec/cli.md`).
- Methodology: 4-tier systematic approach (Category-Partition, Boundary Value Analysis, Pairwise Combinatorial Testing, Real-World Workload Testing).

## Feature Inventory
| # | Feature | Source | Tier 1 | Tier 2 | Tier 3 |
|---|---|---|:---:|:---:|:---:|
| 1 | R1: WASM Plugin Multi-Call Execution | ORIGINAL_REQUEST § R1 | 5 | 5 | ✓ |
| 2 | R2: Pane Zoom Toggle (`Ctrl-b z` / `zoom-pane`) | ORIGINAL_REQUEST § R2 | 5 | 5 | ✓ |
| 3 | R3: Screen Text Synchronization (`wait-for`) | ORIGINAL_REQUEST § R3 | 5 | 5 | ✓ |
| 4 | R4: Direct Process Spawning (`run`) | ORIGINAL_REQUEST § R4 | 5 | 5 | ✓ |
| 5 | R5: PR CI Pipeline Hardening | ORIGINAL_REQUEST § R5 | 5 | 5 | ✓ |

## Test Architecture
- **Test Harness**: `cargo +1.97 test -p mux-tui --test cli` for CLI verbs and E2E multiplexer flows.
- **Unit Suites**:
  - `mux-tui`: `plugin_host` multi-call unit and integration tests.
  - `mux-core`: `layout` zoom calculation tests, `model` screen zoom state tests, `server` IPC command tests.
  - `daemon/remote`: `go test ./...` in `daemon/remote`.
  - CI: `.github/workflows/pr-build.yml` syntax, jobs, and step configuration validation.
- **Directory Layout**:
  - `mux/crates/mux-tui/tests/cli.rs`: Integration tests for CLI commands (`wait-for`, `run`, `zoom-pane`).
  - `mux/crates/mux-tui/src/plugin_host.rs`: Unit tests for WASM multi-call counter.
  - `mux/crates/mux-core/tests/`: Integration tests for core server commands and layout zoom.

## Real-World Application Scenarios (Tier 4)
| # | Scenario | Features Exercised | Complexity |
|---|---|---|---|
| 1 | Scripted Automation Workflow | R4 (`run`) spawns process, R3 (`wait-for`) waits for expected banner, exits cleanly | High |
| 2 | Interactive Split Navigation & Zoom | Split panes, focus pane, R2 (`zoom-pane`) toggle on/off, verifying geometry preservation | High |
| 3 | Multi-Query WASM Plugin Lifecycle | WASM plugin guest invokes multiple host queries in sequence without `StaleId` | High |
| 4 | CI Multi-Target Verification | CI pipeline triggers, tests `mux-core`, `mux-tui`, and Go daemon | Medium |
| 5 | Complex Regex Screen Sync with Timeout | R3 (`wait-for`) evaluates complex regex, handles match vs timeout exit codes (0 vs 1) | Medium |

## Coverage Thresholds
- Tier 1: >=5 test cases per feature (Total: 25)
- Tier 2: >=5 boundary and corner cases per feature (Total: 25)
- Tier 3: Pairwise cross-feature interactions (Total: 5)
- Tier 4: Realistic end-to-end workload scenarios (Total: 5)
- Total Target: >=60 test cases
