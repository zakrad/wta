use anyhow::{bail, Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{status, tmux};

/// Stable short id for a repo (hash of its canonical root path). Namespaces tmux
/// session names + on-disk state so two repos with the same task name never
/// collide (`wta-<repo>-<task>`, `~/.wta/state/<repo>/<task>.json`).
pub fn repo_id_of(root: &Path) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut h = DefaultHasher::new();
    h.write(canon.to_string_lossy().as_bytes());
    format!("{:08x}", h.finish() as u32)
}

pub fn repo_id() -> Result<String> {
    Ok(repo_id_of(&repo_root()?))
}

pub struct Worktree {
    pub task: String,
    pub path: PathBuf,
    pub branch: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn worktree_subdir() -> String {
    env_or("WTA_WORKTREE_DIR", ".agents")
}
fn agent_cmd() -> String {
    env_or("WTA_AGENT_CMD", "claude")
}
/// Args appended to the agent command when *resuming* a stopped agent, so it
/// continues the previous conversation instead of starting fresh. Default is
/// Claude Code's `--continue` (continues the latest session in that directory).
/// Set empty to just relaunch the agent with no resume flag.
fn resume_args() -> Vec<String> {
    env_or("WTA_AGENT_RESUME_ARGS", "--continue")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}
fn context_files() -> Vec<String> {
    env_or(
        "WTA_CONTEXT_FILES",
        "CLAUDE.local.md .env .env.local .mcp.json",
    )
    .split_whitespace()
    .map(|s| s.to_string())
    .collect()
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut c = Command::new("git");
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output().context("failed to spawn git")?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn repo_root() -> Result<PathBuf> {
    let out = run_git(&["rev-parse", "--show-toplevel"], None).context("not inside a git repo")?;
    Ok(PathBuf::from(out.trim()))
}

pub fn base_branch(root: &Path) -> String {
    for b in ["main", "master"] {
        if run_git(
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{b}"),
            ],
            Some(root),
        )
        .is_ok()
        {
            return b.to_string();
        }
    }
    "HEAD".to_string()
}

fn worktrees_dir(root: &Path) -> PathBuf {
    root.join(worktree_subdir())
}
fn branch_name(task: &str) -> String {
    format!("agent/{task}")
}

/// Create the worktree + copy context + optional setup, returning the worktree
/// path. If `base` is given, the new `agent/<task>` branch starts from it;
/// otherwise from HEAD.
fn make_worktree(task: &str, base: Option<&str>) -> Result<(PathBuf, PathBuf)> {
    let root = repo_root()?;
    let branch = branch_name(task);
    let wt = worktrees_dir(&root).join(task);
    if wt.exists() {
        bail!("worktree already exists: {}", wt.display());
    }
    let wt_str = wt.to_string_lossy().into_owned();

    let branch_exists = run_git(
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        Some(&root),
    )
    .is_ok();
    if branch_exists {
        run_git(&["worktree", "add", &wt_str, &branch], Some(&root))?;
    } else if let Some(base) = base {
        run_git(
            &["worktree", "add", "-b", &branch, &wt_str, base],
            Some(&root),
        )?;
    } else {
        run_git(&["worktree", "add", "-b", &branch, &wt_str], Some(&root))?;
    }

    for name in context_files() {
        let src = root.join(&name);
        if src.exists() {
            let dst = wt.join(&name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if src.is_dir() {
                Command::new("cp")
                    .args(["-R"])
                    .arg(&src)
                    .arg(&dst)
                    .status()
                    .ok();
            } else {
                std::fs::copy(&src, &dst).ok();
            }
        }
    }

    let setup = root.join(".wta/setup.sh");
    if setup.exists() {
        let _ = Command::new("bash")
            .arg(&setup)
            .current_dir(&wt)
            .env("WTA_TASK", task)
            .env("WTA_ROOT", &root)
            .status();
    }
    Ok((root, wt))
}

/// Build the tmux pane command: `env WTA_TASK=<task> WTA_REPO=<repo> <agent_cmd> <tail...>`.
/// The `env` wrapper lets the agent's Claude Code hooks (`wta status`) record
/// state under the right repo + task.
fn agent_argv(repo: &str, task: &str, tail: &[String]) -> (String, Vec<String>) {
    let mut extra = vec![format!("WTA_TASK={task}"), format!("WTA_REPO={repo}"), agent_cmd()];
    extra.extend_from_slice(tail);
    ("env".to_string(), extra)
}

pub fn new(task: &str, agent_args: &[String]) -> Result<()> {
    new_impl(task, agent_args, None)
}

/// Like `new`, but base the agent's branch on an existing branch.
pub fn new_with_base(task: &str, agent_args: &[String], base: &str) -> Result<()> {
    new_impl(task, agent_args, Some(base))
}

fn new_impl(task: &str, agent_args: &[String], base: Option<&str>) -> Result<()> {
    let (root, wt) = make_worktree(task, base)?;
    let repo = repo_id_of(&root);
    let wt_str = wt.to_string_lossy().into_owned();
    let session = tmux::session_name(&repo, task);
    let (prog, extra) = agent_argv(&repo, task, agent_args);
    tmux::new_session(&session, &wt, &prog, &extra)?;
    let _ = status::record(&repo, task, "running", &wt_str);
    Ok(())
}

/// Spawn N agents on the same prompt off one base branch — the front half of a
/// "spawn → compare (`wta matrix`) → merge the winner (`wta push`) → drop the
/// rest (`wta rm`)" loop. Names them `<name>-1..N`.
pub fn fanout(name: &str, count: u32, base: Option<&str>, agent_args: &[String]) -> Result<()> {
    if count == 0 {
        bail!("--count must be >= 1");
    }
    eprintln!("fanout: starting {count} agents on the same prompt — each is a full agent run (token cost ×{count}).");
    let mut ok = 0u32;
    for i in 1..=count {
        let task = format!("{name}-{i}");
        let r = match base {
            Some(b) => new_with_base(&task, agent_args, b),
            None => new(&task, agent_args),
        };
        match r {
            Ok(_) => {
                ok += 1;
                println!("  started {task}");
            }
            Err(e) => println!("  skipped {task}: {e}"),
        }
    }
    println!("fanout: {ok}/{count} started. Compare with `wta matrix`, review diffs in `wta dash`,");
    println!("        merge the winner with `wta push <task> --pr`, drop the rest with `wta rm <task> --force`.");
    Ok(())
}

/// Local branches a new agent could be based on (excludes wta's own `agent/*`).
pub fn list_branches() -> Result<Vec<String>> {
    let root = repo_root()?;
    let out = run_git(&["branch", "--format=%(refname:short)"], Some(&root))?;
    Ok(out
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("agent/"))
        .collect())
}

/// Re-spawn an agent's session in its EXISTING worktree (its session was stopped
/// or died). Reuses the branch + all uncommitted work.
pub fn resume_at(task: &str, wt: &Path) -> Result<()> {
    if !wt.exists() {
        bail!("no worktree at {} to resume", wt.display());
    }
    let repo = repo_id()?;
    let session = tmux::session_name(&repo, task);
    let (prog, extra) = agent_argv(&repo, task, &resume_args());
    tmux::new_session(&session, wt, &prog, &extra)?;
    let _ = status::record(&repo, task, "running", &wt.to_string_lossy());
    Ok(())
}

/// Resume by task name (looks up the worktree under the current repo).
pub fn resume(task: &str) -> Result<()> {
    let root = repo_root()?;
    let wt = worktrees_dir(&root).join(task);
    resume_at(task, &wt)
}

/// Stop an agent WITHOUT destroying anything: kills the tmux session but keeps
/// the worktree (and uncommitted work) so it can be resumed later. Contrast with
/// `rm`, which also removes the worktree and branch.
pub fn stop(task: &str) -> Result<()> {
    let root = repo_root()?;
    let repo = repo_id_of(&root);
    tmux::kill(&tmux::session_name(&repo, task))?;
    // mark exited so the dashboard AND the Telegram bridge stop reporting it as
    // running (the bridge reads state without checking tmux liveness).
    let wt = worktrees_dir(&root).join(task);
    let _ = status::record(&repo, task, "exited", &wt.to_string_lossy());
    Ok(())
}

fn diffstat(path: &Path, base: &str) -> String {
    let mb = match run_git(&["merge-base", "HEAD", base], Some(path)) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return "—".to_string(),
    };
    match run_git(&["diff", "--shortstat", &mb], Some(path)) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "clean".to_string(),
    }
}

