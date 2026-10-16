# lib/panel.sh - Glue stub for the pifactory-fleet plugin.
#
# This file mirrors the mtyx verbs that the upstream
# scripts/cmux-panel-lib.sh (in the pifactory repo) wraps, but
# expressed in the mtyx-plugin manifest shape so a plugin consumer
# can see exactly which mtyx verbs the plugin's verbs correspond to.
#
# It is NOT sourced by anything in this directory. The plugin's
# runtime artifact is bin/fleet.wasm (built from src/lib.rs); this
# file is the documented bridge from the plugin's surface to the
# underlying mtyx verb set.
#
# Why it exists:
#   - cmux-panel-lib.sh is the source of truth for the pifactory
#     fleet's mtyx glue. It is not vendored here because it lives in
#     a separate repo.
#   - The mapping from plugin verb -> mtyx verb is small but
#     non-obvious (e.g. "deploy" -> new-workspace, not "send"). A
#     short reference file alongside the WASM keeps the rationale
#     close to the code that implements it.
#
# Australian English. No em dashes.

# pifactory_panel_verb_map <plugin-verb>
# Echo the mtyx verb (or verb sequence) the given plugin verb maps
# to. This is a pure mapping helper for documentation; the WASM
# adapter (src/lib.rs) hard-codes the same map.
#
# Output: a single mtyx verb name, or "<verb>+<verb>" to denote a
# multi-step sequence (note: the current loader permits only one
# mtyx_call per invocation, so multi-step sequences are deferred to
# a future loader fix that increments expected_request_id per call).
pifactory_panel_verb_map() {
    case "$1" in
        ping)         echo "identify" ;;
        status)       echo "list-workspaces" ;;
        deploy)       echo "new-workspace" ;;
        dispatch)     echo "new-workspace+send" ;;
        rollback)     echo "close-workspace" ;;
        *)            return 1 ;;
    esac
}

# pifactory_panel_capabilities
# Echo the manifest's effective capability defaults for the plugin.
# Kept here so a reviewer can confirm the manifest matches the
# plugin's stated needs without opening mtyx-plugin.toml.
pifactory_panel_capabilities() {
    cat <<'CAPS'
socket        = "write"     # deploy / dispatch / rollback mutate mtyx state
filesystem    = []          # only the plugin's install dir (the default)
env           = [HOME, USER, PIFACTORY_ROOT]
network       = "off"
memory_mib    = 64
fuel          = 1000000
max_runtime_ms = 5000
CAPS
}
