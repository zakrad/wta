//! tmux backend: each agent is a detached tmux session (`wta-<task>`) on a
//! DEDICATED tmux server (socket `-L wta`), so it never touches the user's own
//! tmux, and we can configure it to feel seamless (no status bar, Ctrl-q detach).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

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
    // Session-scoped (`-t <session>`): safe on any server — only affects our sessions.
    for (opt, val) in [
        ("status", "off"),
        ("mouse", "on"),
        ("history-limit", "10000"),
    ] {
        let _ = tmux().args(["set-option", "-t", name, opt, val]).status();
    }
    // Server-global tweaks: ONLY on our dedicated socket, so we never clobber the
    // user's own tmux (escape-time is server-wide; `bind -n C-q` rebinds Ctrl-q
    // for every pane on the server).
    if dedicated() {
        let _ = tmux()
            .args(["set-option", "-g", "escape-time", "0"])
            .status();
        let _ = tmux()
            .args(["bind-key", "-n", "C-q", "detach-client"])
            .status();
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

/// Type `text` into a session and submit it (literal text, then Enter).
/// Used by the Telegram bridge to relay remote messages into an agent.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    let out = tmux()
        .args(["send-keys", "-t", name, "-l", text])
        .status()
        .context("tmux send-keys failed")?;
    if !out.success() {
        bail!("tmux send-keys failed for session {name}");
    }
    let _ = tmux().args(["send-keys", "-t", name, "Enter"]).status();
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

    // On our dedicated socket, launched from inside the user's tmux, a plain
    // attach would nest tmux-in-tmux; use a popup (tmux >= 3.2) to isolate it.
    if dedicated() && inside_tmux {
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

    // best-effort hint shown briefly in the agent's message line (dedicated socket)
    if dedicated() {
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
    }
    tmux()
        .args(["attach-session", "-t", name])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("tmux attach failed")?;
    Ok(())
}
