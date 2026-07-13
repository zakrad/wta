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
    timestamp: Option<String>,
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

/// One assistant message's usage in time order — the raw material for a per-agent
/// spend-over-time chart, model-change tracking, and cross-agent comparison. All from
/// the transcript Claude Code already writes (no polling / background tracking).
#[derive(serde::Serialize, Clone)]
pub struct Sample {
    pub ts: String,
    pub model: String,
    pub delta_tokens: u64,
    pub delta_usd: f64,
    pub cum_tokens: u64,
    pub cum_usd: f64,
}

/// The agent's full spend timeline, one entry per assistant message, sorted by time
/// (merged across resumed sessions), with running cumulative totals.
pub fn timeline(wt: &Path) -> Vec<Sample> {
    let dir = match dirs::home_dir() {
        Some(h) => h.join(".claude/projects").join(encode(wt)),
        None => return Vec::new(),
    };
    let mut raw: Vec<(String, String, u64, f64)> = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
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
            let ts = l.timestamp.unwrap_or_default();
            let (model, u) = match l.message.and_then(|m| m.usage.map(|u| (m.model.unwrap_or_default(), u))) {
                Some(v) => v,
                None => continue,
            };
            let tok = u.input_tokens + u.output_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
            if tok == 0 {
                continue;
            }
            let (pin, pout) = price(&model);
            let usd = (u.input_tokens as f64 * pin
                + u.cache_creation_input_tokens as f64 * pin * 1.25
                + u.cache_read_input_tokens as f64 * pin * 0.10
                + u.output_tokens as f64 * pout)
                / 1_000_000.0;
            raw.push((ts, model, tok, usd));
        }
    }
    raw.sort_by(|a, b| a.0.cmp(&b.0)); // ISO timestamps sort lexically = chronological
    let (mut ct, mut cu) = (0u64, 0.0);
    raw.into_iter()
        .map(|(ts, model, tok, usd)| {
            ct += tok;
            cu += usd;
            Sample { ts, model, delta_tokens: tok, delta_usd: usd, cum_tokens: ct, cum_usd: cu }
        })
        .collect()
}

/// A unicode sparkline of `vals`, bucketed to at most `width` columns (each column is
/// the SUM of its bucket — so it shows *where* spend happened, not a monotone ramp).
pub fn sparkline(vals: &[f64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if vals.is_empty() || width == 0 {
        return String::new();
    }
    let cols = width.min(vals.len()).max(1);
    let mut buckets = vec![0.0f64; cols];
    for (i, v) in vals.iter().enumerate() {
        buckets[i * cols / vals.len()] += *v;
    }
    let max = buckets.iter().cloned().fold(0.0, f64::max).max(1e-12);
    buckets
        .iter()
        .map(|b| {
            let lvl = ((b / max) * 7.0).round() as usize;
            BARS[lvl.min(7)]
        })
        .collect()
}

/// A multi-row vertical bar chart of `values` bucketed into `width` columns (each
/// column = the SUM of its bucket), `height` rows tall. Uses eighth-block glyphs for
/// sub-row resolution. Returns `height` strings, top row first. `max` (returned) is
/// the value of a full-height column, for the Y-axis label.
pub fn barchart(values: &[f64], width: usize, height: usize) -> (Vec<String>, f64) {
    const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() || width == 0 || height == 0 {
        return (Vec::new(), 0.0);
    }
    let cols = width.min(values.len()).max(1);
    let mut buckets = vec![0.0f64; cols];
    for (i, v) in values.iter().enumerate() {
        buckets[i * cols / values.len()] += *v;
    }
    let max = buckets.iter().cloned().fold(0.0, f64::max).max(1e-12);
    let mut rows = Vec::with_capacity(height);
    for r in 0..height {
        let row_from_bottom = (height - 1 - r) as i64; // 0 = bottom row
        let line: String = buckets
            .iter()
            .map(|b| {
                let filled8 = ((b / max) * height as f64 * 8.0).round() as i64;
                let in_cell = filled8 - row_from_bottom * 8; // eighths filled in this cell
                if in_cell >= 8 {
                    '█'
                } else if in_cell <= 0 {
                    ' '
                } else {
                    BARS[in_cell as usize]
                }
            })
            .collect();
        rows.push(line);
    }
    (rows, max)
}

/// The model changes across a timeline, as `(model, first_ts, message_count)` runs.
pub fn model_runs(tl: &[Sample]) -> Vec<(String, String, u64)> {
    let mut runs: Vec<(String, String, u64)> = Vec::new();
    for s in tl {
        match runs.last_mut() {
            Some(r) if r.0 == s.model => r.2 += 1,
            _ => runs.push((s.model.clone(), s.ts.clone(), 1)),
        }
    }
    runs
}
