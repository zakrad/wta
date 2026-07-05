# Changelog

All notable changes to **wta** are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

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
