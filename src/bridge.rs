//! Remote notifications: ping a Telegram chat when an agent needs you or
//! finishes its turn. Reads the same `~/.wta/state` files the dashboard uses
//! (populated by the optional Claude Code hooks), so it needs no extra wiring.
//!
//! Config (env):
//!   WTA_TELEGRAM_TOKEN   bot token from @BotFather
//!   WTA_TELEGRAM_CHAT    your chat id (numeric)
//!
//! Outbound only for now; inbound control (reply -> tmux send-keys) is roadmap.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;

use crate::status;

struct Cfg {
    token: String,
    chat: String,
}

fn cfg() -> Result<Cfg> {
    let token = std::env::var("WTA_TELEGRAM_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .context("set WTA_TELEGRAM_TOKEN (bot token from @BotFather)")?;
    let chat = std::env::var("WTA_TELEGRAM_CHAT")
        .ok()
        .filter(|s| !s.is_empty())
        .context("set WTA_TELEGRAM_CHAT (your numeric chat id)")?;
    Ok(Cfg { token, chat })
}

fn send(c: &Cfg, text: &str) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", c.token);
    ureq::post(&url)
        .send_form(&[("chat_id", c.chat.as_str()), ("text", text)])
        .map_err(|e| anyhow::anyhow!("telegram send failed: {e}"))?;
    Ok(())
}

/// Decide whether a status transition is worth a notification.
/// `prev` is the last-seen status for this task (None on first sight).
pub fn attention_message(prev: Option<&str>, cur: &str, task: &str) -> Option<String> {
    let fresh = |s: &str| prev.map(|p| p != s).unwrap_or(true);
    match cur {
        "needs_input" if fresh("needs_input") => Some(format!("⚠️ wta: '{task}' needs input")),
        "waiting" if fresh("waiting") => Some(format!("✅ wta: '{task}' finished — your turn")),
        _ => None,
    }
}

pub fn run(test: bool) -> Result<()> {
    let c = cfg()?;
    if test {
        send(&c, "✅ wta bridge connected")?;
        println!("sent a test message to Telegram chat {}", c.chat);
        return Ok(());
    }

    println!("wta bridge running — pings on needs-input / finished. Ctrl-C to stop.");
    let mut last: HashMap<String, String> = HashMap::new();
    let mut primed = false; // skip the first scan so we don't replay existing states
    loop {
        if let Ok(states) = status::read_all_states() {
            for st in states {
                if primed {
                    let prev = last.get(&st.task).map(|s| s.as_str());
                    if let Some(msg) = attention_message(prev, &st.status, &st.task) {
                        if let Err(e) = send(&c, &msg) {
                            eprintln!("wta bridge: {e}");
                        }
                    }
                }
                last.insert(st.task, st.status);
            }
            primed = true;
        }
        std::thread::sleep(Duration::from_secs(8));
    }
}

#[cfg(test)]
mod tests {
    use super::attention_message as am;

    #[test]
    fn only_notifies_on_fresh_attention_states() {
        assert_eq!(am(None, "running", "x"), None);
        assert!(am(None, "needs_input", "x")
            .unwrap()
            .contains("needs input"));
        assert_eq!(am(Some("needs_input"), "needs_input", "x"), None); // no re-notify
        assert!(am(Some("running"), "waiting", "x")
            .unwrap()
            .contains("your turn"));
        assert_eq!(am(Some("waiting"), "running", "x"), None);
    }
}
