//! Per-agent token usage + an estimated dollar cost, read from Claude Code's
//! transcripts at `~/.claude/projects/<encoded-worktree-path>/*.jsonl` (the dir name
//! is the worktree path with every non-alphanumeric char replaced by `-`). **Tokens
//! are ground truth**; the `$` is an ESTIMATE from a built-in price table and is
//! labeled as such everywhere it's shown. Zero for non-claude agents.

use serde::Deserialize;
use std::path::Path;

#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub est_usd: f64,
    pub messages: u64,
}

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.est_usd += o.est_usd;
        self.messages += o.messages;
    }
    pub fn is_zero(&self) -> bool {
        self.messages == 0
    }
}

/// USD per million tokens (input, output). Anthropic cache pricing is standard:
/// cache-write = 1.25× input, cache-read = 0.10× input. Unknown models fall back to
/// a sonnet-class estimate. Prices drift — this is an estimate, not a bill.
fn price(model: &str) -> (f64, f64) {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        (15.0, 75.0)
    } else if m.contains("haiku") {
        (0.80, 4.0)
    } else {
        (3.0, 15.0) // sonnet / fable / unknown
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<Msg>,
}
#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Use>,
}
#[derive(Deserialize, Default)]
struct Use {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn encode(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Total usage for an agent's worktree, summed across all its Claude Code sessions.
pub fn for_worktree(wt: &Path) -> Usage {
    let mut total = Usage::default();
    let dir = match dirs::home_dir() {
        Some(h) => h.join(".claude/projects").join(encode(wt)),
        None => return total,
    };
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return total,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines() {
            let l: Line = match serde_json::from_str(line) {
                Ok(l) => l,
                Err(_) => continue,
            };
            if l.kind != "assistant" {
                continue;
            }
            let u = match l.message.and_then(|m| m.usage.map(|u| (m.model, u))) {
                Some(v) => v,
                None => continue,
            };
            let (model, u) = u;
            if u.input_tokens == 0 && u.output_tokens == 0 && u.cache_creation_input_tokens == 0 && u.cache_read_input_tokens == 0 {
                continue; // synthetic / empty
            }
            let (pin, pout) = price(model.as_deref().unwrap_or(""));
            total.est_usd += (u.input_tokens as f64 * pin
                + u.cache_creation_input_tokens as f64 * pin * 1.25
                + u.cache_read_input_tokens as f64 * pin * 0.10
                + u.output_tokens as f64 * pout)
                / 1_000_000.0;
            total.input += u.input_tokens;
            total.output += u.output_tokens;
            total.cache_write += u.cache_creation_input_tokens;
            total.cache_read += u.cache_read_input_tokens;
            total.messages += 1;
        }
    }
    total
}

pub fn human_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Compact render, e.g. `~$0.42 · 1.3M tok`.
pub fn short(u: &Usage) -> String {
    format!("~${:.2} · {} tok", u.est_usd, human_tokens(u.tokens()))
}
