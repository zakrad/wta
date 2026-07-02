use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata sidecar for an agent — the `dash` reads these for the (optional)
/// hook-driven `needs_input`/`waiting` states. Liveness/preview come from tmux.
#[derive(Serialize, Deserialize, Clone)]
pub struct AgentState {
    pub task: String,
    pub status: String, // running | needs_input | waiting | ...
    pub cwd: String,
    pub updated_unix: u64,
}

pub fn wta_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot locate home directory")?;
    Ok(home.join(".wta"))
}

pub fn state_dir() -> Result<PathBuf> {
    let d = wta_dir()?.join("state");
    std::fs::create_dir_all(&d).ok();
    Ok(d)
}

pub fn state_path(task: &str) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("{task}.json")))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save(st: &AgentState) -> Result<()> {
    let final_path = state_path(&st.task)?;
    let tmp = final_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
    std::fs::rename(&tmp, &final_path)?; // atomic
    Ok(())
}

/// Record state directly (used by `wta new`/`resume`).
pub fn record(task: &str, status: &str, cwd: &str) -> Result<()> {
    save(&AgentState {
        task: task.to_string(),
        status: status.to_string(),
        cwd: cwd.to_string(),
        updated_unix: now_unix(),
    })
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
pub fn emit(state: &str) -> Result<()> {
    let task = std::env::var("WTA_TASK").unwrap_or_default();
    // OSC 1337 user-var (optional terminal-tab integration; harmless elsewhere)
    emit_uservar("agent_status", state).ok();
    if !task.is_empty() {
        emit_uservar("agent_task", &task).ok();
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        record(&task, state, &cwd)?;
    }
    Ok(())
}

// ---- manual list ordering (persisted separately from state so hooks don't clobber it) ----

fn order_path() -> Result<PathBuf> {
    Ok(wta_dir()?.join("order.json"))
}

/// task -> position; smaller = higher in the list. Tasks absent here sort last.
pub fn read_order() -> HashMap<String, u32> {
    let path = match order_path() {
        Ok(p) => p,
        Err(_) => return HashMap::new(),
    };
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn write_order(map: &HashMap<String, u32>) -> Result<()> {
    let path = order_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(map)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_all_states() -> Result<Vec<AgentState>> {
    let dir = state_dir()?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(st) = serde_json::from_slice::<AgentState>(&bytes) {
                    out.push(st);
                }
            }
        }
    }
    Ok(out)
}

pub fn install_hooks(global: bool) -> Result<()> {
    let self_path = std::env::current_exe().context("cannot resolve own path")?;
    let self_str = self_path.to_string_lossy();
    let target = if global {
        dirs::home_dir()
            .context("no home dir")?
            .join(".claude/settings.json")
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
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    for (event, state) in [
        ("UserPromptSubmit", "running"),
        ("Notification", "needs_input"),
        ("Stop", "waiting"),
    ] {
        let cmd = format!("{self_str} status {state}");
        // Append to (not overwrite) the event's hook array, preserving any hooks
        // the user already configured. Idempotent: skip if our command is present.
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
