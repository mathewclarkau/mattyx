#!/usr/bin/env bash
# Extract per-tag migration fixtures from the release history (issue #144,
# review finding F1).
#
# For each `v0.*` tag (all of them by default, or a subset via --tags):
#
#   mux/crates/mux-tui/tests/fixtures/<tag>-mux.json
#       The tag's own config.rs `//!` JSON doc example -- the canonical
#       config shape documented at that release.
#
#   mux/crates/mux-core/tests/fixtures/<tag>-session.json
#       The tag's own persist.rs `round_trips_through_json` snapshot
#       struct literal, transcoded to the JSON the era's serializer wrote
#       (Option fields set to None are omitted, matching how old snapshots
#       looked on disk; serde accepts absent and null alike on load).
#
# The fixture tests loop over whatever files are present in those
# directories, so newly extracted eras are covered by the next test run
# with no further code changes. Running the extractor across all ~35 tags
# is deferred operator work (see fix-notes); the committed set covers the
# representative eras already pinned inline (v0.5, v0.10, v0.17, v0.22,
# v0.27 -- 6 fixture rows over 5 distinct tags).
#
# Requirements: git (release tags fetched), python3 (struct-literal
# transcoder). No network, no builds: every source is `git show`.
#
# Usage:
#   scripts/extract-migration-fixtures.sh [--tags v0.10.0,v0.17.0] [--dry-run]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TUI_FIXTURES="${REPO_ROOT}/mux/crates/mux-tui/tests/fixtures"
CORE_FIXTURES="${REPO_ROOT}/mux/crates/mux-core/tests/fixtures"

TAGS=""
DRY_RUN=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --tags)
      [ "$#" -ge 2 ] || { echo "extract-migration-fixtures.sh: --tags requires a comma-separated list" >&2; exit 2; }
      TAGS="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help)
      sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "extract-migration-fixtures.sh: unknown option '$1' (try --help)" >&2; exit 2 ;;
  esac
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "extract-migration-fixtures.sh: python3 is required (snapshot transcoder)" >&2
  exit 2
fi

if [ -z "$TAGS" ]; then
  # NB: `git tag 'v0.*'` is not a valid invocation on git >= 2.56 (it is
  # parsed as a tag *name*); the list form is required.
  TAGS="$(git -C "$REPO_ROOT" tag -l 'v0.*' | sort -V | tr '\n' ' ')"
else
  TAGS="${TAGS//,/ }"
fi

[ -n "$TAGS" ] || { echo "extract-migration-fixtures.sh: no v0.* tags found" >&2; exit 1; }

# --- config fixture: the tag's config.rs `//!` JSON doc example -----------
extract_config_example() {
  local tag="$1"
  # NB: no early `exit` in awk — with `set -o pipefail` an early exit
  # SIGPIPEs `git show` and the pipeline is marked failed. Drain stdin
  # and gate on a `done` flag instead.
  git -C "$REPO_ROOT" show "${tag}:mux/crates/mux-tui/src/config.rs" \
    | awk '
        /^\/\/! ```json$/ { if (!done) capture = 1; next }
        capture && /^\/\/! ```$/ { capture = 0; done = 1 }
        capture { sub(/^\/\/! ?/, ""); print }
      '
}

