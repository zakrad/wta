use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, Terminal};

use crate::{status, tmux, worktree};

// ---- palette: ANSI-indexed so it maps through the terminal theme exactly like
// tmux does (green = ANSI 2, bright green = ANSI 10). No hardcoded hex. ----
const GREEN: Color = Color::Green;
const GREEN_BRIGHT: Color = Color::LightGreen;
// Body text (preview + diff context) uses the terminal's DEFAULT foreground so
// the UI blends into whatever theme you run. All colors are ANSI-indexed (no
// RGB), so the whole theme inherits your terminal palette at zero binary cost.
const GREEN_SOFT: Color = Color::Reset;
const SEL_BG: Color = Color::Green;
const SEL_FG: Color = Color::Black;
const HL: Color = Color::Green;
const ADD: Color = Color::LightGreen;
const DEL: Color = Color::Red;
const RED: Color = Color::Red;
const HUNK: Color = Color::Green;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Running,
    Ready,
    NeedsInput,
    Exited,
    Idle,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Preview,
    Diff,
}

enum Modal {
    None,
    NewTask { name: String, prompt: bool },
    NewPrompt { task: String, prompt: String },
    Confirm(String),
    Resume(String),
    Push(String),
    Help,
}

struct Row {
    task: String,
    branch: String,
    status: Status,
    added: u32,
    removed: u32,
    session: String,
    alive: bool,
    path: Option<PathBuf>,
}

struct App {
    rows: Vec<Row>,
    sel: usize,
    tab: Tab,
    modal: Modal,
    preview: String,
    diff_text: String,
    hashes: HashMap<String, u64>,
    spin: usize,
    msg: Option<(String, bool, Instant)>, // (text, is_error, when)
    attach: Option<String>,               // session to attach to after this frame
    scroll: u16,                          // preview/diff scroll offset
}

impl App {
    fn new() -> Self {
        App {
            rows: Vec::new(),
            sel: 0,
            tab: Tab::Preview,
            modal: Modal::None,
            preview: String::new(),
            diff_text: String::new(),
            hashes: HashMap::new(),
            spin: 0,
            msg: None,
            attach: None,
            scroll: 0,
        }
    }
    fn selected(&self) -> Option<&Row> {
        self.rows.get(self.sel)
    }
    fn set_err(&mut self, e: impl ToString) {
        self.msg = Some((e.to_string(), true, Instant::now()));
    }
    fn set_info(&mut self, s: impl ToString) {
        self.msg = Some((s.to_string(), false, Instant::now()));
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let mut term = Term::new(CrosstermBackend::new(stdout))?;
    let res = event_loop(&mut term);
    disable_raw_mode().ok();
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )
    .ok();
    term.show_cursor().ok();
    res
}

/// Suspend the TUI, attach to the tmux session inline, resume on detach.
fn attach_inline(term: &mut Term, session: &str) {
    disable_raw_mode().ok();
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )
    .ok();
    let _ = tmux::attach_blocking(session);
    enable_raw_mode().ok();
    execute!(
        term.backend_mut(),
        EnterAlternateScreen,
        crossterm::cursor::Hide
    )
    .ok();
    let _ = term.clear();
}

fn event_loop(term: &mut Term) -> Result<()> {
    let mut app = App::new();
    refresh(&mut app);
    load_detail(&mut app);
    let mut last_tick = Instant::now();

    loop {
        if let Some((_, _, t)) = &app.msg {
            if t.elapsed() > Duration::from_secs(6) {
                app.msg = None;
            }
        }
        term.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && handle_key(&mut app, key)? {
                    return Ok(());
                }
            }
        }

        if let Some(session) = app.attach.take() {
            attach_inline(term, &session);
            refresh(&mut app);
            load_detail(&mut app);
            last_tick = Instant::now();
        }

        if last_tick.elapsed() >= Duration::from_millis(600) {
            app.spin = (app.spin + 1) % SPINNER.len();
            refresh(&mut app);
            load_detail(&mut app);
            last_tick = Instant::now();
        }
    }
}

