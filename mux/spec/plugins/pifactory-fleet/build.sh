#!/usr/bin/env bash
# build.sh - Compile the pifactory-fleet plugin's WASM adapter.
#
# Produces bin/fleet.wasm from src/lib.rs via cargo's
# wasm32-unknown-unknown target. Idempotent: re-running rebuilds in
# place.
#
# Requires:
#   - cargo (1.75+; tested against the mattyx toolchain pin of 1.97)
#   - the `wasm32-unknown-unknown` rustup target
#     (`rustup target add wasm32-unknown-unknown`)
#
# Does NOT require:
#   - the mattyx build toolchain (no zig vendored here — this
#     crate is freestanding and does not link against mux-tui or
#     wasmtime at compile time; the wasmtime side is the host)
#   - network access, after the first build (cargo caches in
#     `target/`).
#
# Usage:
#   ./build.sh
#
# Output:
#   bin/fleet.wasm   the loader-installable artifact (commit this
#                    into the repo so users can install without
#                    needing a wasm32 toolchain themselves)

set -euo pipefail

# Resolve our own directory so the script works regardless of the
# caller's cwd. BASH_SOURCE is reliable here because we are run
# directly (not sourced — see scripts/cmux-panel-lib.sh:23 for the
# sourced-library caveat).
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH"
cd "$HERE"

# Sanity-check the toolchain before doing any work. Failing fast
# here is friendlier than a half-built artifact.
if ! command -v cargo >/dev/null 2>&1; then
    echo "build.sh: cargo not found on PATH" >&2
    echo "  install rustup: https://rustup.rs/" >&2
    exit 1
fi

# Detect the wasm32 target. We probe via `rustup target list
# --installed` so we surface a useful message ("run rustup target
# add wasm32-unknown-unknown") rather than cargo's own cryptic
# "can't find crate for std".
if rustup target list --installed >/dev/null 2>&1; then
    if ! rustup target list --installed | grep -q '^wasm32-unknown-unknown$'; then
        echo "build.sh: wasm32-unknown-unknown target not installed" >&2
        echo "  install it: rustup target add wasm32-unknown-unknown" >&2
        exit 1
    fi
else
    # No rustup (e.g. distro package). Best-effort: try the build
    # and let cargo produce its own error.
    :
fi

# Build into a target/ subdir so we don't pollute the user's
# workspace-wide target/ (this crate is NOT in the mattyx
# workspace, so a sibling target/ is the only choice anyway).
echo "build.sh: compiling src/lib.rs -> bin/fleet.wasm"
cargo build \
    --release \
    --target wasm32-unknown-unknown \
    --target-dir "$HERE/target"

# Cargo's wasm32 cdylib output is named after the crate, not the
# binary (we have no [[bin]] in Cargo.toml — only a [lib]). The
# output is target/wasm32-unknown-unknown/release/mtyx_pifactory_fleet.wasm.
ARTIFACT="$HERE/target/wasm32-unknown-unknown/release/mtyx_pifactory_fleet.wasm"
if [[ ! -f "$ARTIFACT" ]]; then
    echo "build.sh: expected artifact missing at $ARTIFACT" >&2
    echo "  (cargo's cdylib output name may have changed; inspect target/)" >&2
    exit 1
fi

mkdir -p "$HERE/bin"
cp "$ARTIFACT" "$HERE/bin/fleet.wasm"

# Print a one-line summary so the user can eyeball the size.
SIZE=$(stat -c '%s' "$HERE/bin/fleet.wasm" 2>/dev/null || stat -f '%z' "$HERE/bin/fleet.wasm")
echo "build.sh: wrote bin/fleet.wasm ($SIZE bytes)"
