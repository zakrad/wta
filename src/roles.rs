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
    match std::fs::read(path) {
        // Absent is fine (no config). A present-but-invalid file must NOT be silently
        // discarded — that would quietly drop every role's configured model/effort.
        Err(_) => HashMap::new(),
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("wta: ignoring {} — not valid JSON ({e})", path.display());
                HashMap::new()
            }
        },
    }
}

/// A model name / effort level must be a single flag-free token. Anything else (a
/// value with spaces, or one starting with `-`) would split into extra `claude` argv
/// tokens when the command is re-tokenized — an arg-injection that, with
/// `--dangerously-skip-permissions` on by default, lets an untrusted repo's
/// `.wta/roles.json` smuggle `--mcp-config`/`--settings`/… onto every agent. Reject it.
fn safe_token(v: Option<String>) -> Option<String> {
    let t = v?.trim().to_string();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('-') || t.chars().any(char::is_whitespace) {
        eprintln!("wta: ignoring model/effort value {t:?} — it must be a single token (no spaces, no leading '-')");
        return None;
    }
    Some(t)
}

/// Program is the `claude` CLI (by basename, tolerating wrappers like `claude.sh` /
/// `claude-1.2` / an absolute path). The single source of truth for "is this claude"
/// across the crate — gates `--model`/`--effort`, `--dangerously-skip-permissions`,
/// and the folder-trust pre-seed, which must all agree on the same command.
pub(crate) fn is_claude(prog: &str) -> bool {
    let base = prog.rsplit('/').next().unwrap_or(prog);
    base == "claude" || base.starts_with("claude-") || base.starts_with("claude.")
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
    let eq = format!("{flag}=");
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == flag {
            i += 2; // "--model x" — drop the flag and its separate value
            continue;
        }
        if toks[i].starts_with(&eq) {
            i += 1; // "--model=x" — value is glued, drop just this token
            continue;
        }
        out.push(toks[i]);
        i += 1;
    }
    out.join(" ")
}

/// Resolve the agent command for `role`. `base` is the role's fallback command
/// (e.g. `WTA_AGENT_CMD`). Returns (command, a one-line note of what won).
pub fn resolve(role: &str, cli_cmd: Option<&str>, cli_model: Option<&str>, cli_effort: Option<&str>, base: &str, root: Option<&Path>) -> (String, String) {
    let (g, r) = configs(role, root);
    let up = role.to_uppercase();
    let env = |suf: &str| std::env::var(format!("WTA_{up}_{suf}")).ok().filter(|s| !s.trim().is_empty());

    // Base command precedence: explicit CLI command (e.g. `wta review --by`) > global
    // config `cmd` > the role's base. A REPO config must never choose which binary
    // runs (a pulled repo could point it anywhere) — its `cmd` is refused.
    if r.cmd.is_some() {
        eprintln!("wta: ignoring `cmd` for role '{role}' from the repo's .wta/roles.json (only global config / env / --by may set a command)");
    }
    let base_cmd = cli_cmd
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| g.cmd.clone().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| base.to_string());

    // model/effort: highest non-empty, *validated* value wins — an empty or unsafe
    // value at any tier falls through to the next (never shadows, never injects).
    let model = safe_token(cli_model.map(str::to_string))
        .or_else(|| safe_token(env("MODEL")))
        .or_else(|| safe_token(r.model.clone()))
        .or_else(|| safe_token(g.model.clone()));
    let effort = safe_token(cli_effort.map(str::to_string))
        .or_else(|| safe_token(env("EFFORT")))
        .or_else(|| safe_token(r.effort.clone()))
        .or_else(|| safe_token(g.effort.clone()));

    let prog = base_cmd.split_whitespace().next().unwrap_or("");
    let claude = is_claude(prog);
    let mut cmd = base_cmd.clone();
    if claude {
        if let Some(m) = &model {
            cmd = strip_flag(&cmd, "--model");
            cmd.push_str(&format!(" --model {m}"));
        }
        if let Some(e) = &effort {
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
        let (cmd, note) = resolve(role, None, None, None, &base, root);
        println!("  {role:<9} {cmd}");
        println!("            {note}");
    }
    println!("\nconfig: ~/.wta/roles.json (global) + <repo>/.wta/roles.json (model/effort only)");
    println!("  e.g.  {{ \"worker\": {{ \"model\": \"opus-4.8\", \"effort\": \"high\" }},");
    println!("          \"reviewer\": {{ \"model\": \"sonnet-5\", \"effort\": \"medium\" }} }}");
}