/// Returns Ok(true) to quit.
fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match &mut app.modal {
        Modal::NewTask { name, prompt } => {
            match key.code {
                KeyCode::Enter => {
                    let task = name.trim().to_string();
                    let want_prompt = *prompt;
                    if task.is_empty() {
                        app.modal = Modal::None;
                    } else if want_prompt {
                        app.modal = Modal::NewPrompt {
                            task,
                            prompt: String::new(),
                        };
                    } else {
                        app.modal = Modal::None;
                        if let Err(e) = worktree::new(&task, &[]) {
                            app.set_err(e);
                        }
                        refresh(app);
                    }
                }
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => name.push(c),
                _ => {}
            }
            return Ok(false);
        }
        Modal::NewPrompt { task, prompt } => {
            match key.code {
                KeyCode::Enter => {
                    let task = std::mem::take(task);
                    let prompt = std::mem::take(prompt);
                    app.modal = Modal::None;
                    // Claude Code accepts an initial prompt as a positional arg:
                    // `claude "<prompt>"`. Empty prompt → plain new agent.
                    let args: Vec<String> = if prompt.trim().is_empty() {
                        vec![]
                    } else {
                        vec![prompt]
                    };
                    if let Err(e) = worktree::new(&task, &args) {
                        app.set_err(e);
                    }
                    refresh(app);
                }
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Backspace => {
                    prompt.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    prompt.push(c)
                }
                _ => {}
            }
            return Ok(false);
        }
        Modal::Confirm(task) => {
            match key.code {
                KeyCode::Char('y') => {
                    let task = task.clone();
                    app.modal = Modal::None;
                    if let Err(e) = worktree::rm(&task, false) {
                        app.set_err(e);
                    }
                    refresh(app);
                    load_detail(app);
                }
                KeyCode::Char('n') | KeyCode::Esc => app.modal = Modal::None,
                _ => {}
            }
            return Ok(false);
        }
        Modal::Resume(task) => {
            match key.code {
                KeyCode::Char('y') => {
                    let task = task.clone();
                    app.modal = Modal::None;
                    match app
                        .rows
                        .iter()
                        .find(|r| r.task == task)
                        .and_then(|r| r.path.clone())
                    {
                        Some(p) => {
                            if let Err(e) = worktree::resume_at(&task, &p) {
                                app.set_err(e);
                            }
                        }
                        None => app.set_err("worktree not found to resume"),
                    }
                    refresh(app);
                    load_detail(app);
                }
                KeyCode::Char('n') | KeyCode::Esc => app.modal = Modal::None,
                _ => {}
            }
            return Ok(false);
        }
        Modal::Push(task) => {
            match key.code {
                KeyCode::Char('y') => {
                    let task = task.clone();
                    app.modal = Modal::None;
                    app.set_info(format!("pushing {task}…"));
                    match worktree::push(&task, true) {
                        Ok(summary) => app.set_info(summary),
                        Err(e) => app.set_err(e),
                    }
                    refresh(app);
                    load_detail(app);
                }
                KeyCode::Char('n') | KeyCode::Esc => app.modal = Modal::None,
                _ => {}
            }
            return Ok(false);
        }
        Modal::Help => {
            app.modal = Modal::None;
            return Ok(false);
        }
        Modal::None => {}
    }

    // Shift+Up/Down scroll the preview/diff pane.
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Up => {
                app.scroll = app.scroll.saturating_sub(1);
                return Ok(false);
            }
            KeyCode::Down => {
                app.scroll = app.scroll.saturating_add(1);
                return Ok(false);
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Char('?') => app.modal = Modal::Help,
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.rows.is_empty() {
                app.sel = (app.sel + 1) % app.rows.len();
                app.scroll = 0;
                load_detail(app);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.rows.is_empty() {
                app.sel = (app.sel + app.rows.len() - 1) % app.rows.len();
                app.scroll = 0;
                load_detail(app);
            }
        }
        KeyCode::Tab => {
            app.tab = if app.tab == Tab::Preview {
                Tab::Diff
            } else {
                Tab::Preview
            };
            app.scroll = 0;
            load_detail(app);
        }
        KeyCode::Enter | KeyCode::Char('o') => match app.selected() {
            Some(r) if r.alive => app.attach = Some(r.session.clone()),
            Some(r) if r.path.is_some() => app.modal = Modal::Resume(r.task.clone()),
            Some(_) => app.set_err("no session and no worktree to resume"),
            None => {}
        },
        KeyCode::Char('n') => {
            app.modal = Modal::NewTask {
                name: String::new(),
                prompt: false,
            }
        }
        KeyCode::Char('N') => {
            app.modal = Modal::NewTask {
                name: String::new(),
                prompt: true,
            }
        }
        KeyCode::Char('s') => {
            if let Some(task) = app.selected().map(|r| r.task.clone()) {
                if let Err(e) = worktree::stop(&task) {
                    app.set_err(e);
                }
                refresh(app);
                load_detail(app);
            }
        }
        KeyCode::Char('D') => {
            if let Some(task) = app.selected().map(|r| r.task.clone()) {
                app.modal = Modal::Confirm(task);
            }
        }
        KeyCode::Char('p') => {
            if let Some(task) = app.selected().map(|r| r.task.clone()) {
                app.modal = Modal::Push(task);
            }
        }
        KeyCode::Char('r') => {
            refresh(app);
            load_detail(app);
        }
        _ => {}
    }
    Ok(false)
}

