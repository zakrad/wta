# wta manual

A per-feature guide. For the overview see the [README](README.md); this covers
how to use each feature on its own.

- [Concept](#concept)
- [Creating agents](#creating-agents) · `new`, `--base`, prompts, fan-out
- [The dashboard](#the-dashboard) · keys, panes, status glyphs
- [Attaching & sending input](#attaching--sending-input) · attach, quick-send
- [Reviewing & merging](#reviewing--merging) · diff, matrix, push/PR
- [Verification gate](#verification-gate) · `.wta/verify.sh`
- [Cross-agent review](#cross-agent-review) · `review`
- [Open in your editor](#open-in-your-editor) · `open`, nvim/GUI
- [Notifications](#notifications) · banner, sound, review glyph, hooks
- [Agent lifecycle](#agent-lifecycle) · stop/resume/kill, merged
- [Per-repo setup](#per-repo-setup) · `init`, setup/teardown, isolation slots, run-log
- [Cross-agent awareness](#cross-agent-awareness) · `send`, `board`, fleet digest
- [Multiple repos](#multiple-repos)
- [Remote control (Telegram)](#remote-control-telegram)
- [Using a different agent CLI](#using-a-different-agent-cli)
- [Configuration reference](#configuration-reference)

---

## Concept

Each agent runs in **its own git worktree** (a checkout of a fresh `agent/<task>`
branch under `.agents/`) inside **its own tmux session**, on a **dedicated tmux
server** (`tmux -L wta`) so it never mixes with your own `tmux ls`. The agent is
any CLI you like (`claude` by default). wta is the harness around it — it doesn't
store your conversation (Claude Code does, per directory).

---

## Creating agents

```sh
wta new fix-auth                       # worktree + agent/fix-auth branch + agent, from HEAD
wta new fix-auth --base develop        # base the branch on an existing branch
wta new fix-auth -- "add a login test" # everything after -- is passed to the agent (an initial prompt for claude)
wta new fix-auth --yolo                # run with no permission prompts (--dangerously-skip-permissions)
```

Task names must be letters/digits/`-`/`_` (≤64 chars). From the dashboard press
`n` (blank agent), `N` (agent + initial prompt), or `b` (pick an existing branch
to base on, with type-to-filter).

**Fan-out** — try one prompt N ways, keep the best:

```sh
wta fanout refactor -n 3 -- "refactor the auth module, keep behavior identical"
# creates refactor-1, refactor-2, refactor-3
wta matrix                # see which of them conflict with each other / main
# review each in `wta dash`, then:
wta push refactor-2 --pr  # keep the winner
wta rm refactor-1 --force # drop the rest
wta rm refactor-3 --force
```

---

## The dashboard

```sh
wta               # global — a tree of every repo's agents (same as `wta dash`)
wta dash --here   # only the current repo's agents
```

**Global by default:** bare `wta` opens one dashboard showing **every repo** you
have agents in, grouped into a tree by repo and selectable. Start an agent in any
repo (from its directory) and it appears under that repo automatically — you never
relaunch per repo. Every action (attach, kill, push, verify…) runs in the selected
agent's own repo. Pressing `n` asks which repo to create the new agent in.

Left: the sidebar — in the global view, repo headers (`▸ name (N)`) with agents
indented under them; each agent shows task, branch, `+adds/-dels`, status.
Right: **Preview** (live, full-color capture of the agent's pane) and **Diff**
(colorized diff vs the base branch, including untracked files) — `Tab` switches.

| Key | Action |
|---|---|
| `j` / `k` | move selection |
| `Enter` / `o` | attach into the agent (exited/merged → resume) |
| `Tab` | switch Preview / Diff |
| `Shift+↑` / `↓` | scroll; first `Shift+↑` enters scroll mode over full scrollback, `Esc` exits |
| `i` | send one line without attaching (only when `●` ready) |
| `v` | run `.wta/verify.sh` checks for the selected agent |
| `e` | open the worktree in your editor |
| `m` | mergeability matrix overlay |
| `n` / `N` | new agent / new agent with an initial prompt |
| `b` | new agent based on an existing branch |
| `s` | stop (keep the worktree, resume later) |
| `D` | kill (destroy worktree + branch; confirms, and warns on unpushed commits) |
| `p` | commit + push + open a PR |
| `J` / `K` | reorder the list (persisted) |
| `r` | refresh · `?` help · `q` quit |

**Status glyphs:** `⠋` running · `●` ready · `▲` needs input · `◆` review
(finished, unseen) · `✓` merged (landed in base) · `✗` exited. A verify check adds
`⟳`/`✓`/`✗` to the left of the status. (`▲` needs-input requires the Claude Code
hooks — [see Notifications](#notifications); other agents show running/ready/exited.)

---

## Attaching & sending input

- **Attach** — `Enter`/`o` (or `wta attach <task>`) drops you into the agent's
  real terminal. Type normally. **`Ctrl-q` detaches** back to wta (not tmux's
  `Ctrl-b d`).
- **Quick-send** — press `i`, type one line, `Enter`. It's injected into the
  agent without attaching. Gated to when the agent is `●` ready and idle, so you
  never inject mid-stream.

---

## Reviewing & merging

- **Diff tab** — colorized diff vs the base branch, with `+adds/-dels` counts;
  agent-created (untracked) files are included.
- **Mergeability matrix** — `m` in the dash or `wta matrix`. A pairwise grid of
  which agent branches merge cleanly with each other **and** the base, via
  `git merge-tree` (read-only — nothing is committed or checked out). Agents
  failing their verify checks are shown in red.
- **Push / PR** — `p` in the dash or `wta push <task> [--pr]`. Commits any
  uncommitted work (excluding injected context files), pushes `agent/<task>`, and
  with `--pr` opens a PR via `gh`.

---

## Verification gate

Drop an executable `.wta/verify.sh` in the repo (or run `wta init`). Make it exit
non-zero on failure:

```sh
#!/usr/bin/env bash
set -e
cargo test      # or: npm test / pytest -q / make check
```

wta runs it for each agent **when it finishes** (and on demand with `v`),
**asynchronously** so a slow suite never blocks the UI. Results show as `⟳`/`✓`/`✗`
in the sidebar; failing agents are **grayed red in the matrix**, so you don't
merge on "the agent said it's done." A failing check surfaces its last line in the
message bar. (Auto-retry-on-red is intentionally not enabled.)

---

## Cross-agent review

Agents can't reliably grade themselves, so spawn an independent reviewer on the
builder's branch:

```sh
wta review fix-auth                         # spawns review-fix-auth on fix-auth's branch
wta review fix-auth --by "claude --model haiku"   # use a cheaper/different model
```

The reviewer gets a prompt to inspect the diff vs base, run the tests, and end
with `REVIEW: PASS` or `REVIEW: FAIL`. Watch it in `wta dash` like any agent.
Default reviewer CLI: `WTA_REVIEW_AGENT_CMD`, else `WTA_AGENT_CMD`.

---

## Open in your editor

```sh
wta open fix-auth       # or press `e` in the dash
```

Opens the selected agent's worktree in `WTA_OPEN_CMD` (falls back to `$EDITOR`):

- **GUI editors** (`code`, `cursor`, `zed`, JetBrains…) launch **detached** — wta
  stays on the dashboard.
- **Terminal editors** (`nvim`/LazyVim, `vim`, `helix`, `emacs -nw`…) open
  **inline** — wta suspends, you edit in the worktree, and `:q` returns you to the
  dashboard.

Force either behavior with `WTA_OPEN_INLINE=1` (inline) or `0` (detached).

---

## Notifications

The banner + sound are **fired by the Claude Code hooks**, so they reach you
regardless of the dashboard — **even while you're attached inside an agent** or have
the dashboard closed entirely. Install them once:

```sh
wta install-hooks --global   # all repos (~/.claude/settings.json) — recommended
wta install-hooks            # or just this repo (.claude/settings.json)
```

This wires `UserPromptSubmit`/`Notification`/`Stop` to `wta status`. Then, each time
an agent **finishes a turn** (Stop) or **asks a question** (Notification), `wta`:

- plays a **sound** (the reliable baseline — the terminal bell is muted in many
  terminals; `WTA_NOTIFY_SOUND=0` to silence, or a file path for your own), and
- pops a **compact top-right toast** inside your terminal — a small box naming the
  agent (`wta · <repo>` / `<task> finished — ready for you`) that dismisses itself
  after `WTA_TMUX_SECS` seconds (default 2). Disable with `WTA_TMUX_NOTIFY=0`.

It fires **once per turn** (not by polling), for wta-managed agents only (gated on
`WTA_TASK`, so plain `claude` sessions that share the global hooks stay silent).
Hooks are appended, never clobbered — Superset's hooks (or your own) are left intact.

The toast renders *inside* the terminal via `tmux display-popup`, so it works on
macOS even where CLI **desktop banners** are silently dropped (recent macOS delivers
`terminal-notifier`/`osascript` notifications to Notification Center without showing a
banner). It reaches your terminal even from an agent's hook via a small bridge: any
`wta` you run inside tmux records your tmux socket to `~/.wta/tmux-client`, and the
hook pops the toast there. Requires running inside **tmux**.

Want a real desktop banner too? Set `WTA_NOTIFY_DESKTOP=1` — wta then also tries
`terminal-notifier` (`brew install terminal-notifier`) → a terminal-native escape
(kitty OSC 99 / WezTerm OSC 777 / iTerm2 OSC 9) → `osascript`/`notify-send`. Run
`wta notify-test` to see which your setup actually shows.

Separately, the **dashboard** marks a finished/needs-input agent `◆` (review /
unseen) with a **"N need you"** count when it's off-screen; selecting it clears it.
This is visual only — the audible/desktop alert is the hook, above. Running / ready /
exited status is detected automatically for any agent, with or without hooks.

---

## Agent lifecycle

```sh
wta stop fix-auth      # kill the tmux session, KEEP the worktree (resumable)
wta resume fix-auth    # re-spawn the session in the existing worktree (claude --continue)
wta rm fix-auth        # destroy: session + worktree + branch
wta rm fix-auth --force # also discard uncommitted work / an unmerged branch
```

- **Persistence** — sessions survive closing the terminal and laptop sleep. A
  reboot ends them, but the worktrees remain; `Enter` (or `resume`) re-spawns and
  `--continue`s the previous conversation.
- **Kill safety** — `D` in the dash confirms, and if there's committed-but-unpushed
  work it warns before discarding. On a dirty worktree it asks a second time.
- **Merged** — once an agent's branch has landed in the base branch it shows `✓
  merged`, so you know it's safe to `rm`.

---

## Per-repo setup

```sh
wta init    # scaffold .wta/{verify.sh, setup.sh, teardown.sh} (idempotent)
```

- **`.wta/setup.sh`** runs in each fresh worktree on `wta new` — install deps,
  symlink `node_modules`, copy fixtures, etc.
- **`.wta/teardown.sh`** runs on `wta rm`, before the worktree is removed — stop
  containers, free ports.
- **Context files** — `WTA_CONTEXT_FILES` (default `CLAUDE.local.md .env
  .env.local .mcp.json`) are copied into every worktree at creation and are kept
  out of `push` commits so secrets don't land in a PR.
- **Isolation slots** — each agent gets a stable `WTA_INDEX` (a distinct 0–99
  slot) and `WTA_PORT_BASE` (a unique 10-port block) in its pane **and** in
  `setup.sh`. Use them so parallel dev servers / DBs don't collide:
  ```sh
  # in .wta/setup.sh
  echo "PORT=$WTA_PORT_BASE" >> .env.local
  createdb "myapp_$WTA_INDEX" 2>/dev/null || true
  ```
- **Run-log** — when a `.wta/` dir exists, wta appends `stop`/`rm`/`push` events to
  `.wta/run-log.md`.

Add `.agents/` (and `.wta/run-log.md` if you like) to `.gitignore`.

---

## Cross-agent awareness

Agents are **file-isolated** (separate worktrees) but can be made **aware of each
other** through three advisory channels. Isolation + the mergeability matrix stay
the real safety layer — these are for coordination, not enforcement.

**The honest limit:** agents don't re-read files mid-session, so a shared file only
helps at turn-zero or when an agent is told to re-read. The **only channel that
reaches a running agent is the relay** (a typed line into its pane).

- **Fleet digest (automatic).** When you create an agent, wta injects a short
  "other agents active now + the files they're touching" snapshot into that
  worktree's `CLAUDE.local.md`, so a new agent starts aware of its peers and how to
  coordinate. Derived from the worktrees/branches wta already tracks; kept out of
  pushes.

- **Peer relay** — `wta send <task> "<message>"` types a one-line note into another
  agent's pane. **Agents can call it themselves** (their pane has the `wta` binary
  + `WTA_*` env), so one agent can tell another "I finished auth, rebase." It
  **refuses to send when the target is at a permission/trust dialog** (so a message
  can't accidentally answer it) or busy.

- **Shared board** — `wta board` prints `<repo>/.wta/board.md`; `wta board
  "<claim>"` appends a line (e.g. `owning src/auth/**`). Works from any worktree.
  Advisory claims agents read at turn-zero / when told.

```sh
wta send payments "auth is done on agent/auth — rebase before you touch src/user.rs"
wta board "auth: owning src/auth/** and src/user.rs"
wta board                       # see all claims
```

Not built (by design): no daemon/message queue, no shared DB, no enforced locks,
no task-claiming scheduler. If it must reach an agent mid-session, use the relay.

## Multiple repos

Sessions and state are namespaced per repo, so the **same task name in two repos
never collides**. Bare `wta` (the global dashboard) shows every repo's agents in
one tree; `wta dash --here` scopes to the current repo.

---

## Remote control (Telegram)

Build with the feature, set your bot token + chat id, and run the bridge:

```sh
cargo install --git https://github.com/zakrad/wta --features telegram
export WTA_TELEGRAM_TOKEN=…  WTA_TELEGRAM_CHAT=…
wta bridge          # test config first with: wta bridge --test
```

It pings you when an agent needs input / finishes, and you can drive agents back:

- `/agents` — list agents
- `/use <task>` then just type — send to the picked agent
- `/send <task> <text>` — send to a specific agent

Needs the Claude Code hooks (above) for "needs input" pings.

---

## Using a different agent CLI

wta is agent-agnostic — point `WTA_AGENT_CMD` at any interactive CLI:

```sh
WTA_AGENT_CMD=codex wta new thing
WTA_AGENT_CMD="claude --model haiku" wta new thing     # multi-word is fine
WTA_AGENT_CMD=aider wta new thing
WTA_AGENT_CMD=bash  wta new scratch                    # kick the tyres, no tokens
```

`--continue`-style resume is Claude's default; set `WTA_AGENT_RESUME_ARGS` for
another CLI (or empty to just relaunch it).

### Permissions & trust (Claude)

Every wta worktree is a **new folder path** to Claude Code, so by default a fresh
agent would hit two prompts. wta handles them like this:

- **Folder-trust dialog** ("Is this a directory you created or one you trust?") —
  wta **pre-accepts trust** for the worktree path in `~/.claude.json` at spawn
  (and the dashboard dismisses the live dialog as a backstop). On by default;
  disable with `WTA_AUTO_TRUST=0`. It only writes for the `claude` CLI, never
  clobbers an unparseable config, and keeps the file `0600`.
- **Per-tool permission prompts** ("Do you want to allow Bash/Edit…") — **by
  default wta runs claude with `--dangerously-skip-permissions`**, so agents run
  fully unattended with no prompts. The worktree isolates *files*, but the agent
  can still run any command on your machine — this is a deliberate "trust the
  task" default. To dial it back:
  - **`wta new <task> --safe`** — keep prompts ON for that one agent. (`--yolo`
    forces the skip explicitly; it's the default anyway.)
  - **`WTA_SKIP_PERMISSIONS=0`** — turn the default off globally (put it in your
    shell profile).
  - With prompts on, avoid re-approving every call: `WTA_COPY_PERMISSIONS=1`
    copies your repo's `.claude/settings.local.json` grants into each worktree, or
    `WTA_AGENT_CMD="claude --permission-mode acceptEdits"` auto-accepts edits
    (Bash still asks), or promote stable rules into the **tracked**
    `.claude/settings.json` (worktrees inherit it via `git checkout`).

### What's Claude Code-specific

The **core is agent-agnostic** — worktrees, tmux, attach/quick-send, the
mergeability matrix, verify gate, cross-agent review, fanout, open-in-editor, and
**finish notifications** all work with any CLI. Two conveniences are **Claude Code
only** and simply don't apply to other agents:

- **`▲ needs input` status** (and its Telegram "needs input" pings) comes from the
  Claude Code hooks that `wta install-hooks` writes into `.claude/settings.json`.
  Other agents still get running / ready / finished / exited from their pane —
  just never `▲`.
- **Auto-trust-dismiss** (`WTA_AUTO_TRUST`) only matches Claude's folder-trust
  prompt; it's a harmless no-op for other agents.

The defaults also lean Claude — `WTA_AGENT_CMD=claude`,
`WTA_AGENT_RESUME_ARGS=--continue`, and `CLAUDE.local.md`/`.mcp.json` in
`WTA_CONTEXT_FILES`. Override them for your CLI and everything else works.

---

## Configuration reference

| Var | Default | What |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | agent CLI started in each session (multi-word OK) |
| `WTA_AGENT_RESUME_ARGS` | `--continue` | args appended when resuming (empty = none) |
| `WTA_REVIEW_AGENT_CMD` | `$WTA_AGENT_CMD` | agent CLI for `wta review` |
| `WTA_WORKTREE_DIR` | `.agents` | where worktrees live under the repo root |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree (and kept out of pushes) |
| `WTA_AUTO_TRUST` | `1` | pre-accept + dismiss Claude's folder-trust prompt (`0` off) — **Claude only** |
| `WTA_COPY_PERMISSIONS` | `0` | copy `.claude/settings.local.json` (tool grants) into each worktree — **Claude only, opt-in** |
| `WTA_SKIP_PERMISSIONS` | `1` | agents run with `--dangerously-skip-permissions` (no prompts). `0` or `wta new --safe` re-enables prompts — **Claude only** |
| `WTA_OPEN_CMD` | `$EDITOR` | editor for `e` / `wta open` |
| `WTA_OPEN_INLINE` | auto | force editor inline (`1`) or detached (`0`) |
| `WTA_NOTIFY_SOUND` | `1` | notification sound (`0` = silent, or a sound-file path) |
| `WTA_TMUX_NOTIFY` | `1` | compact top-right terminal toast (`0` = off) |
| `WTA_TMUX_SECS` | `2` | seconds the toast stays before auto-dismissing |
| `WTA_NOTIFY_DESKTOP` | `0` | opt into a real desktop banner (`1` = on) |
| `WTA_TMUX_SOCKET` | `wta` | tmux server socket (`default` = your own tmux; same as `--server`) |
| `WTA_TELEGRAM_TOKEN` / `WTA_TELEGRAM_CHAT` | — | Telegram bridge bot token + chat id |

Exported **to** each agent (read them in `setup.sh` / your agent): `WTA_TASK`,
`WTA_REPO`, `WTA_INDEX`, `WTA_PORT_BASE`, `WTA_ROOT` (setup/teardown only).
