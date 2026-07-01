# wta

Run a fleet of AI coding agents in parallel, each isolated in its own **git
worktree** and a **persistent tmux session**, from one keyboard-driven
dashboard. Agents survive closing your terminal and laptop sleep; attach into
any of them and detach back to the board without leaving the app.

A single ~1 MB Rust binary. Terminal-agnostic (works in any terminal — it does
**not** modify your terminal config). Review diffs in the built-in Diff tab or
your own editor.

```
┌ Instances ─────────────────┐┌ auth ───────────────────────────────────┐
│ 1. auth                  ⠋ ││ Preview   Diff                          │
│   Ꮧ-agent/auth      +40,-4 ││$ cargo test                             │
│                            ││running 8 tests                          │
│ 2. flaky                 ▲ ││test result: ok. 8 passed                │
└────────────────────────────┘└─────────────────────────────────────────┘
       n new  D kill  │  ↵/o attach  tab switch  │  j/k move  q quit
```

## Requirements

- **tmux** — the agent runtime (sessions persist across terminal close / sleep).
- **git** ≥ 2.20 — worktree-per-agent.
- an **agent CLI** on your PATH — `claude` by default (override with `WTA_AGENT_CMD`).
- Rust toolchain to build (until binaries are published).

## Install

```sh
cargo install --git https://github.com/<you>/wta        # once published
# or from a checkout:
cargo build --release && cp target/release/wta ~/.local/bin/
```

## Commands

```
wta new <task> [-- <agent args>]   worktree + branch agent/<task>, copy context, start agent in tmux
wta ls                             list agents with live state + diffstat
wta attach <task>                  attach to an agent's tmux session (Ctrl-b d to detach)
wta rm <task> [--force]            kill the session, remove worktree + branch
wta dash                           the live dashboard (below)
wta status <state>                 emit status (for Claude Code hooks; optional)
wta install-hooks [--global]       wire Claude Code hooks -> `wta status`
```

## Dashboard (`wta dash`)

Left: an **Instances** sidebar — `N. task`, a live status glyph, `Ꮧ-branch`,
and `+adds,-dels`. Right: **Preview** (live agent output) / **Diff** (colorized,
with add/del counts) tabs.

| key | action |
|---|---|
| `j`/`k` or ↑/↓ | move selection |
| `Tab` | switch Preview / Diff |
| `Enter` / `o` | **attach** to the agent inline (Ctrl-b d detaches back to the board); on an exited agent, offers **resume** |
| `n` | new agent (worktree + session) |
| `D` | kill agent (confirm) |
| `r` | refresh · `q` | quit |

**Status is live without any setup:** each tick captures the agent's tmux pane
and hashes it — changed → `⠋ running`, unchanged → `● ready`, session gone but
worktree kept → `✗ exited` (resumable). If you wire the optional Claude Code
hooks, an agent asking for input shows `▲`.

## Persistence

Agents run in `wta-<task>` tmux sessions owned by the tmux server, so they
survive closing the terminal, GUI restarts, and laptop sleep. On reboot the
sessions are gone but the worktrees remain — such agents show `✗ exited`, and
`Enter` re-spawns them in place (branch + uncommitted work intact).

## Testing locally (safe)

Nothing here touches your shell or terminal config. To try it without risk:

```sh
cd some-git-repo
WTA_AGENT_CMD=bash wta new scratch     # a plain shell instead of a real agent
wta dash                               # browse, Tab, Enter to attach, Ctrl-b d to detach
wta rm scratch --force                 # clean up (worktrees live in .agents/)
```

Add `.agents/` to the repo's `.gitignore`.

## Config (env)

| Var | Default | Meaning |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | program started in each session |
| `WTA_WORKTREE_DIR` | `.agents` | worktree dir under the repo root |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree |

Per-repo bootstrap: make `<repo>/.wta/setup.sh` executable; `wta new` runs it in
the fresh worktree (install deps, symlink `node_modules`, etc.).

## License

MIT
