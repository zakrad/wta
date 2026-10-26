//! Organizing and finding agents from the CLI: `wta tag`, `wta pin`, `wta compare`,
//! `wta search`, `wta usage`. The dashboard has the same features on keys
//! (`T`, `P`, `=`, `/`); these are for scripts and for agents themselves.

use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;

use crate::{cost, status, worktree};

/// `wta tag <task> [+x | -x | x ...]` — no edits prints the tags.
pub fn tag(task: &str, edits: &[String]) -> Result<()> {
    let (repo, _, _) = worktree::locate(task)?;
    let cur = status::read_state(&repo, task).map(|s| s.tags).unwrap_or_default();
    if edits.is_empty() {
        println!("{}", if cur.is_empty() { "(no tags)".to_string() } else { format!("#{}", cur.join(" #")) });
        return Ok(());
    }
    let tags = status::edit_tags(&cur, edits);
    status::update(&repo, task, |s| s.tags = tags.clone())?;
    println!("{task}: {}", if tags.is_empty() { "(no tags)".to_string() } else { format!("#{}", tags.join(" #")) });
    Ok(())
}

/// `wta pin <task> [--off]`.
pub fn pin(task: &str, off: bool) -> Result<()> {
    let (repo, _, _) = worktree::locate(task)?;
    status::update(&repo, task, |s| s.pinned = !off)?;
    println!("{} '{task}'", if off { "unpinned" } else { "pinned" });
    Ok(())
}

struct Member {
    task: String,
    path: PathBuf,
    added: u64,
    deleted: u64,
    ac: Option<(u32, u32)>,
    tokens: u64,
    usd: f64,
    check: Option<i32>,
}

