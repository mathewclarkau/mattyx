#!/usr/bin/env bash
# End-to-end dogfood demo for `mtyx` multi-agent orchestration (issue #143).
#
# Stages the headline "run 3 named agents, wait for the blocked one, read its
# output, answer its prompt, wait for completion, and clean up with zero leaked
# sessions/processes" workflow in an isolated runtime directory.
#
# Usage:
#   ./scripts/demo-factory.sh [path/to/mtyx]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

resolve_mtyx_bin() {
  if [ "${1:-}" != "" ]; then
    printf '%s' "$1"
    return 0
  fi
  if [ -n "${MTYX_BIN:-}" ] && [ -x "$MTYX_BIN" ]; then
    printf '%s' "$MTYX_BIN"
    return 0
  fi
  if [ -x "$ROOT/mux/target/release/mtyx" ]; then
    printf '%s' "$ROOT/mux/target/release/mtyx"
    return 0
  fi
  if [ -x "$ROOT/mux/target/debug/mtyx" ]; then
    printf '%s' "$ROOT/mux/target/debug/mtyx"
    return 0
  fi
  if command -v mtyx >/dev/null 2>&1; then
    command -v mtyx
    return 0
  fi
  echo "demo-factory.sh: mtyx binary not found; build with ./scripts/bootstrap.sh or pass path as \$1" >&2
  return 1
}

MTYX="$(resolve_mtyx_bin "${1:-}")"
# Use `/tmp` directly so Unix domain socket paths stay well under macOS's
# 104-byte SUN_LEN limit (macOS $TMPDIR is /var/folders/.../T/ which is ~50 chars).
RUNTIME_ROOT="$(mktemp -d "/tmp/mtyx-demo-XXXXXX")"
SESSION="demo-$$"
DAEMON_PID=""

# Isolated environment per #139 no-leak conventions:
# never touch the user's real XDG_RUNTIME_DIR, config, or sessions.
export XDG_RUNTIME_DIR="$RUNTIME_ROOT/xdg"
export TMPDIR="$RUNTIME_ROOT/tmp"
export HOME="$RUNTIME_ROOT/home"
export MTYX_DISABLE_UPDATE_CHECK=1
unset MTYX_MUX_SOCKET MTYX_MUX_SURFACE CMUX_MUX_SOCKET CMUX_MUX_SURFACE
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR" "$HOME"

cleanup() {
  local exit_code=$?
  if [ -n "$DAEMON_PID" ]; then
    "$MTYX" kill-session --session "$SESSION" >/dev/null 2>&1 || true
    kill "$DAEMON_PID" >/dev/null 2>&1 || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  "$MTYX" kill-stale >/dev/null 2>&1 || true
  rm -rf "$RUNTIME_ROOT"
  exit "$exit_code"
}
trap cleanup EXIT INT TERM

# Create a deterministic fixture worker script that simulates a real coding
# agent reporting lifecycle states via `mtyx report-agent`.
WORKER_SCRIPT="$RUNTIME_ROOT/mock-agent.sh"
cat > "$WORKER_SCRIPT" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
MTYX_BIN="$1"
ROLE="$2"

"$MTYX_BIN" report-agent --state working --source hook --agent "$ROLE" --message "starting $ROLE task" >/dev/null
echo "[$ROLE] ready for task prompt..."

IFS= read -r task_line || exit 0
echo "[$ROLE] received task: $task_line"

if [ "$ROLE" = "coder" ]; then
  echo "[$ROLE] Needs approval: apply retry backoff patch to src/client.rs? (reply yes/no)"
  "$MTYX_BIN" report-agent --state blocked --source hook --agent "$ROLE" --message "awaiting approval for src/client.rs" >/dev/null
  IFS= read -r approval_line || exit 0
  "$MTYX_BIN" report-agent --state working --source hook --agent "$ROLE" --message "applying approved patch" >/dev/null
  echo "[$ROLE] approval received ($approval_line); patch applied + tests passing."
  "$MTYX_BIN" report-agent --state done --source hook --agent "$ROLE" --message "patch committed" >/dev/null
