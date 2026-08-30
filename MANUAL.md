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
- [Loop until green](#loop-until-green) · `loop`
- [Task spec](#task-spec) · `task`, acceptance criteria, context migration
- [Locked regression checks](#locked-regression-checks) · `lock`
- [Roles — model & effort](#roles--model--effort) · `roles`
- [Cost & spend](#cost--spend) · `cost`, charts
- [Supervising the fleet](#supervising-the-fleet) · `supervise`
- [Scheduled routines](#scheduled-routines) · `cron`
- [Open in your editor](#open-in-your-editor) · `open`, nvim/GUI
- [Notifications](#notifications) · sound, terminal toast, hooks
- [Agent lifecycle](#agent-lifecycle) · stop/resume/kill, merged
- [Per-repo setup](#per-repo-setup) · `init`, setup/teardown, isolation slots, run-log
- [Safety guard](#safety-guard) · `guard`, block destructive commands
- [Cross-agent awareness](#cross-agent-awareness) · `send`, `board`, fleet digest
- [Multiple repos](#multiple-repos)
- [Remote control (Telegram)](#remote-control-telegram)
- [Using a different agent CLI](#using-a-different-agent-cli)
- [Health check](#health-check) · `doctor`
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

**Handoff** — migrate one agent's work into a fresh one:

```sh
wta handoff old-attempt new-attempt -- "carry on, but start from a clean design"
```

The new agent branches off `old-attempt`'s branch (carrying its **committed** work)
and is seeded with a handoff note — the files changed, the commits, and a warning
listing `old-attempt`'s *uncommitted* paths (which are **not** carried). Useful when
a session has drifted and you want a clean context with the work so far.

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
indented under them; each agent shows task, its base branch, tokens used,
`+adds/-dels`, and status.
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
- **Scrolling the agent's chat with the keyboard** — press **`PgUp`/`PgDn`** (Mac:
  `Fn+↑/↓`) while attached. No prefix is needed, which matters **if you run wta inside
  your own tmux** (WezTerm → tmux → wta): the outer tmux eats `Ctrl-b`, so `Ctrl-b [`
  would open copy mode on the *outer* pane — your shell's history, not the agent's chat.
  What the key does depends on how the agent draws:
  - **Claude Code ≥ 2.1 (fullscreen renderer)** runs in the terminal's alternate screen
    and scrolls its own buffer (tmux history is empty there, so copy mode can't help).
    wta passes the key through and Claude scrolls natively (`PgUp`/`PgDn`,
    `Ctrl+Home`/`Ctrl+End`). For vim-style keys, bind Claude's `Scroll` context in
    `~/.claude/keybindings.json`, e.g. `alt+k`/`alt+j` → `scroll:lineUp`/`lineDown`,
    `alt+shift+k`/`alt+shift+j` → `scroll:halfPageUp`/`halfPageDown`, `alt+g`/`alt+shift+g`
    → `scroll:top`/`bottom` — wta forwards `Alt-k` too. (Or switch Claude to the classic
    renderer with `/tui default`, which puts the chat in tmux history.)
  - **Classic renderers / plain output** (`/tui default`, other agent CLIs): the chat is
    in tmux history, so `PgUp` / `Alt-k` / `Shift-↑` open tmux copy mode one page up
    (repeat to keep paging); your tmux `mode-keys` apply from there (vi: `j`/`k`,
    `Ctrl-u`/`Ctrl-d`, `/`, `q` to leave; `Shift-↑/↓` page in either mode).
  Mouse wheel works in both cases. `--server default` avoids nesting entirely.
- **Resume** — `Enter` on an exited agent re-spawns it with `--continue`. If the
  session dies right after launch (typically Claude's *"No conversation found to
  continue"* — nothing saved for that worktree), wta shows what it printed and offers
  **`f`** a fresh conversation in the same worktree (branch + changes kept) or **`r`**
  recreate from scratch (`rm` + `new` under the same name and base). CLI equivalents:
  `wta resume <task> --fresh` / `wta rm <task> && wta new <task>`.
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

### Shared-branch teams — several agents converging on one branch

Git allows only **one worktree per branch**, so two agents can't literally edit the
same branch. The way to have a "team" collaborate on one feature branch is: each agent
works on its **own** sub-branch (isolated worktree), and they all **land on the shared
branch**:

```sh
wta new api  --into feature/checkout    # branch off feature/checkout, target it
wta new ui   --into feature/checkout    # a second isolated agent, same target
# … they work in parallel, isolated …
wta land api                            # merge agent/api  → feature/checkout (local, no PR)
wta land ui  --rm                       # merge agent/ui   → feature/checkout, then remove it
```

`--into <target>` branches off `<target>` and makes it the diff/PR/land target (it's
`--base` named for this workflow). `wta land <task>` merges the agent's committed work
into that target **locally** — safely, in a throwaway checkout, so your working copy is
never touched; it aborts on a conflict (resolve it, or fall back to `wta push --pr`).

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

## Loop until green

`wta loop <task>` re-prompts the agent with your `.wta/verify.sh` output until it
passes — automated fix-until-green, with guards so it can't run away:

```sh
wta loop fix-auth                     # re-prompt until verify.sh exits 0
wta loop fix-auth --max 10            # give up after 10 attempts (default 6)
wta loop fix-auth --no-progress 2     # stop if the diff is unchanged 2 rounds running
wta loop fix-auth --timeout 1800      # overall wall-clock budget, seconds (0 = off)
wta loop fix-auth -- "start with a failing test"   # optional kickoff prompt
```

It runs in the foreground and exits **non-zero if a guard trips**, so
`wta loop fix-auth && wta push fix-auth --pr` won't push a failing branch.

---

## Wait — block until an agent reaches a state

`wta wait <task>...` blocks in the foreground until the agent(s) reach a state, so
you can chain steps in a plain shell script without a daemon or an event bus. It
polls the same state files the status bus already writes.

```sh
wta wait build --until idle             # until the agent finishes its turn (Stop hook)
wta wait build --until needs-input      # until it's blocked on a question / permission
wta wait build --until exited           # until its session ends
wta wait build --until done             # any of the above (default) — "stopped working"
wta wait a b c --all --until idle        # wait for EVERY listed agent (default)
w=$(wta wait a b c --any --until idle)   # return on the FIRST; prints its name to stdout
wta wait build --timeout 30m --poll 2s --quiet
```

It resolves state from the **same signals the dashboard uses** — the Claude hook
status *and* the screen-manifest pane detector — so `--until idle`/`needs-input` work
for **any** engine (codex/gemini/…), not just Claude. It is also **fail-closed**: an
agent stuck on a terminal error (invalid API key, quota/login) never reads as a
successful idle — the wait aborts with exit `5`. And it is **turn-bound**: after
`wta send`, a `wait --until idle` waits for the *new* turn to finish rather than
returning on the agent's stale prior-turn idle.

It exits with a **distinct code so scripts can branch**: `0` reached, `3` no such
agent, `4` the agent exited before an `idle`/`needs-input` state could happen, `5` a
terminal error (fail-closed), `124` timed out. That makes real pipelines a few lines
of shell:

```sh
wta new build -- "implement the parser"
wta wait build --until idle --timeout 30m || exit 1
wta review build --by reviewer
wta wait review-build --until idle        || exit 1
wta land build --rm

# fan-in: compare a whole fanout only once every agent has finished
wta fanout impl -n 5 -- "try an approach"
wta wait impl-1 impl-2 impl-3 impl-4 impl-5 --all --until idle
wta matrix
```

`idle` / `needs-input` require the Claude hooks (`wta install-hooks`); without them
only `exited` / `done` (via live session liveness) resolve. Every poll also checks
tmux liveness, so a crashed agent resolves instead of hanging forever.

---

## Task spec

`.wta/task.md` is an optional, per-agent **checkable contract** — the durable context
that survives a compaction, a `resume`, or a `handoff`, where a freeform note can't.
`verify.sh` proves the *build/tests*; the task spec proves the agent met the **intent**
it was given.

```sh
wta task fix-auth --new     # scaffold .wta/task.md (git-excluded — never lands in a PR)
wta task fix-auth           # show acceptance-criteria progress + Final Summary status
wta task fix-auth --json    # { total_ac, checked_ac, has_final_summary, unchecked:[…] }
wta task                    # no name = the current worktree (an agent runs it on itself)
```

The file is plain Markdown; only two sections are load-bearing:

```markdown
## Acceptance Criteria
- [ ] #1 a testable outcome — what "done" means
- [x] #2 another one, already met

## Final Summary
<!-- filled at the end: what changed, why, how it was verified -->
```

What reads it:
- **`wta loop`** — a loop is done only when `verify.sh` passes **and** every acceptance
  criterion is checked. Verify green with unchecked criteria re-prompts the agent to
  finish and check them off, so "green" means *met the reviewed intent*, not just
  "the script exited 0".
- **`wta handoff`** — carries the spec into the new agent (context migration), so the
  successor inherits the goal + criteria, not just the committed code.
- **`wta ls --json`** — includes an `ac: {checked,total}` field per agent.

It's opt-in (nothing happens until a `.wta/task.md` exists) and git-excluded per
worktree, so it never clutters a commit or a PR.

---

## Locked regression checks

`wta lock` freezes a command into a permanent check that every future agent must
pass — turn a bug you just fixed into a regression gate:

```sh
wta lock no-todo -- '! grep -rn TODO src/'   # writes .wta/checks/no-todo.sh
wta lock --list                              # list the repo's locked checks
wta unlock no-todo                           # remove one
```

Locked checks run as part of the verify suite (alongside `.wta/verify.sh`) — on the
dashboard's `v`, on `wta loop`, and on the finish edge — so a failing check grays the
branch in the matrix like any other verify failure.

---

## Roles — a different engine, model & effort per job

Spawn an agent *as a role*, so it runs the engine/model/effort you've chosen for that
job. The built-in roles are the convergent plan → build → review → test set:
`architect`, `backend`, `frontend`, `reviewer`, `tester` (plus `worker`, the default).
Any other name works too if you define it in config.

```sh
wta new checkout-api --role backend      # spawn as the backend role
wta new checkout-ui  --role frontend
wta new spec         --role architect    # coordinator: decompose + design (defaults to --effort high)
wta fanout refactor -n 3 --role backend  # a whole fan-out as one role
wta roles                                # print the resolved engine/model/effort per role
```

**Each role can run a different ENGINE** — set `cmd` in `~/.wta/roles.json` (global
only), so e.g. a planner runs one model while executors run another (or another CLI
entirely):

```json
{ "architect": { "cmd": "claude", "model": "opus-4.8", "effort": "high" },
  "backend":   { "cmd": "codex" },
  "frontend":  { "cmd": "claude", "model": "sonnet-5" },
  "reviewer":  { "cmd": "claude", "model": "haiku-4.5" } }
```

Precedence (highest first): `--model`/`--effort` flags > env (`WTA_<ROLE>_MODEL`/`_EFFORT`)
> `<repo>/.wta/roles.json` > `~/.wta/roles.json` > built-in defaults. `--model`/`--effort`
are only appended for the `claude` CLI. **A repo's `roles.json` may set model/effort but
never the engine (`cmd`)** — a supply-chain guard, since repos you clone shouldn't be
able to change what binary runs.

---

## Cost & spend

`wta cost` reads each agent's Claude Code transcripts for token usage and estimates a
dollar cost from a built-in price table — **tokens are exact, `$` is a labeled
estimate**. No polling, no background tracker.

```sh
wta cost                       # every agent: tokens + ~$ + in/out/cache breakdown + total
wta cost fix-auth              # one agent
wta cost fix-auth --chart      # a tall tokens-over-time chart (Y = tokens/bucket, X = time)
wta cost fix-auth --chart --usd         # dollars instead of tokens
wta cost fix-auth --chart --cumulative  # running-total curve instead of the per-bucket rate
wta cost --chart               # a one-row burn sparkline per agent, for side-by-side comparison
wta cost --json                # the per-message series (ts, tokens, $, model) for external analysis
```

The dashboard shows each agent's token count on its second line, and model changes
appear in the chart's timeline. (The `$` estimate is Claude-only; token counts work
for any agent that writes compatible transcripts.)

---

## Supervising the fleet

`wta supervise` watches every agent and escalates the ones that need you — a
foreground, **read-only** watcher (it never sends, kills, or changes an agent):

```sh
wta supervise                    # watch every repo you have agents in
wta supervise --here             # just this repo
wta supervise --stuck-secs 600   # flag "stuck" after 10 min idle with no new changes (default 5m)
```

It alerts (sound + toast + a printed line) when an agent goes `needs-input`, looks
stuck (idle with no new changes for `--stuck-secs`), or crashed with uncommitted
work, and prints a live status table grouped by repo. Accept or dismiss its
escalations to judge whether the signal is trustworthy.

---

## Scheduled routines

`wta cron` fires `wta new` on a cadence — work while you sleep:

```sh
wta cron add nightly-deps --every 1d -- "update dependencies and run the tests"
wta cron list                    # routines + when each is next due
wta cron disable nightly-deps    # keep it, stop firing
wta cron rm nightly-deps
```

Run the scheduler in the foreground with `wta cron start`, or wire `wta cron tick`
(fire all due routines once) into system cron / launchd. Each routine has a
concurrency cap of one — it won't fire again until you've removed its previous
agent — so it can't pile up a fleet unattended.

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

The sound + toast are **fired by the Claude Code hooks**, so they reach you
regardless of the dashboard — **even while you're attached inside an agent** or have
the dashboard closed entirely. Install them once:

```sh
wta install-hooks --global   # all repos (~/.claude/settings.json) — recommended
wta install-hooks            # or just this repo (.claude/settings.json)
```

This wires `UserPromptSubmit`/`Notification`/`Stop` to `wta status`. Then, each time
an agent **finishes a turn** (Stop) or **asks a question** (Notification), `wta`:

- plays a **sound** (the terminal bell is muted in many terminals; `WTA_NOTIFY_SOUND=0`
  to silence, or a file path for your own), and
- pops a **compact top-right toast** inside your terminal — a small nvim-style box:
  line 1 `⚡ <task>`, line 2 `<repo> · done|needs input · +A -B` (uncommitted diff
  stats) — that dismisses itself after `WTA_TMUX_SECS` seconds (default 4). Disable
  with `WTA_TMUX_NOTIFY=0`.

It fires **once per turn** (not by polling), for wta-managed agents only (gated on
`WTA_TASK`, so plain `claude` sessions that share the global hooks stay silent).
Hooks are appended, never clobbered — your existing hooks are left intact.

The toast is drawn *inside* the terminal via `tmux display-popup` — no macOS
notification, no permissions — so it shows regardless of your OS notification
settings. It reaches your terminal even from an agent's hook via a small bridge: any
`wta` you run inside tmux records your tmux socket to `~/.wta/tmux-client`, and the
hook pops the toast there (on wta's own tmux when you're attached inside an agent,
otherwise on your dashboard tmux). **Requires running inside tmux.**

Separately, the **dashboard** marks a finished/needs-input agent `◆` (review /
unseen) with a **"N need you"** count when it's off-screen; selecting it clears it.
This is visual only — the sound/toast come from the hook, above. Running / ready /
exited status is detected automatically for any agent, with or without hooks.

---

## Agent lifecycle

```sh
wta stop fix-auth      # kill the tmux session, KEEP the worktree (resumable)
wta resume fix-auth    # re-spawn the session in the existing worktree (claude --continue)
wta resume fix-auth --fresh   # same worktree, NEW conversation (when there's nothing to continue)
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
wta init    # scaffold .wta/{verify.sh, setup.sh, teardown.sh, worktree-include} (idempotent)
```

- **`.wta/setup.sh`** runs in each fresh worktree on `wta new` — install deps,
  symlink `node_modules`, copy fixtures, etc.
- **`.wta/teardown.sh`** runs on `wta rm`, before the worktree is removed — stop
  containers, free ports.
- **Context files** — `WTA_CONTEXT_FILES` (default `CLAUDE.local.md .env
  .env.local .mcp.json`) are copied into every worktree at creation and are kept
  out of `push` commits so secrets don't land in a PR.
- **`.wta/worktree-include`** — a fresh worktree only gets *tracked* files, so
  agents miss the gitignored env they need (`.env.local`, `certs/`, `.venv`, build
  artifacts). List them here, one **gitignore-style pattern per line**; wta resolves
  each to the actual ignored files (via git's own matcher) and copies them in,
  git-excluded like context files. Keep it small — **symlink** big dirs
  (`node_modules`) from `setup.sh` rather than copying them.
  ```
  # .wta/worktree-include
  .env.local
  certs/
  ```
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

## Safety guard

wta isolates **files** (a worktree per agent), but not the machine — a fleet running
unattended with `--dangerously-skip-permissions` can still force-push, `rm -rf` your
home, or wipe ignored files. `wta guard` is an **opt-in seatbelt**: it installs a
Claude Code **PreToolUse** hook into each new agent that blocks the clearly-destructive
`Bash` commands before they run.

```sh
wta guard on                       # enable for this repo (new agents get the hook)
wta guard status                   # on/off + the active rules
wta guard test 'git push --force'  # dry-run a command through the rules (no agent needed)
wta guard off
```

Built-in blocks: **force-push** (allows `--force-with-lease`), **`git reset --hard`**,
**`git clean -f` with `-x`/`-X`**, and **`rm -rf`** targeting `/`, `~`, `..`, `$HOME`,
or a wildcard. In-worktree cleanup (`rm -rf target`, `git clean -fd`) is allowed. Add
your own rules as executable **`~/.wta/guard.d/*.sh`** (each gets the command as `$1`; a
non-zero exit blocks, and its stderr is shown to the agent).

It's **off by default**, applies only to agents created after `guard on` (recreate
existing ones to pick it up), and the hook lives in each worktree's
`.claude/settings.local.json` (git-excluded, so it never dirties the repo). It is a
seatbelt, **not a sandbox** — matching a shell string is best-effort and a determined
agent can evade it; the worktree is still the real isolation. It also stays true to
wta's stance: this is deterministic *per-command* policy that escalates to **you**, not
an inter-agent lock or an LLM deciding to kill.

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
  can't accidentally answer it) or busy. It also **confirms the turn was actually
  consumed** — "keystrokes landed in the pane" isn't "the agent accepted the turn" —
  and **fails loud (non-zero exit)** if the text landed but wasn't submitted, so a
  scripted `wta send … && next` won't proceed on a dropped message. `wta send <task>
  --json <msg>` reports `{task, submitted}` for scripts.

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

### Status detection for any agent (screen manifests)

Only Claude reports its state through hooks. For every other engine the dashboard
reads the **pane content** — it pattern-matches the last lines against a per-engine
rule set to decide `▲ needs-input` / `● working` / `● ready`, so a codex/gemini/…
agent still gets a live glyph without hooks. Built-in rules ship for common engines
(a generic `(y/n)`/prompt/spinner set, plus extras for claude/codex/gemini/aider);
Claude's `▲ needs-input` hook stays authoritative where it exists.

Check what wta reads for a running agent, and tune it:

```sh
wta detect build        # prints the resolved engine + detected state right now
```

Extend any engine with `~/.wta/detect/<engine>.json` (patterns are **appended** to
the built-ins, matched as case-insensitive substrings of the pane):

```json
{
  "needs_input": ["apply this patch?"],
  "working": ["crunching"],
  "error": ["seat limit reached"],
  "neutral": ["fzf > ", "-- more --"]
}
```

The `error` category is for **terminal** failures (auth/quota/login) — an agent
matching it is treated as fail-closed by `wta wait` (never a false idle) and shown as
needs-you on the dashboard. Keep it narrow so a transient, self-retrying "overloaded"
doesn't fail-close a working agent. The `neutral` category is a **veto**: when the pane
is showing an overlay that isn't a live agent turn (an editor, a model picker, a pager),
a match here gives *no opinion* so a menu isn't misread as idle/working — built-ins
cover vim mode-lines and the model picker; add your engine's menus here.

Detection never sends keys — a wrong guess is only a wrong glyph that self-corrects
next tick — so it's safe to be generous with `working` patterns and tight with
`needs_input`.

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
mergeability matrix, verify gate, cross-agent review, fanout, open-in-editor,
**status detection** (via screen manifests, above), and **finish notifications**
all work with any CLI. A couple of conveniences lean **Claude Code**:

- **Hook-driven status** — with `wta install-hooks`, Claude reports
  running/needs-input/finished *exactly* (and drives the Telegram "needs input"
  pings). Other agents get the same glyphs inferred from their pane (screen
  manifests); the hook path is just faster and never guesses.
- **Auto-trust-dismiss** (`WTA_AUTO_TRUST`) only matches Claude's folder-trust
  prompt; it's a harmless no-op for other agents.

The defaults also lean Claude — `WTA_AGENT_CMD=claude`,
`WTA_AGENT_RESUME_ARGS=--continue`, and `CLAUDE.local.md`/`.mcp.json` in
`WTA_CONTEXT_FILES`. Override them for your CLI and everything else works.

---

## Health check

`wta doctor` is a one-shot check that the fleet will run what you think it will —
useful the first time you point wta at a non-Claude engine, or when something's off.

```sh
wta doctor
```

It prints, with ✓ / ✗ marks:

- **tools** — tmux (+ version + which server socket), git, and gh (optional, only
  `push --pr` needs it).
- **engines** — for the `worker` and `reviewer` roles, the *resolved* command and
  its `--version`, so you can confirm `--role backend` really launches the codex
  you expect and not a stale default. A missing binary shows ✗ NOT FOUND on PATH.
- **claude hooks** — whether `wta status` hooks are wired globally / in this repo
  (optional: they make Claude's `▲ needs-input` instant; other engines use screen
  manifests regardless).
- **repo** — the worktree dir, which context files are present to inject, and
  whether a `.wta` verify suite exists.
- **config** — the env that steers all of the above (`WTA_AGENT_CMD`, tmux server,
  worktree dir, skip-permissions default, context files).

It's read-only — no changelog feed, no self-update (mutating agent binaries
mid-fleet would break reproducibility; that's your package manager's job).

---

## Scripting (the CLI is the API)

wta has no daemon and no socket — the read commands emit JSON and the action
commands set meaningful exit codes, so you drive a fleet from plain shell, CI, or an
agent itself.

```sh
wta ls --json        # stable fleet snapshot: [{task,alive,status,base,engine,added,deleted,worktree,session}]
wta matrix --json    # the conflict graph: {labels, pairs:[{a,b,clean,conflicts}]}
wta cost --json      # per-message token/$/model time series
```

Combined with `wta wait`'s exit codes (`0` reached · `4` exited-first · `5` errored ·
`124` timed out), that's enough to sequence agents into a pipeline — e.g. a Makefile
where targets are `wta new` and prerequisites are `wta wait`, so `make -j` becomes the
parallel scheduler:

```make
api web:    ; wta new $@ -- "implement $@"
integrate:  api web ; wta wait api web --all --until idle && wta new $@ -- "wire them together"
```

---

## Configuration reference

| Var | Default | What |
|---|---|---|
| `WTA_AGENT_CMD` | `claude` | agent CLI started in each session (multi-word OK) |
| `WTA_AGENT_RESUME_ARGS` | `--continue` | args appended when resuming (empty = none) |
| `WTA_REVIEW_AGENT_CMD` | `$WTA_AGENT_CMD` | agent CLI for `wta review` |
| `WTA_WORKTREE_DIR` | `.agents` | where worktrees live under the repo root |
| `WTA_BRANCH_PREFIX` | `` (none) | prefix for a new agent's branch — empty = the bare task name; set `agent/` to restore the old naming |
| `WTA_CONTEXT_FILES` | `CLAUDE.local.md .env .env.local .mcp.json` | untracked files copied into each worktree (and kept out of pushes) |
| `WTA_AUTO_TRUST` | `1` | pre-accept + dismiss Claude's folder-trust prompt (`0` off) — **Claude only** |
| `WTA_COPY_PERMISSIONS` | `0` | copy `.claude/settings.local.json` (tool grants) into each worktree — **Claude only, opt-in** |
| `WTA_SKIP_PERMISSIONS` | `1` | agents run with `--dangerously-skip-permissions` (no prompts). `0` or `wta new --safe` re-enables prompts — **Claude only** |
| `WTA_OPEN_CMD` | `$EDITOR` | editor for `e` / `wta open` |
| `WTA_OPEN_INLINE` | auto | force editor inline (`1`) or detached (`0`) |
| `WTA_NOTIFY_SOUND` | `1` | notification sound (`0` = silent, or a sound-file path) |
| `WTA_TMUX_NOTIFY` | `1` | compact top-right terminal toast (`0` = off) |
| `WTA_TMUX_SECS` | `4` | seconds the toast stays before auto-dismissing |
| `WTA_TMUX_SOCKET` | `wta` | tmux server socket (`default` = your own tmux; same as `--server`) |
| `WTA_TELEGRAM_TOKEN` / `WTA_TELEGRAM_CHAT` | — | Telegram bridge bot token + chat id |

Exported **to** each agent (read them in `setup.sh` / your agent): `WTA_TASK`,
`WTA_REPO`, `WTA_INDEX`, `WTA_PORT_BASE`, `WTA_ROOT` (setup/teardown only).
