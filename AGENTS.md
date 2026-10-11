# mattyx — agent + contributor notes

This file is for AI coding agents (Claude Code, Codex, Pi, Aider, Antigravity) **and** for human contributors. It pins the toolchain and calls out the gotchas you'll hit on a fresh build.

## Pinned toolchain (non-negotiable)

| Tool | Version | Why |
|------|---------|-----|
| **zig** | **0.15.2** | Enforced at compile time by `mux/crates/ghostty-vt-sys/build.zig` via `@compileError`. **Zig is NOT on the system PATH** — the repo vendors it under `.tools/zig-<arch>-linux-0.15.2/zig` (where `<arch>` is `x86_64` or `aarch64`). System zig 0.16.0 (Ubuntu 24.04, current Arch, current Nix `zigPackages.latest`) will fail with "version mismatch" errors. Do **not** `apt install zig` and assume you're done. Instead: (1) run `scripts/bootstrap.sh` first (it fetches the right zig into `.tools/`), then (2) export `ZIG=$REPO/.tools/zig-$(uname -m | sed 's/x86_64/x86_64/;s/aarch64/aarch64/')-linux-0.15.2/zig` before running cargo, OR (3) just run cargo from within `scripts/bootstrap.sh` (which sets `ZIG` for you). The GitHub Actions pr-build.yml workflow does (1)+(2) and is the canonical example. |
| **rust** | **1.97.0** | The CI-tested version in `release.yml`. Anything newer (Rust 1.82+ `is_none_or`, 1.87+ `is_multiple_of`, 1.88+ `let-chains`, 1.85+ format-capture syntax) will fail to compile if introduced by accident. The CI matrix is pinned to a recent stable release; bumping the pin requires a dedicated toolchain-bump PR; do not bump without a corresponding toolchain bump PR. Use `dtolnay/rust-toolchain@1.97.0` (or `rustup toolchain install 1.97.0 && cargo +1.97.0 …`). |
| **bindgen** | **0.70.1** | Pinned in `mux/crates/ghostty-vt-sys/Cargo.toml` and `mux/Cargo.lock`. 0.71+ requires bindgen APIs not available in Rust 1.75. Do not bump for a security advisory without re-verifying 1.75 builds clean. |

## First build

```bash
# Option A: all-in-one (recommended) — bootstrap.sh does rust + zig + cargo build
./scripts/bootstrap.sh
# → sets ZIG=$REPO/.tools/zig-<arch>-linux-0.15.2/zig and runs `cargo build --release -p mux-tui`

# Option B: two-step — bootstrap.sh downloads zig, then you run cargo with ZIG exported
./scripts/bootstrap.sh
export ZIG="$PWD/.tools/zig-$(uname -m | sed 's/x86_64/x86_64/;s/aarch64/aarch64/')-linux-0.15.2/zig"
cd mux && cargo build                       # build.rs reads $ZIG

# Option C: just cargo (assumes someone else exported ZIG)
cd mux && cargo build                       # build.rs falls back to `zig` (PATH) — DO NOT rely on this on a fresh system
```

`bootstrap.sh` is idempotent; re-run any time you blow away `.tools/`.

**If `cargo build` fails with "version mismatch" on the zig @compileError**: you forgot to export `ZIG`. The build.rs falls back to `zig` on PATH, which on most systems is 0.16.x and rejected.

## Build / build.rs gotchas

- `mux/crates/ghostty-vt-sys/build.rs` discovers system include paths by shelling out to `cc -E -Wp,-v -` (with `clang` and `gcc` as fallbacks). If you have `clang` but not `cc`/`gcc` installed, the probe still works via `clang`; if the `clang` probe also fails, bindgen falls back to libclang's bundled resource-dir headers. This works on Ubuntu with `apt install clang libclang-dev` but can fail on stripped-down clang packages (thin Arch `clang`, distroless images, some Nix shells).
  - **If you see `limits.h` not found during bindgen:** install `cc` (or `gcc`), OR set `BINDGEN_EXTRA_CLANG_ARGS="-resource-dir $(clang -print-resource-dir)"` in the environment. The build.rs will pick this up automatically.

## Cargo / repo conventions

- Workspace is under `mux/` (`mux/Cargo.toml`). Run `cargo` from there, not from the repo root.
- No CI exists for PRs. There is no GitHub Actions workflow that catches Rust 1.75 breakage on push; the only check is `./scripts/bootstrap.sh && cargo build` (and the release.yml workflow on tag pushes, which only builds — it does not lint).
- The repo is a Linux port vendoring from `manaflow-ai/cmux`. Preserve the upstream of `mux/crates/ghostty-vt-sys/include/ghostty` unchanged across rebases.
- `Cargo.lock` version is **3** (current cargo 1.97+ still emits v3 by default; v4 is opt-in via `package.resolver = "3"` or `[cargo-new] version = "4"`). Don't bump it.

### Versioning (issue #71) — do NOT bump a version by hand

**The pushed `v*` tag is the only source of truth for a release version.** There is no manual bump step; adding one back is a regression. `mtyx --version` reported `0.1.0` for seventeen tagged releases precisely because it read `CARGO_PKG_VERSION` and relied on a human remembering.

`mux/crates/mux-core/build.rs` resolves the version at build time and bakes it in as `mux_core::VERSION` (a `rustc-env` constant, so it travels with the binary and needs no git or manifest at run time), in this order:

