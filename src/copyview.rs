//! wta's own vim-style **copy mode**: a scrollable, searchable, line-selectable view of an
//! agent's conversation that works no matter how the agent draws its screen.
//!
//! Why: tmux copy mode only sees tmux's pane history. Full-screen renderers (Claude Code
//! ≥ 2.1's fullscreen TUI, and anything else on the alternate screen) keep their history
//! inside the app, so tmux has nothing to scroll or select — and each agent CLI's own
//! selection story is different (mouse-only, or none). wta is agent-agnostic, so it
//! builds the text itself:
//!
//! - **transcript** — when the agent's conversation is on disk in a format wta knows
//!   (Claude Code JSONL under `~/.claude/projects/<cwd>`), render it: `you ›` / `claude ›`
//!   messages, tool calls summarized, tool results folded to a few lines.
//! - **tmux scrollback** — otherwise, the pane's full history (`capture-pane -S -`),
//!   which is exactly what a classic renderer / any plain CLI leaves behind.
//!
//! Keys (vi): `j`/`k` move, `Ctrl-d`/`Ctrl-u` half page, `PgUp`/`PgDn` page, `g`/`G`
//! top/bottom, `[`/`]` previous/next message, `/` search then `n`/`N`, `v` start/stop a
//! line selection, `y` yank the selection (or current line) to the clipboard, `Y` yank
//! the current line, `m` yank the whole message under the cursor, `Enter` fold/unfold a
//! tool result, `?` help, `Esc` clear, `q` quit. Entirely keyboard-driven.
//!
//! Entry points: `c` in the dashboard, `wta copy <task>`, and `Alt-y` while attached
//! (a tmux popup over the agent — passes through an outer tmux too).

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Terminal;
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

/// What a logical line is, for styling + message navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `you › …` message header
    User,
    /// `claude › …` (agent) message header
    Agent,
    /// tool call / folded tool result
    Tool,
    /// plain content
    Text,
}

/// Membership of a line in a foldable tool result: which result, its position, the total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub id: u32,
    pub idx: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrcLine {
    pub text: String,
    pub kind: Kind,
    /// set on every line of a tool result, so the viewer can fold it past `RESULT_FOLD`
    pub fold: Option<Fold>,
}

impl SrcLine {
    fn new(text: impl Into<String>, kind: Kind) -> Self {
        Self { text: text.into(), kind, fold: None }
    }
    fn text(t: impl Into<String>) -> Self {
        Self::new(t, Kind::Text)
    }
}

/// The text the viewer shows, plus where it came from (for the title bar).
pub struct Source {
    pub title: String,
    pub origin: String,
    pub lines: Vec<SrcLine>,
}

// ───────────────────────────── source resolution ─────────────────────────────

/// Build the copy-mode text for an agent: its transcript when wta can read it, else the
/// full tmux scrollback of its session.
pub fn source_for(repo: &str, task: &str) -> Source {
    let st = crate::status::read_state(repo, task);
    let session = crate::tmux::session_name(repo, task);
    // Where does the agent's transcript live? Claude keys it on the dir it actually RUNS
    // in. For a wta worktree agent that's the worktree, but the state file's `cwd` can be
    // the repo root instead (a hook fired from there), so it's not reliable. The tmux
    // pane's current path IS the agent's real cwd — try it first, then the state cwd.
    let mut cands: Vec<String> = Vec::new();
    if let Some(p) = crate::tmux::pane_path(&session) {
        cands.push(p);
    }
    if let Some(c) = st.as_ref().map(|s| s.cwd.clone()).filter(|c| !c.is_empty()) {
        cands.push(c);
    }
    for cwd in &cands {
        if let Some(lines) = claude_transcript(Path::new(cwd)) {
            return Source { title: task.to_string(), origin: "transcript".into(), lines };
        }
    }
    let text = crate::tmux::capture_full(&session).unwrap_or_default();
    let mut lines: Vec<SrcLine> = text.lines().map(|l| SrcLine::text(l.trim_end())).collect();
    while lines.last().map(|l| l.text.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(SrcLine::text("(nothing to show: no transcript on disk and no tmux history — is the agent running?)"));
    }
    Source { title: task.to_string(), origin: "tmux scrollback".into(), lines }
}

