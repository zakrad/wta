//! tmux backend: each agent is a detached tmux session (`wta-<task>`) that
//! survives closing the terminal / laptop sleep. The dashboard captures its
//! output and attaches to it inline (like a session multiplexer's attach/detach).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Deterministic tmux session name for a task (sanitized: tmux dislikes `.` and `:`).
pub fn session_name(task: &str) -> String {
    let safe: String = task
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("wta-{safe}")
}

pub fn has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start a detached agent session in `cwd` running `program` + `extra` args.
pub fn new_session(name: &str, cwd: &Path, program: &str, extra: &[String]) -> Result<()> {
    if has_session(name) {
        return Ok(());
    }
    let cwd_s = cwd.to_string_lossy().into_owned();
    let mut args: Vec<String> =
        vec!["new-session".into(), "-d", "-s", name, "-c", &cwd_s, program].iter().map(|s| s.to_string()).collect();
    args.extend(extra.iter().cloned());
    let out = Command::new("tmux").args(&args).output().context("failed to run tmux (is it installed?)")?;
    if !out.status.success() {
        bail!("tmux new-session failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Capture the visible pane text of a session (plain text, no escapes).
pub fn capture(name: &str) -> Option<String> {
    let out = Command::new("tmux").args(["capture-pane", "-p", "-t", name]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn kill(name: &str) -> Result<()> {
    // ignore "session not found"
    let _ = Command::new("tmux").args(["kill-session", "-t", name]).stderr(Stdio::null()).status();
    Ok(())
}

/// Attach to a session in the foreground, inheriting the terminal. Blocks until
/// the user detaches (Ctrl-b d). Caller must suspend any raw-mode TUI first.
pub fn attach_blocking(name: &str) -> Result<()> {
    Command::new("tmux").args(["attach", "-t", name]).status().context("tmux attach failed")?;
    Ok(())
}