pub fn list_managed() -> Result<Vec<Worktree>> {
    let root = repo_root()?;
    let base_dir = worktrees_dir(&root);
    let out = run_git(&["worktree", "list", "--porcelain"], Some(&root))?;
    let mut result = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch = String::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(p) = cur_path.take() {
                push_if_managed(&mut result, &base_dir, p, std::mem::take(&mut cur_branch));
            }
            cur_path = Some(PathBuf::from(rest));
            cur_branch.clear();
        } else if let Some(rest) = line.strip_prefix("branch ") {
            cur_branch = rest.trim_start_matches("refs/heads/").to_string();
        }
    }
    if let Some(p) = cur_path.take() {
        push_if_managed(&mut result, &base_dir, p, cur_branch);
    }
    result.sort_by(|a, b| a.task.cmp(&b.task));
    Ok(result)
}

fn push_if_managed(out: &mut Vec<Worktree>, base_dir: &Path, path: PathBuf, branch: String) {
    if path.starts_with(base_dir) {
        let task = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(Worktree { task, path, branch });
    }
}

pub fn ls() -> Result<()> {
    let root = repo_root()?;
    let repo = repo_id_of(&root);
    let base = base_branch(&root);
    let managed = list_managed()?;
    if managed.is_empty() {
        println!("no agents (create one with: wta new <task>)");
        return Ok(());
    }
    println!(
        "{:<20} {:<9} {:<22} CHANGES vs {}",
        "TASK", "STATE", "BRANCH", base
    );
    for w in managed {
        let alive = if tmux::has_session(&tmux::session_name(&repo, &w.task)) {
            "running"
        } else {
            "exited"
        };
        println!(
            "{:<20} {:<9} {:<22} {}",
            w.task,
            alive,
            w.branch,
            diffstat(&w.path, &base)
        );
    }
    Ok(())
}

