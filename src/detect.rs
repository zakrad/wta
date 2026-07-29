//! Agent-agnostic runtime status detection ("screen manifests").
//!
//! wta launches any agent CLI (`WTA_AGENT_CMD` — claude/codex/gemini/…), but only
//! Claude reports its state through hooks. For every other engine the dashboard fell
//! back to a pane-hash heuristic (pane changed = working, unchanged = ready) that
//! can't tell "waiting for you" from "done". This module classifies the pane by
//! pattern-matching its last lines against a per-engine rule set, so a non-Claude
//! agent still gets a live needs-input / working glyph without hooks.
//!
//! Rules are normalized-lowercase substrings (so pane wrapping / padding / case don't
//! defeat a match). Built-ins ship for common engines; a user can extend any engine
//! with `~/.wta/detect/<engine>.json` (`{"needs_input": [...], "working": [...]}` —
//! patterns are APPENDED to the built-ins, never replace them). Detection never sends
//! keys — a wrong guess is only a wrong glyph, self-correcting on the next tick — so
//! Claude's `needs_input` hook stays the authoritative signal where it exists.

use serde::Deserialize;

/// What the pane content says the agent is doing. `None` from [`classify`] means
/// "no opinion" — the caller falls back to its pane-hash heuristic.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PaneState {
    /// blocked on a question / permission / confirmation — wants a human
    NeedsInput,
    /// actively working (spinner / "esc to interrupt" / thinking)
    Working,
}

#[derive(Default, Deserialize)]
struct Rules {
    #[serde(default)]
    needs_input: Vec<String>,
    #[serde(default)]
    working: Vec<String>,
}

impl Rules {
    fn extend(&mut self, other: Rules) {
        self.needs_input.extend(other.needs_input);
        self.working.extend(other.working);
    }
    fn lowercased(mut self) -> Self {
        for v in [&mut self.needs_input, &mut self.working] {
            for p in v.iter_mut() {
                *p = p.to_lowercase();
            }
        }
        self
    }
}

/// Patterns common to most coding-agent TUIs. Conservative on `needs_input` (a false
/// positive is a wrong glyph, but we still keep it tight) — mirrors the dialog wording
/// [`crate::tmux::looks_interactive_dialog`] already trusts to gate relays.
fn generic() -> Rules {
    Rules {
        needs_input: [
            "do you want to",
            "(y/n)",
            "[y/n]",
            "(yes/no)",
            "press enter to continue",
            "waiting for your",
            // selection-menu pointer at the first option — prompt-specific, so a plain
            // numbered list in normal output (e.g. "1. yes we should…") doesn't false-fire
            "❯ 1",
            "│ 1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        working: [
            "esc to interrupt",
            "esc to cancel",
            "ctrl+c to stop",
            "thinking",
            "working…",
            "generating",
            // common braille spinner frames (bottom status line)
            "⠋",
            "⠙",
            "⠹",
            "⠸",
            "⠼",
            "⠴",
            "⠦",
            "⠧",
            "⠇",
            "⠏",
            "⣾",
            "⣽",
            "⣻",
            "⢿",
            "⡿",
            "⣟",
            "⣯",
            "⣷",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    }
}

/// Engine-specific extras (by CLI basename). Bonus signal on top of [`generic`];
/// users add more via `~/.wta/detect/<engine>.json`.
fn engine_extra(engine: &str) -> Rules {
    let (ni, wk): (&[&str], &[&str]) = match engine {
        "claude" => (
            &[
                "no, and tell claude",
                "do you want to proceed",
                "i trust this folder",
            ],
            &["✻", "✽", "✢"],
        ),
        "codex" => (
            &["allow command", "run this command?", "approve"],
            &["codex is working"],
        ),
        "gemini" => (
            &[
                "waiting for confirmation",
                "apply this change?",
                "do you want to proceed",
            ],
            &["gemini is thinking"],
        ),
        "aider" => (&["apply edits?", "add these files"], &[]),
        _ => (&[], &[]),
    };
    Rules {
        needs_input: ni.iter().map(|s| s.to_string()).collect(),
        working: wk.iter().map(|s| s.to_string()).collect(),
    }
}

/// Basename of an engine command (`/usr/bin/claude-1.2` → `claude-1.2`), then trimmed
/// to its leading alpha run so wrappers still match a family (`claude.sh` → `claude`).
fn engine_key(engine: &str) -> String {
    let base = engine.rsplit(['/', '\\']).next().unwrap_or(engine);
    let alpha: String = base
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if alpha.is_empty() {
        base.to_lowercase()
    } else {
        alpha.to_lowercase()
    }
}

/// Load a user override `~/.wta/detect/<engine>.json`, if present and parseable.
fn load_override(engine: &str) -> Option<Rules> {
    let path = crate::status::wta_dir()
        .ok()?
        .join("detect")
        .join(format!("{engine}.json"));
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Rules>(&bytes).ok()
}

fn resolve(engine: Option<&str>) -> Rules {
    let key = engine.map(engine_key).unwrap_or_default();
    let mut rules = generic();
    if !key.is_empty() {
        rules.extend(engine_extra(&key));
        if let Some(user) = load_override(&key) {
            rules.extend(user);
        }
    }
    rules.lowercased()
}

/// The last `n` non-empty lines of the pane, normalized + lowercased into one haystack.
/// We look at the tail because the freshest signal (spinner / prompt) is at the bottom.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    crate::tmux::norm(&lines[start..].join("\n")).to_lowercase()
}

/// Classify the pane for `engine`. `needs_input` wins over `working`; no match → `None`
/// (caller keeps its pane-hash fallback). Case/whitespace-insensitive.
pub fn classify(engine: Option<&str>, text: &str) -> Option<PaneState> {
    let hay = tail(text, 15);
    if hay.is_empty() {
        return None;
    }
    let rules = resolve(engine);
    if rules.needs_input.iter().any(|p| hay.contains(p.as_str())) {
        return Some(PaneState::NeedsInput);
    }
    if rules.working.iter().any(|p| hay.contains(p.as_str())) {
        return Some(PaneState::Working);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_needs_input_and_working() {
        assert_eq!(
            classify(None, "some output\nDo you want to proceed? (y/n)"),
            Some(PaneState::NeedsInput)
        );
        assert_eq!(
            classify(None, "building...\n⠹ esc to interrupt"),
            Some(PaneState::Working)
        );
        assert_eq!(classify(None, "$ done\nnothing special here"), None);
    }

    #[test]
    fn needs_input_beats_working() {
        // both a spinner and a prompt on screen → the prompt (needs you) wins
        let text = "⠹ working\n❯ 1. Yes\n  2. No";
        assert_eq!(classify(Some("claude"), text), Some(PaneState::NeedsInput));
    }

    #[test]
    fn engine_specific_pattern_matches() {
        assert_eq!(
            classify(Some("codex"), "Allow command: rm -rf /?"),
            Some(PaneState::NeedsInput)
        );
        // a wrapper basename still resolves to the engine family
        assert_eq!(
            classify(Some("/opt/bin/codex-0.9"), "codex is working on it"),
            Some(PaneState::Working)
        );
    }

    #[test]
    fn engine_key_trims_path_and_version() {
        assert_eq!(engine_key("/usr/local/bin/claude"), "claude");
        assert_eq!(engine_key("claude.sh"), "claude");
        assert_eq!(engine_key("codex-0.9.1"), "codex");
    }

    #[test]
    fn empty_pane_is_no_opinion() {
        assert_eq!(classify(Some("claude"), "   \n  \n"), None);
    }
}