// ---------- git helpers ----------
fn git_in(path: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}
fn base_of(path: &Path) -> String {
    for b in ["main", "master"] {
        if git_in(
            path,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{b}"),
            ],
        )
        .is_some()
        {
            return b.to_string();
        }
    }
    "HEAD".to_string()
}
fn merge_base(path: &Path) -> Option<String> {
    git_in(path, &["merge-base", "HEAD", &base_of(path)]).filter(|s| !s.is_empty())
}
fn numstat(path: &Path) -> (u32, u32) {
    let mb = match merge_base(path) {
        Some(s) => s,
        None => return (0, 0),
    };
    let out = match git_in(path, &["diff", "--numstat", &mb]) {
        Some(s) => s,
        None => return (0, 0),
    };
    let (mut a, mut d) = (0u32, 0u32);
    for line in out.lines() {
        let mut it = line.split('\t');
        a += it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
        d += it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
    }
    (a, d)
}
fn full_diff(path: &Path) -> String {
    match merge_base(path) {
        Some(mb) => git_in(path, &["diff", &mb]).unwrap_or_default(),
        None => String::new(),
    }
}
fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn refresh(app: &mut App) {
    let states: HashMap<String, status::AgentState> = status::read_all_states()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.task.clone(), s))
        .collect();

    let mut order: Vec<String> = Vec::new();
    let mut paths: HashMap<String, PathBuf> = HashMap::new();
    let mut branches: HashMap<String, String> = HashMap::new();

    if let Ok(managed) = worktree::list_managed() {
        for w in managed {
            order.push(w.task.clone());
            branches.insert(w.task.clone(), w.branch);
            paths.insert(w.task, w.path);
        }
    }
    for (task, st) in &states {
        if !paths.contains_key(task) && !st.cwd.is_empty() {
            order.push(task.clone());
            paths.insert(task.clone(), PathBuf::from(&st.cwd));
        }
    }
    order.sort();
    order.dedup();

    let prev_task = app.selected().map(|r| r.task.clone());

    let mut rows = Vec::new();
    for task in order {
        let path = paths.get(&task).cloned();
        let branch = branches
            .get(&task)
            .cloned()
            .or_else(|| {
                path.as_deref()
                    .and_then(|p| git_in(p, &["rev-parse", "--abbrev-ref", "HEAD"]))
            })
            .unwrap_or_default();
        let (added, removed) = path.as_deref().map(numstat).unwrap_or((0, 0));
        let session = tmux::session_name(&task);
        let alive = tmux::has_session(&session);

        let status = if alive {
            let text = tmux::capture(&session).unwrap_or_default();
            let h = hash_str(&text);
            let changed = app
                .hashes
                .insert(task.clone(), h)
                .map(|old| old != h)
                .unwrap_or(true);
            match states.get(&task).map(|s| s.status.as_str()) {
                Some("needs_input") => Status::NeedsInput,
                _ if changed => Status::Running,
                _ => Status::Ready,
            }
        } else if path.is_some() {
            Status::Exited
        } else {
            Status::Idle
        };

        rows.push(Row {
            task,
            branch,
            status,
            added,
            removed,
            session,
            alive,
            path,
        });
    }

    app.rows = rows;
    app.sel = prev_task
        .and_then(|t| app.rows.iter().position(|r| r.task == t))
        .unwrap_or(0)
        .min(app.rows.len().saturating_sub(1));
}

