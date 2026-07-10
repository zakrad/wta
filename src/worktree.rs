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

/// Keep wta's ephemeral files (`board.md`, `run-log.md`, logs) out of `git status`
/// and out of a stray `git add -A`, without ignoring the tracked `.wta/*.sh` stubs.
fn ensure_wta_gitignore(dir: &Path) {
    let gi = dir.join(".gitignore");
    if !gi.exists() {
        let _ = std::fs::write(&gi, "board.md\nrun-log.md\n*.log\n");
    }
}

/// Add wta-injected files to this worktree's git exclude, so the AGENT's own
/// `git add -A` (or push) can never stage/commit them — closing the "injected
/// context / fleet digest could be committed & pushed" leak. Uses `--git-path` so
/// git itself tells us which exclude file it honors for this worktree.
fn exclude_in_worktree(wt: &Path, files: &[String]) {
    if files.is_empty() {
        return;
    }
    let p = match run_git(&["rev-parse", "--git-path", "info/exclude"], Some(wt)) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };
    let exclude = if Path::new(&p).is_absolute() { PathBuf::from(&p) } else { wt.join(&p) };
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&exclude) {
        for name in files {
            let pat = format!("/{name}"); // anchored to the worktree root
            if !existing.lines().any(|l| l.trim() == pat || l.trim() == name.as_str()) {
                let _ = writeln!(f, "{pat}");
            }
        }
    }
}

