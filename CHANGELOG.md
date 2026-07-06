# Changelog

All notable changes to **wta** are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

## [0.1.18] — 2026-07-06

### Added — cross-agent awareness (agents isolated, but not blind)
- **Fleet digest** — each new agent's `CLAUDE.local.md` is seeded with a snapshot of
  the other active agents and the files they're touching (from the worktrees/branches
  wta already tracks), so it starts aware of its peers. Kept out of pushes.
- **Peer relay** — `wta send <task> "<msg>"` types a note into another agent's pane;
  agents can call it themselves. **Refuses to send when the target is at a
  permission/trust dialog** (so a message can't silently answer it) or busy.
- **Shared board** — `wta board` prints `<repo>/.wta/board.md`; `wta board "<claim>"`
  appends a line. Works from any worktree. Advisory coordination.

## [0.1.17] — 2026-07-06

### Added
- **`wta new <task> --yolo`** (and `wta fanout --yolo`, or `WTA_SKIP_PERMISSIONS=1`
  as a default) — run the agent with **no permission prompts**, i.e. `claude
  --dangerously-skip-permissions`. Fully unattended; the worktree is the only file
  blast radius. Off by default; Claude-only.

## [0.1.16] — 2026-07-06

### Fixed
- **Fresh agents no longer get stuck on Claude's trust prompt.** The matcher was
  looking for pre-2.1 wording ("Do you trust the files in this folder?") that no
  longer exists in Claude Code 2.1.x, so auto-trust never fired. Now: (1) the
  matcher recognizes both wording generations (whitespace-normalized, and refuses
  the "pre-approves" variant); (2) wta **pre-accepts folder-trust** for each new
  worktree path in `~/.claude.json` at spawn — so CLI spawns (`new`/`fanout`/
  `review`) work even with no dashboard watching; (3) the dashboard grace window
  is anchored to the prompt appearing, with a 120s cap. All gated by
  `WTA_AUTO_TRUST`; the pre-seed never clobbers an unparseable config and keeps it `0600`.

### Added
- **`WTA_COPY_PERMISSIONS=1`** (opt-in) — copies the repo's
  `.claude/settings.local.json` tool-permission grants into each worktree, so
  agents stop re-approving every Bash/Edit. Kept out of pushes.

## [0.1.15] — 2026-07-05

### Added
- **Audible notifications.** Off-screen finish / needs-input now plays a real
  system sound (`afplay` on macOS, `paplay` on Linux) in addition to the terminal
  bell, which many terminals mute. Silence with `WTA_NOTIFY_SOUND=0`, or point it
  at your own sound file.

## [0.1.14] — 2026-07-05

### Fixed (audit, cont.)
- **Isolation slots no longer collide.** `WTA_INDEX`/`WTA_PORT_BASE` are now
  assigned as the lowest free slot among the repo's current agents (was a
  `hash % 100` that collided ~50% of the time at ~12 agents) and **persisted per
  agent**, so the slot stays stable across resume.
- **`push` can't leak a custom context file.** It now unstages the exact files
  injected at `new` time (recorded per agent) rather than re-reading
  `WTA_CONTEXT_FILES` from the (possibly different) push-time environment.

## [0.1.13] — 2026-07-05

### Fixed (from a multi-agent code audit)
- **Multi-word agent commands now work.** `WTA_AGENT_CMD="claude --model haiku"`
  and `wta review --by "…"` were passed as a single program name → the pane died
  instantly, leaving an orphan worktree that looked alive. The command is now
  tokenized like the other configurable commands.
- **Task names are validated** (letters, digits, `-`, `_`, ≤64 chars). Names with
  `/`, `.`, `..`, or spaces are rejected up front, so the tmux session, worktree
  path, and state file can never diverge or escape `.agents`.
- **`wta new <task> --base X`** now errors instead of silently reusing a leftover
  unmerged `agent/<task>` branch and ignoring `--base`.
- **Telegram bridge** is keyed by `(repo, task)` — no more spurious/suppressed
  pings for same-named agents across repos; `deliver` prefers a live session.
- **Verify-check processes are reaped** (kill + wait on timeout, on task removal,
  and on dashboard quit) — no orphaned/zombie `verify.sh`.