/// `wta compare <group> [--verify] [--keep <task>] [--json]` — rank a fanout group (the
/// agents whose parent is `<group>`): checks, acceptance criteria, diff size, tokens.
pub fn compare(group: &str, verify: bool, keep: Option<&str>, json: bool) -> Result<()> {
    let root = worktree::repo_root()?;
    let repo = worktree::repo_id_of(&root);
    let mut members: Vec<Member> = status::read_states(&repo)?
        .into_iter()
        .filter(|s| s.parent.as_deref() == Some(group))
        .map(|s| {
            let path = if s.cwd.is_empty() { worktree::worktrees_dir(&root).join(&s.task) } else { PathBuf::from(&s.cwd) };
            let base = s.base.clone().unwrap_or_else(|| worktree::base_branch(&root));
            let (added, deleted) = worktree::changes_vs_base(&path, &base);
            let ac = worktree::read_task_spec(&path).map(|t| (t.checked_ac, t.total_ac));
            let (u, _) = cost::for_worktree(&path);
            Member { task: s.task, path, added, deleted, ac, tokens: u.tokens(), usd: u.est_usd, check: None }
        })
        .collect();
    if members.is_empty() {
        bail!("no agents in group '{group}' here (start one with `wta fanout {group} -n 3 -- \"<prompt>\"`)");
    }
    members.sort_by(|a, b| a.task.cmp(&b.task));
    if verify {
        match worktree::verify_suite_script(&root) {
            Some(script) => {
                for m in members.iter_mut() {
                    eprintln!("verify: {} …", m.task);
                    m.check = Some(worktree::run_verify_sh(&m.path, &script, Some(std::time::Duration::from_secs(600))).0);
                }
            }
            None => eprintln!("no .wta/verify.sh — ranking without checks"),
        }
    }
    let score = |m: &Member| {
        let c = match m.check {
            Some(0) => 2u8,
            None => 1,
            Some(_) => 0,
        };
        let ac = m.ac.and_then(|(d, t)| (d * 100).checked_div(t)).unwrap_or(0);
        (c, ac, -((m.added + m.deleted) as i64), -(m.tokens as i64))
    };
    let best = members.iter().max_by_key(|m| score(m)).map(|m| m.task.clone());

    if let Some(keep) = keep {
        if !members.iter().any(|m| m.task == keep) {
            bail!("'{keep}' is not in group '{group}'");
        }
        for m in members.iter().filter(|m| m.task != keep) {
            match worktree::rm_in(&repo, &root, &m.task, false) {
                Ok(_) => println!("removed {}", m.task),
                Err(e) => println!("kept {} — {e}", m.task),
            }
        }
        println!("kept '{keep}' — push it with `wta push {keep} --pr`");
        return Ok(());
    }

    if json {
        let rows: Vec<Value> = members
            .iter()
            .map(|m| {
                serde_json::json!({
                    "task": m.task, "added": m.added, "deleted": m.deleted,
                    "ac": m.ac.map(|(c, t)| serde_json::json!({"checked": c, "total": t})),
                    "tokens": m.tokens, "est_usd": m.usd, "check": m.check,
                    "best": Some(&m.task) == best.as_ref(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("  {:<24} {:<7} {:<6} {:>14} {:>8} {:>8}", "AGENT", "CHECK", "AC", "CHANGES", "TOKENS", "~$");
    for m in &members {
        let mark = if Some(&m.task) == best.as_ref() && members.len() > 1 { "★" } else { " " };
        let check = match m.check {
            Some(0) => "pass".to_string(),
            Some(c) => format!("fail {c}"),
            None => "-".to_string(),
        };
        let ac = m.ac.map(|(c, t)| format!("{c}/{t}")).unwrap_or_else(|| "-".into());
        println!(
            "{mark} {:<24} {:<7} {:<6} {:>14} {:>8} {:>8.2}",
            m.task,
            check,
            ac,
            format!("+{} -{}", m.added, m.deleted),
            cost::human_tokens(m.tokens),
            m.usd
        );
    }
    println!("\n★ = checks pass › acceptance criteria met › smaller diff › fewer tokens");
    if !verify {
        println!("  add --verify to run .wta/verify.sh on each");
    }
    if let Some(b) = best {
        println!("  keep it:  wta compare {group} --keep {b}");
    }
    Ok(())
}

/// The plain text of one transcript line's message, or `None` if it has none.
fn message_text(v: &Value) -> Option<(String, String)> {
    let role = v.get("type")?.as_str()?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = crate::copyview::strip_system_reminders(&text);
    (!text.trim().is_empty()).then(|| (role.to_string(), text))
}

/// A short one-line excerpt of `text` around the first match of `needle` (lowercased).
fn snippet(text: &str, needle: &str, width: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let at = lower.find(needle).unwrap_or(0);
    let chars: Vec<char> = flat.chars().collect();
    // map the byte offset to a char offset
    let at_c = flat[..at.min(flat.len())].chars().count();
    let start = at_c.saturating_sub(width / 3);
    let end = (start + width).min(chars.len());
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// `wta search <text>` — find text in every agent's conversation (all repos by
/// default). Newest hits first. Matches are case-insensitive.
pub fn search(query: &str, here: bool, limit: usize, json: bool) -> Result<()> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        bail!("give some text to search for");
    }
    let repos: HashMap<String, PathBuf> = worktree::discover_repos().into_iter().collect();
    let here_repo = if here { Some(worktree::repo_id()?) } else { None };
    let mut seen_dirs = HashSet::new();
    // (ts, repo name, task, role, snippet)
    let mut hits: Vec<(String, String, String, String, String)> = Vec::new();
    for st in status::read_all_states().unwrap_or_default() {
        if here_repo.as_deref().map(|r| r != st.repo).unwrap_or(false) || st.cwd.is_empty() {
            continue;
        }
        let Some(dir) = cost::transcript_dir(std::path::Path::new(&st.cwd)) else { continue };
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        let repo_name = repos
            .get(&st.repo)
            .cloned()
            .or_else(|| worktree::main_root_of(std::path::Path::new(&st.cwd)))
            .map(|r| worktree::repo_name(&r))
            .unwrap_or_else(|| st.repo.clone());
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for f in rd.flatten() {
            let p = f.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(file) = std::fs::File::open(&p) else { continue };
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                if !line.to_lowercase().contains(&needle) {
                    continue; // cheap pre-filter before JSON parsing
                }
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let Some((role, text)) = message_text(&v) else { continue };
                if !text.to_lowercase().contains(&needle) {
                    continue;
                }
                let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
                hits.push((ts, repo_name.clone(), st.task.clone(), role, snippet(&text, &needle, 90)));
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    let total = hits.len();
    hits.truncate(limit.max(1));
    if json {
        let rows: Vec<Value> = hits
            .iter()
            .map(|(ts, repo, task, role, snip)| serde_json::json!({"ts": ts, "repo": repo, "task": task, "role": role, "text": snip}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("no matches for '{query}'");
        return Ok(());
    }
    for (ts, repo, task, role, snip) in &hits {
        let when = ts.get(5..16).map(|s| s.replace('T', " ")).unwrap_or_default();
        let who = if role == "user" { "you" } else { "agent" };
        println!("{when}  {repo} › {task}  {who}: {snip}");
    }
    if total > hits.len() {
        println!("… {} more (use -n to show more)", total - hits.len());
    }
    println!("open one:  wta copy <task>   (then / to search inside it)");
    Ok(())
}

/// `wta usage` — tokens used in the last 5 hours and 7 days per Claude account (all
/// sessions on the account, not only wta agents). Same data as `wta account ls`.
pub fn usage() -> Result<()> {
    crate::account::ls()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_reads_strings_and_text_blocks() {
        let u: Value = serde_json::from_str(r#"{"type":"user","message":{"content":"fix the <system-reminder>x</system-reminder>login bug"}}"#).unwrap();
        assert_eq!(message_text(&u), Some(("user".into(), "fix the login bug".into())));
        let a: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"},{"type":"text","text":"Done."}]}}"#,
        )
        .unwrap();
        assert_eq!(message_text(&a), Some(("assistant".into(), "Done.".into())));
        let s: Value = serde_json::from_str(r#"{"type":"summary","summary":"x"}"#).unwrap();
        assert_eq!(message_text(&s), None);
    }

    #[test]
    fn snippet_centers_on_the_match() {
        let t = "a ".repeat(100) + "NEEDLE here" + &" b".repeat(100);
        let s = snippet(&t, "needle", 30);
        assert!(s.contains("NEEDLE"));
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(snippet("short needle", "needle", 30), "short needle");
    }
}
