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

/// Append an audit line to `<repo>/.wta/run-log.md` — but only if the repo has
/// opted into wta conventions (a `.wta/` dir already exists), so we never create
/// files in a repo that isn't using them.
fn append_run_log(root: &Path, task: &str, action: &str) {
    let dir = root.join(".wta");
    if !dir.is_dir() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("- `{now}`  **{action}**  {task}\n");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("run-log.md")) {
        let _ = f.write_all(line.as_bytes());
    }
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

    // Self-heal stale admin entries (a worktree dir deleted out from under git still
    // "claims" its branch) so the add below doesn't fail with "already used by
    // worktree at <missing-path>".
    let _ = run_git(&["worktree", "prune"], Some(&root));

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
        let (idx, port) = port_slot(task);
        let _ = Command::new("bash")
            .arg(&setup)
            .current_dir(&wt)
            .env("WTA_TASK", task)
            .env("WTA_ROOT", &root)
            .env("WTA_INDEX", idx.to_string())
            .env("WTA_PORT_BASE", port.to_string())
            .status();
    }
    Ok((root, wt))
}

/// Deterministic per-agent slot + port block, so parallel agents' dev servers /
/// databases don't collide (port 3000, shared dev DB). Stable per task name, so
/// it survives resume. Exposed as `WTA_INDEX` + `WTA_PORT_BASE` to the agent pane
/// AND `.wta/setup.sh` — use them e.g. `PORT=$WTA_PORT_BASE npm run dev`,
/// DB `myapp_$WTA_INDEX`.
fn port_slot(task: &str) -> (u32, u32) {
    let mut h = DefaultHasher::new();
    h.write(task.as_bytes());
    let idx = (h.finish() % 100) as u32; // 0..=99
    (idx, 13000 + idx * 10) // a 10-port block per agent
}

/// Build the tmux pane command: `env WTA_TASK=… WTA_REPO=… WTA_INDEX=… WTA_PORT_BASE=… <agent_cmd> <tail…>`.
/// The `env` wrapper lets the agent's Claude Code hooks (`wta status`) record
/// state under the right repo + task, and gives the agent its isolation slot.
fn agent_argv(repo: &str, task: &str, tail: &[String]) -> (String, Vec<String>) {
    let (idx, port) = port_slot(task);
    let mut extra = vec![
        format!("WTA_TASK={task}"),
        format!("WTA_REPO={repo}"),
        format!("WTA_INDEX={idx}"),
        format!("WTA_PORT_BASE={port}"),
        agent_cmd(),
    ];
    extra.extend_from_slice(tail);
    ("env".to_string(), extra)
}

