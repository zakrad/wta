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
//! the current line, `Esc` clear, `q` quit. Entirely keyboard-driven.
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
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use serde_json::Value;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrcLine {
    pub text: String,
    pub kind: Kind,
}

impl SrcLine {
    fn new(text: impl Into<String>, kind: Kind) -> Self {
        Self { text: text.into(), kind }
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
    let cwd = st.as_ref().map(|s| s.cwd.clone()).filter(|c| !c.is_empty());
    if let Some(cwd) = cwd.as_deref() {
        if let Some(lines) = claude_transcript(Path::new(cwd)) {
            return Source { title: task.to_string(), origin: "transcript".into(), lines };
        }
    }
    let session = crate::tmux::session_name(repo, task);
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
                        Some("tool_result") => tools.push(tool_result_lines(b)),
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

/// `  ↳ …` — a tool result folded to its first few lines.
fn tool_result_lines(b: &Value) -> Vec<SrcLine> {
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
    let mut out = Vec::new();
    for (i, l) in lines.iter().take(RESULT_FOLD).enumerate() {
        let pre = if i == 0 { "  ↳ " } else { "  │ " };
        let l: String = l.trim_end().chars().take(200).collect();
        out.push(SrcLine::new(format!("{pre}{l}"), Kind::Tool));
    }
    if lines.len() > RESULT_FOLD {
        out.push(SrcLine::new(format!("  │ … (+{} lines)", lines.len() - RESULT_FOLD), Kind::Tool));
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

/// One wrapped display row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub text: String,
}

/// Greedy wrap by display width (tabs → 4 spaces, wide chars counted). Public for tests.
pub fn wrap_rows(lines: &[SrcLine], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let text = l.text.replace('\t', "    ");
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

struct View {
    src: Source,
    rows: Vec<Row>,
    width: usize,
    cursor: usize, // row
    top: usize,    // first visible row
    anchor: Option<usize>, // selection anchor row (line-wise selection)
    search: Option<String>,
    input: Option<String>, // '/' being typed
    msg: Option<String>,
}

impl View {
    fn new(src: Source) -> Self {
        Self {
            src,
            rows: Vec::new(),
            width: 0,
            cursor: 0,
            top: 0,
            anchor: None,
            search: None,
            input: None,
            msg: None,
        }
    }

    fn rewrap(&mut self, width: usize, start_at_end: bool) {
        if width == self.width && !self.rows.is_empty() {
            return;
        }
        let keep_line = self.rows.get(self.cursor).map(|r| r.line);
        self.width = width;
        self.rows = wrap_rows(&self.src.lines, width);
        self.cursor = match keep_line {
            Some(l) => self.rows.iter().position(|r| r.line == l).unwrap_or(0),
            None if start_at_end => self.rows.len().saturating_sub(1),
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

    /// Selected logical-line range (inclusive), if a selection is active.
    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.rows.get(self.anchor?)?.line;
        let c = self.rows.get(self.cursor)?.line;
        Some((a.min(c), a.max(c)))
    }

    fn yank(&mut self, whole_selection: bool) {
        let (lo, hi) = match (whole_selection, self.selection()) {
            (true, Some(r)) => r,
            _ => {
                let l = self.rows.get(self.cursor).map(|r| r.line).unwrap_or(0);
                (l, l)
            }
        };
        let text: Vec<&str> = self.src.lines[lo..=hi].iter().map(|l| l.text.as_str()).collect();
        let n = text.len();
        match copy_to_clipboard(&text.join("\n")) {
            Ok(how) => self.msg = Some(format!("yanked {n} line{} → clipboard ({how})", if n == 1 { "" } else { "s" })),
            Err(e) => self.msg = Some(format!("clipboard failed: {e}")),
        }
        self.anchor = None;
    }

    fn jump_message(&mut self, forward: bool) {
        let is_hdr = |r: &Row| matches!(self.src.lines[r.line].kind, Kind::User | Kind::Agent);
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
            Span::styled(" copy mode ", Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} · {} · {} lines{} ", self.src.title, self.src.origin, self.src.lines.len(), sel_note),
                Style::default().fg(Color::Yellow),
            ),
        ]);
        f.render_widget(Paragraph::new(title), chunks[0]);

        // body
        let sel = self.selection();
        let pat = self.search.as_ref().map(|p| p.to_lowercase());
        let mut lines: Vec<Line> = Vec::with_capacity(height);
        for i in self.top..(self.top + height).min(self.rows.len()) {
            let r = &self.rows[i];
            let kind = self.src.lines[r.line].kind;
            let selected = sel.map(|(lo, hi)| r.line >= lo && r.line <= hi).unwrap_or(false);
            let gutter = match (i == self.cursor, selected) {
                (true, _) => Span::styled("▌ ", Style::default().fg(Color::Yellow)),
                (false, true) => Span::styled("┃ ", Style::default().fg(Color::Magenta)),
                _ => Span::raw("  "),
            };
            let mut base = match kind {
                Kind::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                Kind::Agent => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                Kind::Tool => Style::default().fg(Color::DarkGray),
                Kind::Text => Style::default(),
            };
            if selected {
                base = base.bg(Color::Rgb(60, 40, 80));
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
                    "j/k move · ^d/^u half · PgUp/PgDn · g/G · [ ] message · / n N search · v select · y yank · Y line · q quit",
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        };
        f.render_widget(Paragraph::new(status), chunks[2]);
    }
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

    #[test]
    fn wrap_respects_width_and_keeps_line_index() {
        let lines = vec![SrcLine::text("abcdefghij"), SrcLine::text(""), SrcLine::text("xy")];
        let rows = wrap_rows(&lines, 4);
        let got: Vec<(usize, &str)> = rows.iter().map(|r| (r.line, r.text.as_str())).collect();
        assert_eq!(got, vec![(0, "abcd"), (0, "efgh"), (0, "ij"), (1, ""), (2, "xy")]);
    }

    #[test]
    fn renders_claude_jsonl_into_conversation() {
        let jsonl = r#"
{"type":"user","timestamp":"2026-08-30T14:40:58.987Z","message":{"role":"user","content":"hi there <system-reminder>secret hook noise</system-reminder>"}}
{"type":"assistant","timestamp":"2026-08-30T14:41:01.000Z","message":{"id":"m1","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Hello!\nSecond line."}]}}
{"type":"assistant","timestamp":"2026-08-30T14:41:02.000Z","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test","description":"run tests"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8"}]}}
{"type":"user","isSidechain":true,"message":{"content":"subagent chatter"}}
{"type":"user","isMeta":true,"message":{"content":"meta"}}
{"type":"user","message":{"content":[{"type":"text","text":"thanks"}]}}
"#;
        let lines = render_claude_jsonl(jsonl);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts[0], "▎you › 14:40");
        assert_eq!(texts[1], "hi there"); // system-reminder stripped
        assert_eq!(texts[2], "");
        assert_eq!(texts[3], "▎claude › 14:41");
        assert_eq!(texts[4], "Hello!");
        assert_eq!(texts[5], "Second line.");
        assert_eq!(texts[6], "  ⚙ Bash  cargo test"); // no second header for the same speaker
        assert_eq!(texts[7], "  ↳ l1");
        assert_eq!(texts[7 + RESULT_FOLD], "  │ … (+2 lines)");
        assert!(!texts.iter().any(|t| t.contains("subagent") || t.contains("meta")));
        assert_eq!(*texts.last().unwrap(), "thanks");
        assert_eq!(lines[0].kind, Kind::User);
        assert_eq!(lines[3].kind, Kind::Agent);
        assert_eq!(lines[6].kind, Kind::Tool);
    }

    #[test]
    fn selection_yank_range_is_line_wise_over_wrapped_rows() {
        let src = Source {
            title: "t".into(),
            origin: "test".into(),
            lines: vec![SrcLine::text("aaaaaaaa"), SrcLine::text("b"), SrcLine::text("c")],
        };
        let mut v = View::new(src);
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
    fn message_jumps_land_on_headers() {
        let src = Source {
            title: "t".into(),
            origin: "test".into(),
            lines: vec![
                SrcLine::new("▎you ›", Kind::User),
                SrcLine::text("q"),
                SrcLine::new("▎claude ›", Kind::Agent),
                SrcLine::text("a"),
            ],
        };
        let mut v = View::new(src);
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