- **Verify logs** moved from world-writable `/tmp` into wta's per-user state dir.

## [0.1.12] — 2026-07-05

### Added
- **Cross-agent review** — `wta review <builder> [--by <cmd>]` spawns an
  independent reviewer agent (`review-<builder>`) on the builder's branch that
  critiques the diff against tests/spec and ends with `REVIEW: PASS`/`FAIL`
  (agents can't self-grade). Point `--by` / `WTA_REVIEW_AGENT_CMD` at a cheaper model.
- **`wta init`** — scaffold `.wta/` convention stubs (`verify.sh`, `setup.sh`,
  `teardown.sh`); idempotent, never overwrites.
- **Run-log** — when a `.wta/` dir exists, wta appends stop/rm/push events to
  `.wta/run-log.md` for a lightweight audit trail.

## [0.1.11] — 2026-07-05

### Added
- **Verification gate.** Drop an executable `.wta/verify.sh` (your tests/lint) in
  the repo; wta runs it for each agent when it finishes (or on demand with `v`),
  shows `✓`/`⟳`/`✗` in the sidebar, surfaces the failing line, and **grays out
  failing branches in the mergeability matrix** — so you don't merge on "the agent
  said it's done." Runs **async** (spawn + poll) so a slow suite never blocks the
  dashboard.

## [0.1.10] — 2026-07-05

### Added
- **Per-worktree isolation slots.** Each agent gets a stable `WTA_INDEX` (0–99)
  and `WTA_PORT_BASE` (a unique 10-port block) in its pane and in `setup.sh`, so
  parallel agents stop colliding on port 3000 / a shared dev DB.
- **`✓ merged` status.** Agents whose branch has landed in the base branch show a
  distinct glyph, so you know which are safe to `rm`.
- **No-instructions nudge.** `wta new` prints a one-line tip when the repo has no
  `AGENTS.md`/`CLAUDE.md` (agents ground better with one). Never creates or commits
  anything.

## [0.1.9] — 2026-07-05

### Added
- **Open the worktree in your editor** — `e` in the dashboard (and `wta open <task>`)
  opens the selected agent's worktree via `WTA_OPEN_CMD` / `$EDITOR`. GUI editors
  (`code`, `cursor`, `zed`, JetBrains…) launch detached; terminal editors
  (`nvim`/LazyVim, `vim`, `helix`, `emacs -nw`…) open inline and return you to the
  dashboard on quit. Force with `WTA_OPEN_INLINE=1`/`0`.

## [0.1.8] — 2026-07-05

### Fixed
- **Releases actually publish again.** The release CI built the Intel-Mac binary
  on a `macos-13` runner that was perpetually stuck `queued`, which blocked the
  publish step for every tag since v0.1.4 (so `curl`/release binaries were frozen
  at v0.1.3). The Intel target now cross-compiles on the Apple-Silicon `macos-14`
  runner. No code changes vs 0.1.7.

## [0.1.7] — 2026-07-05

### Added
- **Notifications.** When an off-screen agent finishes or asks for input, wta
  rings the terminal bell and marks it for review (`◆`), with a "N need you"
  count in the menu bar. Viewing the agent clears it.
- **`.wta/teardown.sh` hook.** Mirror of `setup.sh`; runs on `wta rm` inside the
  worktree before it's removed — stop containers, free ports, etc.

### Fixed
- **Untracked files now show in the diff and the sidebar +/- counts.** Files an
  agent *created* were previously invisible.
- **Force-kill warns about unpushed commits** ("⚠ N unpushed commits will be lost
  too") before discarding a branch's committed-but-unpushed work.
- **`git worktree prune` before every add** — self-heals stale worktree claims so
  creation no longer fails with "already used by worktree at <missing-path>".

## [0.1.6] — 2026-07-04

### Added
- **Colored live Preview.** The Preview tab captures with `tmux capture-pane -e`
  and renders the agent's real ANSI colors inline — no need to attach.
- **Preview scroll mode.** The first `Shift+↑` snapshots the full scrollback so
  you can page back through history; `Esc` returns to live output.
- **Branch-picker windowing** — the `b`/`--base` picker scrolls around the cursor
  (selection stays visible) and shows a "no matching branches" empty state.

### Fixed
- Width/unicode-aware truncation of sidebar rows, so long or CJK task/branch names
  no longer overflow the pane.

## [0.1.5] — 2026-07-04

### Added
- **`wta fanout <name> -n N -- <prompt>`** — spawn N agents on the same prompt,
  then compare with `wta matrix` and merge the winner. The front half of a
  spawn → compare → merge loop.

### Fixed
- **Per-repo namespacing.** tmux sessions and `~/.wta` state/order are now keyed
  by repo, so the same agent name in two repos no longer collides (previously a
  silent session/state corruption).
- **`push` no longer leaks context files.** Injected files (`CLAUDE.local.md`,
  `.env`, …) are excluded from the commit, so they can't land in a PR.
- `stop` now records an `exited` status, so the Telegram bridge stops reporting a
  stopped agent as running.

## [0.1.4] — 2026-07-04

### Fixed
- Killing from the dashboard handles dirty worktrees via a force-confirm prompt,
  and `rm` is tolerant of stale/ghost state (always cleans up its state file).
- The dashboard is scoped to the current repo.

## [0.1.3] — 2026-07-02

### Added
- **Quick-send (`i`)** — send one line to a Ready agent without attaching, with a
  hardened send (echo-confirm + submit-confirm).
- **Auto-dismiss** of Claude's per-folder trust prompt on a fresh worktree
  (`WTA_AUTO_TRUST`, strict match, startup-scoped).

### Changed
- Git diffstat moved off the render loop (cache + cadence) for a smoother UI.

## [0.1.2] — 2026-07-02

### Added
- **Mergeability matrix** (`wta matrix` / `m`) — preview which agent branches
  conflict with each other and main *before* merging, via `git merge-tree`
  (read-only).

### Fixed
- `install-hooks` no longer clobbers existing Claude Code hooks (append + idempotent).
- Preview scroll clamping; bounded the status-hash map.

## [0.1.1] — 2026-07-02

### Added
- Reorder agents with `J`/`K`, persisted to `~/.wta/order.json`.

## [0.1.0] — 2026-07-02

Initial release: a keyboard-first TUI dashboard that runs a fleet of AI coding
agents in parallel — each in its own **git worktree + persistent tmux session**
on a dedicated tmux server. Attach/detach (`Ctrl-q`), a Preview/Diff view, live
status, `push`/PR, and `brew`/`curl`/`cargo` install.

[0.1.18]: https://github.com/zakrad/wta/releases/tag/v0.1.18
[0.1.17]: https://github.com/zakrad/wta/releases/tag/v0.1.17
[0.1.16]: https://github.com/zakrad/wta/releases/tag/v0.1.16
[0.1.15]: https://github.com/zakrad/wta/releases/tag/v0.1.15
[0.1.14]: https://github.com/zakrad/wta/releases/tag/v0.1.14
[0.1.13]: https://github.com/zakrad/wta/releases/tag/v0.1.13
[0.1.12]: https://github.com/zakrad/wta/releases/tag/v0.1.12
[0.1.11]: https://github.com/zakrad/wta/releases/tag/v0.1.11
[0.1.10]: https://github.com/zakrad/wta/releases/tag/v0.1.10
[0.1.9]: https://github.com/zakrad/wta/releases/tag/v0.1.9
[0.1.8]: https://github.com/zakrad/wta/releases/tag/v0.1.8
[0.1.7]: https://github.com/zakrad/wta/releases/tag/v0.1.7
[0.1.6]: https://github.com/zakrad/wta/releases/tag/v0.1.6
[0.1.5]: https://github.com/zakrad/wta/releases/tag/v0.1.5
[0.1.4]: https://github.com/zakrad/wta/releases/tag/v0.1.4
[0.1.3]: https://github.com/zakrad/wta/releases/tag/v0.1.3
[0.1.2]: https://github.com/zakrad/wta/releases/tag/v0.1.2
[0.1.1]: https://github.com/zakrad/wta/releases/tag/v0.1.1
[0.1.0]: https://github.com/zakrad/wta/releases/tag/v0.1.0
