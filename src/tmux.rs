//! tmux backend: each agent is a detached tmux session (`wta-<task>`) on a
//! DEDICATED tmux server (socket `-L wta`), so it never touches the user's own
//! tmux, and we can configure it to feel seamless (no status bar, Ctrl-q detach).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Which tmux server to use. Default is a dedicated socket ("wta") so wta is
/// fully isolated from the user's own tmux. `WTA_TMUX_SOCKET=default` (or the
/// `--server default` flag) makes agents live on the user's own tmux server —
/// in which case wta must NOT touch global options/keybindings.
fn socket_name() -> String {
    std::env::var("WTA_TMUX_SOCKET").unwrap_or_else(|_| "wta".into())
}

/// True when using our own dedicated socket (safe to set global tmux options).
fn dedicated() -> bool {
    let s = socket_name();
    !s.is_empty() && s != "default"
}

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    if dedicated() {
        c.arg("-L").arg(socket_name());
    }
    c
}

/// The tmux server label for diagnostics: the dedicated socket name, or "default"
/// when running on the user's own server.
pub(crate) fn server_label() -> String {
    if dedicated() {
        format!("-L {}", socket_name())
    } else {
        "default".into()
    }
}

/// `tmux -V` output (e.g. "tmux 3.4"), or None if the binary isn't found.
pub fn version() -> Option<String> {
    let out = Command::new("tmux").arg("-V").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Map a name to the safe character class shared by tmux session names, state
/// filenames, and worktree dirs — every non-`[A-Za-z0-9_-]` char becomes `_` — so
/// those three representations of a task can never diverge or escape their directory.
pub(crate) fn sanitize_task(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// tmux session name, namespaced by repo id so the same task name in two repos
/// never collides on the (global) tmux server: `wta-<repo>-<task>`.
pub fn session_name(repo: &str, task: &str) -> String {
    format!("wta-{}-{}", sanitize_task(repo), sanitize_task(task))
}

pub fn has_session(name: &str) -> bool {
    tmux()
        .args(["has-session", "-t", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// All live session names on the wta server (empty if none / no server running).
pub fn list_sessions() -> Vec<String> {
    tmux()
        .args(["list-sessions", "-F", "#{session_name}"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Make an agent session feel like a dedicated app, not raw tmux:
/// hide the status bar, enable mouse, zero escape latency, bigger scrollback,
/// and bind Ctrl-q to detach (root table, so no prefix needed).
fn configure(name: &str) {
    // Session-scoped (`-t <session>`): safe on any server — only affects our sessions.
    for (opt, val) in [("mouse", "on"), ("history-limit", "10000")] {
        // stderr silenced: a program that dies instantly takes the session with it,
        // and tmux's "no such session" would leak into the caller's terminal
        let _ = tmux().args(["set-option", "-t", name, opt, val]).stderr(Stdio::null()).status();
    }
    if dedicated() {
        // We own this socket, so set server-globals: zero escape latency + Ctrl-q
        // detaches (root table, no prefix), plus the "return to wta" status bar.
        let _ = tmux().args(["set-option", "-g", "escape-time", "0"]).status();
        let _ = tmux().args(["bind-key", "-n", "C-q", "detach-client"]).status();
        ensure_hint_bar(name);
    } else {
        // On the user's own server keep it seamless — no status bar of ours.
        let _ = tmux().args(["set-option", "-t", name, "status", "off"]).status();
    }
}

/// Turn on the THIN "Ctrl-q ↩ return to wta" status bar for a session on our socket —
/// so it's obvious you're inside a wta agent and how to get out. Idempotent, and
/// re-applied on attach so agents created before this feature existed (which carry a
/// per-session `status off`) pick it up too. It shows only while attached, never in
/// the dashboard Preview (capture-pane grabs pane text, not the status line).
fn ensure_hint_bar(name: &str) {
    if !dedicated() {
        return;
    }
    // Opt out once the keys are muscle memory: no bar at all while attached.
    if std::env::var("WTA_HINT_BAR").map(|v| v == "0").unwrap_or(false) {
        let _ = tmux().args(["set-option", "-g", "status", "off"]).status();
        return;
    }
    // Drop any stale per-session `status` override so the session inherits the bar.
    let _ = tmux().args(["set-option", "-u", "-t", name, "status"]).status();
    ensure_scroll_keys();
    ensure_copy_key();
    // macOS shows ⌥/^ (WezTerm/iTerm/Terminal all send left-Option as Alt/Meta);
    // elsewhere tmux's own M-/C- spelling.
    let (alt_y, ctrl_q) = if cfg!(target_os = "macos") { ("⌥y", "^q") } else { ("M-y", "C-q") };
    // One "chip" per key: the key on a colored block, a one-word label after it.
    let chip = |bg: &str, key: &str, label: &str| {
        format!("#[fg=black,bg={bg},bold] {key} #[fg={bg},bg=default,nobold] {label} ")
    };
    // all green: one hue reads calmer and matches wta's identity (and the left chip)
    let right = format!(
        "{}{}{}",
        chip("green", "PgUp", "scroll"),
        chip("green", alt_y, "copy"),
        chip("green", ctrl_q, "back"),
    );
    for (opt, val) in [
        ("status", "on"),
        ("status-style", "bg=default,fg=default"),
        // left: which agent you're in — `repo › task` as a green chip (set per session by
        // `set_label`; sessions from before that option existed fall back to the name)
        // …with the agent's live state glyph (set by `set_status_chip`); the chip turns
        // yellow when the agent needs you
        (
            "status-left",
            "#[fg=black,bg=#{?#{@wta_attn},yellow,green},bold] #{?@wta_label,#{@wta_label},#{session_name}} #{@wta_glyph}#[default]",
        ),
        ("status-left-length", "48"),
        ("status-right", right.as_str()),
        ("status-right-length", "48"),
    ] {
        let _ = tmux().args(["set-option", "-g", opt, val]).status();
    }
    // Window-status is a window option — clear it so the bar is just the hint.
    for opt in ["window-status-format", "window-status-current-format"] {
        let _ = tmux().args(["set-option", "-gw", opt, ""]).status();
    }
}

pub fn new_session(name: &str, cwd: &Path, program: &str, extra: &[String]) -> Result<()> {
    if has_session(name) {
        return Ok(());
    }
    let cwd_s = cwd.to_string_lossy().into_owned();
    let mut args: Vec<String> = ["new-session", "-d", "-s", name, "-c", &cwd_s, program]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(extra.iter().cloned());
    let out = tmux()
        .args(&args)
        .output()
        .context("failed to run tmux (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    configure(name);
    Ok(())
}

/// Label a session `repo › task` for the attached status bar's left side (a tmux user
/// option on the session, read by the `status-left` format).
pub fn set_label(name: &str, repo_name: &str, task: &str) {
    let label = format!("{repo_name} › {task}");
    let _ = tmux()
        .args(["set-option", "-t", name, "@wta_label", &label])
        .stderr(Stdio::null())
        .status();
}

/// Reflect an agent's state in its status-bar chip: a glyph after `repo › task`, and a
/// yellow chip while it needs you. Same vocabulary as the dashboard. Called by the
/// dashboard on every status change and by the `wta status` hook, so it's live whether
/// or not a dashboard is open.
pub fn set_status_chip(name: &str, status: &str) {
    if !dedicated() {
        return;
    }
    let (glyph, attn) = match status {
        "running" => ("⟳", false),
        "ready" | "waiting" => ("●", false),
        "needs_input" => ("▲", true),
        "merged" => ("✓", false),
        "exited" | "idle" => ("✗", false),
        _ => ("", false),
    };
    for (opt, val) in [("@wta_glyph", glyph), ("@wta_attn", if attn { "1" } else { "" })] {
        let _ = tmux()
            .args(["set-option", "-t", name, opt, val])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status();
    }
}

/// Prefix-free scrollback keys on our dedicated server. When wta itself runs inside
/// the user's tmux (WezTerm → tmux → wta → agent), the OUTER server eats `Ctrl-b`, so
/// `Ctrl-b [` opens copy mode on the outer pane — whose history is the shell that
/// launched wta, not the agent's chat. Unbound Alt/Shift/PageUp keys pass straight
/// through an outer tmux, so they reach us.
///
/// Two kinds of agent screen, handled per keypress via `#{alternate_on}`:
/// - Claude Code ≥2.1's fullscreen renderer (and nvim, less…) runs in the ALTERNATE
///   screen and scrolls its own buffer — tmux history is empty, copy mode would only
///   show the current screen. Forward the key so the app scrolls (Claude: PageUp/Down
///   natively; Alt-k/j etc. via ~/.claude/keybindings.json `Scroll` context).
/// - classic renderers / plain output live in tmux history: open copy mode one page
///   up (repeat to keep paging); the user's own mode-keys take over from there.
fn ensure_scroll_keys() {
    if !dedicated() {
        return;
    }
    for key in ["PPage", "M-k", "S-Up"] {
        let fwd = format!("send-keys {key}");
        let _ = tmux()
            .args(["bind-key", "-n", key, "if-shell", "-F", "#{alternate_on}", &fwd, "copy-mode -u"])
            .status();
    }
    // inside copy mode keep Shift-↑/↓ paging (same as the entry key), in both key tables
    for table in ["copy-mode", "copy-mode-vi"] {
        let _ = tmux().args(["bind-key", "-T", table, "S-Up", "send-keys", "-X", "page-up"]).status();
        let _ = tmux().args(["bind-key", "-T", table, "S-Down", "send-keys", "-X", "page-down"]).status();
    }
}

/// `Alt-y` while attached: wta's own vim-style copy mode in a tmux popup over the agent
/// (see `copyview`). Works for any agent regardless of how it draws — and passes through
/// an outer tmux, since it's an unbound Alt key there. The popup runs THIS binary
/// (`current_exe`) so it doesn't depend on `wta` being on the pane's PATH.
///
/// Goes through `run-shell` because `display-popup` does NOT expand formats in its
/// command, while `run-shell` does — that's how one server-wide binding learns which
/// agent (`#{session_name}`) and which client (`#{client_name}`) the key fired in.
fn ensure_copy_key() {
    if !dedicated() {
        return;
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "wta".into());
    let sh_quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let cmd = format!(
        "tmux -L {sock} display-popup -c '#{{client_name}}' -E -w 96% -h 96% -T ' copy mode · q closes ' \
         {exe} copy --session '#{{session_name}}'",
        sock = socket_name(),
        exe = sh_quote(&exe),
    );
    let _ = tmux()
        .args(["bind-key", "-n", "M-y", "run-shell", "-b", &cmd])
        .stderr(Stdio::null())
        .status();
}

/// Watch a just-spawned session for `grace`: if its program dies inside that window
/// (classic case: `claude --continue` with no saved conversation → "No conversation
/// found to continue" and exit 1), return what the pane printed and kill the session,
/// so the caller can explain the failure instead of showing a bare "exited" row.
/// `None` means it's still running (the pane is left exactly as spawned).
///
/// Works by holding the pane open with `remain-on-exit` for the grace period, which
/// keeps the dying program's last screen readable via capture-pane.
pub fn watch_early_exit(name: &str, grace: Duration) -> Option<String> {
    let set_remain = |val: &str| {
        let _ = tmux()
            .args(["set-option", "-w", "-t", name, "remain-on-exit", val])
            .stderr(Stdio::null())
            .status();
    };
    set_remain("on");
    let start = Instant::now();
    loop {
        if !has_session(name) {
            // died before we could hold the pane — nothing to read back
            return Some(String::new());
        }
        let dead = tmux()
            .args(["display-message", "-p", "-t", name, "#{pane_dead}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false);
        if dead {
            let text = capture(name).unwrap_or_default();
            let _ = kill(name);
            // last few non-blank lines: the error is at the bottom of the pane
            let tail: Vec<&str> = text
                .lines()
                .map(str::trim_end)
                // drop blanks and tmux's own remain-on-exit banner ("Pane is dead …")
                .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("Pane is dead"))
                .collect();
            let keep = tail.len().saturating_sub(4);
            return Some(tail[keep..].join("\n"));
        }
        if start.elapsed() >= grace {
            break;
        }
        sleep(Duration::from_millis(150));
    }
    set_remain("off");
    None
}

/// Visible pane text of a session (plain, no escapes) — for hashing + status/trust
/// matching, which must see clean text.
pub fn capture(name: &str) -> Option<String> {
    let out = tmux()
        .args(["capture-pane", "-p", "-t", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The current working directory of a session's active pane — the agent's real cwd,
/// which is where its transcript is keyed. `None` if the session is gone.
pub fn pane_path(name: &str) -> Option<String> {
    let out = tmux()
        .args(["display-message", "-p", "-t", name, "#{pane_current_path}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then_some(p)
}

/// The pane's FULL history as plain text (`-S -`), for wta's copy mode when there's no
/// transcript to read. Empty for alternate-screen apps (their history isn't tmux's).
pub fn capture_full(name: &str) -> Option<String> {
    let out = tmux()
        .args(["capture-pane", "-p", "-S", "-", "-t", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Visible pane text WITH ANSI escapes (`-e`) so the Preview keeps the agent's
/// real colors without needing to attach. `full` grabs the whole scrollback
/// history (`-S -`) for scroll mode; otherwise just the visible pane.
pub fn capture_colored(name: &str, full: bool) -> Option<String> {
    let mut c = tmux();
    c.args(["capture-pane", "-e", "-p", "-t", name]);
    if full {
        c.args(["-S", "-"]);
    }
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Type literal `text` (the `-- ` ends flag parsing so a leading `-` is safe).
fn send_literal(name: &str, text: &str) -> Result<()> {
    let ok = tmux()
        .args(["send-keys", "-t", name, "-l", "--", text])
        .status()
        .context("tmux send-keys -l failed")?
        .success();
    if !ok {
        bail!("tmux send-keys -l failed for {name}");
    }
    Ok(())
}

/// Press Enter (a real CR — the literal word `Enter`, NOT `-l`).
pub fn send_enter(name: &str) -> Result<()> {
    let ok = tmux()
        .args(["send-keys", "-t", name, "Enter"])
        .status()
        .context("tmux send-keys Enter failed")?
        .success();
    if !ok {
        bail!("tmux send-keys Enter failed for {name}");
    }
    Ok(())
}

// collapse whitespace so pane wrapping/padding doesn't defeat a substring match
pub(crate) fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Heuristic: does the pane look like it's showing an interactive dialog awaiting a
/// keystroke (trust/permission prompt, numbered menu, y/n)? Used to REFUSE relaying
/// a peer message that would otherwise silently answer the dialog. Errs toward true.
pub fn looks_interactive_dialog(text: &str) -> bool {
    let l = norm(text).to_lowercase(); // case-insensitive so [Y/n]/(Y/N)/etc. don't slip through
    l.contains("do you want to")
        || l.contains("do you trust the files")
        || l.contains("i trust this folder")
        || l.contains("you created or one you trust")
        || l.contains("no, exit")
        || (l.contains("1. yes") && l.contains("2. no"))
        || l.contains("(y/n)")
        || l.contains("[y/n]")
        || l.contains("(yes/no)")
        || l.contains("press enter to")
        || l.contains("❯ 1")
        || l.contains("│ 1")
}

/// Two captures a short interval apart are byte-identical => the pane isn't
/// actively rendering (agent idle at its prompt). Used to gate quick-send.
pub fn pane_is_idle(name: &str) -> bool {
    let a = match capture(name) {
        Some(s) => s,
        None => return false,
    };
    sleep(Duration::from_millis(120));
    match capture(name) {
        Some(b) => a == b,
        None => false,
    }
}

/// Type `text` into a session and submit it, hardened against races with a live
/// agent TUI: literal → settle → echo-confirm → Enter → **consumption-confirm**. Only
/// presses Enter once the typed text is confirmed on screen, so a dropped burst becomes
/// a clean error instead of a half-submitted line. Returns whether the turn was actually
/// **submitted** (the agent accepted it) vs merely typed — "keystrokes landed in the
/// pane" is not "the agent consumed the turn" (it may sit at a multiline prompt). Used by
/// the dashboard quick-send, the peer relay (`wta send`), and the Telegram bridge.
pub fn send_text(name: &str, text: &str) -> Result<bool> {
    send_literal(name, text)?;
    // let the editor drain the burst before Enter arrives as a distinct event
    sleep(Duration::from_millis(60));

    // echo-confirm: only submit once the typed chars are visible in the pane
    let needle: String = {
        let c: Vec<char> = text.trim().chars().collect();
        norm(&c[c.len().saturating_sub(24)..].iter().collect::<String>())
    };
    if !needle.is_empty() {
        let seen = |s: Option<String>| s.map(|p| norm(&p).contains(&needle)).unwrap_or(false);
        if !seen(capture(name)) {
            sleep(Duration::from_millis(90));
            if !seen(capture(name)) {
                bail!("send aborted: '{name}' did not echo typed text (agent busy?)");
            }
        }
    }

    // Never press Enter into a dialog — re-check right before submitting, closing
    // the check→send window (a static permission/trust prompt looks "idle").
    if capture(name).map(|p| looks_interactive_dialog(&p)).unwrap_or(false) {
        bail!("send aborted: '{name}' is at a prompt/dialog");
    }
    // Pane state with the text typed but not yet submitted — the baseline the submit
    // must move away from.
    let typed = capture(name).map(|p| norm(&p)).unwrap_or_default();
    send_enter(name)?;

    // Consumption-confirm: a real submit has a visible effect (input clears, the message
    // echoes into the transcript, or the agent starts working). If the pane never moves
    // from the just-typed state, the Enter was swallowed (multiline prompt / a dialog) —
    // the text landed but the turn wasn't consumed. Poll briefly, then retry Enter once
    // (harmless no-op on an empty input) before concluding it wasn't submitted.
    let moved = |base: &str| -> bool {
        for _ in 0..6 {
            sleep(Duration::from_millis(100));
            if capture(name).map(|p| norm(&p)).as_deref() != Some(base) {
                return true;
            }
        }
        false
    };
    if moved(&typed) {
        return Ok(true);
    }
    let _ = send_enter(name);
    sleep(Duration::from_millis(150));
    let submitted = capture(name).map(|p| norm(&p)).as_deref() != Some(typed.as_str());
    Ok(submitted)
}

pub fn kill(name: &str) -> Result<()> {
    let _ = tmux()
        .args(["kill-session", "-t", name])
        .stderr(Stdio::null())
        .status();
    Ok(())
}

/// Attach fullscreen, inheriting the terminal. Blocks until the user hits Ctrl-q
/// (bound to detach-client). Caller must suspend any raw-mode TUI first.
pub fn attach_blocking(name: &str) -> Result<()> {
    let inside_tmux = std::env::var("TMUX").is_ok();
    ensure_hint_bar(name); // so even pre-existing agents show the Ctrl-q bar

    // On the user's OWN server, agents share their tmux — so switch to the
    // session instead of a (guarded) nested attach. Ctrl-q isn't bound here, so
    // the user detaches/returns with their normal tmux keys.
    if !dedicated() && inside_tmux {
        tmux()
            .args(["switch-client", "-t", name])
            .status()
            .context("tmux switch-client failed")?;
        return Ok(());
    }

    // On our dedicated socket, launched from inside the user's tmux, a plain attach
    // would be refused as a nested session. By default we still attach IN THE CURRENT
    // PANE (the fall-through below unsets $TMUX to allow it), so it respects a split
    // layout. Set WTA_ATTACH_POPUP=1 for the old full-window popup overlay instead.
    if dedicated() && inside_tmux && std::env::var("WTA_ATTACH_POPUP").as_deref() == Ok("1") {
        let inner = format!("tmux -L {} attach-session -t {}", socket_name(), name);
        if let Ok(s) = Command::new("tmux")
            .args(["display-popup", "-w", "92%", "-h", "92%", "-E", &inner])
            .status()
        {
            if s.success() {
                return Ok(());
            }
        }
    }

    // (The persistent "Ctrl-q ↩ return to wta" status bar set in configure() makes the
    // exit key obvious while attached, so no transient hint is needed here.)
    tmux()
        .args(["attach-session", "-t", name])
        // Unset $TMUX so tmux attaches in the CURRENT pane (respecting a split
        // layout) rather than refusing this as a nested session. Our agents live on
        // a separate socket, so this is a cross-server attach, not true nesting; the
        // caller already released the pane, and Ctrl-q (bound on our server) returns.
        .env_remove("TMUX")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("tmux attach failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_guard_flags_prompts_but_not_normal_output() {
        // interactive dialogs the relay must refuse to send into
        assert!(looks_interactive_dialog("Do you want to allow Bash(rm)? 1. Yes 2. No"));
        assert!(looks_interactive_dialog("Is this a directory you created or one you trust?"));
        assert!(looks_interactive_dialog("Overwrite file? (y/n)"));
        assert!(looks_interactive_dialog("❯ 1. Accept  2. Reject"));
        // case-insensitive + broadened cues
        assert!(looks_interactive_dialog("Continue? [Y/n]"));
        assert!(looks_interactive_dialog("Proceed (Yes/No)"));
        assert!(looks_interactive_dialog("Press ENTER to continue"));
        // normal agent output must be relayable
        assert!(!looks_interactive_dialog("Running tests... 42 passed"));
        assert!(!looks_interactive_dialog("I'll refactor the auth module now."));
    }
}