/// Resolve `wta copy [TASK] [--session NAME]` to (repo, task). `--session` is the tmux
/// session name (`wta-<repo>-<task>`) — what the attached-mode popup key passes in.
pub fn resolve(task: Option<&str>, session: Option<&str>) -> Result<(String, String)> {
    if let Some(s) = session {
        for st in crate::status::read_all_states().unwrap_or_default() {
            if crate::tmux::session_name(&st.repo, &st.task) == s {
                return Ok((st.repo, st.task));
            }
        }
        bail!("no wta agent owns tmux session '{s}'");
    }
    let task = task.context("which agent? `wta copy <task>` (or --session <tmux session>)")?;
    let repo = crate::worktree::repo_id()?;
    Ok((repo, task.to_string()))
}

/// Render the newest Claude Code transcript for a worktree, if any.
fn claude_transcript(wt: &Path) -> Option<Vec<SrcLine>> {
    let dir = crate::cost::transcript_dir(wt)?;
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            let m = e.metadata().and_then(|m| m.modified()).ok()?;
            if newest.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                newest = Some((m, p));
            }
        }
    }
    let (_, path) = newest?;
    let text = std::fs::read_to_string(path).ok()?;
    let lines = render_claude_jsonl(&text);
    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

/// How many lines of a tool result to keep before folding.
const RESULT_FOLD: usize = 6;

/// Turn Claude Code's JSONL transcript into readable lines. Public for tests.
pub fn render_claude_jsonl(text: &str) -> Vec<SrcLine> {
    let mut out: Vec<SrcLine> = Vec::new();
    // who spoke last, so consecutive assistant records (Claude writes one per content
    // block) and tool-result-only user records don't repeat headers
    let mut last: Option<Kind> = None;
    let mut fold_id: u32 = 0;
    for raw in text.lines() {
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
            || v.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
        {
            continue;
        }
        let kind = match v.get("type").and_then(Value::as_str) {
            Some("user") => Kind::User,
            Some("assistant") => Kind::Agent,
            _ => continue,
        };
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| t.get(11..16))
            .unwrap_or("");
        let content = match v.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };
        // collect this record's pieces first, so an empty record adds nothing
        let mut texts: Vec<String> = Vec::new();
        let mut tools: Vec<Vec<SrcLine>> = Vec::new();
        match content {
            Value::String(s) => texts.push(s.clone()),
            Value::Array(blocks) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                texts.push(t.to_string());
                            }
                        }
                        Some("tool_use") => tools.push(tool_use_lines(b)),
                        Some("tool_result") => {
                            tools.push(tool_result_lines(b, fold_id));
                            fold_id += 1;
                        }
                        _ => {} // thinking, images, …
                    }
                }
            }
            _ => {}
        }
        let texts: Vec<String> = texts
            .into_iter()
            .map(|t| strip_system_reminders(&t))
            .filter(|t| !t.trim().is_empty())
            .collect();
        if texts.is_empty() && tools.is_empty() {
            continue;
        }
        // header: only when the speaker changes, and only if there's something the
        // speaker actually said (tool results alone stay under the agent's turn)
        let speaks = !texts.is_empty() || kind == Kind::Agent;
        if speaks && last != Some(kind) {
            if !out.is_empty() {
                out.push(SrcLine::text(""));
            }
            let who = if kind == Kind::User { "you" } else { "claude" };
            let hdr = if ts.is_empty() { format!("▎{who} ›") } else { format!("▎{who} › {ts}") };
            out.push(SrcLine::new(hdr, kind));
            last = Some(kind);
        }
        for t in texts {
            for l in t.lines() {
                out.push(SrcLine::text(l.trim_end()));
            }
        }
        for t in tools {
            out.extend(t);
        }
    }
    out
}

/// `  ⚙ Bash  cargo build` — one line per tool call, keyed on the most telling input.
fn tool_use_lines(b: &Value) -> Vec<SrcLine> {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
    let input = b.get("input").cloned().unwrap_or(Value::Null);
    let pick = ["command", "file_path", "path", "pattern", "query", "prompt", "description", "url"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str).map(str::to_string));
    let detail = pick.unwrap_or_else(|| match &input {
        Value::Object(m) if m.is_empty() => String::new(),
        Value::Null => String::new(),
        other => other.to_string(),
    });
    let detail: String = detail.lines().next().unwrap_or("").chars().take(160).collect();
    vec![SrcLine::new(format!("  ⚙ {name}  {detail}").trim_end().to_string(), Kind::Tool)]
}

