#!/usr/bin/env bash
# bin/fleet.sh - Reference shell adapter for the pifactory-fleet mtyx
# plugin.
#
# This file is the human-readable counterpart to src/lib.rs (which
# compiles to bin/fleet.wasm and is what the mtyx plugin loader
# actually executes). It documents the adapter logic in the same
# idiom as scripts/cmux-panel-lib.sh in the pifactory repo:
# one bash function per plugin verb, each function shelling out to
# the underlying `mtyx <verb>` CLI.
#
# It is NOT executed by `mtyx pifactory-fleet <verb>` — that path
# runs the WASM adapter. This file exists so a reader who knows the
# cmux-panel-lib.sh idiom can immediately map the plugin's verbs to
# the mtyx verbs it wraps, and so a future plugin-ecosystem tooling
# (e.g. a hypothetical "shell-script plugin loader") has a starting
# point.
#
# Not meant to be sourced. Not meant to be installed. It lives in
# bin/ alongside the .wasm so the directory layout is consistent:
# both the runtime artifact and its source-of-truth reference are in
# bin/.

set -euo pipefail

MTYX_BIN="${MTYX_BIN:-mtyx}"

# ---- plugin verbs ----

# pifactory_fleet_ping
# Read-only smoke test. Forwards to `mtyx identify`.
pifactory_fleet_ping() {
    "$MTYX_BIN" --json identify
}

# pifactory_fleet_status
# Read-only fleet snapshot. Forwards to `mtyx list-workspaces`.
pifactory_fleet_status() {
    "$MTYX_BIN" --json list-workspaces
}

# pifactory_fleet_deploy <role>
# Create a fresh workspace for one role of the fleet. Mirrors the
# `cmux_dispatch_worker_pane` shape in scripts/cmux-panel-lib.sh
# (the "leader splits a pane off itself" step). The actual prompt
# dispatch happens via `pifactory_fleet_dispatch`; `deploy` only
# allocates the workspace slot.
#
# Single mtyx_call per invocation (see the plugin's README on the
# loader's single-call-per-invocation constraint).
pifactory_fleet_deploy() {
    local role="${1:-fleet-scout}"
    "$MTYX_BIN" --json new-workspace --name "$role"
}

# pifactory_fleet_dispatch <workpiece>
# Allocate a fresh workspace anchored at a workpiece path. This is
# the modern equivalent of `cmux_dispatch_worker_pane_interactive`
# from scripts/cmux-panel-lib.sh, minus the prompt — the operator
# supplies the prompt after the workspace is up (the prompt itself
# goes through the cmux_panel_lib dispatch command in a separate
# invocation, once the loader's single-call limitation is lifted).
pifactory_fleet_dispatch() {
    local workpiece="${1:-}"
    if [[ -z "$workpiece" ]]; then
        echo "pifactory-fleet dispatch: missing <workpiece>" >&2
        return 2
    fi
    local name
    name="$(basename "$workpiece")"
    "$MTYX_BIN" --json new-workspace --name "fleet-$name"
}

# pifactory_fleet_rollback <role>
# Close the workspace previously allocated for <role>. Mirrors the
# teardown side of `cmux_dispatch_worker_pane`.
pifactory_fleet_rollback() {
    local role="${1:-fleet-scout}"
    "$MTYX_BIN" --json close-workspace --name "$role"
}

# ---- dispatch ----

# pifactory_fleet_main <verb> [args...]
# Top-level dispatch for `fleet.sh <verb> [args]` invocations. The
# mtyx plugin loader does NOT call this — it loads fleet.wasm — but
# `bash bin/fleet.sh ping` works as a manual smoke test.
pifactory_fleet_main() {
    local verb="${1:-}"
    shift || true
    case "$verb" in
        ping)
            pifactory_fleet_ping "$@"
            ;;
        status)
            pifactory_fleet_status "$@"
            ;;
        deploy)
            pifactory_fleet_deploy "$@"
            ;;
        dispatch)
            pifactory_fleet_dispatch "$@"
            ;;
        rollback)
            pifactory_fleet_rollback "$@"
            ;;
        "" | -h | --help | help)
            echo "usage: fleet.sh <verb> [args...]" >&2
            echo "  ping      identify mtyx" >&2
            echo "  status    list workspaces" >&2
            echo "  deploy    [<role>]               default: fleet-scout" >&2
            echo "  dispatch  <workpiece>" >&2
            echo "  rollback  [<role>]               default: fleet-scout" >&2
            return 0
            ;;
        *)
            echo "pifactory-fleet: unknown verb: $verb" >&2
            return 2
            ;;
    esac
}

# Run main only when invoked directly (not when sourced for tests).
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    pifactory_fleet_main "$@"
fi
