#!/usr/bin/env bash
# Fetches a pinned zig (build.zig hard-requires 0.15.2; see README) into
# .tools/ without touching any system zig install, then builds mux-tui.
# OS table: Linux and macOS fetch a .tar.xz (bsdtar/GNU tar both handle
# xz natively); Windows fetches a .zip and extracts with unzip when
# available, else PowerShell Expand-Archive (paths converted via
# cygpath under Git Bash). The Linux path is byte-identical to the
# pre-cross-OS version of this script.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ZIG_VERSION="0.15.2"
TOOLS_DIR="$ROOT/.tools"

# uname -s reports Linux / Darwin; Windows shells report MINGW*, MSYS*
# or CYGWIN* (Git Bash / MSYS2 / Cygwin). Windows_NT covers any shell
# that forwards the $OS env var instead of a real uname.
case "$(uname -s)" in
  Linux)  ZIG_OS="linux" ;;
  Darwin) ZIG_OS="macos" ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT) ZIG_OS="windows" ;;
  *) echo "error: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64) ZIG_ARCH="x86_64" ;;
  aarch64|arm64) ZIG_ARCH="aarch64" ;;
  *) echo "error: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

ZIG_DIR="$TOOLS_DIR/zig-${ZIG_ARCH}-${ZIG_OS}-${ZIG_VERSION}"
if [ "$ZIG_OS" = "windows" ]; then
  ZIG_TARBALL="zig-${ZIG_ARCH}-${ZIG_OS}-${ZIG_VERSION}.zip"
  ZIG_BIN="$ZIG_DIR/zig.exe"
else
  ZIG_TARBALL="zig-${ZIG_ARCH}-${ZIG_OS}-${ZIG_VERSION}.tar.xz"
  ZIG_BIN="$ZIG_DIR/zig"
fi

if [ ! -x "$ZIG_BIN" ]; then
  echo "==> fetching zig $ZIG_VERSION ($ZIG_ARCH) into $ZIG_DIR"
  mkdir -p "$TOOLS_DIR"
  curl -fsSL -o "$TOOLS_DIR/$ZIG_TARBALL" \
    "https://ziglang.org/download/${ZIG_VERSION}/${ZIG_TARBALL}"
  case "$ZIG_OS" in
    windows)
      # No tar/xz story for zig's .zip: unzip when present (Git Bash
      # ships it on GitHub runners), else Expand-Archive with Windows
      # paths (cygpath converts when we are running under an MSYS
      # lineage shell; plain paths already work under PowerShell).
      if command -v unzip >/dev/null 2>&1; then
        unzip -q "$TOOLS_DIR/$ZIG_TARBALL" -d "$TOOLS_DIR"
      else
        win_tarball="$(cygpath -w "$TOOLS_DIR/$ZIG_TARBALL" 2>/dev/null || echo "$TOOLS_DIR/$ZIG_TARBALL")"
        win_tools="$(cygpath -w "$TOOLS_DIR" 2>/dev/null || echo "$TOOLS_DIR")"
        powershell -NoProfile -Command \
          "Expand-Archive -LiteralPath '$win_tarball' -DestinationPath '$win_tools' -Force"
      fi
      ;;
    *)
      tar -C "$TOOLS_DIR" -xf "$TOOLS_DIR/$ZIG_TARBALL"
      ;;
  esac
  rm "$TOOLS_DIR/$ZIG_TARBALL"
fi

if [ ! -e "$ROOT/ghostty/build.zig" ]; then
  echo "==> initializing ghostty submodule"
  git -C "$ROOT" submodule update --init ghostty
fi

# Line-ending hardening (PR #101, run 35309727154): the windows-latest
# image sets core.autocrlf=true system-wide and this repo carries no
# .gitattributes, so actions/checkout converts patches/*.patch to CRLF
# in the parent worktree — while ghostty's own .gitattributes pins *.zig
# (and friends) to eol=lf. git apply matches context byte-exactly, so a
# CRLF patch against LF files fails with "patch does not apply". Pin the
# submodule to no conversion and rewrite its worktree from the index so
# the target side is always deterministic LF; the patch side is handled
# by the CR-stripping rung in the apply ladder below.
git -C "$ROOT/ghostty" config core.autocrlf false
git -C "$ROOT/ghostty" checkout-index --force --all

