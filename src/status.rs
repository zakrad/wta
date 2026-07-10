use anyhow::{Context, Result};
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
    pub updated_unix: u64,
    #[serde(default)]
    pub index: u32, // stable isolation slot (WTA_INDEX / WTA_PORT_BASE), assigned at creation
    #[serde(default)]
    pub context: Vec<String>, // files injected at `new`, unstaged again by `push`
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
    // sanitize like tmux::session_name so the state filename never diverges from
    // the session name or escapes the repo's state dir (belt-and-suspenders on top
    // of worktree::validate_task)
    let safe: String = task
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
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

fn save(st: &AgentState) -> Result<()> {
    let final_path = state_path(&st.repo, &st.task)?;
    let tmp = final_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
    std::fs::rename(&tmp, &final_path)?; // atomic
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
    });
    st.task = task.to_string();
    st.repo = repo.to_string();
    st.index = index;
    st.context = context.to_vec();
    save(&st)
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
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        record(&repo, &task, state, &cwd)?;
        // Fire the desktop banner + sound straight from the hook, so an agent
        // finishing (Stop → "waiting") or asking a question (Notification →
        // "needs_input") alerts you regardless of the dashboard — even while you're
        // attached inside it. Gated on WTA_TASK/WTA_REPO so a plain `claude` session
        // that merely inherits the global hooks never notifies.
        notify_for_state(state, &task);
    }
    Ok(())
}

/// Banner text for the hook-driven states we alert on. Returns `None` for states
/// (like "running") that shouldn't notify.
fn notify_for_state(state: &str, task: &str) {
    let label = if task.is_empty() { "agent".to_string() } else { task.to_string() };
    let title = match notify_repo_name() {
        Some(r) => format!("wta · {r}"),
        None => "wta".to_string(),
    };
    let body = match state {
        "waiting" => format!("{label} finished — ready for you"),
        "needs_input" => format!("{label} needs your input"),
        _ => return,
    };
    crate::notify::alert(&title, &body);
}

/// Best-effort repo name for the notification title, derived from the worktree cwd
/// (`<repo-root>/<worktree-dir>/<task>` → the repo-root basename).
fn notify_repo_name() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = cwd.parent()?.parent()?;
    root.file_name().map(|s| s.to_string_lossy().into_owned())
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

/// Read every agent's state across all repos (used by the Telegram bridge). Each
/// `AgentState.repo` is filled so callers can compute the tmux session name.
#[cfg_attr(not(feature = "telegram"), allow(dead_code))]
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
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(map)?)?;
    std::fs::rename(&tmp, &path)?;
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

    let mut root: serde_json::Value = if target.exists() {
        serde_json::from_slice(&std::fs::read(&target)?).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        root = serde_json::json!({});
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
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, &target)?;
    println!("wrote wta hooks into {}", target.display());
    Ok(())
}
