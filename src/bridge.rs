//! Remote control via Telegram (feature `telegram`).
//!
//! Outbound: pings the chat when an agent needs input or finishes.
//! Inbound: you reply in the chat to talk to an agent — messages are relayed
//! into the agent's tmux session via `send-keys`. Commands:
//!   /agents            list agents + status
//!   /use <task>        pick an agent to chat with
//!   /send <task> <txt> send to a specific agent
//!   <text>             send to the picked agent
//!   /peek <task>       the last lines of the agent's screen
//!   /diff <task>       the agent's changes vs its base branch
//!
//! A "needs input" ping shows the agent's question with buttons: ✅ Approve (Enter on
//! the dialog), ❌ Deny (Esc), 📄 Diff, 👀 Peek. A "finished" ping has Diff and Peek.
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

/// ureq embeds the full request URL — including the `bot<TOKEN>` path segment — in
/// its error Display, which would leak the bot token to stderr/logs on any HTTP
/// error or rate-limit. Scrub the token out of any message before it's shown.
fn redact(token: &str, msg: String) -> String {
    if token.is_empty() {
        msg
    } else {
        msg.replace(token, "<redacted>")
    }
}

fn send(c: &Cfg, text: &str) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", c.token);
    ureq::post(&url)
        .send_form(&[("chat_id", c.chat.as_str()), ("text", text)])
        .map_err(|e| anyhow::anyhow!("telegram send failed: {}", redact(&c.token, e.to_string())))?;
    Ok(())
}

/// Send `text` with one row of inline buttons `(label, callback data)`.
fn send_buttons(c: &Cfg, text: &str, buttons: &[(&str, String)]) -> Result<()> {
    let row: Vec<serde_json::Value> = buttons
        .iter()
        .filter(|(_, d)| d.len() <= 64) // Telegram's callback_data limit
        .map(|(l, d)| serde_json::json!({ "text": l, "callback_data": d }))
        .collect();
    let markup = serde_json::json!({ "inline_keyboard": [row] }).to_string();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", c.token);
    ureq::post(&url)
        .send_form(&[("chat_id", c.chat.as_str()), ("text", text), ("reply_markup", markup.as_str())])
        .map_err(|e| anyhow::anyhow!("telegram send failed: {}", redact(&c.token, e.to_string())))?;
    Ok(())
}

/// Stop the button's loading spinner, with a short toast.
fn answer_button(c: &Cfg, id: &str, text: &str) {
    let url = format!("https://api.telegram.org/bot{}/answerCallbackQuery", c.token);
    let _ = ureq::post(&url).send_form(&[("callback_query_id", id), ("text", text)]);
}

/// One accepted update: a typed message, or a button press `(callback id, data)`.
#[derive(Debug, PartialEq)]
enum Inbound {
    Text(String),
    Button(String, String),
}

/// Long-poll for new messages from the configured chat. Returns the next offset
/// and each accepted message or button press.
fn get_updates(c: &Cfg, offset: i64) -> Result<(i64, Vec<Inbound>)> {
    let url = format!("https://api.telegram.org/bot{}/getUpdates", c.token);
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(20))
        .query("offset", &offset.to_string())
        .query("timeout", "3")
        .call()
        .map_err(|e| anyhow::anyhow!("getUpdates: {}", redact(&c.token, e.to_string())))?;
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
            // a button press: honored only from the configured chat, like messages
            if let Some(cb) = u.get("callback_query") {
                let cid = cb["message"]["chat"]["id"].as_i64();
                if let (Some(cid), Some(id), Some(data)) = (cid, cb["id"].as_str(), cb["data"].as_str()) {
                    if c.chat == cid.to_string() {
                        texts.push(Inbound::Button(id.to_string(), data.to_string()));
                    }
                }
                continue;
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
                    texts.push(Inbound::Text(text.to_string()));
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
    Peek(String),
    Diff(String),
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
    if let Some(rest) = t.strip_prefix("/peek ") {
        return Cmd::Peek(rest.trim().to_string());
    }
    if let Some(rest) = t.strip_prefix("/diff ") {
        return Cmd::Diff(rest.trim().to_string());
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

const HELP: &str = "wta bridge:\n/agents — list agents\n/use <task> — pick an agent to chat with\n/send <task> <text> — send to a specific agent\n/peek <task> — the last lines of its screen\n/diff <task> — its changes vs base\n<text> — send to the picked agent\n(you get pinged when an agent needs input or finishes, with Approve / Deny / Diff buttons)";

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

/// The agent state behind a session name, if it is one of ours.
fn state_for_session(session: &str) -> Option<status::AgentState> {
    status::read_all_states()
        .ok()?
        .into_iter()
        .find(|s| tmux::session_name(&s.repo, &s.task) == session)
}

/// The one live state for a task name (prefers a live session when names repeat).
fn state_for_task(task: &str) -> Option<status::AgentState> {
    let all: Vec<status::AgentState> =
        status::read_all_states().ok()?.into_iter().filter(|s| s.task == task).collect();
    all.iter()
        .find(|s| tmux::has_session(&tmux::session_name(&s.repo, &s.task)))
        .or(all.first())
        .cloned()
}

/// The last `n` non-blank lines of an agent's screen (its question, when it asks one).
fn screen_tail(session: &str, n: usize) -> String {
    let text = tmux::capture(session).unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end()).filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Keep a message under Telegram's 4096-char limit (cut on a char boundary).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n… (cut)");
    out
}

fn peek(st: &status::AgentState) -> String {
    let session = tmux::session_name(&st.repo, &st.task);
    if !tmux::has_session(&session) {
        return format!("'{}' isn't running", st.task);
    }
    clip(&format!("👀 {}:\n{}", st.task, screen_tail(&session, 30)), 3900)
}

/// `git diff --stat` plus the start of the full diff vs the agent's base branch.
fn diff(st: &status::AgentState) -> String {
    let dir = std::path::PathBuf::from(&st.cwd);
    if st.cwd.is_empty() || !dir.exists() {
        return format!("'{}' has no worktree", st.task);
    }
    let base = st.base.clone().unwrap_or_else(|| "HEAD".into());
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
            .unwrap_or_default()
    };
    let mb = git(&["merge-base", "HEAD", &base]);
    let from = if mb.is_empty() { base.clone() } else { mb };
    let stat = git(&["diff", "--stat", &from]);
    if stat.is_empty() {
        return format!("📄 {}: no changes vs {base}", st.task);
    }
    let body = git(&["diff", &from]);
    clip(&format!("📄 {} vs {base}\n{stat}\n\n{body}", st.task), 3900)
}

