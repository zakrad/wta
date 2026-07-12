//! Agent notifications: a **sound** and a compact, nvim-style **tmux toast**, both
//! fired from the Claude Code Stop/Notification hooks (`wta status`). They reach you
//! independent of the dashboard — even while you're attached inside an agent or have
//! it closed. Everything here is fire-and-forget and never blocks the caller.

use std::process::{Command, Stdio};

/// The "an agent needs you" alert: a sound plus a compact top-right toast in the
/// terminal (see [`tmux_popup`]).
pub fn alert(title: &str, body: &str) {
    sound();
    tmux_popup(title, body);
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

fn wta_home() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".wta"))
}

/// Record the user's own tmux socket (parsed from `$TMUX`) so a hook running inside
/// an agent — which has a *different* `$TMUX` (wta's private server) — can still pop
/// a toast onto the terminal the user is actually looking at. Only for user-initiated
/// runs (WTA_TASK unset) inside tmux; never the agent's hook itself.
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

/// Does the tmux server on `socket` have a client attached right now? (Used to tell
/// whether the user is currently attached inside an agent.)
fn has_client(socket: &str) -> bool {
    Command::new("tmux")
        .args(["-S", socket, "list-clients", "-F", "x"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Pop a **compact, top-right, self-dismissing** toast onto the user's terminal via
/// their tmux (recorded by [`record_user_tmux`]) — like nvim-notify. Purely
/// terminal-native: it draws inside the terminal, no macOS notification involved.
/// Auto-closes after `WTA_TMUX_SECS` seconds (default 4). No-op if we don't know the
/// user's tmux, or with `WTA_TMUX_NOTIFY=0`.
pub fn tmux_popup(title: &str, body: &str) {
    if std::env::var("WTA_TMUX_NOTIFY").unwrap_or_default() == "0" {
        return;
    }
    // Where to pop it: if this hook's own tmux server (wta's) has an attached client,
    // the user is *inside* an agent right now — pop there so it draws over their view.
    // Otherwise pop on the outer tmux the dashboard/shell runs in (the bridge file).
    let own = std::env::var("TMUX")
        .ok()
        .and_then(|t| t.split(',').next().map(str::to_string))
        .filter(|s| !s.is_empty());
    let socket = match own {
        Some(s) if has_client(&s) => s,
        _ => match user_tmux_socket() {
            Some(s) => s,
            None => return,
        },
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
        .unwrap_or(4);
    // Compact box sized to the longer line; anchored top-right (-x R -y 2).
    let w = (t.chars().count().max(b.chars().count()) + 5).clamp(22, 60);
    // Exactly two lines, no trailing newline (with -h 4 that fills the box, no blank
    // bottom line). `stty -echo` on the popup's own pty means keystrokes the modal
    // popup captures don't echo a stray newline into the box. `read -t N -s -n 1`
    // closes the popup on the FIRST keypress (silently) or after N seconds — so it
    // gets out of your way the moment you touch the keyboard. printf uses %s so the
    // message is never a format string. \342\232\241 = ⚡ in UTF-8 octal.
    let script = format!("stty -echo 2>/dev/null; printf '  \\342\\232\\241 %s\\n  %s' '{t}' '{b}'; read -t {secs} -s -n 1 2>/dev/null");
    let _ = Command::new("tmux")
        .args([
            "-S", &socket, "display-popup",
            "-x", "R", "-y", "2",
            "-w", &w.to_string(), "-h", "4",
            "-T", "", "-E", &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
