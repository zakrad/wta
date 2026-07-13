# wta — worktree task agents

**Run a fleet of AI coding agents in parallel — and merge only what passes.**
Each agent gets its own **git worktree + persistent tmux session**; wta previews
which branches conflict *before* you merge, gates every merge on your own test
suite, and lets you race N agents on one prompt and keep the winner — all from one
keyboard-first terminal dashboard. A single ~1 MB Rust binary that runs in any
terminal (or over SSH) and never touches your own tmux.

![wta dashboard — an Instances sidebar of parallel AI agents beside a live, full-color agent Preview](assets/wta.png)

![license](https://img.shields.io/badge/license-MIT-blue) ![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey) ![binary](https://img.shields.io/badge/single%20binary-~1%20MB-green)

## Why wta

Most agent runners stop at "spin up N agents in isolation." wta's job starts where
that ends — **deciding what's safe to merge:**

- **Preview conflicts before you merge** — a pairwise mergeability matrix shows which
  agent branches collide with each other *and* the base, read-only, before you touch
  anything.
- **Gate the merge on your tests** — drop a `.wta/verify.sh`; wta runs it per agent and
  grays out failing branches, so you never merge on "the agent said it's done."
- **Race a prompt, keep the winner** — fan out N agents on one prompt, compare, merge
  the best, discard the rest.
- **See what it costs** — per-agent tokens and an estimated spend, with in-terminal
  burn charts, straight from the agent's transcripts.

## Install

```sh
brew install zakrad/wta/wta                                                       # macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zakrad/wta/main/install.sh | bash    # prebuilt binary
cargo install --git https://github.com/zakrad/wta                                 # from source
```

Needs **tmux**, **git ≥ 2.20**, and an agent CLI on your PATH (`claude` by default —
set `WTA_AGENT_CMD` to change). Add `--features telegram` for remote control.

## Quickstart

```sh
cd your-repo
wta new fix-auth     # new worktree + branch + starts the agent in a tmux session
wta                  # the dashboard — a live tree of EVERY repo's agents
```

Bare `wta` opens a **global dashboard**: one tree of every repo you have agents in,
grouped and selectable — start an agent in any repo and it appears under that repo
automatically. `wta dash --here` scopes to the current repo.

In `dash`: `j`/`k` move · `Enter` attach (type in the agent; `Ctrl-q` returns) ·
`Tab` Preview/Diff · `i` send one line without attaching · `m` conflict matrix ·
`?` help. Kick the tyres without spending tokens: `WTA_AGENT_CMD=bash wta new scratch`.

📖 **[Full per-feature manual → MANUAL.md](MANUAL.md).**

## Concept

Each agent runs in **its own git worktree** (a checkout of a fresh `agent/<task>`
branch under `.agents/`) inside **its own tmux session**, on a **dedicated tmux
server** (`tmux -L wta`) so it never mixes with your own `tmux ls`. Sessions and
state are namespaced per repo, so the same task name in two repos never collides.
Each agent also gets a stable `WTA_INDEX` / `WTA_PORT_BASE` so parallel dev servers
don't fight over the same port or database. wta is the harness around the agent — it
keeps no conversation of its own (Claude Code stores history per directory, and wta
resumes it with `--continue`).

## Feature tour

**Create & scale**
- `wta new <task> [--base <branch>] [--model <m>] [--effort <e>]` — a worktree +
  branch + a running agent. `--base` targets a branch other than the default; the
  dashboard and `wta ls` show each agent's base branch.
- `wta fanout <name> -n N -- "<prompt>"` — spawn N agents on the **same** prompt
  (`<name>-1..N`), compare with the matrix, keep the winner.
- `wta cron add <name> --every <dur> -- "<prompt>"` — schedule a routine that fires
  `wta new` on a cadence. Work while you sleep.

**Review & merge safely**
- **Diff tab** — a colorized diff vs the agent's base branch, including untracked
  files.
- `wta matrix` (`m`) — a pairwise grid of which branches merge cleanly with each
  other **and** the base, via `git merge-tree` — read-only, nothing checked out.
- **Verify gate** — an executable `.wta/verify.sh` runs per agent when it finishes
  (or on demand with `v`), async so it never blocks the UI. Results show `✓`/`✗` in
  the sidebar and **gray failing branches red in the matrix**.
- `wta review <builder> [--by <cmd>]` — spawn an independent reviewer agent on the
  builder's branch (maker/checker); point `--by` at a cheaper model.
- `wta loop <task> [--max N] [--no-progress N] [--timeout S]` — re-prompt the agent
  with `verify.sh` output until it passes, with guards for attempts, stalls, and
  wall-clock.
- `wta lock <name> -- <command>` — freeze a past failure into a regression check
  (`.wta/checks/`) that every future agent must pass.

**Stay in the loop**
- `wta install-hooks --global` — wire Claude Code hooks so every finish / question
  triggers a **sound** + a compact **in-terminal toast** (via `tmux display-popup`,
  nvim-notify style) that names the agent — **even while you're attached inside it or
  have the dashboard closed**, because it's fired by the agent's hook, not the
  dashboard. No OS notification, no permissions.
- `wta supervise [--here] [--stuck-secs N]` — watch the whole fleet and escalate
  stuck / needs-input / crashed agents. Read-only: it never acts on your behalf.
- `wta cost [<task>] [--chart] [--usd] [--cumulative]` — per-agent tokens (ground
  truth) and an estimated cost, with an in-terminal burn chart of usage over time and
  a model timeline. Parsed from transcripts the agent already writes — no tracking
  overhead.

**Coordinate**
- `wta send <task> "<msg>"` — relay a note into another agent's pane (agents can call
  it too); it refuses to type into a permission dialog.