# --- session fixture: transcode the tag's round_trips_through_json literal
extract_session_snapshot() {
  local tag="$1" src_tmp rc
  # The heredoc below supplies python's program on stdin, so the tag's
  # source goes through a temp file instead of a pipe.
  src_tmp="$(mktemp)"
  git -C "$REPO_ROOT" show "${tag}:mux/crates/mux-core/src/persist.rs" > "$src_tmp"
  rc=0
  python3 - "$tag" "$src_tmp" <<'PYEOF' || rc=$?
import json
import re
import sys

tag = sys.argv[1]
with open(sys.argv[2], encoding="utf-8") as f:
    src = f.read()

# Slice out the SessionSnapshot struct literal inside the tag's own
# `round_trips_through_json` test (brace-balanced; searching after the
# test's `fn` line skips `impl SessionSnapshot` blocks in the source).
fn_idx = src.index("fn round_trips_through_json")
start = src.index("SessionSnapshot {", fn_idx)
depth = 0
end = None
for i in range(start, len(src)):
    if src[i] == "{":
        depth += 1
    elif src[i] == "}":
        depth -= 1
        if depth == 0:
            end = i + 1
            break
if end is None:
    sys.exit(f"{tag}: unbalanced SessionSnapshot literal")
literal = src[start:end]

TOKEN = re.compile(r'''
    "(?:[^"\\]|\\.)*"                # string literal
  | [A-Za-z_][A-Za-z0-9_]*(?:\s*::\s*[A-Za-z_][A-Za-z0-9_]*)*   # ident, ::-qualified
  | \d+\.\d+                        # float
  | \d+                             # int
  | [{}()\[\],:]                    # punctuation
  | \.\s*into\s*\(\s*\)             # .into() -- dropped below
  | \s+                             # whitespace -- dropped below
''', re.VERBOSE)


def tokenize(text):
    """Token stream with whitespace and `.into()` noise removed."""
    return [
        m.group(0)
        for m in TOKEN.finditer(text)
        if not m.group(0).strip() == "" and not m.group(0).startswith(".")
    ]


# Preprocess: `Box::new(EXPR)` -> `(EXPR)`; `vec![` -> `[`.
text = literal.replace("Box::new(", "(").replace("vec![", "[")
tokens = tokenize(text)
pos = 0


def peek():
    return tokens[pos] if pos < len(tokens) else None


def take(expected=None):
    global pos
    tok = peek()
    if tok is None or (expected is not None and tok != expected):
        sys.exit(f"{tag}: transcoder hit unexpected token {tok!r} at {pos} "
                 f"(expected {expected!r}) -- handle this tag manually")
    pos += 1
    return tok


def is_name(tok):
    return bool(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", tok or ""))


def parse_value():
    tok = take()
    if tok.startswith('"'):
        return json.loads(tok)
    if tok == "None":
        return None  # caller omits the key
    if tok == "Some":
        take("(")
        inner = parse_value()
        take(")")
        return inner
    if tok == "(":  # preprocessed Box::new(...)
        inner = parse_value()
        take(")")
        return inner
    if re.fullmatch(r"\d+", tok):
        return int(tok)
    if re.fullmatch(r"\d+\.\d+", tok):
        return float(tok)
    if tok == "[":
        items = []
        if peek() == "]":
            take("]")
            return items
        while True:
            items.append(parse_value())
            nxt = take()
            if nxt == "]":
                return items
            if nxt != ",":
                sys.exit(f"{tag}: transcoder expected , or ] in vec, got {nxt!r}")
            if peek() == "]":  # trailing comma
                take("]")
                return items
    # ident or ident::Variant
    if "::" in tok:
        head, variant = [p.strip() for p in tok.split("::", 1)]
        nxt = peek()
        if nxt == "(":
            take("(")
            payload = parse_value()
            take(")")
            return {variant: payload}          # LayoutSnapshot::Leaf(0)
        if nxt == "{":
            return {variant: parse_fields()}    # LayoutSnapshot::Split { .. }
        return variant                           # DirSnapshot::Right
    if is_name(tok):
        return parse_fields()                   # SessionSnapshot { .. }
    sys.exit(f"{tag}: transcoder cannot handle token {tok!r}")


def parse_fields():
    take("{")
    fields = {}
    while True:
        if peek() == "}":  # empty struct or trailing comma
            take("}")
            return fields
        name = take()
        if not is_name(name):
            sys.exit(f"{tag}: transcoder expected field name, got {name!r}")
        take(":")
        value = parse_value()
        if value is not None:  # None -> omit the key (absent Option)
            fields[name] = value
        nxt = take()
        if nxt == "}":
            return fields
        if nxt != ",":
            sys.exit(f"{tag}: transcoder expected , or }} in struct, got {nxt!r}")


snapshot = parse_value()
if pos != len(tokens):
    sys.exit(f"{tag}: transcoder left {len(tokens) - pos} trailing tokens")
print(json.dumps(snapshot, indent=2))
PYEOF
  rm -f "$src_tmp"
  return "$rc"
}

written=0
skipped=0
for tag in $TAGS; do
  tag="${tag%,}"
  [ -n "$tag" ] || continue
  if ! git -C "$REPO_ROOT" rev-parse -q --verify "${tag}^{commit}" >/dev/null; then
    echo "SKIP ${tag}: not a valid tag in this repository" >&2
    skipped=$((skipped + 1))
    continue
  fi

  cfg_out="${TUI_FIXTURES}/${tag}-mux.json"
  snap_out="${CORE_FIXTURES}/${tag}-session.json"

  cfg="$(extract_config_example "$tag")" || {
    echo "SKIP ${tag}: config.rs doc example extraction failed" >&2
    skipped=$((skipped + 1))
    continue
  }
  if [ -z "$cfg" ]; then
    echo "SKIP ${tag}: no config.rs doc example found" >&2
    skipped=$((skipped + 1))
    continue
  fi
  snap="$(extract_session_snapshot "$tag")" || {
    echo "SKIP ${tag}: snapshot transcode failed (see above)" >&2
    skipped=$((skipped + 1))
    continue
  }

  if [ "$DRY_RUN" = "1" ]; then
    echo "DRY-RUN ${tag}: would write ${cfg_out} ($(printf '%s' "$cfg" | wc -c) bytes) and ${snap_out} ($(printf '%s' "$snap" | wc -c) bytes)"
  else
    mkdir -p "$TUI_FIXTURES" "$CORE_FIXTURES"
    printf '%s\n' "$cfg" > "$cfg_out"
    printf '%s\n' "$snap" > "$snap_out"
    echo "wrote ${cfg_out}"
    echo "wrote ${snap_out}"
  fi
  written=$((written + 1))
done

if [ "$DRY_RUN" = "1" ]; then
  echo "dry run complete: ${written} tag(s) extractable, ${skipped} skipped"
else
  echo "extracted ${written} tag(s), ${skipped} skipped"
  echo "run 'cargo test -p mux-tui -p mux-core' to verify the new fixtures on current main"
fi
