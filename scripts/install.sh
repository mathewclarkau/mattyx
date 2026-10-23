#!/usr/bin/env bash
# One-command installer for `mtyx` (issue #141).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mathewclarkau/mattyx/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/mathewclarkau/mattyx/main/scripts/install.sh | bash -s -- --version v0.18.0
#
# Environment overrides (also used by offline CI tests):
#   MTYX_INSTALL_DIR          target directory (default: $HOME/.local/bin)
#   MTYX_INSTALL_VERSION      release tag to install (default: latest via GitHub API)
#   MTYX_INSTALL_API_URL      override GitHub releases/latest JSON endpoint (or file://)
#   MTYX_INSTALL_BASE_URL     override download base URL (default: https://github.com/mathewclarkau/mattyx/releases/download/<tag>)
#   MTYX_INSTALL_OS           override detected OS (linux | macos | windows)
#   MTYX_INSTALL_ARCH         override detected arch (x86_64 | aarch64)
#   MTYX_INSTALL_SKIP_VERIFY_VERSION=1  skip executing `<staged> --version` (for cross-OS tests)
#
# Security posture (per #133 & AGENTS.md):
#   - Mandatory SHA-256 verification against `<asset>.sha256` sidecar before installation.
#   - Refuses to overwrite a symlink target (`[ -L "$target" ]`) with actionable guidance.
#   - Stages into a sibling temp file in the destination directory and atomically `mv`s into place.
#   - Never invokes `sudo`.
set -euo pipefail

REPO_URL="https://github.com/mathewclarkau/mattyx"
RELEASES_URL="${REPO_URL}/releases"
DEFAULT_API_URL="https://api.github.com/repos/mathewclarkau/mattyx/releases/latest"

INSTALL_DIR="${MTYX_INSTALL_DIR:-${HOME}/.local/bin}"
REQUESTED_VERSION="${MTYX_INSTALL_VERSION:-}"

usage() {
  cat <<'EOF'
Usage: install.sh [--dir <path>] [--version <tag>] [--help]

Install the prebuilt mtyx binary for the current OS and architecture,
verifying its .sha256 sidecar before placing it on disk.

Options:
  --dir <path>       Install directory (default: ~/.local/bin)
  --version <tag>    Release tag to install, e.g. v0.18.0 (default: latest)
  -h, --help         Show this help message
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dir)
      [ "$#" -ge 2 ] || { echo "install.sh: --dir requires a path argument" >&2; exit 2; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --dir=*)
      INSTALL_DIR="${1#--dir=}"
      shift
      ;;
    --version)
      [ "$#" -ge 2 ] || { echo "install.sh: --version requires a tag argument" >&2; exit 2; }
      REQUESTED_VERSION="$2"
      shift 2
      ;;
    --version=*)
      REQUESTED_VERSION="${1#--version=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "install.sh: unknown option '$1' (try --help)" >&2
      exit 2
      ;;
  esac
done

detect_os() {
  if [ -n "${MTYX_INSTALL_OS:-}" ]; then
    printf '%s' "$MTYX_INSTALL_OS"
    return 0
  fi
  case "$(uname -s)" in
    Linux)  printf 'linux' ;;
    Darwin) printf 'macos' ;;
    MINGW*|MSYS*|CYGWIN*|Windows_NT) printf 'windows' ;;
    *)
      echo "install.sh: unsupported operating system '$(uname -s)'; download or build manually from ${RELEASES_URL}" >&2
      return 1
      ;;
  esac
}

detect_arch() {
  if [ -n "${MTYX_INSTALL_ARCH:-}" ]; then
    printf '%s' "$MTYX_INSTALL_ARCH"
    return 0
  fi
  case "$(uname -m)" in
    x86_64|amd64)   printf 'x86_64' ;;
    aarch64|arm64)  printf 'aarch64' ;;
    *)
      echo "install.sh: unsupported architecture '$(uname -m)'; download or build manually from ${RELEASES_URL}" >&2
      return 1
      ;;
  esac
}