- `wta board ["<claim>"]` — a shared claims board every agent can read and append to.
- `wta handoff <from> <new> -- "<prompt>"` — migrate one agent's committed work into a
  fresh agent, seeding a handoff note. Each new agent is also auto-seeded with a
  snapshot of the other active agents and the files they're touching.

## Commands & keys

```
wta new <task> [--base <branch>] [--safe] [--model <m>] [--effort <e>]   start an agent (skips permission prompts by default; --safe keeps them)
wta ls | matrix                      list agents (with base branch) · preview pairwise branch conflicts
wta cost [<task>] [--chart|--usd|--cumulative|--json]   per-agent tokens + estimated $, with usage-over-time charts
wta fanout <name> -n N -- <prompt>   spawn N agents on one prompt → compare (matrix) → merge the winner
wta review <builder> [--by <cmd>]    spawn an independent reviewer agent on <builder>'s branch (maker/checker)
wta loop <task> [--max N] [--no-progress N] [--timeout S] [-- <p>]   re-prompt until .wta/verify.sh passes (guards: attempts, no-progress, wall-clock)
wta lock <name> -- <command>         lock a failure into a regression check every future agent must pass
wta lock --list | wta unlock <name>  list / remove locked checks
wta supervise [--here] [--stuck-secs N]   watch the fleet; escalate stuck / needs-input / crashed agents (read-only)
wta cron add <name> --every <dur> [--repo <p>] -- <prompt>   schedule a routine that fires `wta new` on a cadence
wta cron list | rm | enable | disable | tick | start        manage routines · fire-due-once · run the scheduler
wta handoff <from> <new> [-- <p>]    migrate <from>'s committed work into a new agent (branch off it + seed a note)
wta send <task> "<msg>"              relay a note into another agent's pane (agents can call this too)
wta board ["<claim>"]                shared coordination board (print, or append a claim)
wta roles                            show the resolved model/effort per role (config: ~/.wta/roles.json + <repo>/.wta/roles.json)
wta init                             scaffold .wta/ convention stubs (verify.sh, setup.sh, teardown.sh)
wta attach | stop | resume | rm      attach · stop (keep worktree) · resume · destroy
wta open <task>                      open the agent's worktree in your editor ($EDITOR / WTA_OPEN_CMD)
wta push <task> [--pr]               commit + push the branch (--pr opens a PR against the agent's base)
wta install-hooks [--global]         wire Claude Code hooks for finish/needs-input notifications
wta / wta dash [--here]              the live dashboard (all repos by default; --here = current repo)
```

Dashboard keys: `n`/`N` new (with prompt) · `b` new from an existing branch ·
`s` stop · `D` kill · `p` push/PR · `v` run checks · `e` open in your editor ·
`J`/`K` reorder · `Shift+↑`/`↓` scroll the Preview/Diff (first `Shift+↑` pages back
through full scrollback; `Esc` exits) · `q` quit. The Preview keeps the agent's
**real colors** — no need to attach.

Status glyphs: `⠋ running · ● ready · ▲ needs input · ◆ review (finished, unseen) · ✓ merged (landed in base) · ✗ exited`.
Pass `--server default` to run on your own tmux server instead of the isolated one.

## What wta does that the others don't

wta shares the parallel-worktree substrate with tools like Claude Squad and Superset,
but it's built around a different question: **not "how do I run many agents" but "how
do I know what's safe to merge."** The capabilities below are where that shows.

