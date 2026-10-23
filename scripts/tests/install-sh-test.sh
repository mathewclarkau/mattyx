#!/usr/bin/env bash
# Offline test for scripts/install.sh (issue #141; review finding F11).
#
# Plain bash, no test framework and no network: runs install.sh against a
# file:// base URL holding a fake asset + .sha256 sidecar, using the
# MTYX_INSTALL_* hooks documented in the installer header, and asserts:
#   (a) success: the verified binary lands in the install dir and the
#       staged temp files are cleaned up,
#   (b) a checksum mismatch aborts before anything is installed,
#   (c) a symlinked target is refused with actionable guidance.
#
# Usage: bash scripts/tests/install-sh-test.sh
set -euo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="${SCRIPTS_DIR}/install.sh"

if [ ! -f "$INSTALL_SH" ]; then
  echo "install-sh-test.sh: cannot find scripts/install.sh at ${INSTALL_SH}" >&2
  exit 2
fi

FAKE_VERSION="0.99.0"
TAG="v${FAKE_VERSION}"
ASSET="mtyx-linux-x86_64"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/mtyx-install-sh-test-XXXXXX")"
cleanup_test() {
  # Remove staged leftovers first (the installer's own trap should already
  # have); then the scratch tree.
  for d in "$WORK"/bin-ok "$WORK"/bin-mismatch "$WORK"/bin-symlink; do
    [ -d "$d" ] && rm -f "$d"/.mtyx-install-* 2>/dev/null || true
  done
  rm -rf "$WORK"
}
trap cleanup_test EXIT

# --- fixtures: fake asset (reports the expected --version) + sidecars.
# Two base dirs: one with the matching digest, one with a bogus digest.
# (install.sh derives the sidecar name from the asset name, so the two
# cases need separate base dirs.)
mkdir -p "$WORK/repo-good" "$WORK/repo-bad" \
         "$WORK/bin-ok" "$WORK/bin-mismatch" "$WORK/bin-symlink"
for d in "$WORK/repo-good" "$WORK/repo-bad"; do
  cat > "$d/$ASSET" <<EOF
#!/bin/sh
echo "mtyx ${FAKE_VERSION}"
EOF
  chmod 0755 "$d/$ASSET"
done

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA="$(sha256sum -- "$WORK/repo-good/$ASSET" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA="$(shasum -a 256 -- "$WORK/repo-good/$ASSET" | cut -d' ' -f1)"
else
  echo "install-sh-test.sh: need sha256sum or shasum to build the fixture" >&2
  exit 2
fi
printf '%s  %s\n' "$ACTUAL_SHA" "$ASSET" > "$WORK/repo-good/$ASSET.sha256"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$ASSET" \
  > "$WORK/repo-bad/$ASSET.sha256"

run_installer() {
  local dir="$1" base_dir="$2"
  MTYX_INSTALL_DIR="$dir" \
  MTYX_INSTALL_OS=linux \
  MTYX_INSTALL_ARCH=x86_64 \
  MTYX_INSTALL_VERSION="$TAG" \
  MTYX_INSTALL_BASE_URL="file://$base_dir" \
  bash "$INSTALL_SH"
}

failures=0
check() {
  if [ "$2" = "0" ]; then
    echo "PASS: $1"
  else
    echo "FAIL: $1" >&2
    failures=$((failures + 1))
  fi
}

# Bare assertions run as `... || status=1` so a failure is recorded
# instead of aborting the test script under `set -e`.

# --- (a) success: sidecar matches, binary installed, no staged leftovers
LOG="$WORK/out-success.log"
if run_installer "$WORK/bin-ok" "$WORK/repo-good" > "$LOG" 2>&1; then
  check "(a) installer exits 0 with a matching sidecar" 0
else
  check "(a) installer exits 0 with a matching sidecar" 1
  sed 's/^/    | /' "$LOG" >&2
fi
status=0; [ -x "$WORK/bin-ok/mtyx" ] || status=1
check "(a) installed binary exists and is executable" "$status"
status=0; [ "$("$WORK/bin-ok/mtyx" --version)" = "mtyx ${FAKE_VERSION}" ] || status=1
check "(a) installed binary reports the release version" "$status"
status=0; grep -q "sha256 ok" "$LOG" || status=1
check "(a) log reports the verified digest" "$status"
status=0; [ -z "$(find "$WORK/bin-ok" -maxdepth 1 -name '.mtyx-install-*' -print -quit)" ] || status=1
check "(a) no staged temp files remain after success" "$status"

# --- (b) checksum mismatch: wrong sidecar digest, install must abort
LOG="$WORK/out-mismatch.log"
if run_installer "$WORK/bin-mismatch" "$WORK/repo-bad" > "$LOG" 2>&1; then
  check "(b) installer aborts on checksum mismatch" 1
  sed 's/^/    | /' "$LOG" >&2
else
  check "(b) installer aborts on checksum mismatch" 0
  status=0; grep -q "SHA-256 checksum MISMATCH" "$LOG" || status=1
  check "(b) mismatch names the failure (SHA-256 checksum MISMATCH)" "$status"
  status=0; [ ! -e "$WORK/bin-mismatch/mtyx" ] || status=1
  check "(b) no binary is installed on mismatch" "$status"
  status=0; [ -z "$(find "$WORK/bin-mismatch" -maxdepth 1 -name '.mtyx-install-*' -print -quit)" ] || status=1
  check "(b) no staged temp files remain after mismatch" "$status"
fi

# --- (c) symlinked target: must be refused with guidance
echo "do not clobber me" > "$WORK/victim"
ln -s "$WORK/victim" "$WORK/bin-symlink/mtyx"
LOG="$WORK/out-symlink.log"
if run_installer "$WORK/bin-symlink" "$WORK/repo-good" > "$LOG" 2>&1; then
  check "(c) installer refuses a symlinked target" 1
  sed 's/^/    | /' "$LOG" >&2
else
  check "(c) installer refuses a symlinked target" 0
  status=0; grep -q "refusing to overwrite symlink" "$LOG" || status=1
  check "(c) refusal explains the symlink (actionable guidance)" "$status"
  status=0; [ "$(cat "$WORK/victim")" = "do not clobber me" ] || status=1
  check "(c) symlink victim is untouched" "$status"
fi

if [ "$failures" -ne 0 ]; then
  echo "install-sh-test.sh: ${failures} assertion(s) failed" >&2
  exit 1
fi
echo "install-sh-test.sh: all assertions passed"
