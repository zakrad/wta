# wta — worktree task agents

Run a fleet of AI coding agents in parallel, each isolated in its own **git
worktree** and a **persistent tmux session**, driven from one keyboard-first
dashboard. Browse them side-by-side, watch their live output, review their
diffs, and drop into any of them to chat — then detach back to the board.

Agents keep running when you close the terminal or your laptop sleeps. Stop one
and resume it later with its work intact. A single ~1 MB Rust binary that runs
in **any terminal** (it does not modify your terminal or shell config).

```
┌ Instances ─────────────────┐┌ auth ───────────────────────────────────┐
│ 1. auth                  ⠋ ││ Preview   Diff                          │
│   Ꮧ-agent/auth      +40,-4 ││$ cargo test                             │
│                            ││running 8 tests                          │
│ 2. flaky                 ▲ ││test result: ok. 8 passed                │
│ 3. docs                  ✗ ││awaiting your review…                    │
└────────────────────────────┘└─────────────────────────────────────────┘
   n new  s stop  D kill  │  ↵/o attach  tab switch  ?  help  q quit
```

## Features

- **One worktree + one tmux session per agent** — no two agents touch the same
  files; each has its own branch.
- **Live dashboard** — a sidebar of agents with status, branch and `+adds/-dels`,
  plus a **Preview** (live output) / **Diff** (colorized) pane.
- **Attach & chat** — press `Enter` to jump fullscreen into an agent and type;
  `Ctrl-q` drops you back to the board.
- **Live status with no setup** — running vs. waiting is detected automatically.
  Wire the optional Claude Code hooks to also surface “needs input”.
- **Persistent** — sessions survive closing the terminal and laptop sleep.
- **Stop & resume** — stop an agent (keep its worktree) and resume it later; or
  kill it to remove everything.
- **Stays out of your way** — runs on a dedicated tmux server, so it never
  touches your own tmux sessions.

## Requirements

- **tmux** — the agent runtime (persistence, capture, attach).
- **git** ≥ 2.20 — worktree-per-agent.
- an **agent CLI** on your PATH — `claude` by default (`WTA_AGENT_CMD` to change).
- Rust toolchain to build (until prebuilt binaries are published).

## Install

```sh
cargo install --git https://github.com/zakrad/wta      # once published
# or from a checkout:
cargo build --release && cp target/release/wta ~/.local/bin/
```

## Quickstart

```sh
cd your-repo
wta new fix-auth              # worktree .agents/fix-auth on branch agent/fix-auth,
                              #   starts `claude` in a tmux session
wta dash                     # the dashboard: browse, attach, review, manage
```

In the dashboard: `j`/`k` to move, `Enter` to jump into an agent and work,
`Ctrl-q` to come back, `Tab` to see its Diff, `?` for help.

## Agent lifecycle

```
wta new <task>      create worktree + branch + start the agent session
   │
   ├─ attach ────►  Enter/o (or `wta attach <task>`) — type in the agent; Ctrl-q returns
   │
   ├─ stop ──────►  s (or `wta stop <task>`)   session ends, WORKTREE KEPT  → status ✗
   │                     └─ resume ──►  Enter on it (or `wta resume <task>`) — back to work
   │
   └─ kill ──────►  D (or `wta rm <task>`)     session + worktree + branch REMOVED (gone)
```

**Stop vs. kill:** `stop` (`s`) is non-destructive — it ends the tmux session but
keeps the worktree and all uncommitted work, so `resume` re-spawns exactly where
you left off. `kill` (`D`) tears everything down. After a reboot the tmux server
is gone but worktrees remain, so agents show `✗` and resume brings them back.

## Commands

```
wta new <task> [-- <agent args>]   worktree + branch + start the agent session
wta ls                             list agents with live state + diffstat
wta attach <task>                  attach to a session (Ctrl-q to detach)
wta stop <task>                    end the session, keep the worktree (resumable)
wta resume <task>                  re-spawn a stopped agent in its worktree
wta rm <task> [--force]            destroy: session + worktree + branch
wta dash                           the live dashboard
wta status <state>                 emit status (for Claude Code hooks; optional)
wta install-hooks [--global]       wire Claude Code hooks -> `wta status`
```

## Dashboard keys

| key | action |
|---|---|
| `j`/`k` or ↑/↓ | move selection |
| `Shift+↑`/`↓` | scroll the Diff |
| `Tab` | switch Preview / Diff |
| `Enter` / `o` | attach into the agent and type (Ctrl-q returns; on `✗`, resume) |
| `n` | new agent |
| `s` | stop (keep worktree — resumable) |
| `D` | kill (destroy worktree + branch, with confirm) |
| `?` | help · `r` refresh · `q` quit |

Status glyphs: `⠋ running` · `● ready` · `▲ needs input` (hooks) · `✗ exited` ·
`· idle`. The colors are ANSI-indexed, so they match your terminal theme.

## Persistence & isolation

Agents run as `wta-<task>` sessions on a **dedicated tmux server** (`tmux -L wta`)
configured for a seamless attach — status bar off, mouse on, `Ctrl-q` bound to
detach. Because it’s a separate server, wta never appears in your normal
`tmux ls` and never interferes with your own tmux.

## How it compares

wta is in the same family as parallel-agent runners like Crystal or Claude
Squad — a worktree + a multiplexer session per agent, with a dashboard. What
wta leans into:

- **Isolation:** a dedicated tmux socket, so your own tmux stays clean.
- **Terminal-agnostic:** a single static binary, no editor or GUI required.
- **Hook-aware status:** reads Claude Code hooks for accurate “needs input”.
- **Explicit stop/resume** distinct from destroy.

It deliberately does **not** embed a diff-review IDE — review in the Diff tab or
your own editor.

## Config (env)

| Var | Default | Meaning |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | program started in each session |
| `WTA_WORKTREE_DIR` | `.agents` | worktree dir under the repo root |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree |

Per-repo bootstrap: make `<repo>/.wta/setup.sh` executable; `wta new` runs it in
the fresh worktree (install deps, symlink `node_modules`, …). Add `.agents/` to
your repo’s `.gitignore`.

## Try it safely

Nothing here touches your shell or terminal config:

```sh
cd some-git-repo
WTA_AGENT_CMD=bash wta new scratch   # a plain shell instead of a real agent
wta dash                             # Enter to attach & type, Ctrl-q to return
wta rm scratch --force               # clean up
```

## License

MIT