/// Append an audit line to `<repo>/.wta/run-log.md` — but only if the repo has
/// opted into wta conventions (a `.wta/` dir already exists), so we never create
/// files in a repo that isn't using them.
fn append_run_log(root: &Path, task: &str, action: &str) {
    let dir = root.join(".wta");
    if !dir.is_dir() {
        return;
    }
    ensure_wta_gitignore(&dir);
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
    let mut v: Vec<String> = env_or("WTA_CONTEXT_FILES", "CLAUDE.local.md .env .env.local .mcp.json")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    // Opt-in: carry the repo's accumulated tool-permission grants
    // (`.claude/settings.local.json` → permissions.allow) into each worktree so
    // agents don't re-approve every Bash/Edit. OFF by default — it lets agents run
    // those tools UNPROMPTED, which is a real grant you should make deliberately.
    if std::env::var("WTA_COPY_PERMISSIONS").map(|x| x != "0" && !x.is_empty()).unwrap_or(false) {
        v.push(".claude/settings.local.json".to_string());
    }
    v
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_validation_rejects_unsafe_names() {
        for ok in ["good_1", "feature-x", "a1"] {
            assert!(validate_task(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["a/b", "v1.2", "../evil", "-flag", "", "has space", "a\\b"] {
            assert!(validate_task(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_task(&"x".repeat(65)).is_err());
    }

    #[test]
    fn agent_argv_tokenizes_multiword_command() {
        std::env::set_var("WTA_AGENT_CMD", "claude --model haiku");
        std::env::remove_var("WTA_SKIP_PERMISSIONS"); // default
        let (prog, extra) = agent_argv("repo", "task", 0, &[]);
        assert_eq!(prog, "env");
        // the command must be split into separate argv elements, not one string
        assert!(extra.contains(&"claude".to_string()));
        assert!(extra.contains(&"--model".to_string()));
        assert!(extra.contains(&"haiku".to_string()));
        assert!(!extra.iter().any(|s| s == "claude --model haiku"));
        // permission bypass is ON BY DEFAULT for claude
        assert!(extra.contains(&"--dangerously-skip-permissions".to_string()));
        // ...and OFF with WTA_SKIP_PERMISSIONS=0 (`--safe`)
        std::env::set_var("WTA_SKIP_PERMISSIONS", "0");
        let (_, safe) = agent_argv("repo", "task", 0, &[]);
        std::env::remove_var("WTA_SKIP_PERMISSIONS");
        std::env::remove_var("WTA_AGENT_CMD");
        assert!(!safe.contains(&"--dangerously-skip-permissions".to_string()));
    }
}

/// Reject task names that aren't safe as a tmux session name, a filesystem path,
/// and a git branch all at once — keeping those three representations identical
/// (no `/`, `.`, `..`, spaces, etc. that would diverge or escape `.agents`).
fn validate_task(task: &str) -> Result<()> {
    if task.is_empty() || task.len() > 64 {
        bail!("task name must be 1–64 characters");
    }
    if task.starts_with('-') {
        bail!("task name can't start with '-'");
    }
    if !task.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid task name '{task}' — use letters, digits, '-' and '_' only");
    }
    Ok(())
}

/// Create the worktree + copy context + optional setup, returning the worktree
/// path. If `base` is given, the new `agent/<task>` branch starts from it;
/// otherwise from HEAD.
fn make_worktree(task: &str, base: Option<&str>, idx: u32) -> Result<(PathBuf, PathBuf, Vec<String>)> {
    validate_task(task)?;
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
        // Reusing a leftover branch (e.g. after a non-force `rm` that couldn't
        // delete an unmerged branch) would silently ignore an explicit --base and
        // resurrect the old agent's commits. Refuse rather than mislead.
        if base.is_some() {
            bail!("branch {branch} already exists — `wta rm --force {task}` to recreate it from --base, or omit --base to reuse the existing branch");
        }
        run_git(&["worktree", "add", &wt_str, &branch], Some(&root))?;
    } else if let Some(base) = base {
        run_git(
            &["worktree", "add", "-b", &branch, &wt_str, base],
            Some(&root),
        )?;
    } else {
        run_git(&["worktree", "add", "-b", &branch, &wt_str], Some(&root))?;
    }

    let mut injected = Vec::new();
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
            injected.push(name);
        }
    }

    let setup = root.join(".wta/setup.sh");
    if setup.exists() {
        let port = port_base(idx);
        let _ = Command::new("bash")
            .arg(&setup)
            .current_dir(&wt)
            .env("WTA_TASK", task)
            .env("WTA_ROOT", &root)
            .env("WTA_INDEX", idx.to_string())
            .env("WTA_PORT_BASE", port.to_string())
            .status();
    }
    Ok((root, wt, injected))
}

/// Lowest free isolation slot (0..100) among the repo's current agents, so
/// parallel agents get DISTINCT `WTA_INDEX`/`WTA_PORT_BASE` (no birthday
/// collisions). The chosen slot is persisted per agent (`AgentState.index`) so it
/// stays stable across resume. Exposed to the pane AND `.wta/setup.sh` — use e.g.
/// `PORT=$WTA_PORT_BASE npm run dev`, DB `myapp_$WTA_INDEX`.
fn assign_slot(repo: &str) -> u32 {
    let used: std::collections::HashSet<u32> = status::read_states(repo)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.index)
        .collect();
    (0u32..100).find(|i| !used.contains(i)).unwrap_or(0)
}
fn port_base(idx: u32) -> u32 {
    13000 + idx * 10
}

