//! `wta supervise` — a foreground fleet watcher. **Escalate-only**: it classifies
//! every agent and alerts you (sound + toast + a printed line) when one goes
//! `needs-input`, looks stuck (idle with no new changes for a while), or crashes with
//! uncommitted work. By default it is strictly READ-ONLY — it never sends, kills, or
//! changes an agent. The one opt-in exception is `--failover`: an agent that hits its
//! Claude usage limit is moved to the account with the most room left and told to go on.

use crate::{notify, status, tmux, worktree};
use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Per-agent watch state carried across ticks.
#[derive(Default)]
struct Watch {
    prev_pane: Option<u64>,
    idle_ticks: u32, // consecutive ticks the pane was unchanged
    prev_diff: Option<u64>,
    nodiff_ticks: u32, // consecutive ticks the worktree diff was unchanged
    escalated: HashSet<&'static str>, // conditions already alerted this episode
}

fn hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn human(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

/// Does the worktree have uncommitted changes? (Used to flag a crashed agent whose
/// work would be lost.)
fn has_uncommitted(wt: &Path) -> bool {
    std::process::Command::new("git")
        .args(["-C", &wt.to_string_lossy(), "status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.iter().all(|b| b.is_ascii_whitespace()))
        .unwrap_or(false)
}

pub fn supervise(global: bool, interval: u64, stuck_secs: u64, failover: bool) -> Result<()> {
    let interval = interval.max(3);
    println!(
        "wta supervise: watching {} every {interval}s — {}, Ctrl-C to stop",
        if global { "all repos" } else { "this repo" },
        if failover { "escalate + account failover" } else { "escalate-only (read-only)" }
    );
    let mut watches: HashMap<String, Watch> = HashMap::new();
    loop {
        let targets: Vec<(String, PathBuf)> = if global {
            worktree::discover_repos()
        } else {
            match worktree::repo_root() {
                Ok(r) => vec![(worktree::repo_id_of(&r), r)],
                Err(_) => Vec::new(),
            }
        };

        let mut rows: Vec<(String, String, &'static str, String)> = Vec::new(); // repo, task, glyph, note
        let mut live: HashSet<String> = HashSet::new();

        for (repo, root) in &targets {
            let repo_name = worktree::repo_name(root);
            for a in worktree::list_managed_in(root).unwrap_or_default() {
                let session = tmux::session_name(repo, &a.task);
                live.insert(session.clone());
                if failover && tmux::has_session(&session) {
                    if let Some(msg) = try_failover(repo, &a.task, &a.path, &session) {
                        notify::alert(&format!("wta ▸ {repo_name}"), &msg);
                        println!("  ⇄ {msg}");
                    }
                }
                let w = watches.entry(session.clone()).or_default();
                let (glyph, note, escalation) = classify(repo, &a.task, &a.path, &session, w, interval, stuck_secs);
                if let Some((cond, msg)) = escalation {
                    if w.escalated.insert(cond) {
                        notify::alert(&format!("wta ▸ {repo_name}"), &msg);
                        println!("  🔔 {msg}");
                    }
                }
                rows.push((repo_name.clone(), a.task.clone(), glyph, note));
            }
        }
        watches.retain(|k, _| live.contains(k));
        print_table(&rows);
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// `--failover`: if the agent's pane shows a usage-limit stop, move it to the account
/// with the most room left (copying its conversation) and tell it to continue.
fn try_failover(repo: &str, task: &str, wt: &Path, session: &str) -> Option<String> {
    let pane = tmux::capture(session)?;
    if !crate::account::is_limit_hit(&pane) {
        return None;
    }
    let cur = status::read_state(repo, task).and_then(|s| s.account).unwrap_or_else(|| "default".into());
    let Some(next) = crate::account::failover_target(&cur) else {
        return Some(format!("'{task}' hit its usage limit on '{cur}' — no other account to move to (`wta account add`)"));
    };
    match crate::account::switch(repo, task, wt, &next) {
        Ok(summary) => {
            // give the restarted agent a moment to load, then nudge it on
            std::thread::sleep(Duration::from_secs(4));
            let _ = tmux::send_text(session, "You were moved to another account after a usage limit. Continue where you stopped.");
            Some(format!("{summary} (usage limit on '{cur}')"))
        }
        Err(e) => Some(format!("'{task}' hit its usage limit; failover to '{next}' failed: {e}")),
    }
}

/// Classify one agent, updating its watch state. Returns (glyph, note, optional
/// escalation `(condition, message)`).
fn classify(
    repo: &str,
    task: &str,
    wt: &Path,
    session: &str,
    w: &mut Watch,
    interval: u64,
    stuck_secs: u64,
) -> (&'static str, String, Option<(&'static str, String)>) {
    let state = status::read_state(repo, task).map(|s| s.status).unwrap_or_default();

    if !tmux::has_session(session) {
        w.idle_ticks = 0;
        w.prev_pane = None;
        if has_uncommitted(wt) {
            return ("✗", "exited (unsaved!)".into(), Some(("zombie", format!("'{task}' exited with uncommitted work — resume or push it before it's lost"))));
        }
        return ("·", "exited".into(), None);
    }

    // A hook-reported question is the clearest "needs you" signal.
    if state == "needs_input" {
        return ("▲", "needs input".into(), Some(("needs", format!("'{task}' is waiting for your input"))));
    }

    // Idle detection via pane stability across ticks.
    let ph = hash(&tmux::capture(session).unwrap_or_default());
    match w.prev_pane {
        Some(p) if p == ph => w.idle_ticks += 1,
        _ => {
            w.idle_ticks = 0;
            w.prev_pane = Some(ph);
        }
    }
    // Progress detection via the worktree diff.
    let dh = worktree::worktree_diff_hash(wt);
    match w.prev_diff {
        Some(d) if d == dh => w.nodiff_ticks += 1,
        _ => {
            w.nodiff_ticks = 0;
            w.prev_diff = Some(dh);
        }
    }

    // Actively working — re-arm escalations so a later stall alerts again.
    if w.idle_ticks == 0 {
        w.escalated.clear();
        return ("⠋", "working".into(), None);
    }

    let idle_secs = w.idle_ticks as u64 * interval;
    let quiet_secs = w.nodiff_ticks as u64 * interval;
    if idle_secs >= stuck_secs && quiet_secs >= stuck_secs {
        let dur = human(idle_secs);
        // Distinguish "done" from "stuck" without running verify: an idle agent that
        // PRODUCED changes has finished with work to review (the finish hook already
        // pinged you) — don't re-nag it. Only an agent idle a long time having
        // produced NOTHING is genuinely suspicious.
        if has_uncommitted(wt) {
            return ("●", format!("idle {dur}, changes ready"), None);
        }
        return (
            "⚠",
            format!("idle {dur}, no output"),
            Some(("stuck", format!("'{task}' has been idle {dur} and produced no changes — likely stuck; take a look"))),
        );
    }
    ("●", format!("idle {}", human(idle_secs)), None)
}

fn print_table(rows: &[(String, String, &'static str, String)]) {
    if rows.is_empty() {
        println!("  (no agents)");
        return;
    }
    let mut last_repo = String::new();
    for (repo, task, glyph, note) in rows {
        if repo != &last_repo {
            println!("▸ {repo}");
            last_repo = repo.clone();
        }
        println!("   {glyph} {task:<24} {note}");
    }
    println!("{}", "─".repeat(40));
}
