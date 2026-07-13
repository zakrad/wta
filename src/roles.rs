//! Per-role model/effort resolution. Each role (worker, reviewer) resolves to an
//! agent command with precedence, highest first:
//!   CLI flag (--model/--effort) > env (WTA_<ROLE>_MODEL / _EFFORT) >
//!   repo config (<repo>/.wta/roles.json, model/effort only) >
//!   global config (~/.wta/roles.json) > the role's base command.
//! `--model`/`--effort` are appended only when the resolved program is `claude`
//! (Claude Code 2.1+ launch flags); values pass through verbatim so a newer vocab
//! (e.g. a new effort level) just works. Config is JSON (no new parser dep).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Default, Clone)]
struct RoleCfg {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    cmd: Option<String>,
}

fn load(path: &Path) -> HashMap<String, RoleCfg> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn configs(role: &str, root: Option<&Path>) -> (RoleCfg, RoleCfg) {
    let global = dirs::home_dir().map(|h| load(&h.join(".wta/roles.json"))).unwrap_or_default();
    let repo = root.map(|r| load(&r.join(".wta/roles.json"))).unwrap_or_default();
    (
        global.get(role).cloned().unwrap_or_default(),
        repo.get(role).cloned().unwrap_or_default(),
    )
}

/// Remove `flag` and the token after it from a space-separated command, so a
/// higher-precedence value can replace it cleanly (no duplicate `--model`).
fn strip_flag(cmd: &str, flag: &str) -> String {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == flag {
            i += 2; // drop the flag and its value
            continue;
        }
        out.push(toks[i]);
        i += 1;
    }
    out.join(" ")
}

/// Resolve the agent command for `role`. `base` is the role's fallback command
/// (e.g. `WTA_AGENT_CMD`). Returns (command, a one-line note of what won).
pub fn resolve(role: &str, cli_model: Option<&str>, cli_effort: Option<&str>, base: &str, root: Option<&Path>) -> (String, String) {
    let (g, r) = configs(role, root);
    let up = role.to_uppercase();
    let env = |suf: &str| std::env::var(format!("WTA_{up}_{suf}")).ok().filter(|s| !s.trim().is_empty());

    // A repo config must NOT choose which binary runs (a pulled repo could point it
    // at anything) — only the global config's `cmd` may override the base.
    if r.cmd.is_some() {
        eprintln!("wta: ignoring `cmd` for role '{role}' from the repo's .wta/roles.json (only global config / env may set a command)");
    }
    let base_cmd = g.cmd.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| base.to_string());

    let model = cli_model
        .map(str::to_string)
        .or_else(|| env("MODEL"))
        .or_else(|| r.model.clone())
        .or_else(|| g.model.clone());
    let effort = cli_effort
        .map(str::to_string)
        .or_else(|| env("EFFORT"))
        .or_else(|| r.effort.clone())
        .or_else(|| g.effort.clone());

    let prog = base_cmd.split_whitespace().next().unwrap_or("");
    let is_claude = prog.ends_with("claude");
    let mut cmd = base_cmd.clone();
    if is_claude {
        if let Some(m) = model.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            cmd = strip_flag(&cmd, "--model");
            cmd.push_str(&format!(" --model {m}"));
        }
        if let Some(e) = effort.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            cmd = strip_flag(&cmd, "--effort");
            cmd.push_str(&format!(" --effort {e}"));
        }
    } else if model.is_some() || effort.is_some() {
        eprintln!("wta: --model/--effort ignored — role '{role}' runs a non-claude agent ('{prog}')");
    }
    let note = format!(
        "base='{base_cmd}' model={} effort={}",
        model.as_deref().unwrap_or("(claude default)"),
        effort.as_deref().unwrap_or("(claude default)")
    );
    (cmd, note)
}

fn worker_base() -> String {
    std::env::var("WTA_AGENT_CMD").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "claude".into())
}
fn reviewer_base() -> String {
    std::env::var("WTA_REVIEW_AGENT_CMD").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(worker_base)
}

/// `wta roles` — print the resolved command per role (a dry-run + cost view).
pub fn print_roles(root: Option<&Path>) {
    println!("resolved agent command per role (a `--model`/`--effort` flag would override):\n");
    for (role, base) in [("worker", worker_base()), ("reviewer", reviewer_base())] {
        let (cmd, note) = resolve(role, None, None, &base, root);
        println!("  {role:<9} {cmd}");
        println!("            {note}");
    }
    println!("\nconfig: ~/.wta/roles.json (global) + <repo>/.wta/roles.json (model/effort only)");
    println!("  e.g.  {{ \"worker\": {{ \"model\": \"opus-4.8\", \"effort\": \"high\" }},");
    println!("          \"reviewer\": {{ \"model\": \"sonnet-5\", \"effort\": \"medium\" }} }}");
}
