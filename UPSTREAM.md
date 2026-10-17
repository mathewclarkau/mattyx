# Upstream tracking

How `mathewclarkau/mattyx` relates to
[`manaflow-ai/cmux`](https://github.com/manaflow-ai/cmux), and how to re-sync.

See also [`PROVENANCE.md`](./PROVENANCE.md) for the original vendor anchors and
licensing. This file is the living **drift ledger** (issue #29).

## Release ↔ upstream map

| Our release | Merged-from upstream | Upstream HEAD at integration | Notes |
|-------------|----------------------|------------------------------|-------|
| v0.1.0 | (initial vendor) | `adc48877a` | Anchor for `mux/` + `daemon/remote/` |
| v0.4.0 | (n/a — port-only) | `7652d3b1c` (observed, not integrated) | First large port divergence: session CLI, agent hooks, Linux lifecycle |
| *next after #29 batch 1* | `d19f59aa2` (PATH fix only) | `7652d3b1c` | See [Integration log](#integration-log) |
| v0.22.0 | (n/a — port-only) | `7652d3b1c` (unchanged) | Port-only release: #126/#127 blocked-pane keyboard-lockup fix + macOS clipboard (#110). No upstream integration — upstream divergence now ~10k commits past the anchor |

Update this table on every release that either integrates upstream commits or
deliberately ships further port-only work while upstream advances.

## What we deliberately do NOT carry

| Area | Why skipped |
|------|-------------|
| Swift / AppKit / macOS app | Linux port surface is `mux/` + `daemon/remote/` + Ghostty VT only |
| iOS recovery, macOS notifications | Platform-specific |
| Upstream MSRV 1.88 / edition 2024 (`760e343d0`) | We pin **Rust 1.75.0** in CI (`pr-build.yml`); adopting requires a coordinated toolchain bump PR |
| `npx`/`uvx` publish pipelines, Freestyle cloud slots | Distribution / cloud product paths, not the Linux TUI core |
| Sidebar plugin manager (`a19223059`, `a1c5295dd`) | Large feature; evaluate after MSRV / protocol alignment |
| Full protocol-6 + window-manager socket surface | Needs careful merge against our hooks + session CLI; batch later |

## Re-sync procedure

Run this on a cadence (monthly is fine; sooner if a remote-PTY or mux bug is
known fixed upstream). Owner: whoever is on rotation.

### 1. Fetch and inventory

```bash
# Side clone (do not put this inside the mattyx worktree)
git clone --filter=blob:none https://github.com/manaflow-ai/cmux.git /tmp/cmux-upstream
cd /tmp/cmux-upstream
ANCHOR=$(grep -oE 'manaflow-ai/cmux@[0-9a-f]{7,}' \
  /path/to/mattyx/PROVENANCE.md | head -1 | cut -d@ -f2)
# Prefer the latest "Integrated through" SHA from PROVENANCE.md Anchor history.
git fetch origin "$ANCHOR" HEAD
git log --oneline "${ANCHOR}..HEAD" -- mux/ daemon/remote/
```

### 2. Classify each commit

For every commit that touches `mux/` or `daemon/remote/`, label it:

| Label | Meaning | Action |
|-------|---------|--------|
| **(a) integrate** | Linux-port-relevant, applies cleanly or with small edits | Cherry-pick / port into a `re-sync(upstream @ <sha>)` PR |
| **(b) skip** | macOS-only, product packaging, or superseded by our port | Document in the tally; do not code |
| **(c) defer** | Relevant but conflicts with our hooks / MSRV / protocol work | Call out; schedule a later batch |

### 3. Integrate in batches

- One PR per batch. Commit message pattern:

  ```
  re-sync(upstream @ <shortsha>): <one-line summary of what landed>

  Integrated: <list>
  Skipped: <count + reason summary>
  Deferred: <count + reason summary>
  ```

- Prefer cherry-picks of focused Go/`daemon/remote` fixes first (smaller
  conflict surface). Large `mux/` rewrites (MSRV, plugins, rebrand) need their
  own planning PR.

### 4. Update docs in the same PR

1. Append a row to [Release ↔ upstream map](#release--upstream-map) if this
   batch will ship in a named release; otherwise update the integration log.
2. Append to **Anchor history** in `PROVENANCE.md` (never overwrite prior rows).
3. Bump the "Integrated through" / "Classified against" SHAs.
4. Release notes for the next tag should include:

   ```
   Upstream: integrated <range or list>; classified against manaflow-ai/cmux@<sha>
   ```

### 5. Do not re-sync Ghostty here

`ghostty/` tracks `manaflow-ai/ghostty` on its own pin + `patches/`. Re-syncing
the submodule pointer is a separate change from `mux/` / `daemon/remote/`.

## Classification snapshot (against `7652d3b1c`, 2026-07-23)

28 commits touch `mux/` or `daemon/remote/` between anchor `adc48877a` and
upstream HEAD `7652d3b1c`. Full tally at classification time:

| SHA | Subject | Class | Notes |
|-----|---------|-------|-------|
| `e157d80bb` | read-screen reports rendered viewport | **defer** | Useful; merge after our read-screen callers reviewed |
| `b197d2353` | client SDKs TS/Rust/Go/Java + e2e | **skip** | SDK publish surface; optional later |
| `835c46d9b` | PTY-free surfaces for structural tests | **defer** | Good for CI; needs careful port |
| `bd0b24b74` | persistent Freestyle sshd cloud slot | **skip** | Cloud product |
| `7a2d486b2` | bindings: unify SDK identity on cmux | **skip** | Bindings rename churn |
| `bb2d8c070` | protocol-6 commands + tests | **defer** | Protocol expansion batch |
| `f41794c68` | window-manager ops on control socket | **defer** | Depends on protocol-6 direction |
| `fce37f1cb` | purge agent/notification tables on close | **defer** | Relevant lifecycle fix |
| `f452ddbd5` | ping, reload-config, window-title, scroll-changed | **defer** | Useful verbs; batch with protocol work |
| `a44618f64` | SDK publish workflows | **skip** | CI product |
| `0302b40ea` | sdk 0.1.2 npm publish | **skip** | Packaging |
| `be3535344` | npx/uvx cmux distribution | **skip** | Packaging |
| `df264cbfc` | publish cmux-mux artifacts linux/darwin | **skip** | We have our own release.yml |
| `760e343d0` | edition 2024, MSRV 1.88, latest deps | **skip** | Conflicts with Rust 1.75 pin |
| `155d344e3` | tmux/zellij-familiar keybindings | **defer** | UX; evaluate vs our key table |
| `a19223059` | pluggable sidebar | **skip** | Large feature |
| `a1c5295dd` | plugin manager from git | **skip** | Depends on sidebar plugins |
| `d675f0a0e` | rebrand mux → cmux-tui | **skip** | We already renamed independently |
| `c914b23a2` | ssh PTY input loss / reorder fix | **defer** | High value; large patch, needs dedicated batch |
| `f38b30371` | light-theme-aware chrome | **defer** | Nice-to-have TUI polish |
| `97872bc79` | windows-gnu CI + remove mux/target | **skip** | Windows CI; we never commit target |
| `2eeac82c6` | resize-pane remote tmux mirror | **skip** | Mostly Swift; Go relay hunks optional later |
| `b250ada01` | reap persistent SSH daemon on workspace close | **defer** | Lifecycle; depends on persistent-slot work |
| `3c1a6c2f6` | workspace group CLI deletion safe by default | **skip** | macOS/workspace-group product |
| `9a06a6988` | shield persistent remote PTY children | **defer** | Large remote-daemon refactor; foundation for later remote fixes |
| `d19f59aa2` | Fix remote PTY PATH inherited from cmuxd | **integrate** | Batch 1 (this PR) |
| `9f0613e23` | tear a PTY session down once | **defer** | Needs `9a06a6988` structure first |
| `38b8ca796` | respawn-pane in Go relay __tmux-compat | **defer** | SSH teammate panes; after relay baseline |

**Tally (2026-07-23):** integrated **1** · skipped **14** · deferred **13**.

## Integration log

### Batch 1 — `re-sync(upstream @ d19f59aa2)` (issue #29)

- **Integrated:** `d19f59aa2` — remote PTY PATH always includes standard executable directories (`/usr/bin`, `/bin`, …) so a restricted daemon PATH does not strand interactive shells.
- **Skipped / deferred:** see table above (27 remaining of the 28-commit window).
- **Code anchor for `daemon/remote/` PATH helper:** still rooted at original vendor `adc48877a`, plus this cherry-picked fix.
- **`mux/` code anchor:** unchanged at `adc48877a` (+ our port commits).