/// `  ↳ …` — a tool result, every line tagged with its `Fold` so the viewer can show the
/// first `RESULT_FOLD` lines and unfold the rest on demand.
fn tool_result_lines(b: &Value, id: u32) -> Vec<SrcLine> {
    let mut body = String::new();
    match b.get("content") {
        Some(Value::String(s)) => body.push_str(s),
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(t);
                }
            }
        }
        _ => {}
    }
    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let pre = if i == 0 { "  ↳ " } else { "  │ " };
        let l: String = l.trim_end().chars().take(400).collect();
        let mut sl = SrcLine::new(format!("{pre}{l}"), Kind::Tool);
        sl.fold = Some(Fold { id, idx: i, total });
        out.push(sl);
    }
    if out.is_empty() {
        out.push(SrcLine::new("  ↳ (empty result)", Kind::Tool));
    }
    out
}

/// Hook/system context is injected into user turns as `<system-reminder>…</system-reminder>`
/// blocks — noise in a conversation view.
fn strip_system_reminders(t: &str) -> String {
    let mut s = t.to_string();
    while let Some(a) = s.find("<system-reminder>") {
        match s[a..].find("</system-reminder>") {
            Some(rel) => s.replace_range(a..a + rel + "</system-reminder>".len(), ""),
            None => {
                s.truncate(a);
                break;
            }
        }
    }
    s.trim().to_string()
}

// ───────────────────────────── clipboard ─────────────────────────────

/// Put `text` on the system clipboard. Tries the platform CLIs first, then falls back
/// to OSC 52 (the terminal's clipboard — wrapped for tmux passthrough). Returns which
/// mechanism took it, for the status line.
pub fn copy_to_clipboard(text: &str) -> Result<&'static str> {
    let tools: [(&str, &[&str]); 4] = [
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (prog, args) in tools {
        let child = std::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return Ok(prog);
            }
        }
    }
    use base64::{engine::general_purpose::STANDARD, Engine};
    let seq = format!("\x1b]52;c;{}\x07", STANDARD.encode(text));
    let seq = if std::env::var("TMUX").is_ok() {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    };
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()?;
    Ok("OSC 52")
}

// ───────────────────────────── viewer ─────────────────────────────

/// One wrapped display row (`line` indexes the VISIBLE line list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub text: String,
}

/// Greedy wrap by display width (tabs → 4 spaces, wide chars counted). Public for tests.
pub fn wrap_rows(lines: &[&str], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let text = l.replace('\t', "    ");
        if text.is_empty() {
            rows.push(Row { line: i, text: String::new() });
            continue;
        }
        let mut cur = String::new();
        let mut w = 0usize;
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            if w + cw > width && !cur.is_empty() {
                rows.push(Row { line: i, text: std::mem::take(&mut cur) });
                w = 0;
            }
            cur.push(ch);
            w += cw;
        }
        rows.push(Row { line: i, text: cur });
    }
    rows
}

/// A line as currently shown: a source line, or the synthetic `… (+N lines)` marker
/// standing in for a folded tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vis {
    pub text: String,
    pub kind: Kind,
    pub fold: Option<Fold>,
    /// synthetic fold marker (not yanked, `Enter` unfolds)
    pub marker: bool,
}

/// The visible line list for a source given which folds are expanded. Public for tests.
pub fn visible_lines(src: &[SrcLine], expanded: &HashSet<u32>) -> Vec<Vis> {
    let mut out = Vec::with_capacity(src.len());
    for l in src {
        match l.fold {
            Some(f) if f.total > RESULT_FOLD && f.idx >= RESULT_FOLD && !expanded.contains(&f.id) => {
                if f.idx == RESULT_FOLD {
                    out.push(Vis {
                        text: format!("  │ … (+{} lines) ⏎ unfold", f.total - RESULT_FOLD),
                        kind: Kind::Tool,
                        fold: Some(f),
                        marker: true,
                    });
                }
            }
            _ => out.push(Vis { text: l.text.clone(), kind: l.kind, fold: l.fold, marker: false }),
        }
    }
    out
}