pub fn rm(task: &str, force: bool) -> Result<()> {
    let root = repo_root()?;
    let repo = repo_id_of(&root);
    let branch = branch_name(task);
    let wt = worktrees_dir(&root).join(task);
    let wt_str = wt.to_string_lossy().into_owned();

    let _ = tmux::kill(&tmux::session_name(&repo, task));

    // Remove the worktree only if it's actually there. A worktree with
    // uncommitted/untracked files needs --force — surface that precisely so the
    // caller can offer it. A missing worktree (already gone, or a stale/ghost
    // state entry) is not an error.
    if wt.exists() {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&wt_str);
        run_git(&args, Some(&root))
            .with_context(|| format!("'{task}' has uncommitted changes — force to discard"))?;
    }
    let _ = run_git(&["worktree", "prune"], Some(&root)); // clear dangling admin entries

    // best-effort branch delete (may not exist / may be unmerged)
    let flag = if force { "-D" } else { "-d" };
    let _ = run_git(&["branch", flag, &branch], Some(&root));

    // ALWAYS drop the state file so the agent leaves the dashboard even if it was
    // only a stale entry with no worktree.
    status::remove_state(&repo, task);
    Ok(())
}

/// The `.agents` directory of the current repo (where wta keeps its worktrees).
pub fn agents_dir() -> Result<PathBuf> {
    Ok(worktrees_dir(&repo_root()?))
}

/// Commit any uncommitted work in the agent's worktree, push its branch, and
/// (if `make_pr`) open a PR via `gh`. Returns a short human summary.
pub fn push(task: &str, make_pr: bool) -> Result<String> {
    let root = repo_root()?;
    let wt = worktrees_dir(&root).join(task);
    if !wt.exists() {
        bail!("no worktree for '{task}'");
    }
    let branch = branch_name(task);

    // Stage everything, then UNSTAGE the wta-injected context files so secrets /
    // local context (CLAUDE.local.md, .env, …) never land in the PR. Commit only
    // if real changes remain staged.
    run_git(&["add", "-A"], Some(&wt))?;
    for f in context_files() {
        let _ = run_git(&["reset", "-q", "--", &f], Some(&wt));
    }
    let staged = !run_git(&["diff", "--cached", "--name-only"], Some(&wt))?
        .trim()
        .is_empty();
    if staged {
        run_git(&["commit", "-m", &format!("wta: {task}")], Some(&wt))?;
    }

    run_git(&["push", "-u", "origin", &branch], Some(&wt))
        .context("git push failed (is `origin` set?)")?;

    if make_pr {
        // best-effort: create a PR, or report the existing one.
        let created = Command::new("gh")
            .args(["pr", "create", "--fill", "--head", &branch])
            .current_dir(&wt)
            .output();
        if let Ok(o) = created {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && url.starts_with("http") {
                return Ok(format!("PR opened: {url}"));
            }
            // maybe it already exists — look it up
            if let Ok(v) = Command::new("gh")
                .args(["pr", "view", &branch, "--json", "url", "-q", ".url"])
                .current_dir(&wt)
                .output()
            {
                let u = String::from_utf8_lossy(&v.stdout).trim().to_string();
                if v.status.success() && u.starts_with("http") {
                    return Ok(format!("PR: {u}"));
                }
            }
        }
        return Ok(format!("pushed {branch} (PR step needs `gh`)"));
    }
    Ok(format!("pushed {branch}"))
}