else
  echo "[$ROLE] completed analysis for: $task_line"
  "$MTYX_BIN" report-agent --state done --source hook --agent "$ROLE" --message "$ROLE finished" >/dev/null
fi

# Keep surface alive briefly so orchestrator can read final screen before teardown.
sleep 30
EOF
chmod +x "$WORKER_SCRIPT"

echo "=================================================================="
echo "  mtyx Multi-Agent Factory Demo ($( "$MTYX" --version ))"
echo "=================================================================="
echo "==> [1/7] Starting isolated headless session '$SESSION'..."
"$MTYX" --headless --session "$SESSION" >/dev/null 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 50); do
  if "$MTYX" --session "$SESSION" identify >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
"$MTYX" --session "$SESSION" identify --json

echo ""
echo "==> [2/7] Spawning 3 named agent panes (coder, tester, reviewer)..."
"$MTYX" --session "$SESSION" agent start coder    --cwd "$RUNTIME_ROOT" -- "$WORKER_SCRIPT" "$MTYX" coder
"$MTYX" --session "$SESSION" agent start tester   --cwd "$RUNTIME_ROOT" -- "$WORKER_SCRIPT" "$MTYX" tester
"$MTYX" --session "$SESSION" agent start reviewer --cwd "$RUNTIME_ROOT" -- "$WORKER_SCRIPT" "$MTYX" reviewer

# Wait briefly for all 3 worker scripts to register their initial `working` state
sleep 0.6
echo "==> Active agent fleet (mtyx agent list):"
"$MTYX" --session "$SESSION" agent list

echo ""
echo "==> [3/7] Dispatching prompts with 'mtyx agent prompt'..."
"$MTYX" --session "$SESSION" agent prompt coder    "Implement exponential backoff in src/client.rs"
"$MTYX" --session "$SESSION" agent prompt tester   "Add unit tests for retry jitter"
"$MTYX" --session "$SESSION" agent prompt reviewer "Audit cancellation safety in retry loop"

echo ""
echo "==> [4/7] Waiting for 'coder' to reach 'blocked' state (mtyx agent wait coder --status blocked)..."
"$MTYX" --session "$SESSION" agent wait coder --status blocked --timeout 10000 >/dev/null

echo "==> Fleet state while 'coder' is blocked on approval:"
"$MTYX" --session "$SESSION" agent list

echo ""
echo "==> [5/7] Reading 'coder' output (mtyx agent read coder --source recent-unwrapped):"
"$MTYX" --session "$SESSION" agent read coder --source recent-unwrapped --lines 10

echo ""
echo "==> [6/7] Answering 'coder' approval prompt (mtyx agent prompt coder --force yes) and waiting for 'done'..."
"$MTYX" --session "$SESSION" agent prompt coder --force "yes"
"$MTYX" --session "$SESSION" agent wait coder    --status done --timeout 10000 >/dev/null
"$MTYX" --session "$SESSION" agent wait tester   --status done --timeout 10000 >/dev/null
"$MTYX" --session "$SESSION" agent wait reviewer --status done --timeout 10000 >/dev/null

echo "==> Final fleet status:"
"$MTYX" --session "$SESSION" agent list
echo "==> Final 'coder' transcript:"
"$MTYX" --session "$SESSION" agent read coder --source recent-unwrapped --lines 10

echo ""
echo "==> [7/7] Tearing down session '$SESSION' and verifying zero leaked sessions..."
"$MTYX" kill-session --session "$SESSION"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""

SESSIONS_JSON="$("$MTYX" list-sessions --json)"
echo "==> mtyx list-sessions --json: $SESSIONS_JSON"
if ! printf '%s' "$SESSIONS_JSON" | grep -Eq '"sessions":[[:space:]]*\[\]'; then
  echo "error: expected empty sessions list after cleanup, got: $SESSIONS_JSON" >&2
  exit 1
fi
echo "==> Demo complete — zero leaked sessions or processes."
