---
name: t3-show
description: T3 Code only. Show the current app changes in T3 Code's browser pane (the mcp__t3-code__preview_* tools). Use when the user, in a T3 Code session, asks to show, preview, open, or see a change or page in the browser. Not for Claude desktop, Claude in Chrome, or a plain terminal.
---

# Show changes in T3 Code's browser pane

This skill drives T3 Code's preview pane through the `t3-code` MCP server.
It does not apply anywhere else.

## 0. Check you are in T3 Code

The `mcp__t3-code__preview_*` tools are deferred. Load the ones you need in
one call:
`ToolSearch("select:mcp__t3-code__t3_preview_list,mcp__t3-code__preview_open,mcp__t3-code__preview_navigate,mcp__t3-code__preview_status")`.
If they do not exist, this is not a T3 Code session: still run step 1, then
tell the user the URL (`http://minibuntu:3000/<path>`) and stop. Do not reach
for another browser (Claude desktop browser, Claude in Chrome, computer use)
unless the user asks.

## 1. Get the changes into the app

Run `.claude/skills/t3-show/show.sh` from the repo root (Bash, timeout
600000). It picks the quickest way to get the working tree into the running
app on minibuntu:

- `static`: CSS/SVG/fonts copied into the running container, under a second.
- `web`: `web/src` rebuilt in a node container and copied in, a few seconds.
- `full`: image rebuilt and container recreated (Rust, templates,
  migrations, pi), several minutes. Run it with `run_in_background`, then
  load and call `mcp__t3-code__t3_background_wait` with its task id before
  you go on. Never poll with sleep or Monitor.

Force a mode with an argument (`static`, `web`, `full`) when auto guesses
wrong, e.g. after a manual `docker compose up --build`. Wait for the
`ready:` line; if the script fails, show the error and stop.

## 2. Pick the page

Open the page that shows the change (`/`, a recipe page, `/settings`, …).
Find a real recipe id with
`docker compose exec -T recipes python3 -c "import sqlite3;[print(*r) for r in sqlite3.connect('/data/recipes.sqlite3').execute('select id, title from recipes limit 5')]"`
when needed.

## 3. Show it in the pane

1. `mcp__t3-code__t3_preview_list`: find this thread's tabs. Only tabs with
   `runtime: 'server'` can be driven from here. A tab in the user's local
   desktop app cannot; never pass its `tabId` to a `preview_*` tool.
2. No usable server tab: `mcp__t3-code__preview_open` with no `url`
   (`open: true`), then go on with its returned `tabId`.
3. `mcp__t3-code__preview_navigate` with
   `target: {kind: "environment-port", port: 3000, path: "/<path>"}` and the
   `tabId`. The environment port reaches the app on this machine wherever the
   user's T3 window runs; do not use a `minibuntu` URL here. Navigating to
   the page it is already on reloads it, which picks up static and web copies.
4. Signed out? The tab lands on `/login`. Ask the user to sign in in the
   pane; never ask for, read, or type their password.
5. E-ink changes: the mode is a per-browser cookie set at
   `/settings/display`; open that page or tell the user how.

Leave the tab open: the user asked to see it. Take `mcp__t3-code__preview_snapshot`
only when the user asked you to check something. Reply in one or two lines:
the page and the mode used. Do not paste screenshots or restate the page.

Static and web copies live only in the running container; the next full
rebuild bakes the same files in from the checkout, so nothing is lost.