/// Attach to an agent's session in the foreground (blocks until detach).
pub fn attach(task: &str) -> Result<()> {
    let session = tmux::session_name(&repo_id()?, task);
    if !tmux::has_session(&session) {
        bail!("no running session for '{task}' — start it with `wta new {task}` or resume from `wta dash`");
    }
    tmux::attach_blocking(&session)
}

// ---- mergeability matrix (read-only conflict preview across agent branches) ----

pub struct MergePair {
    pub i: usize,
    pub j: usize,
    pub clean: bool,
    pub files: Vec<String>,
}

pub struct MergeMatrix {
    pub labels: Vec<String>,
    pub pairs: Vec<MergePair>,
}

impl MergeMatrix {
    /// grid[i][j] = Some(clean) for i != j.
    pub fn grid(&self) -> Vec<Vec<Option<bool>>> {
        let n = self.labels.len();
        let mut g = vec![vec![None; n]; n];
        for p in &self.pairs {
            g[p.i][p.j] = Some(p.clean);
            g[p.j][p.i] = Some(p.clean);
        }
        g
    }
}

/// 3-way merge A and B in memory (no working-tree changes, no commit) via
/// `git merge-tree --write-tree`: exit 0 = clean, exit 1 = conflict (stdout is
/// the tree oid then the conflicted file names until a blank line).
fn merge_check(root: &Path, a: &str, b: &str) -> (bool, Vec<String>) {
    let out = Command::new("git")
        .args(["merge-tree", "--write-tree", "--name-only", a, b])
        .current_dir(root)
        .output();
    match out {
        Ok(o) if o.status.success() => (true, Vec::new()),
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let files: Vec<String> = text
                .lines()
                .skip(1) // first line is the merged tree oid
                .take_while(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .collect();
            (false, files)
        }
        Err(_) => (false, vec!["<git merge-tree unavailable — needs git ≥ 2.38>".into()]),
    }
}

/// Pairwise mergeability of the base branch + every managed agent branch.
pub fn mergeability() -> Result<MergeMatrix> {
    let root = repo_root()?;
    let base = base_branch(&root);
    let mut labels = vec![base.clone()];
    let mut refs = vec![base];
    for w in list_managed()? {
        labels.push(w.task);
        refs.push(w.branch);
    }
    let n = refs.len();
    let mut pairs = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let (clean, files) = merge_check(&root, &refs[i], &refs[j]);
            pairs.push(MergePair { i, j, clean, files });
        }
    }
    Ok(MergeMatrix { labels, pairs })
}

/// `wta matrix` — print the pairwise conflict grid.
pub fn matrix() -> Result<()> {
    let m = mergeability()?;
    let n = m.labels.len();
    if n <= 1 {
        println!("no agent branches to compare (create some with `wta new`)");
        return Ok(());
    }
    let grid = m.grid();
    let w = 9usize;
    let short = |s: &str| -> String { s.chars().take(w - 1).collect() };

    print!("{:<pad$}", "", pad = w + 2);
    for l in &m.labels {
        print!("{:<w$}", short(l), w = w);
    }
    println!();
    for i in 0..n {
        print!("{:<pad$}", short(&m.labels[i]), pad = w + 2);
        for j in 0..n {
            let cell = if i == j {
                "·"
            } else {
                match grid[i][j] {
                    Some(true) => "✓",
                    Some(false) => "✗",
                    None => "?",
                }
            };
            print!("{:<w$}", cell, w = w);
        }
        println!();
    }

    let conflicts: Vec<&MergePair> = m.pairs.iter().filter(|p| !p.clean).collect();
    if conflicts.is_empty() {
        println!("\nall branches merge cleanly (pairwise `git merge-tree` — no files touched)");
    } else {
        println!("\nconflicts (pairwise `git merge-tree`; no files touched):");
        for p in conflicts {
            let files = if p.files.is_empty() { "conflict".to_string() } else { p.files.join(", ") };
            println!("  {} ✗ {}  — {}", m.labels[p.i], m.labels[p.j], files);
        }
    }
    Ok(())
}