| Capability | wta | Claude Squad | Superset |
|---|---|:---:|:---:|
| Pre-merge conflict preview across branches (`git merge-tree`) | ✅ `matrix` | ❌ | ❌ |
| Test/lint gate that blocks the merge decision | ✅ `.wta/verify.sh` | ❌ | ❌ |
| Fan-out N agents on one prompt, compare, keep the winner | ✅ `fanout` | ❌ | ~ (runs many; no compare) |
| Loop-until-tests-pass, unattended | ✅ `loop` | ❌ | ❌ |
| Independent reviewer agent (maker/checker) | ✅ `review` | ❌ | ❌ |
| Per-agent tokens + estimated cost, burn charts | ✅ `cost --chart` | ❌ | ❌ |
| Cross-agent messaging + shared claims board | ✅ `send` / `board` | ❌ | ❌ |
| Scheduled agent dispatch | ✅ `cron` | ❌ | ❌ |
| Remote control from your phone | ✅ Telegram | ❌ | ❌ |
| Parallel agents in isolated git worktrees | ✅ | ✅ | ✅ |
| Auto-accept / unattended mode | ✅ (default) | ✅ | ✅ |
| Runs in any terminal / over SSH, single small binary | ✅ | ✅ | ❌ (macOS desktop app) |
| Visual side-by-side diff review / open in any IDE | ❌ (Diff tab or `wta open`) | ~ (TUI diff) | ✅ |
| Native Windows | ❌ (tmux → WSL) | ✅ | ❌ (macOS) |
| Maturity / adoption | new | high | high |

> Competitors move fast — verify current capabilities before relying on any ❌.

## Configuration

| Var | Default | |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | program started in each session |
| `WTA_SKIP_PERMISSIONS` | `1` | agents run with `--dangerously-skip-permissions` (unattended). Opt out per-agent with `wta new --safe`, or globally with `0` — Claude only |
| `WTA_AUTO_TRUST` | `1` | pre-accept + dismiss Claude's folder-trust prompt for each worktree (`0` off) — Claude only |
| `WTA_WORKTREE_DIR` | `.agents` | worktree dir under the repo root (gitignore it) |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree |
| `WTA_REVIEW_AGENT_CMD` | `$WTA_AGENT_CMD` | agent CLI used by `wta review` (point it at a cheaper model) |
| `WTA_NOTIFY_SOUND` | `1` | sound on finish/needs-input (`0` = silent, or a path to your own sound file) |
| `WTA_TMUX_NOTIFY` | `1` | compact top-right terminal toast on finish/needs-input (`0` = off) |

The full variable reference (`WTA_OPEN_CMD`, `WTA_COPY_PERMISSIONS`,
`WTA_AGENT_RESUME_ARGS`, `WTA_TMUX_SECS`, `WTA_TMUX_SOCKET`, Telegram) is in
**[MANUAL.md](MANUAL.md)**.

## Per-repo setup

`wta init` scaffolds a `.wta/` directory of convention stubs:

- **`setup.sh`** runs in each fresh worktree (`wta new`) — install deps, symlink
  `node_modules`, start services. It sees `WTA_INDEX` / `WTA_PORT_BASE`, so parallel
  agents don't collide on port 3000 or a shared dev DB (`PORT=$WTA_PORT_BASE npm run
  dev`, `myapp_$WTA_INDEX`).
- **`teardown.sh`** runs on `wta rm`, before the worktree is removed — stop
  containers, free ports.
- **`verify.sh`** is the merge gate above (exit non-zero on failure).

When a `.wta/` dir exists, wta appends lifecycle events (stop/rm/push) to
`.wta/run-log.md`.

## Requirements & what's Claude-specific

wta needs **tmux** and **git ≥ 2.20**; native Windows means WSL. The core is
**agent-agnostic** — any CLI works via `WTA_AGENT_CMD`. Three conveniences are
Claude Code-specific and degrade gracefully with other agents: the `▲ needs-input`
status and the finish/needs-input notifications (both driven by Claude Code hooks),
the auto-dismiss of Claude's folder-trust prompt, and the estimated `$` in `wta cost`
(from a Claude price table; token counts are exact for any agent that writes
compatible transcripts). Everything else — isolation, the dashboard, the matrix, the
verify gate, fan-out, loop, review, cron — works with any agent.

## Remote control (Telegram)

Build with `--features telegram`, then run `wta bridge` (needs the Claude Code hooks
for "needs input" pings):

```sh
export WTA_TELEGRAM_TOKEN=…  WTA_TELEGRAM_CHAT=…
wta bridge          # /agents · /use <task> then type to send · /send <task> <text>
```

## License

MIT
