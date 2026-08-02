//! `wta guard` — an opt-in, per-agent SAFETY seatbelt for unattended fleets.
//!
//! wta isolates FILES (worktree per agent) but not the machine: a fleet running with
//! `--dangerously-skip-permissions` can still force-push, `rm -rf` your home, or blow
//! away ignored files. Guard installs a Claude Code **PreToolUse** hook into each new
//! worktree that runs every `Bash` command past a deny list and BLOCKS (exit 2) the
//! clearly-destructive ones before they run — deterministic per-command policy that
//! escalates to a human, NOT an inter-agent lock and NOT an LLM deciding to kill.
//!
//! It is a SEATBELT, not a sandbox: string-matching a shell command is best-effort and
//! a determined/obfuscated agent can evade it. The real isolation is still the worktree.
//! Off by default; enable per repo with `wta guard on`. Extend with executable
//! `~/.wta/guard.d/*.sh` (each gets the command as `$1`; a non-zero exit blocks).

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// The built-in deny rules. Returns `Some(reason)` to BLOCK the command, else `None`.
/// Deliberately narrow + high-signal so it catches accidents without tripping normal
/// agent work; users add their own via `~/.wta/guard.d/`.
pub fn guard_evaluate(command: &str) -> Option<String> {
    let n = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let toks: Vec<&str> = n.split_whitespace().collect();
    let has = |flag: &str| toks.iter().any(|t| *t == flag);

    // 1) force-push — agents should converge via `wta push`/`wta land`, never force-push.
    //    (--force-with-lease is the safe variant and is allowed.)
    if n.contains("git push")
        && (has("--force") || has("-f") || n.contains("+refs/") || n.contains(" +"))
        && !n.contains("--force-with-lease")
    {
        return Some(
            "force-push blocked — use `wta push` / `wta land` (or --force-with-lease)".into(),
        );
    }

    // 2) git reset --hard / git clean -f + -x — discards work or nukes ignored files (.env!).
    if n.contains("git reset") && has("--hard") {
        return Some(
            "`git reset --hard` blocked — it discards uncommitted work; commit or stash first"
                .into(),
        );
    }
    if n.contains("git clean")
        && (has_short_flag(&toks, 'f') || has("--force"))
        && (has_short_flag(&toks, 'x') || has_short_flag(&toks, 'X') || has("-X"))
    {
        return Some(
            "`git clean -fdx` blocked — it deletes ignored files (incl. .env/secrets)".into(),
        );
    }

    // 3) rm -rf targeting something OUTSIDE the worktree (home, root, parent, wildcard).
    if toks.first() == Some(&"rm") && recursive_force(&toks) {
        if let Some(t) = dangerous_rm_target(&toks) {
            return Some(format!(
                "`rm -rf {t}` blocked — refusing a recursive delete outside the worktree"
            ));
        }
    }

    None
}

/// A short flag `ch` is set if any `-…` cluster (not `--long`) contains it (`-rf` ⊇ r,f).
fn has_short_flag(toks: &[&str], ch: char) -> bool {
    toks.iter()
        .any(|t| t.starts_with('-') && !t.starts_with("--") && t.contains(ch))
}

/// `rm` recursive-force combo: `-rf`/`-fr`/`-r -f` or the `--recursive --force` long forms.
fn recursive_force(toks: &[&str]) -> bool {
    let r = has_short_flag(toks, 'r') || has_short_flag(toks, 'R') || toks.contains(&"--recursive");
    let f = has_short_flag(toks, 'f') || toks.contains(&"--force");
    r && f
}

/// The first `rm` argument that points outside the worktree, if any.
fn dangerous_rm_target(toks: &[&str]) -> Option<String> {
    for t in toks.iter().skip(1) {
        if t.starts_with('-') {
            continue; // a flag
        }
        let bad = *t == "/"
            || *t == "~"
            || *t == "*"
            || *t == "."
            || t.starts_with('/')
            || t.starts_with("~")
            || t.starts_with("$HOME")
            || t.starts_with("${HOME}")
            || t.starts_with("..")
            || t.contains("/../")
            || t.starts_with("../");
        if bad {
            return Some((*t).to_string());
        }
    }
    None
}

fn guard_d_dir() -> Option<std::path::PathBuf> {
    crate::status::wta_dir().ok().map(|d| d.join("guard.d"))
}

/// Run every executable `~/.wta/guard.d/*.sh` with the command as `$1`; a non-zero exit
/// blocks (its stderr is surfaced). Lets users add project/company rules without a rebuild.
fn run_guard_d(command: &str) -> Option<String> {
    let dir = guard_d_dir()?;
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.extension().and_then(|s| s.to_str()) != Some("sh") {
            continue;
        }
        let out = std::process::Command::new("bash")
            .arg(&p)
            .arg(command)
            .output();
        if let Ok(o) = out {
            if !o.status.success() {
                let msg = String::from_utf8_lossy(&o.stderr);
                let reason = msg.trim();
                return Some(if reason.is_empty() {
                    format!("blocked by {}", p.display())
                } else {
                    reason.to_string()
                });
            }
        }
    }
    None
}

/// `wta guard-check` — the PreToolUse hook target. Reads Claude's tool-call JSON from
/// stdin; if it's a Bash command that trips a rule, prints the reason to stderr and
/// exits 2 (Claude blocks the call and shows the reason). Otherwise exits 0 (allow).
/// Fails OPEN on unparseable input — a broken guard must never wedge the whole fleet.
pub fn run_check() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok();
    let v: serde_json::Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()), // fail-open on malformed input
    };
    if v.get("tool_name").and_then(|t| t.as_str()) != Some("Bash") {
        return Ok(());
    }
    let cmd = v
        .get("tool_input")
        .and_then(|i| i.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if cmd.is_empty() {
        return Ok(());
    }
    if let Some(reason) = guard_evaluate(cmd).or_else(|| run_guard_d(cmd)) {
        eprintln!("wta guard: {reason}");
        std::process::exit(2);
    }
    Ok(())
}

