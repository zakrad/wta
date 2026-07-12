//! Scheduled agent dispatch — "routines" that fire `wta new` on a cadence so a fleet
//! can work unattended ("work while you sleep"). Routines live in
//! `~/.wta/routines.json`. Keep the scheduler alive with `wta cron start` (in a tmux
//! pane / nohup), or wire `wta cron tick` into system cron / launchd.
//!
//! Run ONE scheduler — either `wta cron start` OR `tick` from system cron, not both:
//! there's no cross-process lock, so two schedulers ticking at once could double-fire
//! a routine in the same second (the per-routine concurrency cap below then prevents
//! any further pile-up).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Schedule {
    /// Fire every `secs` seconds (measured from the previous run).
    Every { secs: u64 },
}

#[derive(Serialize, Deserialize, Clone)]
struct Routine {
    name: String,
    schedule: Schedule,
    repo: String,
    #[serde(default)]
    prompt: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    last_run_unix: u64,
}

impl Routine {
    fn interval(&self) -> u64 {
        match &self.schedule {
            Schedule::Every { secs } => *secs,
        }
    }
    fn is_due(&self, now: u64) -> bool {
        self.enabled && (self.last_run_unix == 0 || now.saturating_sub(self.last_run_unix) >= self.interval())
    }
}

fn default_true() -> bool {
    true
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn routines_path() -> Result<PathBuf> {
    let dir = crate::status::wta_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("routines.json"))
}

fn load() -> Result<Vec<Routine>> {
    match std::fs::read(routines_path()?) {
        Ok(b) if b.iter().all(|c| c.is_ascii_whitespace()) => Ok(Vec::new()),
        // Refuse to proceed on a corrupt file rather than returning empty — a later
        // save() would otherwise overwrite it and silently lose every routine.
        Ok(b) => serde_json::from_slice(&b)
            .context("~/.wta/routines.json is corrupt — fix or delete it (refusing to overwrite it)"),
        Err(_) => Ok(Vec::new()),
    }
}

fn save(rs: &[Routine]) -> Result<()> {
    let p = routines_path()?;
    // Per-process temp name so two concurrent writers never share the same tmp file.
    let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(rs)?)?;
    if let Err(e) = std::fs::rename(&tmp, &p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("saving routines.json");
    }
    Ok(())
}

/// Parse `30s` / `15m` / `2h` / `1d` (bare number = seconds) into seconds.
fn parse_dur(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86400)
    } else {
        (s, 1)
    };
    let v: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("bad duration '{s}' — use e.g. 30m, 2h, 1d"))?;
    let secs = v.checked_mul(mult).context("duration too large")?;
    if secs == 0 {
        bail!("duration must be greater than zero");
    }
    Ok(secs)
}

fn human_dur(mut secs: u64) -> String {
    if secs == 0 {
        return "0s".into();
    }
    let d = secs / 86400;
    secs %= 86400;
    let h = secs / 3600;
    secs %= 3600;
    let m = secs / 60;
    let s = secs % 60;
    let mut out = String::new();
    for (n, unit) in [(d, 'd'), (h, 'h'), (m, 'm'), (s, 's')] {
        if n > 0 {
            out.push_str(&format!("{n}{unit}"));
        }
    }
    out
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn add(name: &str, every: &str, repo: Option<PathBuf>, prompt_args: &[String]) -> Result<()> {
    if !valid_name(name) {
        bail!("routine name must be letters/digits/-/_ (≤64 chars)");
    }
    let secs = parse_dur(every)?;
    // Floor the cadence: a routine spawns a whole agent each fire, so sub-minute
    // intervals would flood the fleet (and burn tokens).
    if secs < 60 {
        bail!("--every must be at least 60s — a routine spawns an agent each run");
    }
    let repo = match repo {
        Some(p) => p,
        None => crate::worktree::repo_root().context("run inside a repo, or pass --repo <path>")?,
    };
    let repo = std::fs::canonicalize(&repo).unwrap_or(repo);
    if !repo.join(".git").exists() {
        bail!("{} is not a git repo", repo.display());
    }
    let mut rs = load()?;
    if rs.iter().any(|r| r.name == name) {
        bail!("routine '{name}' already exists — `wta cron rm {name}` first");
    }
    rs.push(Routine {
        name: name.to_string(),
        schedule: Schedule::Every { secs },
        repo: repo.to_string_lossy().into_owned(),
        prompt: prompt_args.join(" "),
        enabled: true,
        last_run_unix: 0,
    });
    save(&rs)?;
    println!("added routine '{name}' — every {} in {}", human_dur(secs), repo.display());
    println!("run `wta cron start` (leave it running) to fire it, or wire `wta cron tick` into cron/launchd");
    Ok(())
}

pub fn list() -> Result<()> {
    let rs = load()?;
    if rs.is_empty() {
        println!("no routines — add one with `wta cron add <name> --every <dur> -- <prompt>`");
        return Ok(());
    }
    let now = now_unix();
    println!("{:<18} {:<8} {:<10} {:<12} repo", "NAME", "EVERY", "NEXT", "LAST");
    for r in &rs {
        let next = if !r.enabled {
            "disabled".to_string()
        } else if r.is_due(now) {
            "due now".to_string()
        } else {
            format!("in {}", human_dur((r.last_run_unix + r.interval()).saturating_sub(now)))
        };
        let last = if r.last_run_unix == 0 {
            "never".to_string()
        } else {
            format!("{} ago", human_dur(now.saturating_sub(r.last_run_unix)))
        };
        println!("{:<18} {:<8} {:<10} {:<12} {}", r.name, human_dur(r.interval()), next, last, r.repo);
    }
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    let mut rs = load()?;
    let before = rs.len();
    rs.retain(|r| r.name != name);
    if rs.len() == before {
        bail!("no routine named '{name}'");
    }
    save(&rs)?;
    println!("removed routine '{name}'");
    Ok(())
}

pub fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    let mut rs = load()?;
    let r = rs.iter_mut().find(|r| r.name == name).with_context(|| format!("no routine named '{name}'"))?;
    r.enabled = enabled;
    save(&rs)?;
    println!("{} routine '{name}'", if enabled { "enabled" } else { "disabled" });
    Ok(())
}