struct View {
    src: Source,
    expanded: HashSet<u32>,
    vis: Vec<Vis>,
    rows: Vec<Row>,
    width: usize,
    dirty: bool,
    cursor: usize, // row
    top: usize,    // first visible row
    anchor: Option<usize>, // selection anchor row (line-wise selection)
    search: Option<String>,
    input: Option<String>, // '/' being typed
    msg: Option<String>,
    help: bool,
}

impl View {
    fn new(src: Source) -> Self {
        let vis = visible_lines(&src.lines, &HashSet::new());
        Self {
            src,
            expanded: HashSet::new(),
            vis,
            rows: Vec::new(),
            width: 0,
            dirty: true,
            cursor: 0,
            top: 0,
            anchor: None,
            search: None,
            input: None,
            msg: None,
            help: false,
        }
    }

    fn rewrap(&mut self, width: usize, start_at_end: bool) {
        if width == self.width && !self.dirty {
            return;
        }
        let first_time = self.rows.is_empty();
        let keep_line = self.rows.get(self.cursor).map(|r| r.line);
        self.width = width;
        self.dirty = false;
        let texts: Vec<&str> = self.vis.iter().map(|v| v.text.as_str()).collect();
        self.rows = wrap_rows(&texts, width);
        self.cursor = match keep_line {
            Some(l) => self.rows.iter().position(|r| r.line == l).unwrap_or(0),
            None if first_time && start_at_end => self.rows.len().saturating_sub(1),
            None => 0,
        };
        if let Some(a) = self.anchor {
            self.anchor = Some(a.min(self.rows.len().saturating_sub(1)));
        }
    }

    fn clamp(&mut self, height: usize) {
        let n = self.rows.len().max(1);
        self.cursor = self.cursor.min(n - 1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + height.max(1) {
            self.top = self.cursor + 1 - height.max(1);
        }
    }

    fn move_by(&mut self, d: isize) {
        let n = self.rows.len() as isize;
        self.cursor = (self.cursor as isize + d).clamp(0, (n - 1).max(0)) as usize;
    }

    fn cur_line(&self) -> usize {
        self.rows.get(self.cursor).map(|r| r.line).unwrap_or(0)
    }

    /// Selected visible-line range (inclusive), if a selection is active.
    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.rows.get(self.anchor?)?.line;
        let c = self.cur_line();
        Some((a.min(c), a.max(c)))
    }

    /// Yank visible lines `lo..=hi` (fold markers excluded) to the clipboard.
    fn yank_range(&mut self, lo: usize, hi: usize, what: &str) {
        let text: Vec<&str> = self.vis[lo..=hi.min(self.vis.len().saturating_sub(1))]
            .iter()
            .filter(|v| !v.marker)
            .map(|v| v.text.as_str())
            .collect();
        let n = text.len();
        match copy_to_clipboard(&text.join("\n")) {
            Ok(how) => {
                self.msg = Some(format!("yanked {what}{n} line{} → clipboard ({how})", if n == 1 { "" } else { "s" }))
            }
            Err(e) => self.msg = Some(format!("clipboard failed: {e}")),
        }
        self.anchor = None;
    }

    fn yank(&mut self, whole_selection: bool) {
        let (lo, hi) = match (whole_selection, self.selection()) {
            (true, Some(r)) => r,
            _ => {
                let l = self.cur_line();
                (l, l)
            }
        };
        self.yank_range(lo, hi, "");
    }

    /// The message containing visible line `at`: from its `you ›`/`claude ›` header
    /// (exclusive) to just before the next header, trailing blanks dropped. Without
    /// headers (plain scrollback) it's the blank-line-delimited paragraph. Public for tests.
    fn message_range(&self, at: usize) -> Option<(usize, usize)> {
        let is_hdr = |v: &Vis| matches!(v.kind, Kind::User | Kind::Agent);
        let has_headers = self.vis.iter().any(is_hdr);
        let n = self.vis.len();
        if n == 0 {
            return None;
        }
        let (mut lo, mut hi);
        if has_headers {
            let hdr = (0..=at).rev().find(|&i| is_hdr(&self.vis[i]))?;
            lo = hdr + 1;
            hi = (hdr + 1..n).find(|&i| is_hdr(&self.vis[i])).unwrap_or(n).saturating_sub(1);
        } else {
            if self.vis[at].text.trim().is_empty() {
                return None;
            }
            lo = at;
            while lo > 0 && !self.vis[lo - 1].text.trim().is_empty() {
                lo -= 1;
            }
            hi = at;
            while hi + 1 < n && !self.vis[hi + 1].text.trim().is_empty() {
                hi += 1;
            }
        }
        while hi > lo && self.vis[hi].text.trim().is_empty() {
            hi -= 1;
        }
        (lo <= hi).then_some((lo, hi))
    }