for patch in "$ROOT"/patches/*.patch; do
  [ -e "$patch" ] || continue
  # Already applied? The reverse check tolerates line-ending drift in
  # both directions (strict first, then whitespace-insensitive).
  if git -C "$ROOT/ghostty" apply --check --reverse "$patch" 2>/dev/null \
     || git -C "$ROOT/ghostty" apply --check --reverse --ignore-whitespace "$patch" 2>/dev/null; then
    continue # already applied
  fi
  echo "==> applying $(basename "$patch") to ghostty/"
  patch_lf="$(mktemp)"
  tr -d '\r' < "$patch" > "$patch_lf"
  if git -C "$ROOT/ghostty" apply --check "$patch" 2>/dev/null; then
    git -C "$ROOT/ghostty" apply "$patch"
  elif ! cmp -s "$patch_lf" "$patch" \
     && git -C "$ROOT/ghostty" apply --check "$patch_lf" 2>/dev/null; then
    # Parent checkout CRLF-converted the patch; the CR-stripped copy is
    # byte-identical to the committed LF patch, so this stays an exact
    # apply, not a fuzzy one.
    git -C "$ROOT/ghostty" apply "$patch_lf"
  elif git -C "$ROOT/ghostty" apply --check --ignore-whitespace "$patch" 2>/dev/null; then
    git -C "$ROOT/ghostty" apply --ignore-whitespace "$patch"
  else
    echo "error: $(basename "$patch") does not apply to ghostty/" >&2
    rm -f "$patch_lf"
    exit 1
  fi
  rm -f "$patch_lf"
done

echo "==> building mux-tui (release)"
cd "$ROOT/mux"
CARGO_TARGET_ARGS=()
if [ -n "${MTYX_CARGO_TARGET:-}" ]; then
  CARGO_TARGET_ARGS=(--target "$MTYX_CARGO_TARGET")
fi
# Pin cargo to the Rust 1.97 toolchain via rustup's "+toolchain" syntax
# when rustup is available. The dtolnay/rust-toolchain action installs Rust
# 1.97 under ~/.cargo but the CI image's PATH may still surface an older
# system cargo first (1.74 on ubuntu-22.04, 1.82 on ubuntu-24.04). Older
# cargo < 1.78 cannot read Cargo.lock v4, which we generate by default. The
# "+1.97" prefix routes through rustup regardless of PATH ordering. See
# AGENTS.md "Pinned toolchain" for the rust pin.
#
# On a box without rustup (system cargo only) "+1.97" is "no such command",
# so fall back to the plain cargo invocation. An old system cargo is only a
# warning, not a hard failure — the lockfile parser is the real constraint.
if command -v cargo >/dev/null 2>&1; then
    CARGO_BIN="$(command -v cargo)"
else
    echo "error: cargo not found on PATH (dtolnay/rust-toolchain should install it)" >&2
    exit 1
fi
CARGO_VERSION="$("$CARGO_BIN" --version)"
echo "    using: $CARGO_VERSION"
# "cargo 1.99.0 (...)" -> minor "99"
CARGO_MINOR="${CARGO_VERSION#*.}"
CARGO_MINOR="${CARGO_MINOR%%.*}"
case "$CARGO_MINOR" in
    ''|*[!0-9]*) ;;
    *)
        if [ "$CARGO_MINOR" -lt 78 ]; then
            echo "warning: $CARGO_VERSION is older than 1.78; Cargo.lock may not parse" >&2
        fi
        ;;
esac
if rustup --version >/dev/null 2>&1; then
    ZIG="$ZIG_BIN" "$CARGO_BIN" "+1.97" build --release -p mux-tui "${CARGO_TARGET_ARGS[@]+"${CARGO_TARGET_ARGS[@]}"}"
else
    echo "    rustup not found; building with $CARGO_BIN directly (no +1.97 pin)"
    ZIG="$ZIG_BIN" "$CARGO_BIN" build --release -p mux-tui "${CARGO_TARGET_ARGS[@]+"${CARGO_TARGET_ARGS[@]}"}"
fi

if [ -n "${MTYX_CARGO_TARGET:-}" ]; then
  BUILT_BIN="$ROOT/mux/target/$MTYX_CARGO_TARGET/release/mtyx"
  [ "$ZIG_OS" = "windows" ] && BUILT_BIN="${BUILT_BIN}.exe"
  mkdir -p "$ROOT/mux/target/release"
  cp "$BUILT_BIN" "$ROOT/mux/target/release/"
fi

if [ "$ZIG_OS" = "windows" ]; then
  echo "==> built: $ROOT/mux/target/release/mtyx.exe"
else
  echo "==> built: $ROOT/mux/target/release/mtyx"
fi