asset_name_for() {
  local os="$1" arch="$2"
  case "${os}-${arch}" in
    linux-x86_64)   printf 'mtyx-linux-x86_64' ;;
    linux-aarch64)  printf 'mtyx-linux-aarch64' ;;
    macos-aarch64)  printf 'mtyx-macos-aarch64' ;;
    macos-x86_64)   printf 'mtyx-macos-x86_64' ;;
    windows-x86_64) printf 'mtyx-windows-x86_64.exe' ;;
    *)
      echo "install.sh: no prebuilt release binary for ${os}-${arch}; download or build manually from ${RELEASES_URL}" >&2
      return 1
      ;;
  esac
}

compute_sha256() {
  local file="$1" hex=""
  if command -v sha256sum >/dev/null 2>&1; then
    hex="$(sha256sum -- "$file" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1; then
    hex="$(shasum -a 256 -- "$file" | cut -d' ' -f1)"
  elif command -v openssl >/dev/null 2>&1; then
    hex="$(openssl dgst -sha256 -- "$file" | awk '{print $NF}')"
  elif command -v certutil >/dev/null 2>&1; then
    local wfile
    wfile="$(cygpath -w -- "$file" 2>/dev/null || printf '%s' "$file")"
    hex="$(certutil -hashfile "$wfile" SHA256 | sed -n '2p' | tr -d '\r\n ' | tr 'A-F' 'a-f')"
  else
    echo "install.sh: no SHA-256 tool found (need sha256sum, shasum, openssl, or certutil)" >&2
    return 1
  fi
  hex="$(printf '%s' "$hex" | tr 'A-F' 'a-f')"
  if ! printf '%s' "$hex" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "install.sh: failed to compute SHA-256 digest for $file (got '$hex')" >&2
    return 1
  fi
  printf '%s' "$hex"
}

fetch_to_file() {
  local url="$1" dest="$2"
  case "$url" in
    file://*)
      local src="${url#file://}"
      cp -- "$src" "$dest"
      ;;
    *)
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$dest" "$url"
      elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url"
      else
        echo "install.sh: neither curl nor wget is installed" >&2
        return 1
      fi
      ;;
  esac
}

resolve_version_tag() {
  if [ -n "$REQUESTED_VERSION" ]; then
    case "$REQUESTED_VERSION" in
      v*) printf '%s' "$REQUESTED_VERSION" ;;
      *)  printf 'v%s' "$REQUESTED_VERSION" ;;
    esac
    return 0
  fi
  local api_url="${MTYX_INSTALL_API_URL:-$DEFAULT_API_URL}"
  local tmp_json
  tmp_json="$(mktemp)"
  if ! fetch_to_file "$api_url" "$tmp_json"; then
    rm -f "$tmp_json"
    echo "install.sh: failed to query latest release from ${api_url}" >&2
    return 1
  fi
  local tag
  tag="$(grep -Eo '"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"' "$tmp_json" | head -n 1 | sed -E 's/.*"([^"]+)"$/\1/')"
  rm -f "$tmp_json"
  if [ -z "$tag" ]; then
    echo "install.sh: could not parse tag_name from ${api_url}" >&2
    return 1
  fi
  case "$tag" in
    v*) printf '%s' "$tag" ;;
    *)  printf 'v%s' "$tag" ;;
  esac
}

OS="$(detect_os)"
ARCH="$(detect_arch)"
ASSET="$(asset_name_for "$OS" "$ARCH")"

BIN_NAME="mtyx"
if [ "$OS" = "windows" ]; then
  BIN_NAME="mtyx.exe"
fi

if [ -L "$INSTALL_DIR" ]; then
  echo "install.sh: refusing to install into symlinked directory ${INSTALL_DIR}" >&2
  exit 1