/// Build the tmux pane command: `env WTA_TASK=… WTA_REPO=… WTA_INDEX=… WTA_PORT_BASE=… <agent_cmd> <tail…>`.
/// The `env` wrapper lets the agent's Claude Code hooks (`wta status`) record
/// state under the right repo + task, and gives the agent its isolation slot.
fn agent_argv(repo: &str, task: &str, idx: u32, tail: &[String]) -> (String, Vec<String>) {
    let mut extra = vec![
        format!("WTA_TASK={task}"),
        format!("WTA_REPO={repo}"),
        format!("WTA_INDEX={idx}"),
        format!("WTA_PORT_BASE={}", port_base(idx)),
    ];
    // tmux execs program+args directly (no shell), so a multi-word agent command
    // (`claude --model haiku`, `npx foo`) must be tokenized, not one argv element.
    let cmd = agent_cmd();
    let is_claude = cmd.split_whitespace().next().map(|c| c.ends_with("claude")).unwrap_or(false);
    extra.extend(cmd.split_whitespace().map(String::from));
    // Permission bypass is ON BY DEFAULT for the claude CLI (Claude's flag is
    // `--dangerously-skip-permissions`) — agents run in isolated worktrees. Opt out
    // per-agent with `wta new --safe`, or globally with WTA_SKIP_PERMISSIONS=0.
    if is_claude && std::env::var("WTA_SKIP_PERMISSIONS").map(|v| v != "0").unwrap_or(true) {
        extra.push("--dangerously-skip-permissions".to_string());
    }
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

/// Pre-accept Claude Code's folder-trust for a worktree path by setting
/// `projects[<abs path>].hasTrustDialogAccepted = true` in `~/.claude.json` (the
/// vendor-suggested workaround) BEFORE the agent starts — so agents spawned from
/// the CLI (`wta new`/`fanout`/`review`), where the dash isn't watching to dismiss
/// the dialog, don't get stuck on the trust prompt. Claude-only, gated by
/// `WTA_AUTO_TRUST`, best-effort, preserves every existing key.
fn preseed_claude_trust(wt: &Path) {
    if std::env::var("WTA_AUTO_TRUST").map(|v| v == "0").unwrap_or(false) {
        return;
    }
    let is_claude = agent_cmd().split_whitespace().next().map(|c| c.ends_with("claude")).unwrap_or(false);
    if !is_claude {
        return; // only meaningful for the claude CLI
    }
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir);
    let path = match base {
        Some(b) => b.join(".claude.json"),
        None => return,
    };
    let abs = std::fs::canonicalize(wt).unwrap_or_else(|_| wt.to_path_buf());
    let key = abs.to_string_lossy().into_owned();
    // Missing file → create; existing-but-unparseable → BAIL (never clobber the
    // real config, which holds the oauth token + all session state).
    let mut root: serde_json::Value = match std::fs::read(&path) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(_) => return,
        },
        Err(_) => serde_json::json!({}),
    };
    if !root.is_object() {
        return;
    }
    let projects = root.as_object_mut().unwrap().entry("projects").or_insert_with(|| serde_json::json!({}));
    if !projects.is_object() {
        return;
    }
    let entry = projects.as_object_mut().unwrap().entry(key).or_insert_with(|| serde_json::json!({}));
    match entry.as_object_mut() {
        // already trusted → skip the rewrite (shrinks the last-writer-wins window)
        Some(o) if o.get("hasTrustDialogAccepted").and_then(|v| v.as_bool()) == Some(true) => return,
        Some(o) => {
            o.insert("hasTrustDialogAccepted".into(), serde_json::json!(true));
        }
        None => return,
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&root) {
        // per-process tmp so two concurrent writers never share the same inode
        // (a shared tmp could be truncated by one while the other renames it)
        let tmp = path.with_extension(format!("json.wta-tmp.{}", std::process::id()));
        if std::fs::write(&tmp, &bytes).is_ok() {
            // the config holds the oauth token — keep it private
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            if std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp); // don't orphan a config copy
            }
        }
    }
}

pub fn new(task: &str, agent_args: &[String]) -> Result<()> {
    new_impl(task, agent_args, None)
}

/// Like `new`, but base the agent's branch on an existing branch.
pub fn new_with_base(task: &str, agent_args: &[String], base: &str) -> Result<()> {
    new_impl(task, agent_args, Some(base))
}

