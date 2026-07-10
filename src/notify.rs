//! Desktop notification + sound, shared by the dashboard and the Claude Code hooks.
//!
//! The hook path (`wta status waiting` on Claude's Stop event) is the primary
//! trigger: it fires whenever an agent finishes a turn, independent of whether the
//! dashboard is open, focused, or you're attached inside the agent. Everything here
//! is fire-and-forget and never blocks the caller.

use std::process::{Command, Stdio};

/// Banner + sound together — the normal "an agent needs you" alert.
pub fn alert(title: &str, body: &str) {
    banner(title, body);
    sound();
}

/// Post a desktop notification. Prefers `terminal-notifier` (its own signed app
/// identity → a real system banner that shows on any screen regardless of terminal
/// or focus, like the GUI tools); then a terminal-native OSC escape; then
/// `osascript`/`notify-send`. Opt out with `WTA_NOTIFY_DESKTOP=0`.
pub fn banner(title: &str, body: &str) {
    if std::env::var("WTA_NOTIFY_DESKTOP").unwrap_or_default() == "0" {
        return;
    }
    // 1) terminal-notifier: most reliable, works even when the terminal isn't focused
    //    and from inside a tmux session (so the Stop hook can post it).
    if which("terminal-notifier") {
        let _ = Command::new("terminal-notifier")
            .args(["-title", title, "-message", body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        return;
    }
    // 2) Terminal-native escape: no install/permission, but only renders in the
    //    terminal the program is attached to (unreliable from inside tmux).
    if std::env::var("WTA_NOTIFY_ESCAPE").unwrap_or_default() != "0" {
        if let Some(seq) = term_notify_escape(title, body) {
            if write_to_tty(seq.as_bytes()) {
                return;
            }
        }
    }
    // 3) Last resort: osascript (macOS) / notify-send (Linux). macOS may drop the
    //    osascript one if "Script Editor" lacks notification permission.
    let mut cmd;
    #[cfg(target_os = "macos")]
    {
        cmd = Command::new("osascript");
        cmd.args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 1 of argv) with title (item 2 of argv)",
            "-e",
            "end run",
            "--",
        ]);
        cmd.arg(body).arg(title);
    }
    #[cfg(not(target_os = "macos"))]
    {
        cmd = Command::new("notify-send");
        cmd.arg(title).arg(body);
    }
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Play a notification sound (audible even when the terminal bell is muted). Opt out
/// with `WTA_NOTIFY_SOUND=0`; point it at a path to use your own file.
pub fn sound() {
    let cfg = std::env::var("WTA_NOTIFY_SOUND").unwrap_or_default();
    if cfg == "0" {
        return;
    }
    let custom = (!cfg.is_empty() && cfg != "1").then_some(cfg);
    let (player, file) = if cfg!(target_os = "macos") {
        ("afplay", custom.unwrap_or_else(|| "/System/Library/Sounds/Glass.aiff".into()))
    } else {
        ("paplay", custom.unwrap_or_else(|| "/usr/share/sounds/freedesktop/stereo/complete.oga".into()))
    };
    let _ = Command::new(player)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Build the terminal-native desktop-notification escape for the current terminal,
/// if we recognize it. Returns `None` for terminals we don't know.
pub fn term_notify_escape(title: &str, body: &str) -> Option<String> {
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    // Semicolons would prematurely end OSC parameters; strip them defensively.
    let clean = |s: &str| s.replace(['\n', '\r', '\x1b', ';'], " ");
    let (t, b) = (clean(title), clean(body));
    let one = if b.is_empty() { t.clone() } else { format!("{t} — {b}") };
    if prog == "kitty" || term.contains("kitty") || std::env::var("KITTY_WINDOW_ID").is_ok() {
        return Some(format!("\x1b]99;;{one}\x1b\\"));
    }
    if prog == "WezTerm" || std::env::var("WEZTERM_PANE").is_ok() {
        return Some(format!("\x1b]777;notify;{t};{b}\x07"));
    }
    if prog == "iTerm.app" || prog == "vscode" || term.contains("iterm") {
        return Some(format!("\x1b]9;{one}\x07"));
    }
    None
}

/// Write bytes straight to the controlling terminal (`/dev/tty`). Returns whether it
/// succeeded (false when there is no controlling tty, e.g. under a hook).
pub fn write_to_tty(bytes: &[u8]) -> bool {
    use std::io::Write;
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(bytes);
        let _ = tty.flush();
        true
    } else {
        false
    }
}

/// Is `prog` on PATH? (shell-less `which`-style probe.)
pub fn which(prog: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(prog);
                std::fs::metadata(&p).map(|m| !m.is_dir()).unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
