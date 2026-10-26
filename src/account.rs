//! `wta account` — run agents on more than one Claude account.
//!
//! Each account is a Claude Code config dir under `~/.wta/accounts/<name>/`. An agent
//! on an account gets `CLAUDE_CONFIG_DIR=<that dir>` in its pane, so its login, usage
//! limit and transcripts are that account's. "default" means your normal `~/.claude`
//! (or `$CLAUDE_CONFIG_DIR`). Switching an agent to another account copies its
//! conversation over first, so `claude --continue` picks up where it stopped.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::{cost, status, tmux, worktree};

fn accounts_root() -> Option<PathBuf> {
    status::wta_dir().ok().map(|d| d.join("accounts"))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "default"
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every account wta knows, sorted by name: `(name, config dir)`.
pub fn list() -> Vec<(String, PathBuf)> {
    let Some(root) = accounts_root() else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(&root)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(|n| (n.to_string(), e.path())))
                .filter(|(n, _)| valid_name(n))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Config dir for an account name; `None` for "default" or an unknown name.
pub fn dir_for(name: &str) -> Option<PathBuf> {
    if name == "default" {
        return None;
    }
    list().into_iter().find(|(n, _)| n == name).map(|(_, d)| d)
}

/// The default config dir (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
fn default_dir() -> Option<PathBuf> {
    cost::config_dirs().into_iter().next()
}

/// The config dir an account name resolves to, "default" included.
fn resolve_dir(name: &str) -> Result<PathBuf> {
    if name == "default" {
        return default_dir().context("cannot locate ~/.claude");
    }
    dir_for(name).with_context(|| format!("no account '{name}' — add it with `wta account add {name}`"))
}

/// Names to cycle through for one agent: "default", then every account.
pub fn cycle() -> Vec<String> {
    std::iter::once("default".to_string()).chain(list().into_iter().map(|(n, _)| n)).collect()
}

/// `wta account add <name>` — create the config dir, copy your Claude settings in (so
/// wta's status hooks work there too), then start `claude` on it so you can `/login`.
pub fn add(name: &str, login: bool) -> Result<()> {
    if !valid_name(name) {
        bail!("account names use letters, digits, - and _ (and 'default' is reserved)");
    }
    let dir = accounts_root().context("cannot locate ~/.wta")?.join(name);
    std::fs::create_dir_all(&dir)?;
    // settings.json carries the hooks (`wta status …`) that report needs-input/finished;
    // without it, agents on this account fall back to screen detection
    if let Some(src) = default_dir().map(|d| d.join("settings.json")) {
        let dst = dir.join("settings.json");
        if src.exists() && !dst.exists() {
            std::fs::copy(&src, &dst).ok();
            println!("copied your Claude settings (hooks) into the account");
        }
    }
    println!("account '{name}' → {}", dir.display());
    if !login {
        println!("log in later with:  CLAUDE_CONFIG_DIR={} claude   (then /login)", dir.display());
        return Ok(());
    }
    println!("starting claude on this account — run /login, then /exit when done");
    let status = std::process::Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", &dir)
        .status()
        .context("could not start `claude` (is it on your PATH?)")?;
    if !status.success() {
        eprintln!("claude exited with {status}");
    }
    Ok(())
}

/// `wta account ls` — accounts, the agents on each, and tokens used in the last 5h / 7d.
pub fn ls() -> Result<()> {
    let states = status::read_all_states().unwrap_or_default();
    let now = cost::now_unix();
    let mut rows: Vec<(String, PathBuf)> = Vec::new();
    if let Some(d) = default_dir() {
        rows.push(("default".into(), d));
    }
    rows.extend(list());
    println!("{:<12} {:>8} {:>8}  AGENTS", "ACCOUNT", "5h", "7d");
    for (name, dir) in rows {
        let mut agents: Vec<&str> = states
            .iter()
            .filter(|s| s.account.as_deref().unwrap_or("default") == name)
            .map(|s| s.task.as_str())
            .collect();
        agents.sort();
        agents.dedup();
        let shown = match agents.len() {
            0 => "-".to_string(),
            n if n <= 4 => agents.join(", "),
            n => format!("{} … ({n})", agents[..4].join(", ")),
        };
        let h5 = cost::usage_since(&dir, now.saturating_sub(5 * 3600));
        let d7 = cost::usage_since(&dir, now.saturating_sub(7 * 86400));
        println!("{name:<12} {:>8} {:>8}  {shown}", cost::human_tokens(h5), cost::human_tokens(d7));
    }
    println!("\ntokens = input + output, from transcripts (an estimate of what the limit counts)");
    Ok(())
}

/// Copy an agent's transcripts from one config dir to another so `--continue` works on
/// the new account. Never deletes the source. Returns how many files were copied.
fn carry_conversation(wt: &Path, from: &Path, to: &Path) -> usize {
    let src = cost::transcript_dir_in(from, wt);
    let dst = cost::transcript_dir_in(to, wt);
    let Ok(rd) = std::fs::read_dir(&src) else { return 0 };
    if std::fs::create_dir_all(&dst).is_err() {
        return 0;
    }
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            if let Some(f) = p.file_name() {
                if std::fs::copy(&p, dst.join(f)).is_ok() {
                    // keep the mtime: `--continue` resumes the most recent conversation
                    if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                        if let Ok(out) = std::fs::File::options().write(true).open(dst.join(f)) {
                            let _ = out.set_modified(t);
                        }
                    }
                    n += 1;
                }
            }
        }
    }
    n
}

