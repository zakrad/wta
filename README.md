# wta — worktree task agents

Run a fleet of AI coding agents in parallel — each in its own **git worktree +
persistent tmux session** — from one keyboard-first dashboard. Attach to any
agent, review its diff, and preview branch conflicts before you merge. A single
~1 MB Rust binary that runs in **any terminal** and never touches your own tmux.

![wta dashboard — an Instances sidebar of parallel AI agents beside a live, full-color agent Preview](assets/wta.png)

## Install

```sh
brew install zakrad/wta/wta                                                       # macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zakrad/wta/main/install.sh | bash    # prebuilt binary
cargo install --git https://github.com/zakrad/wta                                 # from source
```

Needs **tmux**, **git ≥ 2.20**, and an agent CLI on your PATH (`claude` by
default — set `WTA_AGENT_CMD` to change). Add `--features telegram` for remote
control. The core is agent-agnostic; two conveniences (`▲ needs-input` status and
auto-trust-dismiss) are Claude Code-specific — see
[MANUAL: what's Claude-specific](MANUAL.md#whats-claude-code-specific).

## Quickstart

```sh
cd your-repo
wta new fix-auth     # new worktree + branch + starts the agent in a tmux session
wta                  # the dashboard — a live tree of EVERY repo's agents
```

Bare `wta` opens a **global dashboard**: one tree of every repo you've got agents
in, grouped and selectable — start an agent in any repo and it appears under that
repo automatically. Use `wta dash --here` for just the current repo.

In `dash`: `j`/`k` move · `Enter` attach (type in the agent; `Ctrl-q` returns) ·
`Tab` Preview/Diff · `i` send one line without attaching · `m` conflict matrix ·
`?` help. Kick the tyres without spending tokens: `WTA_AGENT_CMD=bash wta new scratch`.

📖 **[Full per-feature manual → MANUAL.md](MANUAL.md)** — how to use every command
and feature, with examples.

## Why it's different

- **Isolated** — one worktree + one tmux session per agent; no two touch the same
  files. Runs on a dedicated tmux server, so it stays out of your own `tmux ls`.
  Sessions and state are namespaced per repo, so the same agent name in two repos
  never collides.
- **Persistent** — agents survive closing the terminal and laptop sleep (they
  resume on wake). A reboot ends the sessions, but the worktrees remain and
  `Enter` re-spawns them, continuing the previous conversation (`--continue`).
- **Mergeability matrix** (`m` / `wta matrix`) — preview which agent branches
  conflict with each other *and* main **before** merging, via `git merge-tree`
  (read-only, nothing committed). Most tools only show conflicts after you try.
- **Verification gate** — drop a `.wta/verify.sh` (your tests/lint) in the repo and
  wta runs it for each agent when it finishes (or on demand with `v`), shows
  `✓`/`✗` in the sidebar, and **grays out failing branches in the matrix** — so you
  never merge on "the agent said it's done." Runs async; never blocks the UI.
- **Live status, zero setup** — running / ready / needs-input / exited detected
  automatically; optional Claude Code hooks (`wta install-hooks`) add "needs input".
- **Notifies you — banner + sound** — when an agent finishes (or needs input, with
  Claude's hooks), wta posts a desktop notification naming *which* agent (`wta ·
  <repo>` → `<task> finished`) **and** plays a system sound (not just the terminal
  bell, which many terminals mute) — even for the agent you're watching, so leaving
  the dashboard open in one tab while you work in another still alerts you.
  Off-screen agents are also marked for review (`◆`) with a "N need you" count in the
  menu bar; viewing one clears it. `WTA_NOTIFY_DESKTOP=0` silences the banner,
  `WTA_NOTIFY_SOUND=0` the sound (or point the latter at your own sound file).
- **Cross-agent awareness** — isolated but not blind: each new agent is seeded with
  a snapshot of the others (and the files they're touching), agents can message each
  other (`wta send`, refuses to type into a dialog), and a shared `wta board` holds
  claims. Advisory — the worktree isolation stays the safety layer.
- **Remote** — an optional Telegram bridge pings you when an agent needs you and
  lets you reply to drive it from your phone.

## Commands & keys

```
wta new <task> [--base <branch>] [--safe]   start an agent (agents skip permission prompts by default; --safe keeps them)
wta ls | matrix                      list agents · preview pairwise branch conflicts
wta fanout <name> -n N -- <prompt>   spawn N agents on one prompt → compare (matrix) → merge the winner
wta review <builder> [--by <cmd>]    spawn an independent reviewer agent on <builder>'s branch (maker/checker)
wta send <task> "<msg>"              relay a note into another agent's pane (agents can call this too)
wta board ["<claim>"]                shared coordination board (print, or append a claim)
wta init                             scaffold .wta/ convention stubs (verify.sh, setup.sh, teardown.sh)
wta attach | stop | resume | rm      attach · stop (keep worktree) · resume · destroy
wta open <task>                      open the agent's worktree in your editor ($EDITOR / WTA_OPEN_CMD)
wta push <task> [--pr]               commit + push the branch (--pr opens a PR via gh)
wta install-hooks [--global]         wire Claude Code hooks for "needs input" status
wta / wta dash [--here]              the live dashboard (all repos by default; --here = current repo)
```

Dashboard keys: `n`/`N` new (with prompt) · `b` new from an existing branch ·
`s` stop · `D` kill · `p` push/PR · `v` run checks · `e` open in your editor · `J`/`K` reorder · `Shift+↑`/`↓` scroll the
Preview/Diff (first `Shift+↑` pages back through full scrollback; `Esc` exits) ·
`q` quit. The Preview keeps the agent's **real colors** (no need to attach).
Status glyphs: `⠋ running · ● ready · ▲ needs input · ◆ review (finished, unseen) · ✓ merged (landed in base) · ✗ exited`.
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
| `WTA_AUTO_TRUST` | `1` | pre-accept + dismiss Claude's folder-trust prompt for each worktree (`0` off) — Claude only |
| `WTA_COPY_PERMISSIONS` | `0` | copy `.claude/settings.local.json` (tool grants) into each worktree so agents don't re-approve (opt-in) — Claude only |
| `WTA_SKIP_PERMISSIONS` | `1` | agents run with `--dangerously-skip-permissions` (no prompts, unattended). Opt out per-agent with `wta new --safe`, or globally with `0` — Claude only |
| `WTA_WORKTREE_DIR` | `.agents` | worktree dir under the repo root (gitignore it) |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree |
| `WTA_OPEN_CMD` | `$EDITOR` | editor for `e` / `wta open` (GUI editors like `code` open detached; terminal editors like `nvim` open inline and return to the dash on quit) |
| `WTA_REVIEW_AGENT_CMD` | `$WTA_AGENT_CMD` | agent CLI used by `wta review` (point it at a cheaper/different model) |
| `WTA_NOTIFY_DESKTOP` | `1` | desktop notification banner on finish/needs-input, naming the task + repo (`0` = off; macOS `osascript` / Linux `notify-send`) |
| `WTA_NOTIFY_SOUND` | `1` | system sound on finish/needs-input (`0` = silent, or a path to your own sound file) |

More vars (`WTA_AGENT_RESUME_ARGS`, `WTA_OPEN_INLINE`, `WTA_TMUX_SOCKET`, Telegram)
and the full per-feature guide are in **[MANUAL.md](MANUAL.md)**.

Per-repo setup/teardown: make `<repo>/.wta/setup.sh` executable — `wta new` runs
it in the fresh worktree (install deps, symlink `node_modules`, …). A matching
`<repo>/.wta/teardown.sh` runs on `wta rm`, before the worktree is removed (stop
containers, free ports, …).

`wta init` scaffolds the `.wta/` stubs below. Verification: an executable
`<repo>/.wta/verify.sh` (run your tests/lint, exit non-zero on failure) runs per
agent when it finishes and on `v`, surfacing `✓`/`✗` in the dashboard and matrix.
When a `.wta/` dir exists, wta appends lifecycle events (stop/rm/push) to
`.wta/run-log.md`.

**Isolation slots:** each agent gets a stable `WTA_INDEX` (0–99) and
`WTA_PORT_BASE` (a unique 10-port block) in its pane *and* in `setup.sh`, so
parallel agents don't collide on port 3000 or a shared dev DB — e.g.
`PORT=$WTA_PORT_BASE npm run dev`, or a `myapp_$WTA_INDEX` database.

## How it compares

Same family as **Claude Squad** (a git worktree + tmux session per agent, in a
TUI). wta leans into tighter isolation (its own tmux socket), hook-aware status,
an upfront **mergeability matrix**, **quick-send** without attaching, and
**Telegram** remote control. It deliberately doesn't embed a diff-review IDE —
review in the Diff tab or your own editor.

## License

MIT
