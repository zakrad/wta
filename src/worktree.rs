use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{status, tmux};

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

/// Create the worktree + copy context + optional setup, returning the worktree path.
fn make_worktree(task: &str) -> Result<(PathBuf, PathBuf)> {
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

/// Build the tmux pane command: `env WTA_TASK=<task> <agent_cmd> <tail...>`.
/// The `env` wrapper is what lets the agent's Claude Code hooks (`wta status`)
/// know which task they belong to.
fn agent_argv(task: &str, tail: &[String]) -> (String, Vec<String>) {
    let mut extra = vec![format!("WTA_TASK={task}"), agent_cmd()];
    extra.extend_from_slice(tail);
    ("env".to_string(), extra)
}

pub fn new(task: &str, agent_args: &[String]) -> Result<()> {
    let (_root, wt) = make_worktree(task)?;
    let wt_str = wt.to_string_lossy().into_owned();
    let session = tmux::session_name(task);
    let (prog, extra) = agent_argv(task, agent_args);
    tmux::new_session(&session, &wt, &prog, &extra)?;
    let _ = status::record(task, "running", &wt_str);
    Ok(())
}

/// Re-spawn an agent's session in its EXISTING worktree (its session was stopped
/// or died). Reuses the branch + all uncommitted work.
pub fn resume_at(task: &str, wt: &Path) -> Result<()> {
    if !wt.exists() {
        bail!("no worktree at {} to resume", wt.display());
    }
    let session = tmux::session_name(task);
    let (prog, extra) = agent_argv(task, &resume_args());
    tmux::new_session(&session, wt, &prog, &extra)?;
    let _ = status::record(task, "running", &wt.to_string_lossy());
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
    tmux::kill(&tmux::session_name(task))
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
        let alive = if tmux::has_session(&tmux::session_name(&w.task)) {
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
    let branch = branch_name(task);
    let wt = worktrees_dir(&root).join(task);
    let wt_str = wt.to_string_lossy().into_owned();

    let _ = tmux::kill(&tmux::session_name(task));

    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&wt_str);
    run_git(&args, Some(&root)).context("worktree dirty? re-run with --force to discard")?;

    let flag = if force { "-D" } else { "-d" };
    let _ = run_git(&["branch", flag, &branch], Some(&root));
    if let Ok(p) = status::state_path(task) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
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

    // commit uncommitted changes, if any
    let dirty = !run_git(&["status", "--porcelain"], Some(&wt))?
        .trim()
        .is_empty();
    if dirty {
        run_git(&["add", "-A"], Some(&wt))?;
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
    let session = tmux::session_name(task);
    if !tmux::has_session(&session) {
        bail!("no running session for '{task}' — start it with `wta new {task}` or resume from `wta dash`");
    }
    tmux::attach_blocking(&session)
}
