# wta — worktree task agents

**Run a fleet of AI coding agents in parallel — and merge only what passes.**
Each agent gets its own **git worktree + tmux session**; wta previews which branches
conflict *before* you merge, gates every merge on your test suite, and re-prompts an
agent until its tests pass — all from one keyboard-first terminal dashboard. A single
~1 MB Rust binary that runs in any terminal (or over SSH).

![wta dashboard — an Instances sidebar of parallel AI agents beside a live, full-color agent Preview](assets/wta.png)

![license](https://img.shields.io/badge/license-MIT-blue) ![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey) ![binary](https://img.shields.io/badge/single%20binary-~1%20MB-green)

## Why wta

Most agent runners stop at “spin up N agents in isolation.” wta is the **harness
around the loop** — it decides what’s safe to merge and drives agents to done:

- **Preview conflicts before you merge** — a pairwise mergeability matrix, read-only.
- **Gate the merge on your tests** — a `.wta/verify.sh` grays out failing branches.
- **Close the loop** — re-prompt an agent until its tests pass, then lock the fix in.
- **Race a prompt, keep the winner** — fan out N agents on one prompt and compare.

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
`Tab` Preview/Diff · `i` send a line without attaching · `m` conflict matrix · `?`
help. Try it free with `WTA_AGENT_CMD=bash wta new scratch`.

Each agent runs in its own worktree (`agent/<task>` under `.agents/`) and its own
tmux session on a dedicated server (`tmux -L wta`), namespaced per repo, with a
stable `WTA_INDEX`/`WTA_PORT_BASE` so parallel dev servers don’t collide.

📖 **[Full per-feature manual → MANUAL.md](MANUAL.md).**

## Features

**Create & scale** — `wta new` (with `--base`, `--model`, `--effort`) starts an agent;
`wta fanout <name> -n N` runs N agents on the **same** prompt to compare and keep the
winner; `wta cron add … --every <dur>` fires `wta new` on a schedule.

**Verify before you merge** — the **Diff tab** shows a colorized diff vs the agent’s
base branch; `wta matrix` previews which branches merge cleanly with each other **and**
the base (`git merge-tree`, read-only); a `.wta/verify.sh` runs per agent (async) and
**grays failing branches red in the matrix**; `wta review <builder>` spawns an
independent maker/checker agent.

**Close the loop** — this is the harness: give it a goal and a machine-checkable
“done,” and wta drives the agent there. `wta loop <task>` re-prompts the agent with
your `verify.sh` output until it passes, with guards (`--max`, `--no-progress`,
`--timeout`); `wta lock <name> -- <cmd>` freezes a past failure into a check every
future agent must pass.

**Observe & coordinate** — `wta cost` shows per-agent tokens + an estimated spend with
usage-over-time charts; `wta supervise` escalates stuck / needs-input / crashed agents
(read-only); `wta install-hooks` adds a sound + in-terminal toast on finish/needs-input
(even while attached); and `wta send` / `wta board` / `wta handoff` coordinate across
agents.

## Dashboard

Keys: `n`/`N` new · `b` new from a branch · `s` stop · `D` kill · `p` push/PR ·
`v` run checks · `e` open in your editor · `J`/`K` reorder · `Shift+↑`/`↓` scroll ·
`q` quit. The Preview keeps the agent’s **real colors** — no need to attach.

Status glyphs: `⠋ running · ● ready · ▲ needs input · ◆ review (finished, unseen) · ✓ merged · ✗ exited`.

## What wta does that the others don’t

wta shares the parallel-worktree substrate with tools like Claude Squad and Superset,
but it’s built around a different question: not “how do I run many agents” but “how do
I know what’s safe to merge.”

| Capability | wta | Claude Squad | Superset |
|---|:---:|:---:|:---:|
| Pre-merge conflict preview across branches (`git merge-tree`) | ✅ | ❌ | ❌ |
| Test/lint gate that blocks the merge decision | ✅ | ❌ | ❌ |
| Fan-out N agents on one prompt, compare, keep winner | ✅ | ❌ | ~ |
| Loop-until-tests-pass, unattended | ✅ | ❌ | ❌ |
| Independent reviewer agent (maker/checker) | ✅ | ❌ | ❌ |
| Per-agent tokens + estimated cost, burn charts | ✅ | ❌ | ❌ |
| Scheduled agent dispatch · remote control from your phone | ✅ | ❌ | ❌ |
| Parallel agents in isolated git worktrees | ✅ | ✅ | ✅ |
| Runs in any terminal / over SSH, single small binary | ✅ | ✅ | ❌ |
| Visual side-by-side diff review / open in any IDE | ❌ | ~ | ✅ |
| Native Windows · maturity | WSL · new | ✅ · high | macOS · high |

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
