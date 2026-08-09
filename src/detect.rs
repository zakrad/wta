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
    /// stuck on a TERMINAL error (auth/quota/login) — needs a human, and must never
    /// read as a successful idle. `wta wait` treats this as fail-closed.
    Error,
}

#[derive(Default, Deserialize)]
struct Rules {
    #[serde(default)]
    needs_input: Vec<String>,
    #[serde(default)]
    working: Vec<String>,
    #[serde(default)]
    error: Vec<String>,
    // A VETO list (herdr's `skip_state_update`): when the pane is showing something
    // that isn't a live agent turn — an editor overlay, a model picker, a pager — a
    // match here forces "no opinion" so a menu/scrollback isn't misread as idle/working.
    #[serde(default)]
    neutral: Vec<String>,
}

impl Rules {
    fn extend(&mut self, other: Rules) {
        self.needs_input.extend(other.needs_input);
        self.working.extend(other.working);
        self.error.extend(other.error);
        self.neutral.extend(other.neutral);
    }
    fn lowercased(mut self) -> Self {
        for v in [
            &mut self.needs_input,
            &mut self.working,
            &mut self.error,
            &mut self.neutral,
        ] {
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
        // TERMINAL errors only — an agent stuck here needs a human and did NOT finish
        // its turn. Kept deliberately narrow (auth / quota / login) so a transient,
        // self-retrying "overloaded"/"rate limit" does NOT fail-close a working agent.
        error: [
            "invalid api key",
            "authentication_error",
            "authentication failed",
            "login expired",
            "please run /login",
            "session expired",
            "usage limit reached",
            "quota exceeded",
            "insufficient_quota",
            "insufficient credit",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        // Veto: the pane is showing an overlay/menu, not a live agent turn — defer to the
        // caller's pane-hash heuristic instead of guessing. Kept unmistakable so a real
        // state is never suppressed; extend per engine via ~/.wta/detect/<engine>.json.
        neutral: [
            "-- insert --", // an editor (vim/nvim) opened in the pane
            "-- visual --",
            "-- normal --",
            "select a model", // a model-picker menu is open
            "switch model",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    }
}

/// Engine-specific extras (by CLI basename). Bonus signal on top of [`generic`];
/// users add more via `~/.wta/detect/<engine>.json`.
fn engine_extra(engine: &str) -> Rules {
    let (ni, wk, er): (&[&str], &[&str], &[&str]) = match engine {
        "claude" => (
            &[
                "no, and tell claude",
                "do you want to proceed",
                "i trust this folder",
            ],
            &["✻", "✽", "✢"],
            &["credit balance is too low", "please run /login"],
        ),
        "codex" => (
            &["allow command", "run this command?", "approve"],
            &["codex is working"],
            &["not logged in", "run `codex login`"],
        ),
        "gemini" => (
            &[
                "waiting for confirmation",
                "apply this change?",
                "do you want to proceed",
            ],
            &["gemini is thinking"],
            &[],
        ),
        "aider" => (&["apply edits?", "add these files"], &[], &[]),
        _ => (&[], &[], &[]),
    };
    Rules {
        needs_input: ni.iter().map(|s| s.to_string()).collect(),
        working: wk.iter().map(|s| s.to_string()).collect(),
        error: er.iter().map(|s| s.to_string()).collect(),
        neutral: Vec::new(),
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

/// Classify the pane for `engine`. Precedence: `error` (fail-closed — a broken agent
/// must never read as success) > `needs_input` > `working`; no match → `None` (caller
/// keeps its pane-hash fallback). Case/whitespace-insensitive.
pub fn classify(engine: Option<&str>, text: &str) -> Option<PaneState> {
    let hay = tail(text, 15);
    if hay.is_empty() {
        return None;
    }
    let rules = resolve(engine);
    // Veto first: if the pane is an editor/menu/pager overlay, give no opinion so the
    // caller falls back to its pane-hash heuristic (never misreads a menu as idle).
    if rules.neutral.iter().any(|p| hay.contains(p.as_str())) {
        return None;
    }
    if rules.error.iter().any(|p| hay.contains(p.as_str())) {
        return Some(PaneState::Error);
    }
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
    fn neutral_veto_suppresses_state() {
        // an editor overlay in the pane must NOT read as needs-input just because it
        // contains a "(y/n)"-ish string in a buffer; veto → no opinion
        assert_eq!(classify(None, "editing config\n-- INSERT --\nsave? (y/n)"), None);
        // a model picker menu (with a ❯ pointer) must not read as needs-input
        assert_eq!(classify(Some("claude"), "Select a model\n❯ 1. Opus\n  2. Sonnet"), None);
        // but a genuine prompt with no overlay still classifies
        assert_eq!(classify(None, "Do you want to proceed? (y/n)"), Some(PaneState::NeedsInput));
    }

    #[test]
    fn terminal_error_is_fail_closed_and_beats_idle() {
        // an auth failure must classify as Error, never read as idle/no-opinion
        assert_eq!(
            classify(Some("claude"), "…\nInvalid API key · Please run /login"),
            Some(PaneState::Error)
        );
        assert_eq!(
            classify(None, "usage limit reached, try again later"),
            Some(PaneState::Error)
        );
        // a transient, self-retrying overload must NOT fail-close (agent is still working)
        assert_ne!(
            classify(None, "API Error: Overloaded (retrying 1/5)"),
            Some(PaneState::Error)
        );
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
