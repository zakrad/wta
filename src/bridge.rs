//! Remote control via Telegram (feature `telegram`).
//!
//! Outbound: pings the chat when an agent needs input or finishes.
//! Inbound: you reply in the chat to talk to an agent — messages are relayed
//! into the agent's tmux session via `send-keys`. Commands:
//!   /agents            list agents + status
//!   /use <task>        pick an agent to chat with
//!   /send <task> <txt> send to a specific agent
//!   <text>             send to the picked agent
//!
//! Config (env): WTA_TELEGRAM_TOKEN (from @BotFather), WTA_TELEGRAM_CHAT (chat id).
//! Only messages from that chat id are honored. Run the bridge with the same
//! `--server` as your agents so it targets the right tmux server.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;

use crate::{status, tmux};

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

/// Long-poll for new messages from the configured chat. Returns the next offset
/// and the text of each accepted message.
fn get_updates(c: &Cfg, offset: i64) -> Result<(i64, Vec<String>)> {
    let url = format!("https://api.telegram.org/bot{}/getUpdates", c.token);
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(20))
        .query("offset", &offset.to_string())
        .query("timeout", "3")
        .call()
        .map_err(|e| anyhow::anyhow!("getUpdates: {e}"))?;
    let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;

    let mut next = offset;
    let mut texts = Vec::new();
    if let Some(arr) = body["result"].as_array() {
        for u in arr {
            if let Some(id) = u["update_id"].as_i64() {
                if id + 1 > next {
                    next = id + 1;
                }
            }
            let msg = if u.get("message").is_some() {
                &u["message"]
            } else {
                &u["edited_message"]
            };
            let cid = msg["chat"]["id"].as_i64();
            let text = msg["text"].as_str();
            if let (Some(cid), Some(text)) = (cid, text) {
                // security: only honor messages from the configured chat
                if c.chat == cid.to_string() {
                    texts.push(text.to_string());
                }
            }
        }
    }
    Ok((next, texts))
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    List,
    Use(String),
    Send(String, String),
    Plain(String),
}

fn parse(text: &str) -> Cmd {
    let t = text.trim();
    match t {
        "/help" | "/start" => return Cmd::Help,
        "/agents" | "/ls" => return Cmd::List,
        _ => {}
    }
    if let Some(rest) = t.strip_prefix("/use ") {
        return Cmd::Use(rest.trim().to_string());
    }
    if let Some(rest) = t.strip_prefix("/send ") {
        let mut it = rest.trim().splitn(2, char::is_whitespace);
        let task = it.next().unwrap_or("").trim().to_string();
        let body = it.next().unwrap_or("").trim().to_string();
        return Cmd::Send(task, body);
    }
    if t.starts_with('/') {
        return Cmd::Help;
    }
    Cmd::Plain(t.to_string())
}

const HELP: &str = "wta bridge:\n/agents — list agents\n/use <task> — pick an agent to chat with\n/send <task> <text> — send to a specific agent\n<text> — send to the picked agent\n(you get pinged when an agent needs input or finishes)";

fn list_agents(selected: &Option<String>) -> String {
    match status::read_all_states() {
        Ok(mut states) if !states.is_empty() => {
            states.sort_by(|a, b| a.task.cmp(&b.task));
            let mut out = vec!["agents:".to_string()];
            for st in states {
                let mark = if selected.as_deref() == Some(st.task.as_str()) {
                    "* "
                } else {
                    "  "
                };
                out.push(format!("{mark}{} [{}]", st.task, st.status));
            }
            out.join("\n")
        }
        _ => "no agents yet".to_string(),
    }
}

fn deliver(task: &str, body: &str) -> String {
    if body.is_empty() {
        return "nothing to send".to_string();
    }
    // state is per-repo now — find this agent's repo. With same-named agents in
    // multiple repos, prefer one whose tmux session is actually live.
    let repos: Vec<String> = match status::read_all_states() {
        Ok(states) => states.into_iter().filter(|s| s.task == task).map(|s| s.repo).collect(),
        Err(_) => Vec::new(),
    };
    if repos.is_empty() {
        return format!("'{task}' not found");
    }
    let session = repos
        .iter()
        .map(|r| tmux::session_name(r, task))
        .find(|s| tmux::has_session(s));
    let session = match session {
        Some(s) => s,
        None => return format!("'{task}' isn't running (resume it in wta first)"),
    };
    match tmux::send_text(&session, body) {
        Ok(_) => {
            let echo: String = body.chars().take(60).collect();
            format!("→ {task}: {echo}")
        }
        Err(e) => format!("send failed: {e}"),
    }
}

fn dispatch(cmd: Cmd, selected: &mut Option<String>) -> String {
    match cmd {
        Cmd::Help => HELP.to_string(),
        Cmd::List => list_agents(selected),
        Cmd::Use(task) => {
            *selected = Some(task.clone());
            format!("now sending to '{task}' — just type to chat")
        }
        Cmd::Send(task, body) => deliver(&task, &body),
        Cmd::Plain(body) => match selected {
            Some(task) => deliver(task, &body),
            None => "pick an agent first: /use <task>  (see /agents)".to_string(),
        },
    }
}

/// Decide whether a status transition is worth a notification.
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
        send(&c, "✅ wta bridge connected — send /help")?;
        println!("sent a test message to Telegram chat {}", c.chat);
        return Ok(());
    }

    let _ = send(&c, "wta bridge online — /help for commands");
    // skip any backlog so old messages don't replay as commands
    let mut offset = get_updates(&c, 0).map(|(o, _)| o).unwrap_or(0);
    let mut last: HashMap<(String, String), String> = HashMap::new();
    let mut primed = false;
    let mut selected: Option<String> = None;
    println!("wta bridge running (inbound + outbound). Ctrl-C to stop.");

    loop {
        // inbound: relay chat messages into agents
        match get_updates(&c, offset) {
            Ok((next, texts)) => {
                offset = next;
                for t in texts {
                    let reply = dispatch(parse(&t), &mut selected);
                    let _ = send(&c, &reply);
                }
            }
            Err(e) => {
                eprintln!("wta bridge (inbound): {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }

        // outbound: notify on attention-worthy status transitions
        if let Ok(states) = status::read_all_states() {
            for st in states {
                // key by (repo, task) so same-named agents in different repos don't
                // share one slot (which caused spurious/suppressed pings)
                let key = (st.repo.clone(), st.task.clone());
                if primed {
                    let prev = last.get(&key).map(|s| s.as_str());
                    if let Some(msg) = attention_message(prev, &st.status, &st.task) {
                        let _ = send(&c, &msg);
                    }
                }
                last.insert(key, st.status);
            }
            primed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_notifies_on_fresh_attention_states() {
        assert!(attention_message(None, "needs_input", "x")
            .unwrap()
            .contains("needs input"));
        assert_eq!(
            attention_message(Some("needs_input"), "needs_input", "x"),
            None
        );
        assert!(attention_message(Some("running"), "waiting", "x")
            .unwrap()
            .contains("your turn"));
        assert_eq!(attention_message(None, "running", "x"), None);
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse("/agents"), Cmd::List);
        assert_eq!(parse("/use foo"), Cmd::Use("foo".into()));
        assert_eq!(
            parse("/send foo hello world"),
            Cmd::Send("foo".into(), "hello world".into())
        );
        assert_eq!(parse("just chatting"), Cmd::Plain("just chatting".into()));
        assert_eq!(parse("/whatever"), Cmd::Help);
    }
}