/// Move one agent to account `name` ("default" = your normal Claude). Copies its
/// conversation over, records the account, and — if the agent is running — restarts it
/// with `--continue` on the new account. Returns a one-line summary.
pub fn switch(repo: &str, task: &str, wt: &Path, name: &str) -> Result<String> {
    let to = resolve_dir(name)?;
    let cur = status::read_state(repo, task).and_then(|s| s.account).unwrap_or_else(|| "default".into());
    if cur == name {
        return Ok(format!("'{task}' is already on account '{name}'"));
    }
    let from = resolve_dir(&cur).ok();
    // the conversation lives where the agent really ran (an adopted agent's own dir)
    let session = tmux::session_name(repo, task);
    let live = tmux::has_session(&session);
    let run_dir = tmux::pane_path(&session).map(PathBuf::from).unwrap_or_else(|| wt.to_path_buf());
    let copied = from.as_deref().map(|f| carry_conversation(&run_dir, f, &to)).unwrap_or(0);
    status::update(repo, task, |s| s.account = (name != "default").then(|| name.to_string()))?;
    if live {
        tmux::kill(&session)?;
        worktree::resume_session(repo, task, &run_dir, copied == 0)?;
        let how = if copied > 0 { "conversation continued" } else { "fresh conversation (nothing to carry)" };
        Ok(format!("'{task}' → account '{name}' — restarted, {how}"))
    } else {
        Ok(format!("'{task}' → account '{name}' (applies when it is resumed)"))
    }
}

/// `wta account use <task> <name>`.
pub fn use_cmd(task: &str, name: &str) -> Result<()> {
    let (repo, _, wt) = worktree::locate(task)?;
    println!("{}", switch(&repo, task, &wt, name)?);
    Ok(())
}

/// Does the pane say the account ran out of usage? (the cue to switch accounts)
pub fn is_limit_hit(pane: &str) -> bool {
    let t = pane.to_lowercase();
    ["usage limit reached", "you've hit your limit", "you have hit your limit", "limit will reset", "quota exceeded"]
        .iter()
        .any(|p| t.contains(p))
}

/// The best account to fail over to from `cur`: the one (not `cur`) with the fewest
/// tokens used in the last 5h. `None` when there is no other account.
pub fn failover_target(cur: &str) -> Option<String> {
    let since = cost::now_unix().saturating_sub(5 * 3600);
    let mut best: Option<(u64, String)> = None;
    for name in cycle() {
        if name == cur {
            continue;
        }
        let Ok(dir) = resolve_dir(&name) else { continue };
        let used = cost::usage_since(&dir, since);
        if best.as_ref().map(|(u, _)| used < *u).unwrap_or(true) {
            best = Some((used, name));
        }
    }
    best.map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        assert!(valid_name("work"));
        assert!(valid_name("team_2"));
        assert!(!valid_name("default"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(""));
    }

    #[test]
    fn limit_messages_are_detected() {
        assert!(is_limit_hit("Claude usage limit reached. Your limit will reset at 5pm"));
        assert!(is_limit_hit("You've hit your limit · resets 3am"));
        assert!(!is_limit_hit("Rate limit, retrying in 3s"));
        assert!(!is_limit_hit("all tests passed"));
    }
}
