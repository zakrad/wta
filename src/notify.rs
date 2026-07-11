//! Desktop notification + sound, shared by the dashboard and the Claude Code hooks.
//!
//! The hook path (`wta status waiting` on Claude's Stop event) is the primary
//! trigger: it fires whenever an agent finishes a turn, independent of whether the
//! dashboard is open, focused, or you're attached inside the agent. Everything here
//! is fire-and-forget and never blocks the caller.

use std::process::{Command, Stdio};

/// The "an agent needs you" alert. **Sound** is the reliable baseline. A compact,
/// self-dismissing **tmux popup** (top-right toast) shows the agent + status right in
/// the terminal — the part that actually works on macOS, where CLI desktop banners
/// are unreliable. The macOS desktop **banner** is opt-in (`WTA_NOTIFY_DESKTOP=1`)
/// because on recent macOS it usually shows nothing and just clutters Notification
/// Center. All fire-and-forget.
pub fn alert(title: &str, body: &str) {
    sound();
    tmux_popup(title, body);
    if std::env::var("WTA_NOTIFY_DESKTOP").unwrap_or_default() == "1" {
        banner(title, body);
    }
}

/// Post a macOS/Linux desktop notification (opt-in; the caller decides whether to
/// call it). Prefers `terminal-notifier`, then a terminal-native OSC escape, then
/// `osascript`/`notify-send`. On recent macOS these often only reach Notification
/// Center without a visible banner — that's why it's opt-in.
pub fn banner(title: &str, body: &str) {
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

fn wta_home() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".wta"))
}

/// Record the user's own tmux socket (parsed from `$TMUX`) so a hook running inside
/// an agent — which has a *different* `$TMUX` (wta's private server) — can still pop
/// a notification onto the terminal the user is actually looking at. Only for
/// user-initiated runs (WTA_TASK unset) inside tmux; never the agent's hook itself.
pub fn record_user_tmux() {
    if std::env::var_os("WTA_TASK").is_some() {
        return;
    }
    let tmux = match std::env::var("TMUX") {
        Ok(t) if !t.is_empty() => t,
        _ => return,
    };
    // $TMUX = "<socket-path>,<pid>,<session-index>"
    let socket = match tmux.split(',').next() {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    if let Some(dir) = wta_home() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("tmux-client"), socket);
    }
}

fn user_tmux_socket() -> Option<String> {
    let s = std::fs::read_to_string(wta_home()?.join("tmux-client")).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Pop a **compact, top-right, self-dismissing** toast onto the user's terminal via
/// their tmux (recorded by [`record_user_tmux`]) — like nvim-notify. Purely
/// terminal-native: it draws inside the terminal and bypasses macOS Notification
/// Center entirely, so it shows even when desktop banners are suppressed. Auto-closes
/// after `WTA_TMUX_SECS` seconds (default 2). No-op if we don't know the user's tmux,
/// or with `WTA_TMUX_NOTIFY=0`.
pub fn tmux_popup(title: &str, body: &str) {
    if std::env::var("WTA_TMUX_NOTIFY").unwrap_or_default() == "0" {
        return;
    }
    let socket = match user_tmux_socket() {
        Some(s) => s,
        None => return,
    };
    // Strip chars that would break the single-quoted shell args below.
    let clean = |s: &str| -> String {
        s.chars().filter(|c| !matches!(c, '\'' | '\n' | '\r' | '\\')).collect()
    };
    let (t, b) = (clean(title), clean(body));
    let secs = std::env::var("WTA_TMUX_SECS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(2);
    // Compact box sized to the longer line; anchored top-right (-x R -y 2).
    let w = (t.chars().count().max(b.chars().count()) + 5).clamp(22, 60);
    // printf uses %s (never the message as a format string); `sleep` auto-closes with
    // no keypress. \342\232\241 = ⚡ in UTF-8 octal, kept out of the Rust string.
    let script = format!("printf '\\n  \\342\\232\\241 %s\\n  %s\\n' '{t}' '{b}'; sleep {secs}");
    let _ = Command::new("tmux")
        .args([
            "-S", &socket, "display-popup",
            "-x", "R", "-y", "2",
            "-w", &w.to_string(), "-h", "5",
            "-T", "", "-E", &script,
        ])
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