/// A short "other agents active now" snapshot for turn-zero awareness, derived
/// from the worktrees + branches wta already tracks. Empty if this is the only agent.
fn fleet_digest(root: &Path, exclude: &str) -> String {
    let base = base_branch(root);
    let mut rows = Vec::new();
    for w in list_managed().unwrap_or_default() {
        if w.task == exclude {
            continue;
        }
        let files = run_git(&["diff", "--name-only", &format!("{base}...{}", w.branch)], Some(root)).unwrap_or_default();
        let list: Vec<&str> = files.lines().filter(|l| !l.is_empty()).take(4).collect();
        let f = if list.is_empty() { "no changes yet".to_string() } else { list.join(", ") };
        rows.push(format!("- **{}** (branch `{}`): {}", w.task, w.branch, f));
    }
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n## Other wta agents active now (snapshot at your creation)\n");
    out.push_str("You are one of several parallel agents, each isolated in its own git worktree:\n");
    for r in rows.iter().take(20) {
        out.push_str(r);
        out.push('\n');
    }
    out.push_str(
        "Avoid editing files another agent owns. Coordinate: `wta send <agent> \"msg\"` to \
         message a peer, `wta board` to see/append shared claims. Re-read this before big refactors/merges.\n",
    );
    out
}

/// Append the fleet digest to the worktree's CLAUDE.local.md (create if absent).
/// Returns "CLAUDE.local.md" if written, so the caller records it as injected
/// (keeping it out of pushes).
fn write_fleet_digest(root: &Path, wt: &Path, task: &str) -> Option<String> {
    let digest = fleet_digest(root, task);
    if digest.is_empty() {
        return None;
    }
    let file = wt.join("CLAUDE.local.md");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file).ok()?;
    f.write_all(digest.as_bytes()).ok()?;
    Some("CLAUDE.local.md".to_string())
}

fn new_impl(task: &str, agent_args: &[String], base: Option<&str>) -> Result<()> {
    validate_task(task)?;
    let repo = repo_id()?;
    let idx = assign_slot(&repo); // pick the slot before creating, so setup.sh sees it
    let (root, wt, mut injected) = make_worktree(task, base, idx)?;
    // turn-zero awareness: tell the new agent who else is active + how to coordinate
    if let Some(f) = write_fleet_digest(&root, &wt, task) {
        if !injected.contains(&f) {
            injected.push(f);
        }
    }
    // git-exclude every injected file in the worktree so the agent's own commits
    // (or push) can never publish local context / the fleet digest
    exclude_in_worktree(&wt, &injected);
    // persist slot + injected list first, so the "running" record below merges over it
    let _ = status::record_meta(&repo, task, idx, &injected);
    preseed_claude_trust(&wt); // avoid the folder-trust dialog on this fresh worktree
    let wt_str = wt.to_string_lossy().into_owned();
    let session = tmux::session_name(&repo, task);
    let (prog, extra) = agent_argv(&repo, task, idx, agent_args);
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
    ensure_wta_gitignore(&dir);
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

/// Resolve the repo id, preferring the `WTA_REPO` env (set in each agent's pane)
/// so a `wta send`/`wta board` invoked BY an agent inside its worktree resolves the
/// MAIN repo, not the worktree (which `repo_root()` would return).
fn resolve_repo() -> Result<String> {
    match std::env::var("WTA_REPO") {
        Ok(r) if !r.is_empty() => Ok(r),
        _ => repo_id(),
    }
}

/// The MAIN repo root, resolved from anywhere (incl. a linked worktree) via
/// `--git-common-dir` — so `wta board` works whether you or an agent runs it.
fn main_root() -> Result<PathBuf> {
    let out = run_git(&["rev-parse", "--git-common-dir"], None).context("not inside a git repo")?;
    let gc = std::fs::canonicalize(out.trim()).unwrap_or_else(|_| PathBuf::from(out.trim()));
    Ok(gc.parent().map(|p| p.to_path_buf()).unwrap_or(gc))
}

/// Shared coordination board at `<main-root>/.wta/board.md`. `wta board` prints it;
/// `wta board "<text>"` appends a claim (O_APPEND, one line, no locks). Advisory —
/// agents read it at turn-zero / when told; the relay is the mid-session channel.
pub fn board(entry: Option<&str>) -> Result<()> {
    let dir = main_root()?.join(".wta");
    let file = dir.join("board.md");
    match entry {
        Some(text) if !text.trim().is_empty() => {
            std::fs::create_dir_all(&dir)?;
            ensure_wta_gitignore(&dir);
            let from = std::env::var("WTA_TASK").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "you".into());
            let line = format!("- [{from}] {}\n", text.trim().replace('\n', " "));
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
            f.write_all(line.as_bytes())?;
            println!("board += {}", line.trim());
        }
        _ => match std::fs::read_to_string(&file) {
            Ok(s) if !s.trim().is_empty() => print!("{s}"),
            _ => println!("(board empty — claim work with: wta board \"owning src/auth/**\")"),
        },
    }
    Ok(())
}