    fn yank_message(&mut self) {
        match self.message_range(self.cur_line()) {
            Some((lo, hi)) => self.yank_range(lo, hi, "message: "),
            None => self.msg = Some("no message here".into()),
        }
    }

    /// `Enter`: fold/unfold the tool result under the cursor.
    fn toggle_fold(&mut self) {
        let Some(f) = self.vis.get(self.cur_line()).and_then(|v| v.fold) else {
            self.msg = Some("not a tool result (⏎ folds/unfolds results)".into());
            return;
        };
        if f.total <= RESULT_FOLD {
            self.msg = Some("this result is already fully shown".into());
            return;
        }
        let opening = !self.expanded.contains(&f.id);
        if opening {
            self.expanded.insert(f.id);
        } else {
            self.expanded.remove(&f.id);
        }
        self.vis = visible_lines(&self.src.lines, &self.expanded);
        self.dirty = true;
        // land on the fold's first hidden/marker line so the toggle is visible
        let target = self
            .vis
            .iter()
            .position(|v| v.fold.map(|g| g.id == f.id && g.idx == RESULT_FOLD).unwrap_or(false))
            .unwrap_or(0);
        let texts: Vec<&str> = self.vis.iter().map(|v| v.text.as_str()).collect();
        self.rows = wrap_rows(&texts, self.width.max(1));
        self.dirty = false;
        self.cursor = self.rows.iter().position(|r| r.line == target).unwrap_or(0);
        self.anchor = None;
    }

    fn jump_message(&mut self, forward: bool) {
        let is_hdr = |r: &Row| matches!(self.vis[r.line].kind, Kind::User | Kind::Agent);
        let found = if forward {
            self.rows.iter().enumerate().skip(self.cursor + 1).find(|(_, r)| is_hdr(r)).map(|(i, _)| i)
        } else {
            self.rows.iter().enumerate().take(self.cursor).rev().find(|(_, r)| is_hdr(r)).map(|(i, _)| i)
        };
        match found {
            Some(i) => self.cursor = i,
            None => self.msg = Some(if forward { "no next message".into() } else { "no previous message".into() }),
        }
    }