/// Hint for `wta new` when the repo has no agent-instructions file — agents ground
/// much better with one. `None` if a file already exists. Prints only; never
/// creates or commits anything.
pub fn instructions_hint() -> Option<String> {
    let root = repo_root().ok()?;
    let has = ["AGENTS.md", "CLAUDE.md", "GEMINI.md", ".github/copilot-instructions.md", ".cursorrules"]
        .iter()
        .any(|f| root.join(f).exists());
    if has {
        None
    } else {
        Some("tip: no AGENTS.md/CLAUDE.md in this repo — a short instructions file makes agents noticeably more reliable.".into())
    }
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

/// Spawn an INDEPENDENT reviewer agent on a builder's branch (maker/checker —
/// agents can't self-grade). Names it `review-<builder>`, based on the builder's
/// branch so it can see + test the changes. Optionally uses a different/cheaper
/// agent CLI (`--by` / `WTA_REVIEW_AGENT_CMD`).
pub fn review(builder: &str, by: Option<&str>) -> Result<()> {
    let root = repo_root()?;
    let builder_branch = branch_name(builder);
    let exists = run_git(
        &["show-ref", "--verify", "--quiet", &format!("refs/heads/{builder_branch}")],
        Some(&root),
    )
    .is_ok();
    if !exists {
        bail!("no agent '{builder}' (branch {builder_branch} not found)");
    }
    let base = base_branch(&root);
    let reviewer = format!("review-{builder}");
    let _ = rm(&reviewer, true); // clear any prior review so re-review is clean

    let prompt = format!(
        "You are an INDEPENDENT code reviewer. Another agent wrote the changes on \
         this branch versus `{base}`. Do NOT trust the author's claims. Inspect the \
         diff (`git diff {base}...HEAD`), run the tests/build, and confirm it does \
         what was intended. Report concrete issues, then end with a single line \
         exactly: `REVIEW: PASS` or `REVIEW: FAIL`."
    );
    // reviewer runs an (optionally cheaper / different) agent CLI. This is a
    // one-shot CLI process, so overriding its own WTA_AGENT_CMD env is safe.
    let cmd = by
        .map(String::from)
        .or_else(|| std::env::var("WTA_REVIEW_AGENT_CMD").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(agent_cmd);
    std::env::set_var("WTA_AGENT_CMD", &cmd);
    new_with_base(&reviewer, &[prompt], &builder_branch)?;
    println!("started reviewer '{reviewer}' on {builder}'s branch — watch it in `wta dash`");
    Ok(())
}

/// `wta init` — scaffold `.wta/` convention stubs (verify/setup/teardown) if
/// absent. Explicit + idempotent; never overwrites, never touches tracked files.
pub fn init() -> Result<()> {
    let root = repo_root()?;
    let dir = root.join(".wta");
    std::fs::create_dir_all(&dir)?;
    let files = [
        ("verify.sh", "#!/usr/bin/env bash\n# wta runs this per agent to gate merges — exit non-zero on failure.\nset -e\n# e.g.: cargo test   |   npm test   |   pytest -q   |   make check\n"),
        ("setup.sh", "#!/usr/bin/env bash\n# Runs in each fresh worktree on `wta new`.\n# Isolation slots are available as $WTA_INDEX (0-99) and $WTA_PORT_BASE.\n# e.g.: ln -s ../../node_modules node_modules\n"),
        ("teardown.sh", "#!/usr/bin/env bash\n# Runs on `wta rm`, before the worktree is removed.\n# e.g.: docker compose -p \"$WTA_TASK\" down\n"),
    ];
    let mut created = Vec::new();
    for (name, body) in files {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(&p, body)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            }
            created.push(name);
        }
    }
    if created.is_empty() {
        println!(".wta/ is already set up (verify.sh, setup.sh, teardown.sh)");
    } else {
        println!("scaffolded .wta/{{{}}} — edit them for your stack", created.join(", "));
    }
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
    append_run_log(&root, task, "stop");
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

    // Optional teardown hook (mirror of setup.sh) — run WHILE the worktree still
    // exists so agents can stop docker/dev-servers/ports before it's removed.
    let teardown = root.join(".wta/teardown.sh");
    if wt.exists() && teardown.exists() {
        let _ = Command::new("bash")
            .arg(&teardown)
            .current_dir(&wt)
            .env("WTA_TASK", task)
            .env("WTA_ROOT", &root)
            .status();
    }

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
    append_run_log(&root, task, "rm");
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
    append_run_log(&root, task, "push");

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

/// The editor/opener command: `WTA_OPEN_CMD`, else `$EDITOR`/`$VISUAL` (may be
/// multi-word, e.g. "code --reuse-window").
pub fn editor_cmd() -> Option<String> {
    for k in ["WTA_OPEN_CMD", "EDITOR", "VISUAL"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// GUI editors fork and return immediately (open detached); terminal editors
/// (nvim/vim/helix/emacs -nw/…) take over the tty and must run inline.
pub fn is_gui_editor(cmd: &str) -> bool {
    let prog = cmd.split_whitespace().next().unwrap_or("");
    let base = Path::new(prog).file_name().and_then(|s| s.to_str()).unwrap_or(prog);
    let base = base.trim_end_matches(".sh").trim_end_matches(".exe");
    matches!(
        base,
        "code" | "code-insiders" | "codium" | "vscodium" | "cursor" | "windsurf"
            | "subl" | "sublime_text" | "zed" | "zed-preview" | "idea" | "webstorm"
            | "pycharm" | "rustrover" | "clion" | "goland" | "phpstorm" | "rubymine"
            | "nova" | "atom" | "bbedit" | "mate" | "fleet"
    )
}

/// `wta open <task>` — open the agent's worktree in the editor (foreground; GUI
/// apps return immediately, terminal editors run until you quit).
pub fn open(task: &str) -> Result<()> {
    let root = repo_root()?;
    let wt = worktrees_dir(&root).join(task);
    if !wt.exists() {
        bail!("no worktree for '{task}'");
    }
    let cmd = editor_cmd().context("set WTA_OPEN_CMD or $EDITOR to an editor (e.g. nvim, code)")?;
    let mut it = cmd.split_whitespace();
    let prog = it.next().unwrap();
    let args: Vec<&str> = it.collect();
    Command::new(prog)
        .args(&args)
        .arg(&wt)
        .current_dir(&wt)
        .status()
        .with_context(|| format!("failed to run '{cmd}'"))?;
    Ok(())
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