1. **`$MTYX_VERSION`** — `release.yml` sets it from `GITHUB_REF_NAME` minus the leading `v`. Authoritative for a release.
2. **`git describe --tags`** — `0.17.2` on an exact tag, `0.17.2-14-gabc1234` off one, `-dirty` appended for local modifications. This is what dev builds get.
3. **`CARGO_PKG_VERSION` + `-g<sha>`** — a repo with no tag reachable from `HEAD`. This is the *normal* case in CI, not an edge case: `actions/checkout` grafts a depth-1 clone, so no tag is an ancestor of `HEAD` even when tag refs were fetched.
4. **`CARGO_PKG_VERSION`** — no-git fallback (source tarball). `version` in `[workspace.package]` is a floor for tiers 3 and 4 only; `release.yml` rewrites it to match the tag before building, so it never needs a manual edit.

Consequences worth knowing before you touch any of this:

- **Read `mux_core::VERSION`, never `env!("CARGO_PKG_VERSION")`,** anywhere a version is shown to a user or sent to a peer (`-V`, the socket `identify` reply, the `-ldflags` stamp on the cross-compiled `mtyxd-remote`).
- **Dirtiness ignores submodules.** `bootstrap.sh` patches `ghostty/` on every checkout, so a plain `git describe --dirty` would mark *every* build dirty, CI included. `build.rs` uses `git status --ignore-submodules=all` instead.
- **`build.rs` lists its `rerun-if-changed` inputs explicitly** (`build.rs`, `.git/HEAD`, `.git/packed-refs`, `.git/refs/tags`) because emitting any one of them opts out of cargo's default whole-package watch. Drop the git refs and an incremental rebuild after tagging silently reuses the stale version. Only paths that exist are emitted — cargo treats an unstattable path as permanently dirty and would rerun the script on every build.
- **Every tier keeps a leading semver triple**, which is the invariant `version_is_not_the_stale_placeholder` pins; only the suffix marking a build as *not* a release varies. Never resolve with `describe --always`: its bare-SHA fallback (`79e84a4`) drops the triple entirely, which is what broke PR CI on the first cut of this.
- **release.yml fails the release** if the built binary's `--version` disagrees with the tag, so this cannot break silently again.

## LLM harness hooks (under `mux/crates/mux-tui/src/`)

The binary subcommands `antigravity`, `codex`, `grok`, `pi`, `aider` (and the existing `claude` on main) all install hooks by reading a config file (or writing a wrapper/extension), merging in our entries, and writing it back. They share parsing/writing helpers and a `--uninstall`/`--global` flag pair.

**Editing one of these files?** Consider a refactor PR first — there's substantial duplication across them (load-or-default JSON, write-pretty, `--uninstall`/`--global` parse, the `MTYX-START`/`MTYX-END` block rewriter). A `hook_merge` module with `load_or_default<T>`, `save_pretty<T>`, `parse_flags(&[String]) -> (bool, bool)`, and `replace_marked_block(path, start, end, content)` would collapse ~200 lines of duplication.

**Security gotchas these files all share (review checklist):**

- **Agents dispatching into mtyx panes should pass `--shell fish` (or `auto`) to `mtyx send`** so shell-aware sanitisation resets the pane's input buffer before metacharacter-leading text (issue #35); default stays `raw`/verbatim.

- **JSON parse errors must propagate, not silently `unwrap_or_default()`.** A schema-drift user config should not be silently overwritten with `Default::default() + our hooks`.
- **Symlink check before write.** `fs::write` on a symlink path overwrites the *target*, not the symlink. Use `fs::symlink_metadata` to detect, or `OpenOptions::new().custom_flags(libc::O_NOFOLLOW)` to fail open.
- **Toml edits use line-by-line checks, not `String::contains()`.** A key like `docs.codex_hooks = true` would false-positive-match `contains("codex_hooks = true")`.
- **Shell-exec escapes arg arrays.** When emitting shell scripts or Node `exec()` calls, use `execFile(cmd, [args], cb)` with an arg array — never string-interpolate an env-var value into a shell command.

## Output style for this repo

- Keep `git diff` clean and minimal — no whitespace-only churn.
- Use `cargo check`, not `cargo build`, for fast iteration.
- Never commit `.tools/`, `mux/target/`, `.venv/`, or anything in `.gitignore`.
- Commit messages: `type(scope): short description` — types: `feat`, `fix`, `refactor`, `docs`, `chore`, `test`.

## Known issues / follow-ups

- The hook-file duplication noted above is the highest-priority refactor.
- The `claude_hook.rs` `map_or(true, ...)` rewrite (PR #1) will trip `clippy::unnecessary_map_or` and `clippy::manual_is_multiple_of` under Rust 1.96+ — the `// allow(clippy::unnecessary_map_or)` workaround is fine for now, but a cleaner alternative is `.is_some_and(|list| !list.is_empty())` which works on Rust 1.75.
- PRs are now auto-gated by `.github/workflows/pr-build.yml` (pinned to Rust 1.97.0 + zig 0.15.2; runs `cargo check -p mux-tui` and `cargo test -p mux-tui` on `ubuntu-latest`/x86_64 and `ubuntu-24.04-arm`/aarch64). Tag-push releases remain owned by `.github/workflows/release.yml`.