    fn find(&mut self, forward: bool) {
        let Some(pat) = self.search.clone() else { return };
        let pat = pat.to_lowercase();
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = if forward { (self.cursor + step) % n } else { (self.cursor + n - step % n) % n };
            if self.rows[i].text.to_lowercase().contains(&pat) {
                self.cursor = i;
                return;
            }
        }
        self.msg = Some(format!("pattern not found: {pat}"));
    }

    /// Returns true to quit.
    fn key(&mut self, k: KeyEvent, height: usize) -> bool {
        if k.kind != KeyEventKind::Press {
            return false;
        }
        self.msg = None;
        if self.help {
            // any key closes help; `q` still quits
            self.help = false;
            return matches!(k.code, KeyCode::Char('q'));
        }
        // search prompt swallows keys
        if let Some(buf) = self.input.as_mut() {
            match k.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    let pat = self.input.take().unwrap_or_default();
                    if !pat.is_empty() {
                        self.search = Some(pat);
                        self.find(true);
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => buf.push(c),
                _ => {}
            }
            return false;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (height / 2).max(1) as isize;
        let page = height.max(1) as isize;
        match (k.code, ctrl) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), true) => return true,
            (KeyCode::Char('?'), false) => self.help = true,
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => self.move_by(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => self.move_by(-1),
            (KeyCode::Char('d'), true) => self.move_by(half),
            (KeyCode::Char('u'), true) => self.move_by(-half),
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => self.move_by(page),
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => self.move_by(-page),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => self.cursor = 0,
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => self.cursor = self.rows.len().saturating_sub(1),
            (KeyCode::Char(']'), false) => self.jump_message(true),
            (KeyCode::Char('['), false) => self.jump_message(false),
            (KeyCode::Char('v'), false) | (KeyCode::Char('V'), false) => {
                self.anchor = if self.anchor.is_some() { None } else { Some(self.cursor) };
            }
            (KeyCode::Char('y'), false) => self.yank(true),
            (KeyCode::Char('Y'), false) => self.yank(false),
            (KeyCode::Char('m'), false) => self.yank_message(),
            (KeyCode::Enter, _) | (KeyCode::Char('z'), false) => self.toggle_fold(),
            (KeyCode::Char('/'), false) => self.input = Some(String::new()),
            (KeyCode::Char('n'), false) => self.find(true),
            (KeyCode::Char('N'), false) => self.find(false),
            (KeyCode::Esc, _) => {
                if self.anchor.is_some() {
                    self.anchor = None;
                } else {
                    self.search = None;
                }
            }
            _ => {}
        }
        false
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
        let body = chunks[1];
        let width = body.width.saturating_sub(2) as usize; // 2-col gutter: cursor + selection marks
        self.rewrap(width, true);
        let height = body.height as usize;
        self.clamp(height);

        // title
        let sel_note = match self.selection() {
            Some((lo, hi)) => format!(" · selecting {} line{}", hi - lo + 1, if hi == lo { "" } else { "s" }),
            None => String::new(),
        };
        let title = Line::from(vec![
            Span::styled(" copy mode ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} · {} · {} lines{} ", self.src.title, self.src.origin, self.src.lines.len(), sel_note),
                Style::default().fg(Color::Green),
            ),
        ]);
        f.render_widget(Paragraph::new(title), chunks[0]);

        // body
        let sel = self.selection();
        let pat = self.search.as_ref().map(|p| p.to_lowercase());
        let mut lines: Vec<Line> = Vec::with_capacity(height);
        for i in self.top..(self.top + height).min(self.rows.len()) {
            let r = &self.rows[i];
            let v = &self.vis[r.line];
            let selected = sel.map(|(lo, hi)| r.line >= lo && r.line <= hi).unwrap_or(false);
            let gutter = match (i == self.cursor, selected) {
                (true, _) => Span::styled("▌ ", Style::default().fg(Color::Green)),
                (false, true) => Span::styled("┃ ", Style::default().fg(Color::Green)),
                _ => Span::raw("  "),
            };
            let mut base = match v.kind {
                Kind::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                Kind::Agent => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                Kind::Tool if v.marker => Style::default().fg(Color::Yellow),
                Kind::Tool => Style::default().fg(Color::DarkGray),
                Kind::Text => Style::default(),
            };
            if selected {
                base = base.bg(Color::Rgb(30, 60, 40));
            }
            let mut spans = vec![gutter];
            spans.extend(highlight(&r.text, pat.as_deref(), base));
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), body);

        // status
        let status = if let Some(buf) = &self.input {
            Line::from(vec![Span::styled(format!("/{buf}"), Style::default().fg(Color::Yellow))])
        } else if let Some(m) = &self.msg {
            Line::from(Span::styled(format!(" {m}"), Style::default().fg(Color::Green)))
        } else {
            let pct = if self.rows.len() <= 1 { 100 } else { self.cursor * 100 / (self.rows.len() - 1) };
            Line::from(vec![
                Span::styled(format!(" {pct:>3}% "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "j/k · v y yank · m message · ⏎ fold · / search · ? help · q",
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        };
        f.render_widget(Paragraph::new(status), chunks[2]);

        if self.help {
            draw_help(f, area);
        }
    }
}

/// The `?` overlay: every key on one card.
fn draw_help(f: &mut ratatui::Frame, area: Rect) {
    let k = |key: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {key:<12}"), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(what.to_string()),
        ])
    };
    let body = vec![
        k("j / k  ↑ ↓", "move one row"),
        k("^d / ^u", "half page down / up"),
        k("PgDn / PgUp", "page down / up  (also ^f / ^b)"),
        k("g / G", "top / bottom"),
        k("] / [", "next / previous message"),
        k("/  n  N", "search (case-insensitive), next, previous"),
        k("v", "start / stop a line selection"),
        k("y", "yank selection (or current line) → clipboard"),
        k("Y", "yank current line"),
        k("m", "yank the whole message under the cursor"),
        k("⏎ / z", "unfold / fold a tool result"),
        k("Esc", "clear selection, then search"),
        k("q", "quit copy mode"),
        Line::from(""),
        Line::styled("  any key closes this card", Style::default().fg(Color::DarkGray)),
    ];
    let w = 62u16.min(area.width.saturating_sub(2)).max(30);
    let h = (body.len() as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let card = Rect { x, y, width: w, height: h };
    f.render_widget(Clear, card);
    f.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Green))
                .title(" copy mode keys "),
        ),
        card,
    );
}