fn load_detail(app: &mut App) {
    let (alive, session, path) = match app.selected() {
        Some(r) => (r.alive, r.session.clone(), r.path.clone()),
        None => {
            app.preview.clear();
            app.diff_text.clear();
            return;
        }
    };
    match app.tab {
        Tab::Preview => {
            app.preview = if alive {
                tmux::capture(&session).unwrap_or_else(|| "(no output)".into())
            } else {
                "Session not running. Press Enter to resume.".into()
            };
        }
        Tab::Diff => {
            app.diff_text = match path.as_deref() {
                Some(p) => {
                    let d = full_diff(p);
                    if d.is_empty() {
                        "No changes.".into()
                    } else {
                        d
                    }
                }
                None => "No worktree.".into(),
            };
        }
    }
}

fn status_glyph(app: &App, s: Status) -> Span<'static> {
    match s {
        Status::Running => Span::styled(
            SPINNER[app.spin].to_string(),
            Style::default().fg(GREEN_BRIGHT),
        ),
        Status::Ready => Span::styled("● ".to_string(), Style::default().fg(GREEN)),
        Status::NeedsInput => Span::styled("▲ ".to_string(), Style::default().fg(Color::Yellow)),
        Status::Exited => Span::styled("✗ ".to_string(), Style::default().fg(RED)),
        Status::Idle => Span::styled("· ".to_string(), Style::default().fg(Color::DarkGray)),
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(root[0]);
    render_list(f, app, cols[0]);
    render_right(f, app, cols[1]);
    render_menu(f, app, root[1]);
    render_err(f, app, root[2]);
    render_modal(f, app);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HL))
        .title(" Instances ")
        .title_style(
            Style::default()
                .bg(GREEN)
                .fg(SEL_FG)
                .add_modifier(Modifier::BOLD),
        );
    let inner_w = area.width.saturating_sub(2) as usize;
    let mut items: Vec<ListItem> = Vec::new();
    for (i, r) in app.rows.iter().enumerate() {
        let head = format!(" {}. {}", i + 1, r.task);
        let glyph = status_glyph(app, r.status);
        let pad1 = inner_w.saturating_sub(head.chars().count() + 2);
        let line1 = Line::from(vec![Span::raw(head), Span::raw(" ".repeat(pad1)), glyph]);

        let bhead = format!("   Ꮧ-{}", r.branch);
        let counts_len = format!("+{},-{} ", r.added, r.removed).chars().count();
        let pad2 = inner_w.saturating_sub(bhead.chars().count() + counts_len);
        let line2 = Line::from(vec![
            Span::styled(bhead, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(pad2)),
            Span::styled(format!("+{}", r.added), Style::default().fg(GREEN)),
            Span::styled(",", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("-{} ", r.removed), Style::default().fg(RED)),
        ]);
        items.push(ListItem::new(vec![line1, line2, Line::from("")]));
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no agents — press 'n'",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(SEL_BG)
            .fg(SEL_FG)
            .add_modifier(Modifier::BOLD),
    );
    let mut st = ListState::default();
    if !app.rows.is_empty() {
        st.select(Some(app.sel));
    }
    f.render_stateful_widget(list, area, &mut st);
}

fn render_right(f: &mut Frame, app: &App, area: Rect) {
    let title = match app.selected() {
        Some(r) => format!(" {} ", r.task),
        None => " agent ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HL))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let tabs = Tabs::new(vec!["Preview", "Diff"])
        .select(if app.tab == Tab::Preview { 0 } else { 1 })
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(GREEN)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .divider(" ");
    f.render_widget(tabs, parts[0]);

    let body_h = parts[1].height as usize;
    let (content, scroll): (Vec<Line>, u16) = match app.tab {
        // Preview is a live tail (latest output pinned to the bottom); no scroll.
        Tab::Preview => (
            tail_lines(&app.preview, body_h)
                .into_iter()
                .map(|l| Line::styled(l.to_string(), Style::default().fg(GREEN_SOFT)))
                .collect(),
            0,
        ),
        // Diff renders in full and is scrollable with Shift+↑/↓.
        Tab::Diff => {
            let (a, d) = app
                .selected()
                .map(|r| (r.added, r.removed))
                .unwrap_or((0, 0));
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{a} additions(+)"), Style::default().fg(ADD)),
                Span::raw("  "),
                Span::styled(format!("{d} deletions(-)"), Style::default().fg(DEL)),
            ])];
            for l in app.diff_text.lines() {
                lines.push(colorize_diff_line(l));
            }
            (lines, app.scroll)
        }
    };
    f.render_widget(Paragraph::new(content).scroll((scroll, 0)), parts[1]);
}

