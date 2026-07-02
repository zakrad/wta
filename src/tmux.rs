//! tmux backend: each agent is a detached tmux session (`wta-<task>`) on a
//! DEDICATED tmux server (socket `-L wta`), so it never touches the user's own
//! tmux, and we can configure it to feel seamless (no status bar, Ctrl-q detach).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Dedicated socket so wta's tmux is fully isolated from the user's tmux server.
const SOCKET: &str = "wta";

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", SOCKET]);
    c
}

pub fn session_name(task: &str) -> String {
    let safe: String = task
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("wta-{safe}")
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

/// Make an agent session feel like a dedicated app, not raw tmux:
/// hide the status bar, enable mouse, zero escape latency, bigger scrollback,
/// and bind Ctrl-q to detach (root table, so no prefix needed).
fn configure(name: &str) {
    for (opt, val) in [
        ("status", "off"),
        ("mouse", "on"),
        ("escape-time", "0"),
        ("history-limit", "10000"),
    ] {
        let _ = tmux().args(["set-option", "-t", name, opt, val]).status();
    }
    // Ctrl-q detaches (only affects this dedicated server).
    let _ = tmux()
        .args(["bind-key", "-n", "C-q", "detach-client"])
        .status();
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

/// Visible pane text of a session (plain, no escapes).
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
    // If launched from inside the user's OWN tmux, a plain attach to our
    // dedicated socket would nest tmux-in-tmux (stacked status lines, prefix
    // clashes). Prefer a popup on the outer server (tmux >= 3.2), which isolates
    // the agent visually; fall back to a nested attach if popups aren't available.
    if std::env::var("TMUX").is_ok() {
        let inner = format!("tmux -L {SOCKET} attach-session -t {name}");
        if let Ok(s) = Command::new("tmux")
            .args(["display-popup", "-w", "92%", "-h", "92%", "-E", &inner])
            .status()
        {
            if s.success() {
                return Ok(());
            }
        }
    }

    // best-effort hint shown briefly in the agent's message line
    let _ = tmux()
        .args([
            "display-message",
            "-d",
            "1200",
            "-t",
            name,
            "press Ctrl-q to return to wta",
        ])
        .status();
    tmux()
        .args(["attach-session", "-t", name])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("tmux attach failed")?;
    Ok(())
}
