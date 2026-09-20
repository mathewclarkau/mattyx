#!/usr/bin/env bash
# Release-artifact checksum helper (issue #90).
#
# `release.yml` runs on four runner families (ubuntu x86_64/arm64,
# macOS arm64, Windows Git Bash) and none of them agree on the coreutils
# that ship a sha256 tool:
#
#   * Linux            -> `sha256sum`   (coreutils)
#   * macOS            -> `shasum -a 256` (Perl shasum; `sha256sum` is
#                          only present if coreutils was brew-installed,
#                          which the runner image does not guarantee)
#   * Windows Git Bash -> `sha256sum`   (also has `sha256sum` from its
#                          bundled coreutils, but `certutil` is the
#                          always-present fallback)
#
# Usage:
#   scripts/sha256.sh gen    <file> [<file>...]  # write <file>.sha256 sidecars
#   scripts/sha256.sh verify <dir>               # sha256sum -c every *.sha256 in <dir>
#
# Sidecar format is the canonical `<hex>  <basename>` (two spaces, LF
# line ending) so a plain `sha256sum -c` accepts it unmodified. The
# checksum names only the basename, so verification must run from the
# directory holding the files — which is exactly how release.yml uses it
# (`dist/`).
set -euo pipefail

usage() {
  echo "usage: $0 gen <file>... | verify <dir>" >&2
  exit 2
}

# Emit "<hex>  <basename>" for one file on stdout.
hash_one() {
  local file="$1" hex
  if command -v sha256sum >/dev/null 2>&1; then
    hex="$(sha256sum -- "$file" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1; then
    hex="$(shasum -a 256 -- "$file" | cut -d' ' -f1)"
  elif command -v openssl >/dev/null 2>&1; then
    hex="$(openssl dgst -sha256 -- "$file" | awk '{print $NF}')"
  elif command -v certutil >/dev/null 2>&1; then
    # Windows-native fallback (Git Bash). certutil hashes *Windows*
    # paths and prints UTF-16LE, so normalise both.
    local wfile
    wfile="$(cygpath -w -- "$file" 2>/dev/null || printf '%s' "$file")"
    hex="$(certutil -hashfile "$wfile" SHA256 | sed -n '2p' | tr -d '\r\n ' | tr 'A-F' 'a-f')"
  else
    echo "sha256.sh: no sha256 tool found (need sha256sum, shasum, openssl or certutil)" >&2
    return 1
  fi
  if ! printf '%s' "$hex" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "sha256.sh: failed to compute a sha256 for $file (got '$hex')" >&2
    return 1
  fi
  printf '%s  %s\n' "$hex" "$(basename -- "$file")"
}

cmd_gen() {
  [ "$#" -ge 1 ] || usage
  local file
  for file in "$@"; do
    if [ ! -f "$file" ]; then
      echo "sha256.sh: no such file: $file" >&2
      return 1
    fi
    # `printf > file` writes LF verbatim; a CRLF sidecar would make
    # `sha256sum -c` report "no properly formatted checksum lines".
    hash_one "$file" > "$file.sha256"
    echo "wrote $file.sha256"
  done
}

cmd_verify() {
  [ "$#" -eq 1 ] || usage
  local dir="$1"
  [ -d "$dir" ] || { echo "sha256.sh: no such directory: $dir" >&2; return 1; }
  local found=0 sidecar
  # Null-glob-safe: no match leaves the literal pattern, which -e rejects.
  shopt -s nullglob
  for sidecar in "$dir"/*.sha256; do
    found=1
    local base recorded file actual
    base="$(basename -- "$sidecar")"
    # Sidecar format is `<hex>  <basename>`; the basename it names is
    # the asset file, not the sidecar itself.
    recorded="$(cut -d' ' -f1 < "$sidecar")"
    file="$dir/$(basename -- "${base%.sha256}")"
    if [ ! -f "$file" ]; then
      echo "sha256.sh: $base names a missing file: $(basename -- "$file")" >&2
      return 1
    fi
    echo "verifying $base..."
    actual="$(hash_one "$file" | cut -d' ' -f1)"
    if [ "$actual" != "$recorded" ]; then
      echo "sha256.sh: checksum MISMATCH for $(basename -- "$file")" >&2
      echo "  expected: $recorded" >&2
      echo "  actual:   $actual" >&2
      return 1
    fi
    echo "$(basename -- "$file"): OK"
  done
  shopt -u nullglob
  if [ "$found" -eq 0 ]; then
    echo "sha256.sh: no *.sha256 sidecars found in $dir" >&2
    return 1
  fi
}

case "${1:-}" in
  gen)
    shift
    cmd_gen "$@"
    ;;
  verify)
    shift
    cmd_verify "$@"
    ;;
  *)
    usage
    ;;
esac