fn colorize_diff_line(l: &str) -> Line<'static> {
    let owned = l.to_string();
    let style = if l.starts_with("@@") {
        Style::default().fg(HUNK)
    } else if l.starts_with("+++") || l.starts_with("---") {
        Style::default().fg(Color::DarkGray)
    } else if l.starts_with('+') {
        Style::default().fg(ADD)
    } else if l.starts_with('-') {
        Style::default().fg(DEL)
    } else {
        Style::default().fg(GREEN_SOFT)
    };
    Line::styled(owned, style)
}

fn render_menu(f: &mut Frame, app: &App, area: Rect) {
    let action = Style::default().fg(GREEN);
    let muted = Style::default().fg(Color::DarkGray);
    let spans = if app.rows.is_empty() {
        vec![
            Span::styled("n", action),
            Span::styled(" new  │  ", muted),
            Span::styled("q", action),
            Span::styled(" quit", muted),
        ]
    } else {
        let exited = app
            .selected()
            .map(|r| r.status == Status::Exited)
            .unwrap_or(false);
        let (fkey, flabel) = if exited {
            ("↵", " resume  ")
        } else {
            ("↵/o", " attach  ")
        };
        vec![
            Span::styled("n/N", action),
            Span::styled(" new  ", muted),
            Span::styled("s", action),
            Span::styled(" stop  ", muted),
            Span::styled("D", action),
            Span::styled(" kill  │  ", muted),
            Span::styled(fkey, action),
            Span::styled(flabel, muted),
            Span::styled("p", action),
            Span::styled(" push  │  ", muted),
            Span::styled("tab", action),
            Span::styled(" switch  ", muted),
            Span::styled("?", action),
            Span::styled(" help  ", muted),
            Span::styled("q", action),
            Span::styled(" quit", muted),
        ]
    };
    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn render_err(f: &mut Frame, app: &App, area: Rect) {
    if let Some((text, is_err, _)) = &app.msg {
        let color = if *is_err { Color::Red } else { GREEN };
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(" {text} "),
                Style::default().fg(color),
            ))
            .alignment(Alignment::Center),
            area,
        );
    }
}

fn render_modal(f: &mut Frame, app: &App) {
    match &app.modal {
        Modal::NewTask { name, prompt } => {
            let area = centered(50, 3, f.area());
            f.render_widget(Clear, area);
            let title = if *prompt {
                " new task name → then a prompt (Esc cancels) "
            } else {
                " new task name (Esc cancels) "
            };
            let p = Paragraph::new(format!("{name}▏")).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(GREEN))
                    .title(title),
            );
            f.render_widget(p, area);
        }
        Modal::NewPrompt { task, prompt } => {
            let area = centered(64, 6, f.area());
            f.render_widget(Clear, area);
            let p = Paragraph::new(format!("{prompt}▏"))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(GREEN))
                        .title(format!(" prompt for '{task}' (Enter starts, Esc cancels) ")),
                );
            f.render_widget(p, area);
        }
        Modal::Confirm(task) => {
            let area = centered(50, 4, f.area());
            f.render_widget(Clear, area);
            let body = vec![
                Line::raw(format!("Kill agent '{task}'?")),
                Line::from(vec![
                    Span::raw("Press "),
                    Span::styled("y", Style::default().fg(GREEN)),
                    Span::raw(" to confirm, "),
                    Span::styled("n", Style::default().fg(RED)),
                    Span::raw("/esc to cancel"),
                ]),
            ];
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(RED))
                        .title(" confirm "),
                ),
                area,
            );
        }
        Modal::Resume(task) => {
            let area = centered(56, 4, f.area());
            f.render_widget(Clear, area);
            let body = vec![
                Line::raw(format!("Agent '{task}' has exited.")),
                Line::from(vec![
                    Span::raw("Re-spawn in its worktree?  "),
                    Span::styled("y", Style::default().fg(GREEN)),
                    Span::raw(" / "),
                    Span::styled("n", Style::default().fg(RED)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(HL))
                        .title(" resume "),
                ),
                area,
            );
        }
        Modal::Push(task) => {
            let area = centered(58, 5, f.area());
            f.render_widget(Clear, area);
            let body = vec![
                Line::raw(format!("Commit, push & open a PR for agent/{task}?")),
                Line::raw("(commits any uncommitted work first)"),
                Line::from(vec![
                    Span::raw("Press "),
                    Span::styled("y", Style::default().fg(GREEN)),
                    Span::raw(" to push, "),
                    Span::styled("n", Style::default().fg(RED)),
                    Span::raw("/esc to cancel"),
                ]),
            ];
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(HL))
                        .title(" push "),
                ),
                area,
            );
        }
        Modal::Help => {
            let area = centered(52, 17, f.area());
            f.render_widget(Clear, area);
            let k = |key: &str, desc: &str| {
                Line::from(vec![
                    Span::styled(
                        format!("  {key:<10}"),
                        Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
                ])
            };
            let body = vec![
                k("j / k", "move up / down"),
                k("↵ / o", "attach into the agent (type here)"),
                k("Ctrl-q", "detach back to wta (while attached)"),
                k("tab", "switch Preview / Diff"),
                k("Shift+↑↓", "scroll the Diff"),
                k("n", "new agent"),
                k("N", "new agent with an initial prompt"),
                k("s", "stop (keep worktree — resume later)"),
                k("D", "kill (destroy worktree + branch)"),
                k("p", "commit + push + open a PR"),
                k("r", "refresh"),
                k("q", "quit"),
                Line::from(""),
                Line::styled(
                    "  agents run in tmux and survive closing",
                    Style::default().fg(Color::DarkGray),
                ),
                Line::styled(
                    "  the terminal. press any key to close.",
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(HL))
                        .title(" wta — keys "),
                ),
                area,
            );
        }
        Modal::None => {}
    }
}

