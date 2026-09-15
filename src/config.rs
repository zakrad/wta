//! Global user settings at `~/.wta/config.json`.
//!
//! Precedence is **env var > this file > built-in default**. We honour that with a
//! single trick: at startup [`apply_env_defaults`] seeds each backing env var from the
//! file *only when it is unset*, so every existing env-reader downstream keeps working
//! unchanged and a real env var always wins. Values with no single backing env var
//! (default `--model`/`--effort`) are resolved at their call sites via [`load`].
//!
//! Edit it three ways: the dashboard settings page (`,`), the `wta config` CLI, or by
//! hand — a missing file or missing key just falls through to the default.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// editor command for `wta open` / dash `e` — backs `WTA_OPEN_CMD` (e.g. "nvim", "code").
    pub editor: Option<String>,
    /// default agent CLI for `wta new` — backs `WTA_AGENT_CMD` (e.g. "claude", "codex").
    pub agent: Option<String>,
    /// default `--model` for new agents (e.g. "opus-4.8"); no flag/role given → this.
    pub model: Option<String>,
    /// default `--effort` for new agents: low | medium | high | xhigh | max.
    pub effort: Option<String>,
    /// show the green hint bar in agent sessions — backs `WTA_HINT_BAR` (false → "0").
    pub hint_bar: Option<bool>,
    /// how `wta open` / dash `e` launches a terminal editor — backs `WTA_OPEN_TMUX`:
    /// "auto" (new tmux window if inside tmux, else inline), "window", or "inline".
    pub open_mode: Option<String>,
}

/// The fields shown in the settings page / `wta config`, in display order.
/// (key, one-line help). Keep in sync with [`Config::get`]/[`Config::set`].
pub const FIELDS: &[(&str, &str)] = &[
    ("editor", "editor for `open`/`e` (nvim, code, cursor)"),
    ("agent", "default agent CLI for `new` (claude, codex, gemini)"),
    ("model", "default --model for new agents (opus-4.8, sonnet-5)"),
    ("effort", "default --effort (low|medium|high|xhigh|max)"),
    ("hint_bar", "green hint bar in sessions (on|off)"),
    ("open_mode", "editor launch: auto|window|inline"),
];

pub fn path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".wta").join("config.json"))
}

/// Load `~/.wta/config.json`; a missing/unreadable/malformed file yields defaults.
pub fn load() -> Config {
    let Some(p) = path() else {
        return Config::default();
    };
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the config back to `~/.wta/config.json` (pretty-printed, dir created).
pub fn save(c: &Config) -> Result<()> {
    let p = path().context("no home directory")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let s = serde_json::to_string_pretty(c)?;
    std::fs::write(&p, s).with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

/// Seed backing env vars from the file where the user hasn't set them, so env wins
/// and all downstream env-readers pick up the file's defaults. Call once at startup.
pub fn apply_env_defaults() {
    let c = load();
    let seed = |key: &str, val: Option<&str>| {
        if let Some(v) = val {
            if !v.trim().is_empty() && std::env::var_os(key).is_none() {
                std::env::set_var(key, v);
            }
        }
    };
    seed("WTA_OPEN_CMD", c.editor.as_deref());
    seed("WTA_AGENT_CMD", c.agent.as_deref());
    seed("WTA_OPEN_TMUX", c.open_mode.as_deref());
    if let Some(hb) = c.hint_bar {
        if std::env::var_os("WTA_HINT_BAR").is_none() {
            std::env::set_var("WTA_HINT_BAR", if hb { "1" } else { "0" });
        }
    }
}

impl Config {
    /// Value of `key` as a display string ("" when unset). Unknown key → None.
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "editor" => self.editor.clone().unwrap_or_default(),
            "agent" => self.agent.clone().unwrap_or_default(),
            "model" => self.model.clone().unwrap_or_default(),
            "effort" => self.effort.clone().unwrap_or_default(),
            "hint_bar" => match self.hint_bar {
                Some(true) => "on".into(),
                Some(false) => "off".into(),
                None => String::new(),
            },
            "open_mode" => self.open_mode.clone().unwrap_or_default(),
            _ => return None,
        })
    }

    /// Set `key` from a user string; empty/"-"/"default" clears it. Validates enums.
    /// Returns Err on an unknown key or an invalid value.
    pub fn set(&mut self, key: &str, val: &str) -> Result<()> {
        let v = val.trim();
        let clear = v.is_empty() || v == "-" || v.eq_ignore_ascii_case("default");
        let opt = |v: &str| (!clear).then(|| v.to_string());
        match key {
            "editor" => self.editor = opt(v),
            "agent" => self.agent = opt(v),
            "model" => self.model = opt(v),
            "effort" => {
                if !clear && !matches!(v, "low" | "medium" | "high" | "xhigh" | "max") {
                    anyhow::bail!("effort must be one of: low, medium, high, xhigh, max");
                }
                self.effort = opt(v);
            }
            "hint_bar" => {
                self.hint_bar = if clear {
                    None
                } else {
                    match v.to_ascii_lowercase().as_str() {
                        "on" | "true" | "1" | "yes" => Some(true),
                        "off" | "false" | "0" | "no" => Some(false),
                        _ => anyhow::bail!("hint_bar must be on or off"),
                    }
                };
            }
            "open_mode" => {
                if !clear && !matches!(v, "auto" | "window" | "inline") {
                    anyhow::bail!("open_mode must be one of: auto, window, inline");
                }
                self.open_mode = opt(v);
            }
            _ => anyhow::bail!("unknown setting '{key}' (see `wta config`)"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip_and_clear() {
        let mut c = Config::default();
        c.set("editor", "nvim").unwrap();
        assert_eq!(c.get("editor").as_deref(), Some("nvim"));
        c.set("editor", "").unwrap();
        assert_eq!(c.editor, None);
        assert_eq!(c.get("editor").as_deref(), Some(""));
    }

    #[test]
    fn hint_bar_parsing() {
        let mut c = Config::default();
        c.set("hint_bar", "off").unwrap();
        assert_eq!(c.hint_bar, Some(false));
        assert_eq!(c.get("hint_bar").as_deref(), Some("off"));
        c.set("hint_bar", "on").unwrap();
        assert_eq!(c.hint_bar, Some(true));
        assert!(c.set("hint_bar", "maybe").is_err());
    }

    #[test]
    fn enum_validation() {
        let mut c = Config::default();
        assert!(c.set("effort", "high").is_ok());
        assert!(c.set("effort", "turbo").is_err());
        assert!(c.set("open_mode", "window").is_ok());
        assert!(c.set("open_mode", "sideways").is_err());
        assert!(c.set("nope", "x").is_err());
    }

    #[test]
    fn default_keyword_clears() {
        let mut c = Config::default();
        c.set("model", "opus-4.8").unwrap();
        c.set("model", "default").unwrap();
        assert_eq!(c.model, None);
    }
}
