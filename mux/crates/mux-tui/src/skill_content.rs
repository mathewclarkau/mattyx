pub const ORCHESTRATION_SKILL: &str = r#"---
name: mtyx-orchestration
description: Orchestrate mtyx panes, agents, and browser tabs from a natural-language layout request
argument-hint: <describe the panes, agents, and browser tabs you want, in plain English>
---

You are orchestrating **mtyx**, the terminal multiplexer this Claude Code session
is (probably) running inside of. The user's request is:

$ARGUMENTS

Turn that request into real panes/tabs by driving the `mtyx` CLI with the `bash`
tool. Do not just describe a plan — actually create the layout, launch the agent(s),
and report back what you built (pane/surface ids, what's running where).

## 0. Preconditions

Run `env | grep '^MTYX_MUX_'`. You need both `MTYX_MUX_SOCKET` and `MTYX_MUX_SURFACE`
set — every pane spawned by a running `mtyx` session gets these automatically.
If either is missing, stop and tell the user this only works from inside a pane of a
live `mtyx` session (`mtyx` to start one, or `mtyx attach --session <name>`
to join one already running) — there's nothing to orchestrate otherwise.

`mtyx <verb> ...` reads `MTYX_MUX_SOCKET` itself, so you don't need `--socket` on
any command below.

## 1. Find out where you are

Run `mtyx list-workspaces`. It prints the whole tree: workspaces → screens →
panes → tabs (surfaces), one line each, e.g.:

```
pane id=2 screen=3 name=null active_tab=0
tab surface=1 pane=2 kind=pty browser_source=null name=null title="" cols=80 rows=24
```

Find the tab whose `surface=` matches `$MTYX_MUX_SURFACE` — its `pane=` is the pane
you (this Claude session) are running in. That's your anchor for "left"/"here"/"this
pane" in the user's request; other panes are laid out relative to it with `split`.

## 2. CLI cheat sheet (exact flags — nothing else is accepted)

```
mtyx split --pane <pane> --dir right|down [--cols N --rows N]
    → creates a new pane (splitting the given one) with a fresh shell tab.
      Prints the new surface id. There is no --cwd; cd inside it via `send`.

mtyx new-tab --pane <pane> [--cwd <dir>] [--cols N --rows N]
    → new shell tab in an existing pane (not a new pane/split). Prints surface id.

mtyx new-browser-tab --url <url> --pane <pane> [--cols N --rows N]
    → new browser tab in a pane. Prints surface id.

mtyx send --surface <id> --text "<text>"
    → types literal text into a pty surface. Include your own trailing \n to
      submit a shell command, e.g.: --text $'cd /some/dir && claude "do the thing"\n'
      Does NOT work on browser surfaces (see below).

mtyx read-screen --surface <id>
    → dumps a pty surface's visible screen text. Browser surfaces reject this
      with "browser surface does not support PTY/VT socket commands" — don't
      try to introspect a browser tab's content this way, it's expected to fail.

mtyx browser-reload --surface <id>
    → reloads/refreshes a browser tab.

mtyx close-surface --surface <id>
    → closes one tab/surface (pane/screen/workspace stay if other tabs remain).

mtyx list-agents [--state working|blocked|idle|done]
    → shows Claude Code hook-reported agent state per surface, if the hook is
      installed (`mtyx claude install-hooks`). Useful to check whether an
      agent you launched is still working vs. waiting on you.
```

## 3. Building the layout

- Map each thing the user describes ("Claude Code on the left", "a browser on the
  right") to a pane. Use `split --pane <anchor-pane> --dir right` (or `down`) for
  each additional pane; note the returned surface id each time.
- To launch an agent in a pane: `send --surface <id> --text $'cd <dir> && claude "<task, quoted>"\n'`.
  Prefer passing the task as `claude`'s prompt argument over a bare `claude` +
  a second `send`, so it starts working immediately without you having to guess
  timing.
- To open a browser pointed at a URL: `new-browser-tab --url <url> --pane <pane>`.
- If the user wants a dev server (e.g. "build a tiny web app" + "point the browser at
  its dev server"), run the scaffolding/dev-server command in the agent's own pane
  (as part of its task, or via a `new-tab` you drive yourself first) — don't try to
  run it from your own pane, since your pane keeps running this command.

## 4. "Reload the browser when the code changes"

You can reload/refresh a browser tab in-place using:

    mtyx browser-reload --surface <browser-surface-id>

For watching the agent's working directory for changes, prefer:

```
command -v inotifywait >/dev/null && inotifywait -r -m -e modify,create,delete,move <dir>
```

as a background loop that reloads the browser tab on each event. If `inotifywait`
isn't installed, fall back to a polling loop (check mtimes every ~1s) — write it as a
small script under the scratchpad directory (not the user's project), launch it with
`nohup ... >/dev/null 2>&1 & disown`, and mention that a real filesystem watcher would
be more efficient if the user wants to `apt/dnf/pacman install inotify-tools`.

## 5. Report back

Once built, tell the user plainly what you created: which pane/surface has the agent,
which has the browser, what URL, and how the reload loop is running (and its PID, so
they can kill it later). If any step failed (e.g. dev server didn't come up in time),
say so and what you'd try next — don't claim success you didn't verify.
"#;

pub const HOTFIX_RACE_SKILL: &str =
    include_str!("../../../../.agents/skills/mtyx-hotfix-race/SKILL.md");