fn tail_lines(s: &str, n: usize) -> Vec<&str> {
    if n == 0 {
        return Vec::new();
    }
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| ui(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let width = buf.area.width as usize;
        let mut out = String::new();
        for (i, cell) in buf.content.iter().enumerate() {
            if i > 0 && i % width == 0 {
                out.push('\n');
            }
            out.push_str(cell.symbol());
        }
        out
    }

    fn row(task: &str, s: Status, alive: bool, a: u32, d: u32) -> Row {
        Row {
            task: task.into(),
            branch: format!("agent/{task}"),
            status: s,
            added: a,
            removed: d,
            session: tmux::session_name(task),
            alive,
            path: Some(PathBuf::from("/tmp/x")),
        }
    }

    #[test]
    fn sidebar_layout() {
        let mut app = App::new();
        app.rows = vec![
            row("auth", Status::Running, true, 40, 4),
            row("flaky", Status::NeedsInput, true, 0, 0),
        ];
        app.sel = 0;
        app.preview = "$ cargo test\nrunning 8 tests\ntest result: ok. 8 passed".into();
        let screen = render_to_string(&mut app, 100, 16);
        println!("\n{screen}\n");
        assert!(screen.contains("Instances"));
        assert!(screen.contains("1. auth"));
        assert!(screen.contains("Ꮧ-agent/auth"));
        assert!(screen.contains("+40"));
        assert!(screen.contains("Preview") && screen.contains("Diff"));
        assert!(screen.contains("test result: ok"));
        assert!(screen.contains("attach"));
    }

    #[test]
    fn exited_agent_shows_resume() {
        let mut app = App::new();
        app.rows = vec![row("gone", Status::Exited, false, 1, 0)];
        app.sel = 0;
        let screen = render_to_string(&mut app, 100, 12);
        println!("\n{screen}\n");
        assert!(screen.contains('✗'));
        assert!(screen.contains("resume"));
    }

    #[test]
    fn diff_tab_colorizes_and_shows_counts() {
        let mut app = App::new();
        app.rows = vec![row("x", Status::Ready, true, 2, 1)];
        app.sel = 0;
        app.tab = Tab::Diff;
        app.diff_text = "@@ -1 +1 @@\n-old\n+new\n+extra".into();
        let screen = render_to_string(&mut app, 100, 16);
        println!("\n{screen}\n");
        assert!(screen.contains("2 additions(+)"));
        assert!(screen.contains("1 deletions(-)"));
        assert!(screen.contains("+new"));
    }
}
