#!/usr/bin/env bash
# Sync `mux/docs/{protocol,configuration}.md` into `docs-site/src/` and build
# the static HTML site in `docs-site/dist/` (issue #142).
#
# Usage:
#   scripts/sync-docs-site.sh           # sync sources + build docs-site/dist/
#   scripts/sync-docs-site.sh --check   # fail if docs-site/src/ drifted from mux/docs/
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$ROOT/docs-site/build.py" "$@"