fn marker(root: &Path) -> std::path::PathBuf {
    root.join(".wta/guard.enabled")
}

/// Guard is active for this repo's new worktrees when the marker exists (or WTA_GUARD=1).
pub fn is_enabled(root: &Path) -> bool {
    std::env::var("WTA_GUARD")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
        || marker(root).exists()
}

/// Seed the guard PreToolUse hook into a fresh worktree's `.claude/settings.local.json`
/// (local, so it never dirties the tracked repo). Returns the rel path when written so
/// the caller git-excludes it like other injected files. No-op when guard is off.
pub fn seed_worktree(root: &Path, wt: &Path) -> Option<String> {
    if !is_enabled(root) {
        return None;
    }
    let self_exe = std::env::current_exe().ok()?;
    let cmd = format!("{} guard-check", self_exe.to_string_lossy());
    let path = wt.join(".claude/settings.local.json");
    if write_pretooluse(&path, &cmd).is_ok() {
        Some(".claude/settings.local.json".to_string())
    } else {
        None
    }
}

/// Merge a PreToolUse Bash hook into a Claude settings file, fail-closed (never clobber
/// an existing file we can't parse), idempotent (don't double-add our command).
fn write_pretooluse(path: &Path, command: &str) -> Result<()> {
    let mut root: serde_json::Value = if path.exists() {
        let bytes = std::fs::read(path)?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} isn't valid JSON — refusing to overwrite",
                    path.display()
                )
            })?
        }
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        anyhow::bail!("{} is not a JSON object", path.display());
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }
    let arr = hooks
        .as_object_mut()
        .unwrap()
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    if !arr.is_array() {
        *arr = serde_json::json!([]);
    }
    let list = arr.as_array_mut().unwrap();
    let already = list.iter().any(|g| {
        g.get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
            })
            .unwrap_or(false)
    });
    if !already {
        list.push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": command }],
        }));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::status::atomic_write(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

// ---- CLI ----

pub fn on(root: &Path) -> Result<()> {
    let m = marker(root);
    if let Some(p) = m.parent() {
        std::fs::create_dir_all(p).ok();
    }
    std::fs::write(&m, "on\n")?;
    println!("guard ON — new agents block force-push, rm -rf outside the worktree, reset --hard, clean -fdx.");
    println!("(existing agents are unaffected; recreate them to pick it up.)");
    Ok(())
}

pub fn off(root: &Path) -> Result<()> {
    let _ = std::fs::remove_file(marker(root));
    println!("guard OFF — new agents run unguarded.");
    Ok(())
}

pub fn status(root: &Path) -> Result<()> {
    println!("guard: {}", if is_enabled(root) { "ON" } else { "off" });
    println!("built-in blocks:");
    println!("  • git push --force (allows --force-with-lease)");
    println!("  • git reset --hard");
    println!("  • git clean -f with -x/-X");
    println!("  • rm -rf targeting / ~ .. $HOME or a wildcard");
    if let Some(d) = guard_d_dir() {
        let extra = std::fs::read_dir(&d)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("sh"))
                    .count()
            })
            .unwrap_or(0);
        println!("custom rules: {extra} in {}", d.display());
    }
    Ok(())
}

/// `wta guard test '<cmd>'` — dry-run a command through the rules without an agent.
pub fn test(command: &str) -> Result<()> {
    match guard_evaluate(command).or_else(|| run_guard_d(command)) {
        Some(reason) => {
            println!("BLOCK: {reason}");
            std::process::exit(2);
        }
        None => {
            println!("allow");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_force_push_but_allows_lease() {
        assert!(guard_evaluate("git push --force origin main").is_some());
        assert!(guard_evaluate("git push -f").is_some());
        assert!(guard_evaluate("git push --force-with-lease origin feat").is_none());
        assert!(guard_evaluate("git push origin main").is_none());
    }

    #[test]
    fn blocks_reset_hard_and_clean_fdx() {
        assert!(guard_evaluate("git reset --hard HEAD~1").is_some());
        assert!(guard_evaluate("git reset --soft HEAD~1").is_none());
        assert!(guard_evaluate("git clean -fdx").is_some());
        assert!(guard_evaluate("git clean -fd").is_none()); // no -x → keeps ignored files
    }

    #[test]
    fn blocks_dangerous_rm_only() {
        assert!(guard_evaluate("rm -rf /").is_some());
        assert!(guard_evaluate("rm -rf ~").is_some());
        assert!(guard_evaluate("rm -rf $HOME/.config").is_some());
        assert!(guard_evaluate("rm -rf ../other").is_some());
        assert!(guard_evaluate("rm -fr /etc").is_some());
        // legitimate in-worktree cleanup is allowed
        assert!(guard_evaluate("rm -rf target").is_none());
        assert!(guard_evaluate("rm -rf build/tmp").is_none());
        assert!(guard_evaluate("rm file.txt").is_none());
    }

    #[test]
    fn allows_ordinary_commands() {
        for ok in [
            "cargo test",
            "ls -la",
            "git commit -m x",
            "git push origin feat",
            "npm run build",
        ] {
            assert!(guard_evaluate(ok).is_none(), "{ok} should be allowed");
        }
    }
}
