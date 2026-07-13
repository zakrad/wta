use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata sidecar for an agent — the `dash` reads these for the (optional)
/// hook-driven `needs_input`/`waiting` states. Liveness/preview come from tmux.
/// Stored per-repo at `~/.wta/state/<repo>/<task>.json` so agents with the same
/// task name in different repos never collide.
#[derive(Serialize, Deserialize, Clone)]
pub struct AgentState {
    pub task: String,
    #[serde(default)]
    pub repo: String, // repo id (hash of the repo root) this agent belongs to
    pub status: String, // running | needs_input | waiting | exited | ...
    pub cwd: String,
    pub updated_unix: u64, // last write time — persisted for inspection / external tooling
    #[serde(default)]
    pub index: u32, // stable isolation slot (WTA_INDEX / WTA_PORT_BASE), assigned at creation
    #[serde(default)]
    pub context: Vec<String>, // files injected at `new`, unstaged again by `push`
    #[serde(default)]
    pub base: Option<String>, // branch this agent is based on / targets — for diffs + PR base
}

pub fn wta_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot locate home directory")?;
    Ok(home.join(".wta"))
}

fn state_root() -> Result<PathBuf> {
    Ok(wta_dir()?.join("state"))
}

/// Per-repo state directory `~/.wta/state/<repo>/`.
pub fn repo_dir(repo: &str) -> Result<PathBuf> {
    let d = state_root()?.join(repo);
    std::fs::create_dir_all(&d).ok();
    Ok(d)
}

pub fn state_path(repo: &str, task: &str) -> Result<PathBuf> {
    // Same sanitizer as the tmux session name, so the state filename can't diverge
    // from it or escape the repo's state dir (a guard on top of validate_task).
    let safe = crate::tmux::sanitize_task(task);
    Ok(repo_dir(repo)?.join(format!("{safe}.json")))
}

