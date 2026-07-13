# wta — worktree task agents

**Stop being the human in the inner loop.** wta runs a fleet of AI coding agents in
parallel — each in its own **git worktree + tmux session** — and gives you the harness
to drive them: define a goal and a machine-checkable “done,” and wta re-prompts an
agent until it passes. Switch between agents and hand off context from one dashboard,
and see what each run costs. A single ~1 MB Rust binary that runs in any terminal.

![wta dashboard — an Instances sidebar of parallel AI agents beside a live, full-color agent Preview](assets/wta.png)

![license](https://img.shields.io/badge/license-MIT-blue) ![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey) ![binary](https://img.shields.io/badge/single%20binary-~1%20MB-green)

## Why wta

Most tools stop at “spin up N agents in isolation.” wta is the **harness around the
loop** — it drives agents to done, lets you move between them, and shows what they cost:

- **Close the loop** — give it a goal and a `verify.sh`; wta re-prompts the agent until
  the tests pass, then locks the fix in as a check every future agent must clear.
- **Work across a fleet** — run many agents at once, switch between them from one
  dashboard, and hand off one agent’s context into a fresh one.
- **Analyze the run** — per-agent tokens and estimated cost, with usage-over-time
  charts so you can see where the budget went and compare agents.

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

Bare `wta` opens a **global dashboard** across every repo you have agents in;
`wta dash --here` scopes to the current one. In it: `j`/`k` move · `Enter` attach ·
`Tab` Preview/Diff · `i` send a line without attaching · `?` help. Try it free with
`WTA_AGENT_CMD=bash wta new scratch`.

Each agent runs in its own worktree (`agent/<task>` under `.agents/`) and its own tmux
session on a dedicated server (`tmux -L wta`), namespaced per repo, with a stable
`WTA_INDEX`/`WTA_PORT_BASE` so parallel dev servers don’t collide.

📖 **[Full per-feature manual → MANUAL.md](MANUAL.md).**

## Features

**Close the loop** — the harness: a goal plus a machine-checkable “done,” and wta drives
the agent there. `wta loop <task>` re-prompts the agent with your `.wta/verify.sh`
output until it passes, with guards (`--max`, `--no-progress`, `--timeout`). `wta lock
<name> -- <cmd>` freezes a past failure into a check every future agent must pass, and
`wta cron add … --every <dur>` fires the loop on a schedule — work while you sleep.

**Work across a fleet** — `wta new` (with `--base`, `--model`, `--effort`) and `wta
fanout <name> -n N` (run N agents on one prompt, compare, keep the winner) scale you
out; the **global dashboard** and `wta attach` / `i` quick-send move you between agents;
`wta handoff <from> <new>` migrates one agent’s context into a fresh one, and `wta send`
/ `wta board` let agents coordinate.

**Analyze the run** — `wta cost [<task>] --chart` shows per-agent tokens and an
estimated spend with a usage-over-time chart (rate or cumulative, tokens or `$`) and a
model timeline, straight from the agent’s transcripts — no tracking overhead. `wta
supervise` watches the fleet and escalates stuck / needs-input / crashed agents.

**Review & merge** — the Diff tab shows a colorized diff vs the agent’s base branch;
`wta review <builder>` spawns an independent maker/checker agent; `wta push --pr` opens
a PR against the agent’s base. (`wta matrix` also previews which branches conflict via
`git merge-tree`, read-only, if you want it before merging.)

## Dashboard

Keys: `n`/`N` new · `b` new from a branch · `s` stop · `D` kill · `p` push/PR ·
`v` run checks · `e` open in your editor · `J`/`K` reorder · `Shift+↑`/`↓` scroll ·
`q` quit. The Preview keeps the agent’s **real colors** — no need to attach.

Status glyphs: `⠋ running · ● ready · ▲ needs input · ◆ review (finished, unseen) · ✓ merged · ✗ exited`.

## How it compares

wta shares the parallel-worktree substrate with tools like Claude Squad and Superset,
but it’s built around driving the agent loop, moving between agents, and measuring the
run — not just launching them.

| Capability | wta | Claude Squad | Superset |
|---|:---:|:---:|:---:|
| Loop an agent to green until the tests pass, unattended | ✅ | ❌ | ❌ |
| Hand off one agent’s context into another | ✅ | ❌ | ❌ |
| Per-agent tokens + estimated cost, usage charts | ✅ | ❌ | ❌ |
| Fan-out N agents on one prompt, compare, keep winner | ✅ | ❌ | ~ |
| Terminal-native — any terminal / over SSH, one binary | ✅ | ✅ | ❌ |
| Parallel agents in isolated git worktrees | ✅ | ✅ | ✅ |
| Visual side-by-side diff / open in any IDE | ❌ | ~ | ✅ |

> Competitors move fast — verify current capabilities before relying on any ❌.

## Requirements & what’s Claude-specific

wta needs **tmux** and **git ≥ 2.20** (native Windows means WSL). The core is
**agent-agnostic** — any CLI works via `WTA_AGENT_CMD`. Three conveniences are Claude
Code-specific and degrade gracefully otherwise: the `▲ needs-input` status and
finish/needs-input notifications (Claude Code hooks), the auto-dismiss of the
folder-trust prompt, and the estimated `$` in `wta cost` (token counts stay exact).

## Configuration

| Var | Default | |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | agent CLI started in each session |
| `WTA_SKIP_PERMISSIONS` | `1` | run with `--dangerously-skip-permissions`; `0` or `wta new --safe` re-enables prompts — Claude only |
| `WTA_WORKTREE_DIR` | `.agents` | worktree dir under the repo root (gitignore it) |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree (kept out of pushes) |
| `WTA_NOTIFY_SOUND` / `WTA_TMUX_NOTIFY` | `1` | finish/needs-input sound / in-terminal toast |

Full variable reference, per-repo `.wta/` setup (`setup.sh`/`verify.sh`/`teardown.sh`),
isolation slots, and Telegram remote control are in **[MANUAL.md](MANUAL.md)**.

## License

MIT