/// Split `text` into spans, highlighting case-insensitive matches of `pat`.
fn highlight<'a>(text: &'a str, pat: Option<&str>, base: Style) -> Vec<Span<'a>> {
    let Some(pat) = pat.filter(|p| !p.is_empty()) else {
        return vec![Span::styled(text, base)];
    };
    let lower = text.to_lowercase();
    // byte offsets line up only when lowercasing didn't change lengths; bail to plain otherwise
    if lower.len() != text.len() {
        return vec![Span::styled(text, base)];
    }
    let hl = Style::default().fg(Color::Black).bg(Color::Yellow);
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(rel) = lower[i..].find(pat) {
        let s = i + rel;
        if s > i {
            spans.push(Span::styled(&text[i..s], base));
        }
        spans.push(Span::styled(&text[s..s + pat.len()], hl));
        i = s + pat.len();
    }
    if i < text.len() {
        spans.push(Span::styled(&text[i..], base));
    }
    spans
}

/// Take over the terminal and run copy mode until `q`. Restores the terminal on exit,
/// including on error. Safe to call from inside the dashboard after it released the TUI.
pub fn run(src: Source) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let res = (|| -> Result<()> {
        let mut v = View::new(src);
        loop {
            term.draw(|f| v.draw(f))?;
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(k) => {
                        let height = term.size().map(|s| s.height.saturating_sub(2) as usize).unwrap_or(20);
                        if v.key(k, height) {
                            break;
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen, crossterm::cursor::Show).ok();
    res
}

/// `wta copy [TASK] [--session NAME]`.
pub fn run_cli(task: Option<&str>, session: Option<&str>) -> Result<()> {
    let (repo, task) = resolve(task, session)?;
    run(source_for(&repo, &task))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(lines: Vec<SrcLine>) -> Source {
        Source { title: "t".into(), origin: "test".into(), lines }
    }

    #[test]
    fn wrap_respects_width_and_keeps_line_index() {
        let rows = wrap_rows(&["abcdefghij", "", "xy"], 4);
        let got: Vec<(usize, &str)> = rows.iter().map(|r| (r.line, r.text.as_str())).collect();
        assert_eq!(got, vec![(0, "abcd"), (0, "efgh"), (0, "ij"), (1, ""), (2, "xy")]);
    }

    const JSONL: &str = r#"
{"type":"user","timestamp":"2026-08-30T14:40:58.987Z","message":{"role":"user","content":"hi there <system-reminder>secret hook noise</system-reminder>"}}
{"type":"assistant","timestamp":"2026-08-30T14:41:01.000Z","message":{"id":"m1","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Hello!\nSecond line."}]}}
{"type":"assistant","timestamp":"2026-08-30T14:41:02.000Z","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test","description":"run tests"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8"}]}}
{"type":"user","isSidechain":true,"message":{"content":"subagent chatter"}}
{"type":"user","isMeta":true,"message":{"content":"meta"}}
{"type":"user","message":{"content":[{"type":"text","text":"thanks"}]}}
"#;

    #[test]
    fn renders_claude_jsonl_into_conversation() {
        let lines = render_claude_jsonl(JSONL);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts[0], "▎you › 14:40");
        assert_eq!(texts[1], "hi there"); // system-reminder stripped
        assert_eq!(texts[2], "");
        assert_eq!(texts[3], "▎claude › 14:41");
        assert_eq!(texts[4], "Hello!");
        assert_eq!(texts[5], "Second line.");
        assert_eq!(texts[6], "  ⚙ Bash  cargo test"); // no second header for the same speaker
        assert_eq!(texts[7], "  ↳ l1");
        assert_eq!(texts[14], "  │ l8"); // ALL result lines are kept in the source…
        assert_eq!(lines[7].fold, Some(Fold { id: 0, idx: 0, total: 8 }));
        assert!(!texts.iter().any(|t| t.contains("subagent") || t.contains("meta")));
        assert_eq!(*texts.last().unwrap(), "thanks");
        assert_eq!(lines[0].kind, Kind::User);
        assert_eq!(lines[3].kind, Kind::Agent);
        assert_eq!(lines[6].kind, Kind::Tool);
    }

    #[test]
    fn folds_long_tool_results_and_unfolds_on_enter() {
        let lines = render_claude_jsonl(JSONL);
        // …but the VIEW folds them past RESULT_FOLD behind a marker
        let vis = visible_lines(&lines, &HashSet::new());
        let texts: Vec<&str> = vis.iter().map(|v| v.text.as_str()).collect();
        assert_eq!(texts[7 + RESULT_FOLD - 1], "  │ l6");
        assert!(texts[7 + RESULT_FOLD].starts_with("  │ … (+2 lines)"));
        assert!(vis[7 + RESULT_FOLD].marker);
        assert_eq!(texts[7 + RESULT_FOLD + 1], "");
        // Enter on the marker (or any line of that result) reveals everything
        let mut v = View::new(src(lines));
        v.rewrap(80, false);
        v.cursor = v.rows.iter().position(|r| v.vis[r.line].marker).unwrap();
        v.toggle_fold();
        let texts: Vec<&str> = v.vis.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts[7 + RESULT_FOLD], "  │ l7");
        assert_eq!(texts[7 + RESULT_FOLD + 1], "  │ l8");
        assert_eq!(v.vis[v.cur_line()].text, "  │ l7", "cursor lands on the first revealed line");
        v.toggle_fold();
        assert!(v.vis[v.cur_line()].marker, "folding again lands back on the marker");
    }

    #[test]
    fn selection_yank_range_is_line_wise_over_wrapped_rows() {
        let mut v = View::new(src(vec![SrcLine::text("aaaaaaaa"), SrcLine::text("b"), SrcLine::text("c")]));
        v.rewrap(4, false); // "aaaaaaaa" → 2 rows
        assert_eq!(v.rows.len(), 4);
        v.cursor = 1; // second row of line 0
        v.anchor = Some(3); // line 2
        assert_eq!(v.selection(), Some((0, 2)));
        v.cursor = 0;
        v.anchor = Some(1);
        assert_eq!(v.selection(), Some((0, 0)), "both rows of one line select just that line");
    }

    #[test]
    fn message_range_covers_body_without_header_and_without_trailing_blank() {
        let mut v = View::new(src(render_claude_jsonl(JSONL)));
        v.rewrap(80, false);
        // cursor on "Second line." (vis 5) → claude's message: lines 4..=last tool line
        let (lo, hi) = v.message_range(5).unwrap();
        assert_eq!(v.vis[lo].text, "Hello!");
        assert!(v.vis[hi].marker || v.vis[hi].text.starts_with("  │"), "{}", v.vis[hi].text);
        assert!(!v.vis[hi].text.is_empty(), "trailing blank dropped");
        // cursor on the first user message → just "hi there"
        assert_eq!(v.message_range(1), Some((1, 1)));
        // plain scrollback (no headers): paragraph between blank lines
        let mut p = View::new(src(vec![
            SrcLine::text("a"),
            SrcLine::text("b"),
            SrcLine::text(""),
            SrcLine::text("c"),
        ]));
        p.rewrap(80, false);
        assert_eq!(p.message_range(1), Some((0, 1)));
        assert_eq!(p.message_range(3), Some((3, 3)));
        assert_eq!(p.message_range(2), None);
    }

    #[test]
    fn message_jumps_land_on_headers() {
        let mut v = View::new(src(vec![
            SrcLine::new("▎you ›", Kind::User),
            SrcLine::text("q"),
            SrcLine::new("▎claude ›", Kind::Agent),
            SrcLine::text("a"),
        ]));
        v.rewrap(80, false);
        v.jump_message(true);
        assert_eq!(v.cursor, 2);
        v.jump_message(true);
        assert_eq!(v.cursor, 2); // stays, with a message
        assert!(v.msg.is_some());
        v.jump_message(false);
        assert_eq!(v.cursor, 0);
    }
}