pub fn remove_state(repo: &str, task: &str) {
    if let Ok(p) = state_path(repo, task) {
        let _ = std::fs::remove_file(p);
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Atomically replace `path` with `bytes` via a **per-process** temp file — so two
/// wta processes writing the same file (e.g. concurrent hooks, or `install-hooks`
/// racing another) never share a temp path and clobber each other's half-write.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn save(st: &AgentState) -> Result<()> {
    let final_path = state_path(&st.repo, &st.task)?;
    atomic_write(&final_path, &serde_json::to_vec_pretty(st)?)?;
    Ok(())
}

/// Load an agent's persisted state, if any.
pub fn read_state(repo: &str, task: &str) -> Option<AgentState> {
    let p = state_path(repo, task).ok()?;
    let bytes = std::fs::read(p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Record status/cwd (used by `wta new`/`resume` and by hooks). Merges over any
/// existing file so the isolation slot + injected-file list set at creation
/// aren't clobbered by later hook writes.
pub fn record(repo: &str, task: &str, status: &str, cwd: &str) -> Result<()> {
    let mut st = read_state(repo, task).unwrap_or(AgentState {
        task: String::new(),
        repo: String::new(),
        status: String::new(),
        cwd: String::new(),
        updated_unix: 0,
        index: 0,
        context: Vec::new(),
        base: None,
    });
    st.task = task.to_string();
    st.repo = repo.to_string();
    st.status = status.to_string();
    st.cwd = cwd.to_string();
    st.updated_unix = now_unix();
    save(&st)
}

/// Record the creation-time metadata (isolation slot + injected files), merging
/// over any existing status/cwd.
pub fn record_meta(repo: &str, task: &str, index: u32, context: &[String]) -> Result<()> {
    let mut st = read_state(repo, task).unwrap_or(AgentState {
        task: task.to_string(),
        repo: repo.to_string(),
        status: "running".to_string(),
        cwd: String::new(),
        updated_unix: now_unix(),
        index: 0,
        context: Vec::new(),
        base: None,
    });
    st.task = task.to_string();
    st.repo = repo.to_string();
    st.index = index;
    st.context = context.to_vec();
    save(&st)
}

/// Record the base branch this agent is based on / targets (merge-write, so it
/// preserves status/slot/context). Read back by the dashboard and `wta push --pr`.
pub fn record_base(repo: &str, task: &str, base: &str) -> Result<()> {
    let mut st = read_state(repo, task).unwrap_or(AgentState {
        task: task.to_string(),
        repo: repo.to_string(),
        status: "running".to_string(),
        cwd: String::new(),
        updated_unix: now_unix(),
        index: 0,
        context: Vec::new(),
        base: None,
    });
    st.task = task.to_string();
    st.repo = repo.to_string();
    st.base = Some(base.to_string());
    save(&st)
}

/// The persisted base branch for an agent, if one was recorded and non-empty.
pub fn base_of(repo: &str, task: &str) -> Option<String> {
    read_state(repo, task).and_then(|s| s.base).filter(|b| !b.is_empty())
}

fn emit_uservar(name: &str, value: &str) -> std::io::Result<()> {
    let payload = STANDARD.encode(value);
    match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(mut tty) => write!(tty, "\x1b]1337;SetUserVar={name}={payload}\x07"),
        Err(_) => {
            let mut so = std::io::stdout();
            write!(so, "\x1b]1337;SetUserVar={name}={payload}\x07")?;
            so.flush()
        }
    }
}

/// `wta status <state>` — called by Claude Code hooks inside an agent session.
/// Reads WTA_TASK + WTA_REPO, both exported into the agent's tmux pane at spawn.
pub fn emit(state: &str) -> Result<()> {
    let task = std::env::var("WTA_TASK").unwrap_or_default();
    let repo = std::env::var("WTA_REPO").unwrap_or_default();
    emit_uservar("agent_status", state).ok();
    if !task.is_empty() {
        emit_uservar("agent_task", &task).ok();
    }
    if !task.is_empty() && !repo.is_empty() {
        // The status BEFORE this event, so we can notify only on the *transition*
        // into idle (not repeatedly while it sits idle).
        let prev = read_state(&repo, &task).map(|s| s.status).unwrap_or_default();
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        record(&repo, &task, state, &cwd)?;
        // Edge-triggered: an agent going idle (Stop → "waiting" / Notification →
        // "needs_input") notifies exactly once. Claude fires both events — and re-fires
        // the idle Notification — for the same idle moment, so a level-triggered notify
        // would sound several times; here the second one sees an already-idle `prev` and
        // stays quiet. Notifies again only after the agent goes active (a new prompt →
        // "running") and finishes again. Gated on WTA_TASK/WTA_REPO so a plain `claude`
        // session that merely inherits the global hooks never notifies.
        let is_idle = |s: &str| matches!(s, "waiting" | "needs_input");
        if is_idle(state) && !is_idle(&prev) {
            notify_for_state(state, &task);
        }
    }
    Ok(())
}

/// The toast for the hook-driven states we alert on. Line 1 = the agent (task),
/// line 2 = `<repo> · <status>[· +A -B]`. Returns for states (like "running") that
/// shouldn't notify.
fn notify_for_state(state: &str, task: &str) {
    let status = match state {
        "waiting" => "done",
        "needs_input" => "needs input",
        _ => return,
    };
    let label = if task.is_empty() { "agent" } else { task };
    let repo = notify_repo_name().unwrap_or_else(|| "wta".to_string());
    let mut line2 = format!("{repo} · {status}");
    if let Some(stats) = diff_stats() {
        line2.push_str(" · ");
        line2.push_str(&stats);
    }
    crate::notify::alert(label, &line2);
}

/// Best-effort repo name for the notification, derived from the worktree cwd
/// (`<repo-root>/<worktree-dir>/<task>` → the repo-root basename).
fn notify_repo_name() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = cwd.parent()?.parent()?;
    root.file_name().map(|s| s.to_string_lossy().into_owned())
}

/// `+A -B` of the agent's uncommitted work vs HEAD, or `None` if nothing/unavailable.
fn diff_stats() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--numstat", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let (mut add, mut del) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split('\t');
        if let (Some(a), Some(d)) = (it.next(), it.next()) {
            add += a.parse::<u64>().unwrap_or(0);
            del += d.parse::<u64>().unwrap_or(0);
        }
    }
    (add != 0 || del != 0).then(|| format!("+{add} -{del}"))
}