/// Fire every due routine once. Returns the number fired.
pub fn tick() -> Result<usize> {
    let mut rs = load()?;
    let now = now_unix();
    if now == 0 {
        return Ok(0); // clock unreadable — skip rather than treat as "never run"
    }
    let live = crate::tmux::list_sessions();

    // Pick the routines to fire: due, AND with no agent of their own still around
    // (per-routine concurrency cap of 1 — so a routine can't pile up a fleet, and a
    // long-running agent blocks the next fire until you've reviewed/removed it).
    let mut to_fire = Vec::new();
    for (i, r) in rs.iter().enumerate() {
        if !r.is_due(now) {
            continue;
        }
        let prefix = format!("{}-", crate::tmux::session_name(&crate::worktree::repo_id_of(std::path::Path::new(&r.repo)), &r.name));
        if live.iter().any(|s| s.starts_with(&prefix)) {
            println!("[cron] skip '{}' — its previous agent is still around (remove it to let it fire again)", r.name);
            continue;
        }
        to_fire.push(i);
    }
    if to_fire.is_empty() {
        return Ok(0);
    }

    // Record the fire durably BEFORE spawning anything (at-most-once): if the save or
    // the process dies now, this cycle is simply skipped — never re-fired as a
    // duplicate autonomous agent next tick.
    for &i in &to_fire {
        rs[i].last_run_unix = now;
    }
    save(&rs)?;

    let mut fired = 0;
    for &i in &to_fire {
        match dispatch(&rs[i]) {
            Ok(task) => {
                println!("[cron] fired '{}' → agent '{}'", rs[i].name, task);
                fired += 1;
            }
            Err(e) => eprintln!("[cron] '{}' failed to dispatch: {e}", rs[i].name),
        }
    }
    Ok(fired)
}

/// Spawn a fresh agent for one routine by shelling out to `wta new` in its repo, so
/// it reuses all of `new`'s worktree/agent logic. Returns the new agent's task name.
fn dispatch(r: &Routine) -> Result<String> {
    let repo = PathBuf::from(&r.repo);
    if !repo.exists() {
        bail!("repo path is gone: {}", repo.display());
    }
    let task = format!("{}-{}", r.name, now_unix());
    let exe = std::env::current_exe().context("cannot resolve wta binary path")?;
    let mut cmd = std::process::Command::new(exe);
    // Null stdin so a setup.sh that reads stdin gets EOF instead of blocking when the
    // scheduler runs in a terminal. (The spawned agent gets its own pty from tmux, so
    // this doesn't affect it — verified the session still persists.)
    cmd.current_dir(&repo).stdin(std::process::Stdio::null()).arg("new").arg(&task);
    if !r.prompt.trim().is_empty() {
        cmd.arg("--").arg(&r.prompt);
    }
    let status = cmd.status().context("spawning `wta new`")?;
    if !status.success() {
        bail!("`wta new` exited with {status}");
    }
    Ok(task)
}

/// Run the scheduler loop in the foreground, checking every `interval` seconds.
pub fn start(interval: u64) -> Result<()> {
    let interval = interval.max(5);
    let n = load()?.len();
    println!("wta cron: scheduling {n} routine(s), checking every {}s — Ctrl-C to stop", interval);
    loop {
        if let Err(e) = tick() {
            eprintln!("[cron] tick error: {e}");
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}