/// Handle a button press. Returns (toast, optional message to post).
fn press(data: &str) -> (String, Option<String>) {
    let Some((action, session)) = data.split_once('|') else {
        return ("unknown button".into(), None);
    };
    let Some(st) = state_for_session(session) else {
        return ("that agent is gone".into(), None);
    };
    let task = st.task.clone();
    match action {
        "ok" | "no" => {
            if !tmux::has_session(session) {
                return (format!("'{task}' isn't running"), None);
            }
            // only answer a dialog that is really on screen — never type into a live turn
            let pane = tmux::capture(session).unwrap_or_default();
            if !tmux::looks_interactive_dialog(&pane) {
                return ("no prompt on screen now".into(), Some(peek(&st)));
            }
            let (key, word) = if action == "ok" { ("Enter", "approved") } else { ("Escape", "denied") };
            match tmux::send_key(session, key) {
                Ok(_) => (format!("{word} → {task}"), Some(format!("{} {word} → {task}", if action == "ok" { "✅" } else { "❌" }))),
                Err(e) => (format!("failed: {e}"), None),
            }
        }
        "diff" => ("diff".into(), Some(diff(&st))),
        "peek" => ("screen".into(), Some(peek(&st))),
        _ => ("unknown button".into(), None),
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
        Cmd::Peek(task) => state_for_task(&task).map(|s| peek(&s)).unwrap_or_else(|| format!("'{task}' not found")),
        Cmd::Diff(task) => state_for_task(&task).map(|s| diff(&s)).unwrap_or_else(|| format!("'{task}' not found")),
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
                    match t {
                        Inbound::Text(t) => {
                            let reply = dispatch(parse(&t), &mut selected);
                            let _ = send(&c, &reply);
                        }
                        Inbound::Button(id, data) => {
                            let (toast, msg) = press(&data);
                            answer_button(&c, &id, &toast);
                            if let Some(m) = msg {
                                let _ = send(&c, &m);
                            }
                        }
                    }
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
                        let session = tmux::session_name(&st.repo, &st.task);
                        let tail = screen_tail(&session, 12);
                        let text = if tail.is_empty() { msg } else { clip(&format!("{msg}\n\n{tail}"), 3900) };
                        let mut buttons: Vec<(&str, String)> = Vec::new();
                        if st.status == "needs_input" {
                            buttons.push(("✅ Approve", format!("ok|{session}")));
                            buttons.push(("❌ Deny", format!("no|{session}")));
                        }
                        buttons.push(("📄 Diff", format!("diff|{session}")));
                        buttons.push(("👀 Peek", format!("peek|{session}")));
                        if send_buttons(&c, &text, &buttons).is_err() {
                            let _ = send(&c, &text);
                        }
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
    fn clip_keeps_messages_short() {
        assert_eq!(clip("abc", 10), "abc");
        let c = clip(&"x".repeat(50), 10);
        assert!(c.starts_with("xxxxxxxxxx") && c.ends_with("(cut)"));
    }

    #[test]
    fn unknown_buttons_are_refused() {
        assert_eq!(press("garbage").0, "unknown button");
        assert_eq!(press("ok|wta-nope-nothing").0, "that agent is gone");
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
        assert_eq!(parse("/peek foo"), Cmd::Peek("foo".into()));
        assert_eq!(parse("/diff foo"), Cmd::Diff("foo".into()));
        assert_eq!(parse("/whatever"), Cmd::Help);
    }
}