/// Read all agent states for one repo.
pub fn read_states(repo: &str) -> Result<Vec<AgentState>> {
    let dir = match state_root() {
        Ok(r) => r.join(repo),
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some("order.json") {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(mut st) = serde_json::from_slice::<AgentState>(&bytes) {
                    if st.repo.is_empty() {
                        st.repo = repo.to_string();
                    }
                    out.push(st);
                }
            }
        }
    }
    Ok(out)
}

/// Read every agent's state across all repos (drives the global dashboard, `supervise`,
/// and the Telegram bridge). Each `AgentState.repo` is filled so callers can compute
/// the tmux session name.
pub fn read_all_states() -> Result<Vec<AgentState>> {
    let root = state_root()?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            if let Some(repo) = entry.file_name().to_str() {
                out.extend(read_states(repo)?);
            }
        }
    }
    Ok(out)
}

// ---- manual list ordering (per-repo, separate from state so hooks don't clobber it) ----

fn order_path(repo: &str) -> Result<PathBuf> {
    Ok(repo_dir(repo)?.join("order.json"))
}

/// task -> position; smaller = higher in the list. Tasks absent here sort last.
pub fn read_order(repo: &str) -> HashMap<String, u32> {
    let path = match order_path(repo) {
        Ok(p) => p,
        Err(_) => return HashMap::new(),
    };
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn write_order(repo: &str, map: &HashMap<String, u32>) -> Result<()> {
    let path = order_path(repo)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    atomic_write(&path, &serde_json::to_vec_pretty(map)?)?;
    Ok(())
}

pub fn install_hooks(global: bool) -> Result<()> {
    let self_path = std::env::current_exe().context("cannot resolve own path")?;
    let self_str = self_path.to_string_lossy();
    let target = if global {
        dirs::home_dir().context("no home dir")?.join(".claude/settings.json")
    } else {
        crate::worktree::repo_root()?.join(".claude/settings.json")
    };

    // Fail CLOSED: never clobber an existing settings.json we can't parse — it may
    // hold permissions.deny / env / model / non-wta hooks we'd silently destroy.
    let mut root: serde_json::Value = if target.exists() {
        let bytes = std::fs::read(&target)?;
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} exists but is not valid JSON — refusing to overwrite it. Fix or move it, then re-run.",
                target.display()
            )
        })?
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        bail!("{} is valid JSON but not an object — refusing to overwrite it.", target.display());
    }
    let hooks = root.as_object_mut().unwrap().entry("hooks").or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    for (event, state) in [("UserPromptSubmit", "running"), ("Notification", "needs_input"), ("Stop", "waiting")] {
        let cmd = format!("{self_str} status {state}");
        let arr = hooks.entry(event).or_insert_with(|| serde_json::json!([]));
        if !arr.is_array() {
            *arr = serde_json::json!([]);
        }
        let list = arr.as_array_mut().unwrap();
        let already = list.iter().any(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()) == Some(cmd.as_str())))
                .unwrap_or(false)
        });
        if !already {
            list.push(serde_json::json!({ "hooks": [{ "type": "command", "command": cmd }] }));
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    atomic_write(&target, &serde_json::to_vec_pretty(&root)?)?;
    println!("wrote wta hooks into {}", target.display());
    Ok(())
}
