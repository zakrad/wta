# wta — worktree task agents

Run a fleet of AI coding agents in parallel — each in its own **git worktree +
persistent tmux session** — from one keyboard-first dashboard. Attach to any
agent, review its diff, and preview branch conflicts before you merge. A single
~1 MB Rust binary that runs in **any terminal** and never touches your own tmux.

![wta dashboard — an Instances sidebar of parallel AI agents beside a live colorized diff](assets/wta.png)

## Install

```sh
brew install zakrad/wta/wta                                                       # macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zakrad/wta/main/install.sh | bash    # prebuilt binary
cargo install --git https://github.com/zakrad/wta                                 # from source
```

Needs **tmux**, **git ≥ 2.20**, and an agent CLI on your PATH (`claude` by
default — set `WTA_AGENT_CMD` to change). Add `--features telegram` for remote
control.

## Quickstart

```sh
cd your-repo
wta new fix-auth     # new worktree + branch + starts the agent in a tmux session
wta dash             # the dashboard
```

In `dash`: `j`/`k` move · `Enter` attach (type in the agent; `Ctrl-q` returns) ·
`Tab` Preview/Diff · `i` send one line without attaching · `m` conflict matrix ·
`?` help. Kick the tyres without spending tokens: `WTA_AGENT_CMD=bash wta new scratch`.

## Why it's different

- **Isolated** — one worktree + one tmux session per agent; no two touch the same
  files. Runs on a dedicated tmux server, so it stays out of your own `tmux ls`.
- **Persistent** — agents survive closing the terminal and laptop sleep (they
  resume on wake). A reboot ends the sessions, but the worktrees remain and
  `Enter` re-spawns them, continuing the previous conversation (`--continue`).
- **Mergeability matrix** (`m` / `wta matrix`) — preview which agent branches
  conflict with each other *and* main **before** merging, via `git merge-tree`
  (read-only, nothing committed). Most tools only show conflicts after you try.
- **Live status, zero setup** — running / ready / needs-input / exited detected
  automatically; optional Claude Code hooks (`wta install-hooks`) add "needs input".
- **Remote** — an optional Telegram bridge pings you when an agent needs you and
  lets you reply to drive it from your phone.

## Commands & keys

```
wta new <task> [--base <branch>]     start an agent (worktree + branch + tmux session)
wta ls | matrix                      list agents · preview pairwise branch conflicts
wta fanout <name> -n N -- <prompt>   spawn N agents on one prompt → compare (matrix) → merge the winner
wta attach | stop | resume | rm      attach · stop (keep worktree) · resume · destroy
wta push <task> [--pr]               commit + push the branch (--pr opens a PR via gh)
wta dash                             the live dashboard
```

Dashboard keys: `n`/`N` new (with prompt) · `b` new from an existing branch ·
`s` stop · `D` kill · `p` push/PR · `J`/`K` reorder · `Shift+↑`/`↓` scroll ·
`r` refresh · `q` quit. Status glyphs: `⠋ running · ● ready · ▲ needs input · ✗ exited`.
Pass `--server default` to run on your own tmux server instead of the isolated one.

## Chat history

wta keeps **no conversation of its own** — Claude Code stores history per working
directory in `~/.claude/projects/`, and wta simply runs `claude` (and `--continue`
on resume) inside each agent's worktree. So each agent has its own thread, separate
from any session you started in the repo root or another tool.

## Remote control (Telegram)

Build with `--features telegram`, then run `wta bridge` (needs the Claude Code
hooks for "needs input" pings):

```sh
export WTA_TELEGRAM_TOKEN=…  WTA_TELEGRAM_CHAT=…
wta bridge          # /agents · /use <task> then type to send · /send <task> <text>
```

## Config

| Var | Default | |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | program started in each session |
| `WTA_AUTO_TRUST` | `1` | auto-accept Claude's per-folder trust prompt (`0` disables) |
| `WTA_WORKTREE_DIR` | `.agents` | worktree dir under the repo root (gitignore it) |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree |

Per-repo setup: make `<repo>/.wta/setup.sh` executable — `wta new` runs it in the
fresh worktree (install deps, symlink `node_modules`, …).

## How it compares

Same family as **Claude Squad** (a git worktree + tmux session per agent, in a
TUI). wta leans into tighter isolation (its own tmux socket), hook-aware status,
an upfront **mergeability matrix**, **quick-send** without attaching, and
**Telegram** remote control. It deliberately doesn't embed a diff-review IDE —
review in the Diff tab or your own editor.

## License

MIT
