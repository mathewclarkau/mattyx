# `mtyx` Public Documentation Site (`docs-site/`)

Zero-JS static documentation site published to GitHub Pages on release tags via [`.github/workflows/docs-site.yml`](../.github/workflows/docs-site.yml).

## Pages

1. **`src/index.md`** — Quickstart (install, first session, headless agent factory, hooks)
2. **`src/agents.md`** — Agent integrations (the 7 harness hooks, ambient `detect-agents`, state authority table)
3. **`src/protocol.md`** — CLI & Control Socket API Reference (**auto-synced** from [`mux/docs/protocol.md`](../mux/docs/protocol.md))
4. **`src/configuration.md`** — Configuration Reference (**auto-synced** from [`mux/docs/configuration.md`](../mux/docs/configuration.md))
5. **`src/comparison.md`** — Honest comparison vs `tmux`, `zellij`, `herdr`, and upstream `cmux` (marking `mtyx` gaps explicitly)
6. **`src/changelog.md`** — Release history and versioning contract

## Building & Syncing

 whenever [`mux/docs/protocol.md`](../mux/docs/protocol.md) or [`mux/docs/configuration.md`](../mux/docs/configuration.md) changes, re-sync the site sources before committing:

```bash
./scripts/sync-docs-site.sh
```

CI runs `./scripts/sync-docs-site.sh --check` on every PR touching `docs-site/` or `mux/docs/` and fails if the synced pages have drifted.