/// Peer relay: type a note into another agent's pane — the only channel that
/// reaches a running agent mid-session. Agents can call `wta send` themselves
/// (their pane has the wta binary + WTA_* env). REFUSES if the target is at a
/// dialog (so the message can't silently answer a permission/trust prompt) or busy.
pub fn send(task: &str, message: &str) -> Result<()> {
    let msg = message.trim();
    if msg.is_empty() {
        bail!("nothing to send");
    }
    let repo = resolve_repo()?;
    let session = tmux::session_name(&repo, task);
    if !tmux::has_session(&session) {
        bail!("'{task}' isn't running (resume it first)");
    }
    let pane = tmux::capture(&session).unwrap_or_default();
    if tmux::looks_interactive_dialog(&pane) {
        bail!("'{task}' is at a prompt/dialog — refusing to send (it could answer it). Try again once it's ready.");
    }
    if !tmux::pane_is_idle(&session) {
        bail!("'{task}' is busy — try again when it's idle");
    }
    let from = std::env::var("WTA_TASK").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "you".into());
    let framed = format!("[wta:{from}] {}", msg.replace('\n', " "));
    tmux::send_text(&session, &framed)?;
    println!("→ {task}: {}", msg.chars().take(70).collect::<String>());
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
    let idx = status::read_state(&repo, task).map(|s| s.index).unwrap_or_else(|| assign_slot(&repo));
    preseed_claude_trust(wt);
    let session = tmux::session_name(&repo, task);
    let (prog, extra) = agent_argv(&repo, task, idx, &resume_args());
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
    list_managed_in(&repo_root()?)
}

/// Short display name for a repo (its root dir name).
pub fn repo_name(root: &Path) -> String {
    root.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// All repos wta currently knows agents in — derived from the global state
/// (`~/.wta/state/<repo>/`), each agent's `cwd` giving `<root>/<subdir>/<task>`.
/// Returns `(repo_id, repo_root)`, sorted by path. Used by the global dashboard.
pub fn discover_repos() -> Vec<(String, PathBuf)> {
    let mut map: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for st in status::read_all_states().unwrap_or_default() {
        if st.cwd.is_empty() {
            continue;
        }
        let wt = PathBuf::from(&st.cwd);
        if let Some(root) = wt.parent().and_then(|p| p.parent()) {
            if root.exists() {
                map.entry(st.repo.clone()).or_insert_with(|| root.to_path_buf());
            }
        }
    }
    let mut v: Vec<(String, PathBuf)> = map.into_iter().collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

/// `list_managed`, but for an explicit repo root (so the global dash can scan
/// repos other than the current directory).
pub fn list_managed_in(root: &Path) -> Result<Vec<Worktree>> {
    let base_dir = worktrees_dir(root);
    let out = run_git(&["worktree", "list", "--porcelain"], Some(root))?;
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
    // unstage the EXACT files injected at `new` time (recorded per agent), so a
    // custom WTA_CONTEXT_FILES can't leak just because push runs in a different env
    let repo = repo_id_of(&root);
    let injected = status::read_state(&repo, task)
        .map(|s| s.context)
        .filter(|c| !c.is_empty())
        .unwrap_or_else(context_files);
    for f in injected {
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