fi
mkdir -p "$INSTALL_DIR"

TARGET_PATH="${INSTALL_DIR%/}/${BIN_NAME}"

# Rule (#133 / AGENTS.md): refuse to overwrite a symlink target so a
# source-checkout `ln -sf .../mux/target/release/mtyx ~/.local/bin/mtyx`
# is never clobbered through the symlink.
if [ -L "$TARGET_PATH" ]; then
  echo "install.sh: refusing to overwrite symlink ${TARGET_PATH} — if installed from a source checkout (ln -sf), pull and rebuild there, or remove the symlink first" >&2
  exit 1
fi

if [ -e "$TARGET_PATH" ] && [ ! -w "$TARGET_PATH" ]; then
  echo "install.sh: cannot write to ${TARGET_PATH} (permission denied) — if installed via a package manager, update with that instead, or download from ${RELEASES_URL}" >&2
  exit 1
fi

TAG="$(resolve_version_tag)"
EXPECTED_VER="${TAG#v}"

if [ -n "${MTYX_INSTALL_BASE_URL:-}" ]; then
  BASE_URL="${MTYX_INSTALL_BASE_URL%/}"
else
  BASE_URL="${RELEASES_URL}/download/${TAG}"
fi

ASSET_URL="${BASE_URL}/${ASSET}"
SIDECAR_URL="${BASE_URL}/${ASSET}.sha256"

STAGED_BIN="${INSTALL_DIR%/}/.mtyx-install-$$.tmp"
STAGED_SHA="${INSTALL_DIR%/}/.mtyx-install-$$.sha256.tmp"

cleanup() {
  rm -f "$STAGED_BIN" "$STAGED_SHA"
}
trap cleanup EXIT

echo "==> fetching ${TAG} (${ASSET})..."
fetch_to_file "$SIDECAR_URL" "$STAGED_SHA"
fetch_to_file "$ASSET_URL" "$STAGED_BIN"

EXPECTED_SHA="$(awk 'NF {print tolower($1); exit}' "$STAGED_SHA" | tr -d '\r')"
if ! printf '%s' "$EXPECTED_SHA" | grep -Eq '^[0-9a-f]{64}$'; then
  echo "install.sh: invalid .sha256 sidecar from ${SIDECAR_URL}: '${EXPECTED_SHA}'" >&2
  exit 1
fi

ACTUAL_SHA="$(compute_sha256 "$STAGED_BIN")"
if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
  echo "install.sh: SHA-256 checksum MISMATCH for ${ASSET}" >&2
  echo "  expected: ${EXPECTED_SHA}" >&2
  echo "  actual:   ${ACTUAL_SHA}" >&2
  exit 1
fi
echo "==> sha256 ok (${ACTUAL_SHA})"

chmod 0755 "$STAGED_BIN"

if [ "${MTYX_INSTALL_SKIP_VERIFY_VERSION:-0}" != "1" ]; then
  REPORTED_VER="$("$STAGED_BIN" --version 2>/dev/null | tr -d '\r' || true)"
  if [ "$REPORTED_VER" != "mtyx ${EXPECTED_VER}" ]; then
    echo "install.sh: staged binary reported '${REPORTED_VER}', expected 'mtyx ${EXPECTED_VER}'" >&2
    exit 1
  fi
fi

# Double-check symlink guard right before atomic rename.
if [ -L "$TARGET_PATH" ]; then
  echo "install.sh: refusing to overwrite symlink ${TARGET_PATH}" >&2
  exit 1
fi

mv -f "$STAGED_BIN" "$TARGET_PATH"
rm -f "$STAGED_SHA"

echo "==> installed mtyx ${EXPECTED_VER} to ${TARGET_PATH}"
case ":${PATH:-}:" in
  *":${INSTALL_DIR%/}:"*) ;;
  *)
    echo "note: ${INSTALL_DIR} is not on your PATH; add this to your shell rc:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
