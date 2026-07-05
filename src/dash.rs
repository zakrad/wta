use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
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
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, Terminal};

use ansi_to_tui::IntoText;
use unicode_width::UnicodeWidthStr;

use crate::{status, tmux, worktree};

/// Truncate `s` to at most `max` display columns, adding `…` if it was clipped.
fn truncate_cols(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.to_string().width();
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

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
    Merged,
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
    NewTask {
        name: String,
        prompt: bool,
    },
    NewPrompt {
        task: String,
        prompt: String,
    },
    Confirm(String),
    ForceKill { task: String, unpushed: u32 },
    Resume(String),
    Push(String),
    BranchPick {
        branches: Vec<String>,
        filter: String,
        sel: usize,
    },
    Matrix(Vec<Line<'static>>),
    QuickSend {
        task: String,
        text: String,
    },
    Help,
}

/// Sanitize a branch name into a task/dir name (e.g. `feature/login` -> `feature_login`).
fn sanitize_task(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Branches matching the filter (case-insensitive substring).
fn branch_matches<'a>(branches: &'a [String], filter: &str) -> Vec<&'a String> {
    let f = filter.to_lowercase();
    branches
        .iter()
        .filter(|b| f.is_empty() || b.to_lowercase().contains(&f))
        .collect()
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

/// A `.wta/verify.sh` run for one agent. Runs async (spawn + poll) so the repo's
/// test/lint suite never blocks the dashboard.
enum Check {
    Running {
        child: std::process::Child,
        log: PathBuf,
        since: Instant,
    },
    Done {
        code: i32,
    },
}

struct App {
    repo: String, // repo id namespacing sessions + state for this dashboard's repo
    root: PathBuf, // repo root (for locating .wta/verify.sh)
    checks: HashMap<String, Check>, // task -> verification result/run
    rows: Vec<Row>,
    sel: usize,
    tab: Tab,
    modal: Modal,
    preview: String,
    diff_text: String,
    hashes: HashMap<String, u64>,
    diffcache: HashMap<String, (u32, u32)>, // task -> (added, removed), cadence-refreshed
    mergedcache: HashMap<String, bool>,     // task -> branch merged into base, cadence-refreshed
    tick: u64,                              // refresh counter driving the diffstat cadence
    trust_seen: HashMap<String, Instant>,   // session -> first time we saw it (trust grace)
    trust_done: HashSet<String>,            // sessions whose trust prompt is handled/disarmed
    spin: usize,
    msg: Option<(String, bool, Instant)>, // (text, is_error, when)
    attach: Option<String>,               // session to attach to after this frame
    open: Option<(String, PathBuf)>,      // (editor cmd, worktree) to open inline after this frame
    scroll: u16,                          // preview/diff scroll offset
    scrollback: Option<String>,           // Some => Preview scroll mode: full (colored) history snapshot
    prev_status: HashMap<String, Status>, // last-seen status per task, for transition detection
    attention: HashSet<String>,           // agents that finished / need input and haven't been viewed
    bell: bool,                           // ring the terminal bell after this refresh
}

impl App {
    fn new() -> Self {
        App {
            repo: worktree::repo_id().unwrap_or_default(),
            root: worktree::repo_root().unwrap_or_default(),
            checks: HashMap::new(),
            rows: Vec::new(),
            sel: 0,
            tab: Tab::Preview,
            modal: Modal::None,
            preview: String::new(),
            diff_text: String::new(),
            hashes: HashMap::new(),
            diffcache: HashMap::new(),
            mergedcache: HashMap::new(),
            tick: 0,
            trust_seen: HashMap::new(),
            trust_done: HashSet::new(),
            spin: 0,
            msg: None,
            attach: None,
            open: None,
            scroll: 0,
            scrollback: None,
            prev_status: HashMap::new(),
            attention: HashSet::new(),
            bell: false,
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

impl Drop for App {
    // never leave a verify.sh orphaned when the dashboard quits
    fn drop(&mut self) {
        for c in self.checks.values_mut() {
            if let Check::Running { child, .. } = c {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
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

/// Ring the terminal bell (BEL is non-printing, safe to inject over the alt screen).
fn ring_bell() {
    use std::io::Write;
    let mut o = std::io::stdout();
    let _ = o.write_all(b"\x07"); // terminal bell (audible or visual, per your terminal)
    let _ = o.flush();
    play_notify_sound(); // an actual, always-audible system sound
}

/// Play a system notification sound so alerts are audible even when the terminal
/// bell is set to visual/off. Opt out with `WTA_NOTIFY_SOUND=0`; point
/// `WTA_NOTIFY_SOUND=/path/to/sound` at your own. Fire-and-forget, non-blocking.
fn play_notify_sound() {
    let cfg = std::env::var("WTA_NOTIFY_SOUND").unwrap_or_default();
    if cfg == "0" {
        return;
    }
    let custom = (!cfg.is_empty() && cfg != "1").then_some(cfg);
    let (player, file) = if cfg!(target_os = "macos") {
        ("afplay", custom.unwrap_or_else(|| "/System/Library/Sounds/Glass.aiff".into()))
    } else {
        ("paplay", custom.unwrap_or_else(|| "/usr/share/sounds/freedesktop/stereo/complete.oga".into()))
    };
    let _ = std::process::Command::new(player)
        .arg(file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Suspend the TUI, run a terminal editor (nvim/…) in the worktree, resume on quit.
fn open_inline(term: &mut Term, cmd: &str, path: &Path) {
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen, crossterm::cursor::Show).ok();
    let mut it = cmd.split_whitespace();
    if let Some(prog) = it.next() {
        let args: Vec<&str> = it.collect();
        let _ = std::process::Command::new(prog)
            .args(&args)
            .arg(path)
            .current_dir(path)
            .status();
    }
    enable_raw_mode().ok();
    execute!(term.backend_mut(), EnterAlternateScreen, crossterm::cursor::Hide).ok();
    let _ = term.clear();
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
        poll_checks(&mut app);
        term.draw(|f| ui(f, &mut app))?;
        if app.bell {
            ring_bell(); // an agent finished / needs input while off-screen
            app.bell = false;
        }

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

        if let Some((cmd, path)) = app.open.take() {
            open_inline(term, &cmd, &path);
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
                    match worktree::rm(&task, false) {
                        Ok(_) => {
                            refresh(app);
                            load_detail(app);
                        }
                        // worktree has uncommitted work → ask before discarding it,
                        // and warn if committed-but-unpushed work would go too
                        Err(_) => {
                            let unpushed = app
                                .rows
                                .iter()
                                .find(|r| r.task == task)
                                .and_then(|r| r.path.as_deref())
                                .map(unpushed_count)
                                .unwrap_or(0);
                            app.modal = Modal::ForceKill { task, unpushed };
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => app.modal = Modal::None,
                _ => {}
            }
            return Ok(false);
        }
        Modal::ForceKill { task, .. } => {
            match key.code {
                KeyCode::Char('y') => {
                    let task = task.clone();
                    app.modal = Modal::None;
                    if let Err(e) = worktree::rm(&task, true) {
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
        Modal::BranchPick {
            branches,
            filter,
            sel,
        } => {
            let n = branch_matches(branches, filter).len();
            match key.code {
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Up => *sel = sel.saturating_sub(1),
                KeyCode::Down => {
                    if *sel + 1 < n {
                        *sel += 1;
                    }
                }
                KeyCode::Enter => {
                    let picked = branch_matches(branches, filter)
                        .get(*sel)
                        .map(|s| (*s).clone());
                    app.modal = Modal::None;
                    if let Some(base) = picked {
                        let task = sanitize_task(&base);
                        if let Err(e) = worktree::new_with_base(&task, &[], &base) {
                            app.set_err(e);
                        }
                        refresh(app);
                    }
                }
                KeyCode::Backspace => {
                    filter.pop();
                    *sel = 0;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    filter.push(c);
                    *sel = 0;
                }
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
        Modal::QuickSend { task, text } => {
            match key.code {
                KeyCode::Enter => {
                    let task = std::mem::take(task);
                    let text = std::mem::take(text);
                    app.modal = Modal::None;
                    if !text.trim().is_empty() {
                        let session = tmux::session_name(&app.repo, &task);
                        if !tmux::has_session(&session) {
                            app.set_err(format!("'{task}' is no longer running"));
                        } else if !tmux::pane_is_idle(&session) {
                            app.set_err(format!("'{task}' became busy — try again"));
                        } else {
                            match tmux::send_text(&session, &text) {
                                Ok(_) => app.set_info(format!("sent → {task}")),
                                Err(e) => app.set_err(e),
                            }
                        }
                        refresh(app);
                        load_detail(app);
                    }
                }
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            return Ok(false);
        }
        Modal::Matrix(_) | Modal::Help => {
            app.modal = Modal::None;
            return Ok(false);
        }
        Modal::None => {}
    }

    // Shift+Up/Down scroll the active pane. `scroll` means "lines away from the
    // resting edge": Diff rests at the top (scroll down into it), Preview rests at
    // the bottom / latest output (scroll up into history).
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match (app.tab, key.code) {
            (Tab::Preview, KeyCode::Up) => {
                // First Shift+↑ enters scroll mode: snapshot the FULL colored
                // scrollback so you can page back through history past the pane.
                if app.scrollback.is_none() {
                    if let Some(r) = app.selected() {
                        if r.alive {
                            app.scrollback = tmux::capture_colored(&r.session, true);
                        }
                    }
                }
                let buf = app.scrollback.as_deref().unwrap_or(&app.preview);
                let max_preview = buf.lines().count().saturating_sub(1) as u16;
                app.scroll = (app.scroll + 1).min(max_preview);
                return Ok(false);
            }
            (Tab::Preview, KeyCode::Down) => {
                app.scroll = app.scroll.saturating_sub(1);
                return Ok(false);
            }
            (Tab::Diff, KeyCode::Up) => {
                app.scroll = app.scroll.saturating_sub(1);
                return Ok(false);
            }
            (Tab::Diff, KeyCode::Down) => {
                app.scroll = app.scroll.saturating_add(1);
                return Ok(false);
            }
            _ => {} // fall through (e.g. Shift+J/K reorder)
        }
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Esc if app.scrollback.is_some() => {
            app.scrollback = None; // leave Preview scroll mode, back to live output
            app.scroll = 0;
        }
        KeyCode::Char('?') => app.modal = Modal::Help,
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.rows.is_empty() {
                app.sel = (app.sel + 1) % app.rows.len();
                app.scroll = 0;
                app.scrollback = None;
                load_detail(app);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.rows.is_empty() {
                app.sel = (app.sel + app.rows.len() - 1) % app.rows.len();
                app.scroll = 0;
                app.scrollback = None;
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
        KeyCode::Char('b') => match worktree::list_branches() {
            Ok(bs) if !bs.is_empty() => {
                app.modal = Modal::BranchPick {
                    branches: bs,
                    filter: String::new(),
                    sel: 0,
                }
            }
            Ok(_) => app.set_err("no branches to base an agent on"),
            Err(e) => app.set_err(e),
        },
        KeyCode::Char('e') => match app.selected().and_then(|r| r.path.clone()) {
            None => app.set_err("no worktree to open"),
            Some(path) => match worktree::editor_cmd() {
                None => app.set_err("set WTA_OPEN_CMD or $EDITOR (e.g. nvim, code)"),
                Some(cmd) => {
                    let forced = std::env::var("WTA_OPEN_INLINE").ok();
                    let inline = match forced.as_deref() {
                        Some("1") => true,
                        Some("0") => false,
                        _ => !worktree::is_gui_editor(&cmd), // terminal editor → inline
                    };
                    if inline {
                        app.open = Some((cmd, path)); // suspend TUI + run after this frame
                    } else {
                        // GUI editor: fire-and-forget so wta stays on the dashboard
                        let mut it = cmd.split_whitespace();
                        let prog = it.next().unwrap_or_default().to_string();
                        let args: Vec<String> = it.map(String::from).collect();
                        match std::process::Command::new(&prog)
                            .args(&args)
                            .arg(&path)
                            .current_dir(&path)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                        {
                            Ok(_) => app.set_info(format!("opened in {prog}")),
                            Err(e) => app.set_err(e),
                        }
                    }
                }
            },
        },
        KeyCode::Char('v') => {
            if let Some((task, path)) =
                app.selected().and_then(|r| r.path.clone().map(|p| (r.task.clone(), p)))
            {
                spawn_check(app, &task, &path);
            }
        }
        KeyCode::Char('J') => reorder(app, 1),
        KeyCode::Char('K') => reorder(app, -1),
        KeyCode::Char('m') => {
            let failing: HashSet<String> = app
                .checks
                .iter()
                .filter_map(|(t, c)| matches!(c, Check::Done { code, .. } if *code != 0).then(|| t.clone()))
                .collect();
            match matrix_lines(&failing) {
                Ok(lines) => app.modal = Modal::Matrix(lines),
                Err(e) => app.set_err(e),
            }
        }
        // quick-send one line to the selected agent, gated so we never inject
        // into a busy/streaming pane (only when it's idle at its prompt = Ready).
        KeyCode::Char('i') => match app.selected() {
            Some(r) if !r.alive => app.set_err("no live session — resume with ↵"),
            Some(r) if r.status == Status::Running => app.set_err("agent is working — wait for ● ready"),
            Some(r) if r.status == Status::NeedsInput => app.set_err("agent needs input — attach (↵) to answer"),
            Some(r) if r.status == Status::Ready => {
                app.modal = Modal::QuickSend { task: r.task.clone(), text: String::new() }
            }
            _ => {}
        },
        KeyCode::Char('r') => {
            refresh(app);
            load_detail(app);
        }
        _ => {}
    }
    Ok(false)
}

/// Build the colored mergeability grid for the `m` overlay (calls git merge-tree
/// pairwise; read-only, touches no working tree).
fn matrix_lines(failing: &HashSet<String>) -> anyhow::Result<Vec<Line<'static>>> {
    let m = worktree::mergeability()?;
    let n = m.labels.len();
    let mut out: Vec<Line<'static>> = Vec::new();
    if n <= 1 {
        out.push(Line::styled(
            "no agent branches to compare — create some with 'n'".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        return Ok(out);
    }
    let grid = m.grid();
    let w = 9usize;
    let short = |s: &str| -> String { s.chars().take(w - 1).collect() };
    // agents failing their .wta/verify.sh checks are shown red — don't merge them
    let label_color = |l: &str| if failing.contains(l) { RED } else { Color::Gray };

    let mut header = vec![Span::raw(format!("{:<pad$}", "", pad = w + 2))];
    for l in &m.labels {
        header.push(Span::styled(format!("{:<w$}", short(l), w = w), Style::default().fg(label_color(l))));
    }
    out.push(Line::from(header));
    for i in 0..n {
        let mut spans = vec![Span::styled(
            format!("{:<pad$}", short(&m.labels[i]), pad = w + 2),
            Style::default().fg(label_color(&m.labels[i])),
        )];
        for j in 0..n {
            let (txt, color) = if i == j {
                ("·", Color::DarkGray)
            } else {
                match grid[i][j] {
                    Some(true) => ("✓", GREEN),
                    Some(false) => ("✗", RED),
                    None => ("?", Color::DarkGray),
                }
            };
            spans.push(Span::styled(format!("{:<w$}", txt, w = w), Style::default().fg(color)));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::from(""));
    let conflicts: Vec<_> = m.pairs.iter().filter(|p| !p.clean).collect();
    if conflicts.is_empty() {
        out.push(Line::styled(
            "all branches merge cleanly — git merge-tree, no files touched".to_string(),
            Style::default().fg(GREEN),
        ));
    } else {
        out.push(Line::styled(
            "conflicts (git merge-tree — no files touched):".to_string(),
            Style::default().fg(Color::Gray),
        ));
        for p in conflicts {
            let files = if p.files.is_empty() { "conflict".to_string() } else { p.files.join(", ") };
            out.push(Line::from(vec![
                Span::styled(format!("  {} ", m.labels[p.i]), Style::default().fg(GREEN_SOFT)),
                Span::styled("✗ ".to_string(), Style::default().fg(RED)),
                Span::styled(format!("{}  ", m.labels[p.j]), Style::default().fg(GREEN_SOFT)),
                Span::styled(files, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    Ok(out)
}

/// Move the selected agent up (delta -1) or down (delta +1) in the list and
/// persist the new order to ~/.wta/order.json so it survives refreshes.
fn reorder(app: &mut App, delta: isize) {
    let i = app.sel;
    let j = i as isize + delta;
    if app.rows.is_empty() || j < 0 || j as usize >= app.rows.len() {
        return;
    }
    let mut tasks: Vec<String> = app.rows.iter().map(|r| r.task.clone()).collect();
    tasks.swap(i, j as usize);
    let map: std::collections::HashMap<String, u32> =
        tasks.iter().enumerate().map(|(idx, t)| (t.clone(), idx as u32)).collect();
    if let Err(e) = status::write_order(&app.repo, &map) {
        app.set_err(e);
        return;
    }
    // refresh() re-sorts by the new order and keeps the selection on the moved task.
    refresh(app);
    load_detail(app);
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
/// True if the agent's branch has landed in the base branch (all its commits are
/// ancestors of base) — and it isn't just a fresh branch sitting at base's tip.
fn is_merged(path: &Path, branch: &str) -> bool {
    let base = base_of(path);
    if base == branch || branch.is_empty() {
        return false;
    }
    if git_in(path, &["merge-base", "--is-ancestor", branch, &base]).is_none() {
        return false; // has commits not in base → not merged
    }
    let bt = git_in(path, &["rev-parse", branch]);
    let base_t = git_in(path, &["rev-parse", &base]);
    bt.is_some() && bt != base_t // exclude the un-worked branch sitting exactly at base
}
/// Like `git_in` but returns stdout even on a non-zero exit (needed for
/// `git diff --no-index`, which exits 1 whenever the files differ).
fn git_stdout(path: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}
/// Untracked, non-ignored files in the worktree — agent-created files that a
/// plain `git diff` wouldn't show.
fn untracked_files(path: &Path) -> Vec<String> {
    git_in(path, &["ls-files", "--others", "--exclude-standard"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}
/// Additions contributed by an untracked file (its line count; 0 if binary/missing).
fn untracked_adds(path: &Path, rel: &str) -> u32 {
    match std::fs::read(path.join(rel)) {
        Ok(bytes) if !bytes.contains(&0) => bytes.iter().filter(|&&b| b == b'\n').count() as u32,
        _ => 0,
    }
}
/// Commits on HEAD not present on any remote — work that a force-kill would destroy.
fn unpushed_count(path: &Path) -> u32 {
    git_in(path, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
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
    // include agent-created (untracked) files, which `git diff` omits
    for f in untracked_files(path) {
        a += untracked_adds(path, &f);
    }
    (a, d)
}
fn full_diff(path: &Path) -> String {
    let mut out = match merge_base(path) {
        Some(mb) => git_in(path, &["diff", &mb]).unwrap_or_default(),
        None => String::new(),
    };
    // append untracked files as new-file diffs (`--no-index` doesn't touch the index)
    for f in untracked_files(path) {
        if let Some(d) = git_stdout(path, &["diff", "--no-index", "--", "/dev/null", &f]) {
            if !d.trim().is_empty() {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&d);
            }
        }
    }
    out
}
fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---- verification gate: run `.wta/verify.sh` per agent, async (never blocks UI) ----

fn read_tail(log: &Path) -> String {
    let s = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(12);
    lines[start..].join("\n")
}

/// Spawn `.wta/verify.sh` for a task in its worktree, output to a temp log,
/// tracked in `app.checks`. Non-blocking — polled by `poll_checks`.
fn spawn_check(app: &mut App, task: &str, wt: &Path) {
    let script = app.root.join(".wta/verify.sh");
    if !script.exists() {
        app.set_err("no .wta/verify.sh in this repo — add one that exits non-zero on failure");
        return;
    }
    // keep the log in wta's own per-user state dir, not world-writable /tmp
    let log = match status::repo_dir(&app.repo) {
        Ok(d) => d.join(format!("verify-{}.log", sanitize_task(task))),
        Err(e) => return app.set_err(e),
    };
    let f = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => return app.set_err(e),
    };
    let f2 = match f.try_clone() {
        Ok(f) => f,
        Err(e) => return app.set_err(e),
    };
    match std::process::Command::new("bash")
        .arg(&script)
        .current_dir(wt)
        .stdout(std::process::Stdio::from(f))
        .stderr(std::process::Stdio::from(f2))
        .spawn()
    {
        Ok(child) => {
            app.checks.insert(task.to_string(), Check::Running { child, log, since: Instant::now() });
            app.set_info(format!("running checks for {task}…"));
        }
        Err(e) => app.set_err(e),
    }
}

/// Poll running checks without blocking; transition to Done when they exit or
/// time out (>5m).
fn poll_checks(app: &mut App) {
    let mut done: Vec<(String, i32, String)> = Vec::new();
    for (task, chk) in app.checks.iter_mut() {
        if let Check::Running { child, log, since } = chk {
            let timed_out = since.elapsed() > Duration::from_secs(300);
            match child.try_wait() {
                Ok(Some(status)) => done.push((task.clone(), status.code().unwrap_or(-1), read_tail(log))),
                Ok(None) if timed_out => {
                    let _ = child.kill();
                    let _ = child.wait(); // reap so it doesn't linger as a zombie
                    done.push((task.clone(), -1, "verify timed out (>5m)".into()));
                }
                Ok(None) => {}
                Err(_) => done.push((task.clone(), -1, "verify failed to run".into())),
            }
        }
    }
    for (task, code, tail) in done {
        let is_sel = app.rows.get(app.sel).map(|r| r.task == task).unwrap_or(false);
        if is_sel {
            if code == 0 {
                app.set_info(format!("✓ checks passed: {task}"));
            } else {
                let last = tail.lines().last().unwrap_or("").trim();
                app.set_err(format!("✗ checks failed (exit {code}) — {last}"));
            }
        }
        app.checks.insert(task, Check::Done { code });
    }
}

fn refresh(app: &mut App) {
    let states: HashMap<String, status::AgentState> = status::read_states(&app.repo)
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
    // Fold in state-file agents (hooks/`wta new` write these), but ONLY ones that
    // belong to this repo's `.agents` dir and whose worktree still exists — so
    // stale/ghost entries and agents from other repos don't pollute the board.
    let agents_dir = worktree::agents_dir().ok();
    for (task, st) in &states {
        if paths.contains_key(task) || st.cwd.is_empty() {
            continue;
        }
        let p = PathBuf::from(&st.cwd);
        let in_repo = agents_dir.as_ref().map(|d| p.starts_with(d)).unwrap_or(false);
        if in_repo && p.exists() {
            order.push(task.clone());
            paths.insert(task.clone(), p);
        }
    }
    // Sort by the persisted manual order (from J/K); unranked tasks fall to the
    // end alphabetically. Dedupe first since the sort key isn't the task name.
    let mut seen = HashSet::new();
    order.retain(|t| seen.insert(t.clone()));
    let rank = status::read_order(&app.repo);
    order.sort_by(|a, b| {
        let pa = rank.get(a).copied().unwrap_or(u32::MAX);
        let pb = rank.get(b).copied().unwrap_or(u32::MAX);
        pa.cmp(&pb).then_with(|| a.cmp(b))
    });

    let prev_task = app.selected().map(|r| r.task.clone());
    app.tick = app.tick.wrapping_add(1);
    let full_sweep = app.tick % 5 == 0; // recompute all diffstats every ~5th tick (~3s)
    let sel_task = prev_task.clone();
    let auto_trust = std::env::var("WTA_AUTO_TRUST").map(|v| v != "0").unwrap_or(true);

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

        // Heavy git diffstat runs only for the selected row each tick + all rows on
        // a slower cadence (or when new). Keyed by task, so J/K reorder is safe.
        let (added, removed) = match path.as_deref() {
            Some(p) => {
                let is_sel = sel_task.as_deref() == Some(task.as_str());
                if full_sweep || is_sel || !app.diffcache.contains_key(&task) {
                    let v = numstat(p);
                    app.diffcache.insert(task.clone(), v);
                    v
                } else {
                    app.diffcache.get(&task).copied().unwrap_or((0, 0))
                }
            }
            None => (0, 0),
        };

        // "merged" (branch landed in base) — cached on the same slow cadence as diffstat
        let merged = match path.as_deref() {
            Some(p) => {
                let is_sel = sel_task.as_deref() == Some(task.as_str());
                if full_sweep || is_sel || !app.mergedcache.contains_key(&task) {
                    let v = is_merged(p, &branch);
                    app.mergedcache.insert(task.clone(), v);
                    v
                } else {
                    app.mergedcache.get(&task).copied().unwrap_or(false)
                }
            }
            None => false,
        };

        let session = tmux::session_name(&app.repo, &task);
        let alive = tmux::has_session(&session);

        let status = if alive {
            let text = tmux::capture(&session).unwrap_or_default();
            // Auto-dismiss Claude's per-folder trust prompt within a startup grace
            // window (strict 3-string match, one-shot, opt out via WTA_AUTO_TRUST=0).
            if auto_trust && !app.trust_done.contains(&session) {
                let seen_at = *app.trust_seen.entry(session.clone()).or_insert_with(Instant::now);
                if seen_at.elapsed() > Duration::from_secs(10) {
                    app.trust_done.insert(session.clone()); // grace expired: disarm
                } else if is_trust_prompt(&text) {
                    let _ = tmux::send_enter(&session); // accept the default "Yes, proceed"
                    app.trust_done.insert(session.clone()); // one-shot
                }
            }
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
        // a landed branch reads as "merged" once it's not actively working
        let status = if merged && matches!(status, Status::Ready | Status::Exited | Status::Idle) {
            Status::Merged
        } else {
            status
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

    // bounded memory: drop entries for tasks/sessions that no longer exist
    let current: HashSet<&String> = app.rows.iter().map(|r| &r.task).collect();
    app.hashes.retain(|k, _| current.contains(k));
    app.diffcache.retain(|k, _| current.contains(k));
    app.mergedcache.retain(|k, _| current.contains(k));
    let live: HashSet<String> = app.rows.iter().map(|r| r.session.clone()).collect();
    app.trust_seen.retain(|k, _| live.contains(k));
    app.trust_done.retain(|k| live.contains(k));

    // Attention: flag "needs review" + ring the bell on the edges that matter — an
    // agent finishing a run, or newly asking for input — unless it's the pane you're
    // already looking at. Only on genuine transitions (prev must be known), so a
    // fresh dashboard doesn't nag about pre-existing state.
    let mut ring = false;
    let has_verify = app.root.join(".wta/verify.sh").exists();
    let sel_now = app.rows.get(app.sel).map(|r| r.task.clone());
    let mut to_check: Vec<(String, PathBuf)> = Vec::new();
    let mut invalidate: Vec<String> = Vec::new();
    for r in &app.rows {
        let now = r.status;
        let prev = app.prev_status.get(&r.task).copied();
        let became_needs = prev.is_some() && now == Status::NeedsInput && prev != Some(Status::NeedsInput);
        let finished = prev == Some(Status::Running) && matches!(now, Status::Ready | Status::Exited);
        if (became_needs || finished) && sel_now.as_deref() != Some(r.task.as_str()) && app.attention.insert(r.task.clone()) {
            ring = true;
        }
        // auto-run checks when an agent finishes; drop stale results when it resumes
        if finished && has_verify && !matches!(app.checks.get(&r.task), Some(Check::Running { .. })) {
            if let Some(p) = &r.path {
                to_check.push((r.task.clone(), p.clone()));
            }
        }
        if now == Status::Running && matches!(app.checks.get(&r.task), Some(Check::Done { .. })) {
            invalidate.push(r.task.clone());
        }
    }
    app.prev_status = app.rows.iter().map(|r| (r.task.clone(), r.status)).collect();
    app.attention.retain(|t| current.contains(t));
    // drop checks for gone tasks — but kill+reap any still-running verify first
    app.checks.retain(|t, c| {
        if current.contains(t) {
            return true;
        }
        if let Check::Running { child, .. } = c {
            let _ = child.kill();
            let _ = child.wait();
        }
        false
    });
    for t in invalidate {
        app.checks.remove(&t);
    }
    for (task, path) in to_check {
        spawn_check(app, &task, &path);
    }
    if ring {
        app.bell = true;
    }
}

/// Claude Code's per-folder trust dialog. Strict 3-string co-occurrence so normal
/// agent output can't trip it (strings confirmed current: claude-code #6797/#9256).
fn is_trust_prompt(text: &str) -> bool {
    text.contains("Do you trust the files in this folder?")
        && text.contains("Yes, proceed")
        && text.contains("No, exit")
}

fn load_detail(app: &mut App) {
    // viewing an agent clears its "needs review" flag
    if let Some(t) = app.selected().map(|r| r.task.clone()) {
        app.attention.remove(&t);
    }
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
                tmux::capture_colored(&session, false).unwrap_or_else(|| "(no output)".into())
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
        Status::Merged => Span::styled("✓ ".to_string(), Style::default().fg(Color::Cyan)),
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
        // finished-but-unviewed agents get a bright review glyph; needs-input keeps ▲
        let glyph = if app.attention.contains(&r.task) && r.status != Status::NeedsInput {
            Span::styled("◆ ".to_string(), Style::default().fg(Color::LightYellow))
        } else {
            status_glyph(app, r.status)
        };
        // optional verification-check indicator, shown just left of the status glyph
        let check: Option<Span> = app.checks.get(&r.task).map(|c| match c {
            Check::Running { .. } => Span::styled("⟳ ".to_string(), Style::default().fg(Color::DarkGray)),
            Check::Done { code: 0, .. } => Span::styled("✓ ".to_string(), Style::default().fg(GREEN_BRIGHT)),
            Check::Done { .. } => Span::styled("✗ ".to_string(), Style::default().fg(RED)),
        });
        let extra = if check.is_some() { 2 } else { 0 };
        // reserve cols for status glyph (+ check); truncate wide/long names cleanly
        let head = truncate_cols(&format!(" {}. {}", i + 1, r.task), inner_w.saturating_sub(2 + extra));
        let pad1 = inner_w.saturating_sub(head.width() + 2 + extra);
        let mut spans1 = vec![Span::raw(head), Span::raw(" ".repeat(pad1))];
        if let Some(c) = check {
            spans1.push(c);
        }
        spans1.push(glyph);
        let line1 = Line::from(spans1);

        let counts_len = format!("+{},-{} ", r.added, r.removed).width();
        // reserve room for the +/- counts first, then truncate the branch to fit
        let bhead = truncate_cols(&format!("   Ꮧ-{}", r.branch), inner_w.saturating_sub(counts_len));
        let pad2 = inner_w.saturating_sub(bhead.width() + counts_len);
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

fn render_right(f: &mut Frame, app: &mut App, area: Rect) {
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
    let (content, scroll): (Text, u16) = match app.tab {
        // Preview pins to the latest output (bottom); Shift+↑ scrolls up into
        // history (and snapshots the full scrollback into a scroll mode).
        Tab::Preview => {
            let scrolling = app.scrollback.is_some();
            let src = app.scrollback.as_deref().unwrap_or(&app.preview);
            // Parse tmux's ANSI (`-e`) so the agent's real colors show inline.
            let mut text = src
                .into_text()
                .unwrap_or_else(|_| Text::from(src.to_string()));
            if scrolling {
                text.lines.push(Line::styled(
                    "── scroll mode · ↑↓ history · Esc to exit ──",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let max_off = text.lines.len().saturating_sub(body_h) as u16;
            app.scroll = app.scroll.min(max_off);
            let top = max_off.saturating_sub(app.scroll);
            (text, top)
        }
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
            let max_off = lines.len().saturating_sub(body_h) as u16;
            app.scroll = app.scroll.min(max_off);
            (Text::from(lines), app.scroll)
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
        let dead = app.selected().map(|r| !r.alive).unwrap_or(false);
        let (fkey, flabel) = if dead {
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
            Span::styled("i", action),
            Span::styled(" send  ", muted),
            Span::styled("p", action),
            Span::styled(" push  ", muted),
            Span::styled("v", action),
            Span::styled(" check  ", muted),
            Span::styled("e", action),
            Span::styled(" edit  │  ", muted),
            Span::styled("tab", action),
            Span::styled(" switch  ", muted),
            Span::styled("?", action),
            Span::styled(" help  ", muted),
            Span::styled("q", action),
            Span::styled(" quit", muted),
        ]
    };
    // prepend an attention count when agents finished / need input off-screen
    let n = app.attention.len();
    let spans = if n > 0 {
        let mut lead = vec![
            Span::styled(
                format!("◆ {n} need you"),
                Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  │  ", muted),
        ];
        lead.extend(spans);
        lead
    } else {
        spans
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
        Modal::ForceKill { task, unpushed } => {
            let area = centered(64, if *unpushed > 0 { 6 } else { 5 }, f.area());
            f.render_widget(Clear, area);
            let mut body = vec![Line::raw(format!("'{task}' has uncommitted changes."))];
            if *unpushed > 0 {
                let s = if *unpushed == 1 { "" } else { "s" };
                body.push(Line::styled(
                    format!("⚠ {unpushed} unpushed commit{s} will be lost too."),
                    Style::default().fg(Color::Yellow),
                ));
            }
            body.push(Line::from(vec![
                Span::styled("Force-kill", Style::default().fg(RED)),
                Span::raw(" and discard that work?  "),
                Span::styled("y", Style::default().fg(RED)),
                Span::raw(" / "),
                Span::styled("n", Style::default().fg(GREEN)),
            ]));
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(RED))
                        .title(" force kill "),
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
        Modal::BranchPick {
            branches,
            filter,
            sel,
        } => {
            let area = centered(58, 16, f.area());
            f.render_widget(Clear, area);
            let matches = branch_matches(branches, filter);
            let mut lines = vec![Line::from(vec![
                Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{filter}▏")),
            ])];
            let h = area.height.saturating_sub(3) as usize;
            if matches.is_empty() {
                lines.push(Line::styled(
                    "  no matching branches",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                // window the visible slice so the highlighted row stays on screen
                let start = if *sel >= h { *sel + 1 - h } else { 0 };
                for (off, b) in matches.iter().skip(start).take(h).enumerate() {
                    let idx = start + off;
                    let style = if idx == *sel {
                        Style::default()
                            .bg(SEL_BG)
                            .fg(SEL_FG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(GREEN_SOFT)
                    };
                    lines.push(Line::styled(format!(" {b}"), style));
                }
            }
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(HL))
                        .title(" base a new agent on a branch (↑↓ Enter, Esc) "),
                ),
                area,
            );
        }
        Modal::QuickSend { task, text } => {
            let area = centered(64, 6, f.area());
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("{text}▏"))
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(GREEN))
                            .title(format!(" send to '{task}' (Enter sends, Esc cancels) ")),
                    ),
                area,
            );
        }
        Modal::Matrix(lines) => {
            let h = (lines.len() as u16 + 2).min(f.area().height);
            let w = 78u16.min(f.area().width);
            let area = centered(w, h, f.area());
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines.clone()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(HL))
                        .title(" mergeability — do branches conflict? (any key closes) "),
                ),
                area,
            );
        }
        Modal::Help => {
            let area = centered(52, 19, f.area());
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
                k("J / K", "reorder down / up"),
                k("m", "mergeability matrix (conflict preview)"),
                k("↵ / o", "attach into the agent (type here)"),
                k("i", "send one line to the agent (when ● ready)"),
                k("v", "run .wta/verify.sh checks (auto-runs when an agent finishes)"),
                k("e", "open the worktree in $EDITOR / WTA_OPEN_CMD (nvim, code…)"),
                k("Ctrl-q", "detach back to wta (while attached)"),
                k("tab", "switch Preview / Diff"),
                k("Shift+↑↓", "scroll Preview / Diff"),
                k("n", "new agent"),
                k("N", "new agent with an initial prompt"),
                k("s", "stop (keep worktree — resume later)"),
                k("glyphs", "⠋ running · ● ready · ▲ needs input · ✓ merged · ✗ exited"),
                k("D", "kill (destroy worktree + branch)"),
                k("p", "commit + push + open a PR"),
                k("b", "new agent based on an existing branch"),
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
            session: tmux::session_name("t", task),
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
    fn trust_prompt_match_is_strict() {
        // real dialog: all three strings present -> match
        let dialog = "╭─ Do you trust the files in this folder?\n\
             Claude Code may read, write, or execute files in this folder\n\
             1. Yes, proceed\n 2. No, exit";
        assert!(is_trust_prompt(dialog));
        // normal agent output mentioning one phrase must NOT trip it
        assert!(!is_trust_prompt("I'll proceed. Do you trust the files in this folder?"));
        assert!(!is_trust_prompt("Yes, proceed with the refactor"));
        assert!(!is_trust_prompt("running tests..."));
    }

    /// Turn a rendered ratatui buffer into a terminal-style SVG screenshot,
    /// preserving the real per-cell colors from the actual `ui()` render.
    fn buffer_to_svg(buf: &ratatui::buffer::Buffer) -> String {
        let cols = buf.area.width as usize;
        let rows = buf.area.height as usize;
        let cw = 8.6_f64;
        let ch = 17.5_f64;
        let fs = 14.0_f64;
        let pad = 18.0_f64;
        let tbar = 36.0_f64;
        let cx = pad;
        let cy = tbar + pad;
        let w = cx * 2.0 + cols as f64 * cw;
        let h = cy + rows as f64 * ch + pad;

        let hex = |c: Color| -> String {
            match c {
                Color::Green => "#3fb950".into(),
                Color::LightGreen => "#57e389".into(),
                Color::Red => "#ff7b72".into(),
                Color::Yellow => "#e3b341".into(),
                Color::Cyan => "#56d4dd".into(),
                Color::Blue => "#79c0ff".into(),
                Color::Magenta => "#d2a8ff".into(),
                Color::DarkGray => "#6e7681".into(),
                Color::Gray => "#b8c0b8".into(),
                Color::White => "#e6ede6".into(),
                Color::Black => "#0b0f0b".into(),
                Color::Reset => "#cdd6cd".into(),
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                _ => "#cdd6cd".into(),
            }
        };
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

        let mut o = String::new();
        o.push_str(&format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="ui-monospace,'SFMono-Regular',Menlo,'DejaVu Sans Mono',Consolas,monospace" font-size="{fs}">"#
        ));
        o.push_str(&format!(r##"<rect width="{w:.0}" height="{h:.0}" rx="12" fill="#0b0f0b"/>"##));
        o.push_str(&format!(r##"<path d="M0 12 A12 12 0 0 1 12 0 H{:.0} A12 12 0 0 1 {w:.0} 12 V{tbar:.0} H0 Z" fill="#141a14"/>"##, w - 12.0));
        o.push_str(r##"<circle cx="20" cy="18" r="6" fill="#ff5f57"/><circle cx="40" cy="18" r="6" fill="#febc2e"/><circle cx="60" cy="18" r="6" fill="#28c840"/>"##);
        o.push_str(&format!(
            r##"<text x="{:.0}" y="23" fill="#8b978b" text-anchor="middle">wta — parallel AI agents · git worktree + tmux</text>"##,
            w / 2.0
        ));

        // background cells (selection bar, title label)
        for r in 0..rows {
            for c in 0..cols {
                let bg = buf.content[r * cols + c].bg;
                if !matches!(bg, Color::Reset | Color::Black) {
                    o.push_str(&format!(
                        r#"<rect x="{:.2}" y="{:.2}" width="{cw:.2}" height="{ch:.2}" fill="{}"/>"#,
                        cx + c as f64 * cw,
                        cy + r as f64 * ch,
                        hex(bg)
                    ));
                }
            }
        }
        // text runs grouped by fg
        for r in 0..rows {
            let mut c = 0usize;
            while c < cols {
                let fg = buf.content[r * cols + c].fg;
                let start = c;
                let mut run = String::new();
                while c < cols && buf.content[r * cols + c].fg == fg {
                    run.push_str(buf.content[r * cols + c].symbol());
                    c += 1;
                }
                if run.trim().is_empty() {
                    continue;
                }
                let n = (c - start) as f64;
                o.push_str(&format!(
                    r#"<text x="{:.2}" y="{:.2}" fill="{}" textLength="{:.2}" lengthAdjust="spacingAndGlyphs" xml:space="preserve">{}</text>"#,
                    cx + start as f64 * cw,
                    cy + r as f64 * ch + fs * 0.82,
                    hex(fg),
                    n * cw,
                    esc(&run)
                ));
            }
        }
        o.push_str("</svg>");
        o
    }

    #[test]
    #[ignore = "regenerates assets/wta.svg on demand"]
    fn gen_readme_svg() {
        let mut app = App::new();
        app.rows = vec![
            row("auth-refactor", Status::Running, true, 212, 48),
            row("flaky-test", Status::NeedsInput, true, 12, 3),
            row("payments-api", Status::Ready, true, 64, 8),
            row("docs-site", Status::Exited, false, 5, 0),
        ];
        app.sel = 0;
        // Preview tab, showing the agent's real colors (parsed from tmux `-e`).
        app.tab = Tab::Preview;
        app.preview = [
            "\u{1b}[35m✻\u{1b}[0m \u{1b}[1mRefactor the auth token refresh\u{1b}[0m",
            "",
            "\u{1b}[32m●\u{1b}[0m I'll add retry + backoff to \u{1b}[36msrc/session.rs\u{1b}[0m, then a test.",
            "",
            "\u{1b}[32m●\u{1b}[0m \u{1b}[1mUpdate\u{1b}[0m(\u{1b}[36msrc/session.rs\u{1b}[0m)",
            "  \u{1b}[2m⎿\u{1b}[0m  \u{1b}[32m3 additions\u{1b}[0m · \u{1b}[31m1 removal\u{1b}[0m",
            "     \u{1b}[2m14\u{1b}[0m     pub fn refresh(&mut self) -> Result<()> {",
            "     \u{1b}[31m15 -     self.token = fetch_token()?;\u{1b}[0m",
            "     \u{1b}[32m15 +     self.token = retry(3, || fetch_token())?;\u{1b}[0m",
            "     \u{1b}[32m16 +     self.refreshed_at = Instant::now();\u{1b}[0m",
            "",
            "\u{1b}[32m●\u{1b}[0m \u{1b}[1mBash\u{1b}[0m(\u{1b}[33mcargo test session::\u{1b}[0m)",
            "  \u{1b}[2m⎿\u{1b}[0m  running 8 tests",
            "     \u{1b}[92mtest result: ok. 8 passed\u{1b}[0m; 0 failed",
            "",
            "\u{1b}[32m●\u{1b}[0m Done — refresh retries 3× with backoff.  \u{1b}[2m(esc to interrupt)\u{1b}[0m",
        ]
        .join("\n");
        let mut term = Terminal::new(TestBackend::new(120, 26)).unwrap();
        term.draw(|f| ui(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/wta.svg");
        std::fs::write(path, buffer_to_svg(&buf)).unwrap();
        eprintln!("wrote {path}");
    }

    #[test]
    fn matrix_overlay_renders() {
        let mut app = App::new();
        app.rows = vec![row("auth", Status::Running, true, 1, 0)];
        app.sel = 0;
        app.modal = Modal::Matrix(vec![
            Line::from(vec![Span::raw("        main    auth")]),
            Line::from(vec![
                Span::raw("auth    "),
                Span::styled("✓", Style::default().fg(GREEN)),
                Span::raw("       "),
                Span::styled("✗", Style::default().fg(RED)),
            ]),
        ]);
        let screen = render_to_string(&mut app, 100, 16);
        println!("\n{screen}\n");
        assert!(screen.contains("mergeability"));
        assert!(screen.contains('✓') && screen.contains('✗'));
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

    #[test]
    fn truncate_cols_clips_with_ellipsis() {
        assert_eq!(truncate_cols("short", 10), "short");
        assert_eq!(truncate_cols("hello-world", 6), "hello…");
        assert_eq!(truncate_cols("x", 0), "");
    }

    #[test]
    fn preview_parses_ansi_not_leaking_escapes() {
        let mut app = App::new();
        app.rows = vec![row("x", Status::Ready, true, 0, 0)];
        app.sel = 0;
        app.tab = Tab::Preview;
        // a tmux `-e` capture: green GREEN + red RED with SGR escapes
        app.preview = "\u{1b}[32mGREEN\u{1b}[0m \u{1b}[31mRED\u{1b}[0m".into();
        let screen = render_to_string(&mut app, 100, 16);
        assert!(screen.contains("GREEN") && screen.contains("RED"));
        // the SGR codes must be consumed into styles, not rendered as literal text
        assert!(!screen.contains("[32m") && !screen.contains("[0m"));
    }

    #[test]
    fn scroll_mode_shows_footer() {
        let mut app = App::new();
        app.rows = vec![row("x", Status::Running, true, 0, 0)];
        app.sel = 0;
        app.tab = Tab::Preview;
        app.scrollback = Some("line1\nline2\nline3".into());
        let screen = render_to_string(&mut app, 100, 16);
        assert!(screen.contains("scroll mode"));
    }

    #[test]
    fn untracked_files_count_in_stats_and_diff() {
        let dir = std::env::temp_dir().join(format!("wta-untracked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
        };
        git(&["init", "-qb", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        // agent creates a new (untracked) file — a plain `git diff` would miss it
        std::fs::write(dir.join("new.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let (a, _d) = numstat(&dir);
        assert!(a >= 2, "untracked lines counted in stats, got {a}");
        assert!(full_diff(&dir).contains("new.rs"), "untracked file shown in diff");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_check_runs_async_and_reports_exit_code() {
        let dir = std::env::temp_dir().join(format!("wta-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".wta")).unwrap();
        // a verify script that fails with a specific code + prints a line
        std::fs::write(dir.join(".wta/verify.sh"), "#!/usr/bin/env bash\necho 'boom: 2 tests failed'\nexit 3\n").unwrap();

        let mut app = App::new();
        app.root = dir.clone();
        app.repo = "t".into();
        app.rows = vec![row("x", Status::Ready, true, 0, 0)];
        app.sel = 0;
        spawn_check(&mut app, "x", &dir);
        assert!(matches!(app.checks.get("x"), Some(Check::Running { .. })), "check is running async");

        // poll until it finishes (non-blocking) — should not hang the caller
        let mut done = false;
        for _ in 0..300 {
            poll_checks(&mut app);
            if matches!(app.checks.get("x"), Some(Check::Done { .. })) {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done, "check completed");
        match app.checks.get("x") {
            Some(Check::Done { code }) => assert_eq!(*code, 3),
            _ => panic!("expected Done"),
        }
        // failure surfaced for the selected agent
        assert!(app.msg.as_ref().map(|(t, err, _)| *err && t.contains("failed")).unwrap_or(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merged_detection() {
        let dir = std::env::temp_dir().join(format!("wta-merged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
        };
        git(&["init", "-qb", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a.txt"), "1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        // fresh branch sitting at base tip → NOT merged (no work)
        git(&["branch", "agent/fresh"]);
        assert!(!is_merged(&dir, "agent/fresh"), "unworked branch is not 'merged'");
        // a branch with a commit, not yet in main → NOT merged
        git(&["checkout", "-qb", "agent/work"]);
        std::fs::write(dir.join("b.txt"), "2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "work"]);
        assert!(!is_merged(&dir, "agent/work"), "unmerged work is not 'merged'");
        // merge it into main (no-ff) → now merged
        git(&["checkout", "-q", "main"]);
        git(&["merge", "--no-ff", "-q", "-m", "merge", "agent/work"]);
        assert!(is_merged(&dir, "agent/work"), "landed branch reads as merged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gui_vs_terminal_editor_detection() {
        assert!(worktree::is_gui_editor("code --reuse-window"));
        assert!(worktree::is_gui_editor("cursor"));
        assert!(worktree::is_gui_editor("/usr/local/bin/zed"));
        assert!(!worktree::is_gui_editor("nvim"));
        assert!(!worktree::is_gui_editor("vim"));
        assert!(!worktree::is_gui_editor("hx"));
        assert!(!worktree::is_gui_editor("emacs -nw"));
    }

    #[test]
    fn force_kill_warns_about_unpushed() {
        let mut app = App::new();
        app.rows = vec![row("x", Status::Ready, true, 0, 0)];
        app.sel = 0;
        app.modal = Modal::ForceKill { task: "x".into(), unpushed: 3 };
        let screen = render_to_string(&mut app, 100, 16);
        assert!(screen.contains("3 unpushed"));
    }

    #[test]
    fn attention_shows_review_glyph_and_count() {
        let mut app = App::new();
        app.rows = vec![
            row("a", Status::Ready, true, 0, 0),
            row("b", Status::Ready, true, 0, 0),
        ];
        app.sel = 0;
        app.attention.insert("b".to_string());
        let screen = render_to_string(&mut app, 100, 16);
        assert!(screen.contains("need you"));
        assert!(screen.contains("◆"));
    }

    #[test]
    fn long_task_name_truncates_without_overflow() {
        let mut app = App::new();
        app.rows = vec![row(&"verylongtaskname".repeat(4), Status::Ready, true, 12, 3)];
        app.sel = 0;
        let screen = render_to_string(&mut app, 30, 16);
        assert!(screen.contains("…")); // clipped rather than overflowing the pane
    }
}
