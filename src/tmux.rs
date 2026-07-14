//! tmux backend: each agent is a detached tmux session (`wta-<task>`) on a
//! DEDICATED tmux server (socket `-L wta`), so it never touches the user's own
//! tmux, and we can configure it to feel seamless (no status bar, Ctrl-q detach).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

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
        let _ = tmux().args(["set-option", "-t", name, opt, val]).status();
    }
    if dedicated() {
        // We own this socket, so set server-globals: zero escape latency, Ctrl-q
        // detaches (root table, no prefix), and a THIN status bar whose only content
        // is a hint that you're inside a wta agent and Ctrl-q returns. The bar shows
        // while attached but never in the dashboard Preview (capture-pane grabs pane
        // text, not the status line).
        for (opt, val) in [
            ("escape-time", "0"),
            ("status", "on"),
            ("status-style", "bg=default,fg=green"),
            ("status-left", ""),
            ("status-right", " #[bold]Ctrl-q#[nobold] ↩ return to wta "),
            ("status-right-length", "28"),
        ] {
            let _ = tmux().args(["set-option", "-g", opt, val]).status();
        }
        // Window-status is a window option — clear it so the bar is just the hint.
        for opt in ["window-status-format", "window-status-current-format"] {
            let _ = tmux().args(["set-option", "-gw", opt, ""]).status();
        }
        let _ = tmux().args(["bind-key", "-n", "C-q", "detach-client"]).status();
    } else {
        // On the user's own server keep it seamless — no status bar of ours.
        let _ = tmux().args(["set-option", "-t", name, "status", "off"]).status();
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
/// agent TUI: literal → settle → echo-confirm → Enter → submit-confirm. Only
/// presses Enter once the typed text is confirmed on screen, so a dropped burst
/// becomes a clean error instead of a half-submitted line. Used by the dashboard
/// quick-send, the peer relay (`wta send`), and the Telegram bridge.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    let before = capture(name).map(|p| norm(&p)).unwrap_or_default();

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
    send_enter(name)?;

    // if nothing moved at all, the Enter was likely lost — retry once (a 2nd Enter
    // on an already-empty input is a harmless no-op, so no double-submit risk)
    sleep(Duration::from_millis(50));
    if capture(name).map(|p| norm(&p)) == Some(before) {
        let _ = send_enter(name);
    }
    Ok(())
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
