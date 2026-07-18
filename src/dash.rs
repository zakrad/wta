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
        session: String,
        text: String,
    },
    /// Pick which repo a new agent goes in (global dash). Enter → NewTask in `root`.
    RepoPick {
        repos: Vec<(String, PathBuf)>, // (display name, root)
        filter: String,
        sel: usize,
        prompt: bool, // carry N (new-with-prompt) through the picker
    },
    Help,
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
    repo: String,       // repo id (namespaces the session + state)
    root: PathBuf,      // repo root (for git ops in the global dash)
    repo_name: String,  // display name (root dir), for the tree header
    base: String,       // branch this agent is based on / targets (for the label + diffs)
    status: Status,
    added: u32,
    removed: u32,
    session: String,
    alive: bool,
    path: Option<PathBuf>,
    cost: crate::cost::Usage,
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
    global: bool, // true = show every repo's agents as a tree; false = current repo only
    repo: String, // launch repo id (current-repo mode + the "current" repo default)
    root: PathBuf, // launch repo root
    op_root: PathBuf, // repo root for the pending modal action (kill/push/resume/new)
    checks: HashMap<String, Check>, // session -> verification result/run
    rows: Vec<Row>,
    sel: usize,
    tab: Tab,
    modal: Modal,
    preview: String,
    diff_text: String,
    hashes: HashMap<String, u64>,
    diffcache: HashMap<String, (u32, u32)>, // task -> (added, removed), cadence-refreshed
    costcache: HashMap<String, crate::cost::Usage>, // session -> token/$ usage, cadence-refreshed
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
}

impl App {
    fn new() -> Self {
        App {
            global: false,
            repo: worktree::repo_id().unwrap_or_default(),
            root: worktree::repo_root().unwrap_or_default(),
            op_root: PathBuf::new(),
            checks: HashMap::new(),
            rows: Vec::new(),
            sel: 0,
            tab: Tab::Preview,
            modal: Modal::None,
            preview: String::new(),
            diff_text: String::new(),
            hashes: HashMap::new(),
            diffcache: HashMap::new(),
            costcache: HashMap::new(),
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

pub fn run(here: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let mut term = Term::new(CrosstermBackend::new(stdout))?;
    let res = event_loop(&mut term, !here);
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

/// Run `f` with the process cwd set to `root`, restoring it after — lets the
/// cwd-based worktree ops (rm/push/resume/new) act on any repo from the global
/// dash. Safe because the dashboard is single-threaded.
fn in_repo<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    let prev = std::env::current_dir().ok();
    if root.as_os_str().is_empty() {
        return f();
    }
    let _ = std::env::set_current_dir(root);
    let r = f();
    if let Some(p) = prev {
        let _ = std::env::set_current_dir(p);
    }
    r
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

fn event_loop(term: &mut Term, global: bool) -> Result<()> {
    let mut app = App::new();
    app.global = global;
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
                        let root = app.op_root.clone();
                        if let Err(e) = in_repo(&root, || worktree::new(&task, &[])) {
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
                    let root = app.op_root.clone();
                    if let Err(e) = in_repo(&root, || worktree::new(&task, &args)) {
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
                    let root = app.op_root.clone();
                    match in_repo(&root, || worktree::rm(&task, false)) {
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
                    let root = app.op_root.clone();
                    if let Err(e) = in_repo(&root, || worktree::rm(&task, true)) {
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
                            let root = app.op_root.clone();
                            if let Err(e) = in_repo(&root, || worktree::resume_at(&task, &p)) {
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
                        let task = crate::tmux::sanitize_task(&base);
                        let root = app.op_root.clone();
                        if let Err(e) = in_repo(&root, || worktree::new_with_base(&task, &[], &base)) {
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
        Modal::RepoPick { repos, filter, sel, prompt } => {
            let matches: Vec<(String, PathBuf)> = repos
                .iter()
                .filter(|(name, _)| filter.is_empty() || name.to_lowercase().contains(&filter.to_lowercase()))
                .cloned()
                .collect();
            match key.code {
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Up => *sel = sel.saturating_sub(1),
                KeyCode::Down => {
                    if *sel + 1 < matches.len() {
                        *sel += 1;
                    }
                }
                KeyCode::Enter => {
                    let want_prompt = *prompt;
                    let picked = matches.get(*sel).map(|(_, r)| r.clone());
                    if let Some(root) = picked {
                        app.op_root = root;
                        app.modal = Modal::NewTask { name: String::new(), prompt: want_prompt };
                    } else {
                        app.modal = Modal::None;
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
                    let root = app.op_root.clone();
                    match in_repo(&root, || worktree::push(&task, true)) {
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
        Modal::QuickSend { task, session, text } => {
            match key.code {
                KeyCode::Enter => {
                    let task = std::mem::take(task);
                    let session = std::mem::take(session);
                    let text = std::mem::take(text);
                    app.modal = Modal::None;
                    if !text.trim().is_empty() {
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
        KeyCode::Enter | KeyCode::Char('o') => {
            if let Some(r) = app.selected() {
                let (alive, session, task, root, has_path) =
                    (r.alive, r.session.clone(), r.task.clone(), r.root.clone(), r.path.is_some());
                if alive {
                    app.attach = Some(session);
                } else if has_path {
                    app.op_root = root;
                    app.modal = Modal::Resume(task);
                } else {
                    app.set_err("no session and no worktree to resume");
                }
            }
        }
        KeyCode::Char('n') => open_new(app, false),
        KeyCode::Char('N') => open_new(app, true),
        KeyCode::Char('s') => {
            if let Some(r) = app.selected() {
                let (task, root) = (r.task.clone(), r.root.clone());
                if let Err(e) = in_repo(&root, || worktree::stop(&task)) {
                    app.set_err(e);
                }
                refresh(app);
                load_detail(app);
            }
        }
        KeyCode::Char('D') => {
            if let Some((task, root)) = app.selected().map(|r| (r.task.clone(), r.root.clone())) {
                app.op_root = root;
                app.modal = Modal::Confirm(task);
            }
        }
        KeyCode::Char('p') => {
            if let Some((task, root)) = app.selected().map(|r| (r.task.clone(), r.root.clone())) {
                app.op_root = root;
                app.modal = Modal::Push(task);
            }
        }
        KeyCode::Char('b') => {
            let root = app
                .selected()
                .map(|r| r.root.clone())
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| app.root.clone());
            if root.as_os_str().is_empty() {
                app.set_err("cd into a repo to base an agent on a branch");
            } else {
                app.op_root = root.clone();
                match in_repo(&root, worktree::list_branches) {
                    Ok(bs) if !bs.is_empty() => {
                        app.modal = Modal::BranchPick { branches: bs, filter: String::new(), sel: 0 }
                    }
                    Ok(_) => app.set_err("no branches to base an agent on"),
                    Err(e) => app.set_err(e),
                }
            }
        }
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
            if let Some(r) = app.selected() {
                if let Some(path) = r.path.clone() {
                    let (session, root) = (r.session.clone(), r.root.clone());
                    spawn_check(app, &session, &root, &path);
                }
            }
        }
        KeyCode::Char('J') => reorder(app, 1),
        KeyCode::Char('K') => reorder(app, -1),
        KeyCode::Char('m') => {
            if let Some(r) = app.selected() {
                let (root, repo) = (r.root.clone(), r.repo.clone());
                // task names in THIS repo whose checks are failing
                let failing: HashSet<String> = app
                    .rows
                    .iter()
                    .filter(|x| x.repo == repo)
                    .filter(|x| matches!(app.checks.get(&x.session), Some(Check::Done { code }) if *code != 0))
                    .map(|x| x.task.clone())
                    .collect();
                match in_repo(&root, || matrix_lines(&failing)) {
                    Ok(lines) => app.modal = Modal::Matrix(lines),
                    Err(e) => app.set_err(e),
                }
            }
        }
        // quick-send one line to the selected agent, gated so we never inject
        // into a busy/streaming pane (only when it's idle at its prompt = Ready).
        KeyCode::Char('i') => match app.selected() {
            Some(r) if !r.alive => app.set_err("no live session — resume with ↵"),
            Some(r) if r.status == Status::Running => app.set_err("agent is working — wait for ● ready"),
            Some(r) if r.status == Status::NeedsInput => app.set_err("agent needs input — attach (↵) to answer"),
            Some(r) if r.status == Status::Ready => {
                app.modal = Modal::QuickSend { task: r.task.clone(), session: r.session.clone(), text: String::new() }
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
    #[allow(clippy::needless_range_loop)] // i and j index the correlated grid + labels
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
    let j = j as usize;
    // only reorder within one repo group (rows are grouped by repo)
    let repo = app.rows[i].repo.clone();
    if app.rows[j].repo != repo {
        return;
    }
    let (ti, tj) = (app.rows[i].task.clone(), app.rows[j].task.clone());
    let mut tasks: Vec<String> = app.rows.iter().filter(|r| r.repo == repo).map(|r| r.task.clone()).collect();
    if let (Some(pi), Some(pj)) = (tasks.iter().position(|t| *t == ti), tasks.iter().position(|t| *t == tj)) {
        tasks.swap(pi, pj);
    }
    let map: std::collections::HashMap<String, u32> =
        tasks.iter().enumerate().map(|(idx, t)| (t.clone(), idx as u32)).collect();
    if let Err(e) = status::write_order(&repo, &map) {
        app.set_err(e);
        return;
    }
    refresh(app);
    load_detail(app);
}

/// Open the "new agent" flow: a repo picker in the global dash (unless only one
/// repo is known), else straight to the name modal for the current/selected repo.
fn open_new(app: &mut App, prompt: bool) {
    // All repos that currently have agents…
    let mut list: Vec<(String, PathBuf)> = worktree::discover_repos()
        .iter()
        .map(|(_, root)| (worktree::repo_name(root), root.clone()))
        .collect();
    // …plus the repo the dashboard was launched from, even if it has no agents yet —
    // that's how you add the *first* agent to a new repo from the dashboard. Put it
    // first so it's the default selection.
    if !app.root.as_os_str().is_empty() && !list.iter().any(|(_, r)| r == &app.root) {
        list.insert(0, (worktree::repo_name(&app.root), app.root.clone()));
    }

    if app.global && list.len() > 1 {
        // Default the highlight to the launch repo if it's in the list.
        let sel = list.iter().position(|(_, r)| r == &app.root).unwrap_or(0);
        app.modal = Modal::RepoPick { repos: list, filter: String::new(), sel, prompt };
    } else {
        let root = list
            .first()
            .map(|(_, r)| r.clone())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_default();
        if root.as_os_str().is_empty() {
            app.set_err("cd into a repo to create an agent");
            return;
        }
        app.op_root = root;
        app.modal = Modal::NewTask { name: String::new(), prompt };
    }
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
fn merge_base(path: &Path, base: &str) -> Option<String> {
    git_in(path, &["merge-base", "HEAD", base]).filter(|s| !s.is_empty())
}
/// True if the agent's branch has landed in the base branch (all its commits are
/// ancestors of base) — and it isn't just a fresh branch sitting at base's tip.
fn is_merged(path: &Path, base: &str, branch: &str) -> bool {
    if base == branch || branch.is_empty() {
        return false;
    }
    if git_in(path, &["merge-base", "--is-ancestor", branch, base]).is_none() {
        return false; // has commits not in base → not merged
    }
    let bt = git_in(path, &["rev-parse", branch]);
    let base_t = git_in(path, &["rev-parse", base]);
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
fn numstat(path: &Path, base: &str) -> (u32, u32) {
    let mb = match merge_base(path, base) {
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
fn full_diff(path: &Path, base: &str) -> String {
    let mut out = match merge_base(path, base) {
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
fn spawn_check(app: &mut App, session: &str, root: &Path, wt: &Path) {
    let suite = match worktree::verify_suite_script(root) {
        Some(s) => s,
        None => {
            app.set_err("nothing to verify — add .wta/verify.sh or lock a check with `wta lock`");
            return;
        }
    };
    // keep the log in wta's own per-user dir, not world-writable /tmp
    let log = match status::wta_dir() {
        Ok(d) => {
            let l = d.join("logs");
            let _ = std::fs::create_dir_all(&l);
            l.join(format!("verify-{session}.log"))
        }
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
        .arg("-c")
        .arg(&suite)
        .current_dir(wt)
        .stdout(std::process::Stdio::from(f))
        .stderr(std::process::Stdio::from(f2))
        .spawn()
    {
        Ok(child) => {
            app.checks.insert(session.to_string(), Check::Running { child, log, since: Instant::now() });
            app.set_info("running checks…");
        }
        Err(e) => app.set_err(e),
    }
}

/// Poll running checks without blocking; transition to Done when they exit or
/// time out (>5m). Keyed by session (globally unique).
fn poll_checks(app: &mut App) {
    let mut done: Vec<(String, i32, String)> = Vec::new();
    for (session, chk) in app.checks.iter_mut() {
        if let Check::Running { child, log, since } = chk {
            let timed_out = since.elapsed() > Duration::from_secs(300);
            match child.try_wait() {
                Ok(Some(status)) => done.push((session.clone(), status.code().unwrap_or(-1), read_tail(log))),
                Ok(None) if timed_out => {
                    let _ = child.kill();
                    let _ = child.wait(); // reap so it doesn't linger as a zombie
                    done.push((session.clone(), -1, "verify timed out (>5m)".into()));
                }
                Ok(None) => {}
                Err(_) => done.push((session.clone(), -1, "verify failed to run".into())),
            }
        }
    }
    for (session, code, tail) in done {
        let is_sel = app.rows.get(app.sel).map(|r| r.session == session).unwrap_or(false);
        if is_sel {
            if code == 0 {
                app.set_info("✓ checks passed");
            } else {
                let last = tail.lines().last().unwrap_or("").trim();
                app.set_err(format!("✗ checks failed (exit {code}) — {last}"));
            }
        }
        app.checks.insert(session, Check::Done { code });
    }
}

/// Build the rows for ONE repo (merging managed worktrees + state agents), keyed
/// by the globally-unique session so the global dash's caches never collide across
/// repos that reuse a task name.
fn repo_rows(app: &mut App, repo: &str, root: &Path, out: &mut Vec<Row>) {
    let repo_name = worktree::repo_name(root);
    let states: HashMap<String, status::AgentState> = status::read_states(repo)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.task.clone(), s))
        .collect();

    let mut order: Vec<String> = Vec::new();
    let mut paths: HashMap<String, PathBuf> = HashMap::new();
    let mut branches: HashMap<String, String> = HashMap::new();
    if let Ok(managed) = worktree::list_managed_in(root) {
        for w in managed {
            order.push(w.task.clone());
            branches.insert(w.task.clone(), w.branch);
            paths.insert(w.task, w.path);
        }
    }
    let subdir = std::env::var("WTA_WORKTREE_DIR").unwrap_or_else(|_| ".agents".into());
    let agents_dir = root.join(subdir);
    for (task, st) in &states {
        if paths.contains_key(task) || st.cwd.is_empty() {
            continue;
        }
        let p = PathBuf::from(&st.cwd);
        if p.starts_with(&agents_dir) && p.exists() {
            order.push(task.clone());
            paths.insert(task.clone(), p);
        }
    }
    let mut seen = HashSet::new();
    order.retain(|t| seen.insert(t.clone()));
    let rank = status::read_order(repo);
    order.sort_by(|a, b| {
        let pa = rank.get(a).copied().unwrap_or(u32::MAX);
        let pb = rank.get(b).copied().unwrap_or(u32::MAX);
        pa.cmp(&pb).then_with(|| a.cmp(b))
    });

    let full_sweep = app.tick % 5 == 0;
    let sel_session = app.selected().map(|r| r.session.clone());
    let auto_trust = std::env::var("WTA_AUTO_TRUST").map(|v| v != "0").unwrap_or(true);

    for task in order {
        let path = paths.get(&task).cloned();
        let branch = branches
            .get(&task)
            .cloned()
            .or_else(|| path.as_deref().and_then(|p| git_in(p, &["rev-parse", "--abbrev-ref", "HEAD"])))
            .unwrap_or_default();
        let session = tmux::session_name(repo, &task);
        let is_sel = sel_session.as_deref() == Some(session.as_str());

        // The branch this agent is based on / targets: the persisted base (from `--base`
        // or the branch it was forked off), else a best-effort main/master lookup. Used
        // for the sidebar label AND every diff below, so they stay consistent.
        let base = crate::status::base_of(repo, &task)
            .or_else(|| path.as_deref().map(crate::worktree::base_branch))
            .unwrap_or_else(|| "HEAD".to_string());

        // Token/$ usage, cadence-refreshed like the diffstat (parsing transcripts is
        // heavy, so only on the periodic full sweep or for the selected agent).
        let cost = match path.as_deref() {
            Some(p) => {
                if full_sweep || is_sel || !app.costcache.contains_key(&session) {
                    let u = crate::cost::for_worktree(p);
                    app.costcache.insert(session.clone(), u);
                    u
                } else {
                    app.costcache.get(&session).copied().unwrap_or_default()
                }
            }
            None => crate::cost::Usage::default(),
        };

        let (added, removed) = match path.as_deref() {
            Some(p) => {
                if full_sweep || is_sel || !app.diffcache.contains_key(&session) {
                    let v = numstat(p, &base);
                    app.diffcache.insert(session.clone(), v);
                    v
                } else {
                    app.diffcache.get(&session).copied().unwrap_or((0, 0))
                }
            }
            None => (0, 0),
        };
        let merged = match path.as_deref() {
            Some(p) => {
                if full_sweep || is_sel || !app.mergedcache.contains_key(&session) {
                    let v = is_merged(p, &base, &branch);
                    app.mergedcache.insert(session.clone(), v);
                    v
                } else {
                    app.mergedcache.get(&session).copied().unwrap_or(false)
                }
            }
            None => false,
        };

        let alive = tmux::has_session(&session);
        let status = if alive {
            let text = tmux::capture(&session).unwrap_or_default();
            if auto_trust && !app.trust_done.contains(&session) {
                if is_trust_prompt(&text) {
                    let _ = tmux::send_enter(&session);
                    app.trust_done.insert(session.clone());
                } else {
                    let seen_at = *app.trust_seen.entry(session.clone()).or_insert_with(Instant::now);
                    if seen_at.elapsed() > Duration::from_secs(120) {
                        app.trust_done.insert(session.clone());
                    }
                }
            }
            let h = hash_str(&text);
            // First sight (no prior hash) counts as *unchanged* → Ready, not Running.
            // Otherwise every already-idle agent would read Running on the first
            // refresh and Ready on the next — a phantom "finished" edge that chimed
            // for the whole fleet every time you opened the dashboard.
            let changed = app.hashes.insert(session.clone(), h).map(|old| old != h).unwrap_or(false);
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
        let status = if merged && matches!(status, Status::Ready | Status::Exited | Status::Idle) {
            Status::Merged
        } else {
            status
        };

        out.push(Row {
            task,
            repo: repo.to_string(),
            root: root.to_path_buf(),
            repo_name: repo_name.clone(),
            base,
            status,
            added,
            removed,
            session,
            alive,
            path,
            cost,
        });
    }
}

fn refresh(app: &mut App) {
    let prev_sel = app.selected().map(|r| r.session.clone());
    app.tick = app.tick.wrapping_add(1);

    // targets: every repo with agents (global), or just the launch repo (--here)
    let targets: Vec<(String, PathBuf)> = if app.global {
        worktree::discover_repos()
    } else if !app.repo.is_empty() {
        vec![(app.repo.clone(), app.root.clone())]
    } else {
        Vec::new()
    };

    let mut rows = Vec::new();
    for (repo, root) in &targets {
        repo_rows(app, repo, root, &mut rows);
    }
    app.rows = rows;
    app.sel = prev_sel
        .and_then(|s| app.rows.iter().position(|r| r.session == s))
        .unwrap_or(0)
        .min(app.rows.len().saturating_sub(1));

    // caches are keyed by session (globally unique)
    let live: HashSet<String> = app.rows.iter().map(|r| r.session.clone()).collect();
    app.hashes.retain(|k, _| live.contains(k));
    app.diffcache.retain(|k, _| live.contains(k));
    app.costcache.retain(|k, _| live.contains(k));
    app.mergedcache.retain(|k, _| live.contains(k));
    app.trust_seen.retain(|k, _| live.contains(k));
    app.trust_done.retain(|k| live.contains(k));

    let sel_now = app.rows.get(app.sel).map(|r| r.session.clone());
    let mut to_check: Vec<(String, PathBuf, PathBuf)> = Vec::new(); // (session, root, worktree)
    let mut invalidate: Vec<String> = Vec::new();
    for r in &app.rows {
        let now = r.status;
        let prev = app.prev_status.get(&r.session).copied();

        let became_needs = prev.is_some() && now == Status::NeedsInput && prev != Some(Status::NeedsInput);
        let finished = prev == Some(Status::Running) && matches!(now, Status::Ready | Status::Exited);
        // All alerts (sound + desktop banner + terminal-native tmux popup) are fired
        // by the Claude Stop/Notification hooks (see status::emit) so they reach you
        // even while attached or with the dashboard closed. The dashboard only sets
        // the ◆ review marker, for agents you're NOT currently looking at.
        if (became_needs || finished) && sel_now.as_deref() != Some(r.session.as_str()) {
            app.attention.insert(r.session.clone());
        }
        let has_verify = worktree::has_verify_suite(&r.root);
        if finished && has_verify && !matches!(app.checks.get(&r.session), Some(Check::Running { .. })) {
            if let Some(p) = &r.path {
                to_check.push((r.session.clone(), r.root.clone(), p.clone()));
            }
        }
        if now == Status::Running && matches!(app.checks.get(&r.session), Some(Check::Done { .. })) {
            invalidate.push(r.session.clone());
        }
    }
    app.prev_status = app.rows.iter().map(|r| (r.session.clone(), r.status)).collect();
    app.attention.retain(|s| live.contains(s));
    app.checks.retain(|s, c| {
        if live.contains(s) {
            return true;
        }
        if let Check::Running { child, .. } = c {
            let _ = child.kill();
            let _ = child.wait();
        }
        false
    });
    for s in invalidate {
        app.checks.remove(&s);
    }
    for (session, root, wt) in to_check {
        spawn_check(app, &session, &root, &wt);
    }
}

/// Claude Code's per-folder trust dialog. Matches BOTH wording generations
/// (Claude ≤2.0 and the 2.1.x "Yes, I trust this folder" dialog), on the
/// whitespace-normalized capture so pane wrapping can't defeat it. Strict
/// co-occurrence keeps normal agent output from tripping it.
fn is_trust_prompt(text: &str) -> bool {
    let t = tmux::norm(text);
    // Never auto-accept the "pre-approves" variant — Enter there would also grant
    // the repo's checked-in allow rules on an untrusted (e.g. --base) branch.
    if t.contains("pre-approves") {
        return false;
    }
    let legacy = t.contains("Do you trust the files in this folder?")
        && t.contains("Yes, proceed")
        && t.contains("No, exit");
    let current = t.contains("you created or one you trust")
        && t.contains("Yes, I trust this folder")
        && t.contains("No, exit");
    legacy || current
}

fn load_detail(app: &mut App) {
    // viewing an agent clears its "needs review" flag
    if let Some(s) = app.selected().map(|r| r.session.clone()) {
        app.attention.remove(&s);
    }
    let (alive, session, path, base) = match app.selected() {
        Some(r) => (r.alive, r.session.clone(), r.path.clone(), r.base.clone()),
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
                    let d = full_diff(p, &base);
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
        .title(if app.global { " Agents · all repos " } else { " Instances " })
        .title_style(
            Style::default()
                .bg(GREEN)
                .fg(SEL_FG)
                .add_modifier(Modifier::BOLD),
        );
    let inner_w = area.width.saturating_sub(2) as usize;
    let mut items: Vec<ListItem> = Vec::new();
    let mut sel_visual: Option<usize> = None; // visual item index of the selected agent
    let mut last_repo: Option<String> = None;
    for (i, r) in app.rows.iter().enumerate() {
        // repo header before each group (global tree only)
        if app.global && last_repo.as_deref() != Some(r.repo.as_str()) {
            let n = app.rows.iter().filter(|x| x.repo == r.repo).count();
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("▸ {} ", r.repo_name), Style::default().fg(GREEN_BRIGHT).add_modifier(Modifier::BOLD)),
                Span::styled(format!("({n})"), Style::default().fg(Color::DarkGray)),
            ])));
            last_repo = Some(r.repo.clone());
        }
        // finished-but-unviewed agents get a bright review glyph; needs-input keeps ▲
        let glyph = if app.attention.contains(&r.session) && r.status != Status::NeedsInput {
            Span::styled("◆ ".to_string(), Style::default().fg(Color::LightYellow))
        } else {
            status_glyph(app, r.status)
        };
        // optional verification-check indicator, shown just left of the status glyph
        let check: Option<Span> = app.checks.get(&r.session).map(|c| match c {
            Check::Running { .. } => Span::styled("⟳ ".to_string(), Style::default().fg(Color::DarkGray)),
            Check::Done { code: 0, .. } => Span::styled("✓ ".to_string(), Style::default().fg(GREEN_BRIGHT)),
            Check::Done { .. } => Span::styled("✗ ".to_string(), Style::default().fg(RED)),
        });
        let extra = if check.is_some() { 2 } else { 0 };
        // indent agents under the repo header in the global tree
        let head_txt = if app.global {
            format!("   {}", r.task)
        } else {
            format!(" {}. {}", i + 1, r.task)
        };
        let head = truncate_cols(&head_txt, inner_w.saturating_sub(2 + extra));
        let pad1 = inner_w.saturating_sub(head.width() + 2 + extra);
        let mut spans1 = vec![Span::raw(head), Span::raw(" ".repeat(pad1))];
        if let Some(c) = check {
            spans1.push(c);
        }
        spans1.push(glyph);
        let line1 = Line::from(spans1);

        // compact TOKEN usage, right of the diffstat: "1.3M" (blank when none yet)
        let tok_str = if r.cost.tokens() > 0 {
            format!("{}  ", crate::cost::human_tokens(r.cost.tokens()))
        } else {
            String::new()
        };
        let counts_len = format!("{tok_str}+{},-{} ", r.added, r.removed).width();
        // Show the BASE branch each agent targets (its working branch is always
        // `agent/<task>` = the line-1 name, so it carries no extra info).
        let indent = if app.global { "     Ꮧ " } else { "   Ꮧ " };
        let bhead = truncate_cols(&format!("{indent}{}", r.base), inner_w.saturating_sub(counts_len));
        let pad2 = inner_w.saturating_sub(bhead.width() + counts_len);
        let line2 = Line::from(vec![
            Span::styled(bhead, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(pad2)),
            Span::styled(tok_str, Style::default().fg(Color::Yellow)),
            Span::styled(format!("+{}", r.added), Style::default().fg(GREEN)),
            Span::styled(",", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("-{} ", r.removed), Style::default().fg(RED)),
        ]);
        if i == app.sel {
            sel_visual = Some(items.len());
        }
        items.push(ListItem::new(vec![line1, line2, Line::from("")]));
    }
    if items.is_empty() {
        let msg = if app.global {
            "  no agents anywhere — press 'n'"
        } else {
            "  no agents — press 'n'"
        };
        items.push(ListItem::new(Line::styled(msg, Style::default().fg(Color::DarkGray))));
    }
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(SEL_BG)
            .fg(SEL_FG)
            .add_modifier(Modifier::BOLD),
    );
    let mut st = ListState::default();
    if !app.rows.is_empty() {
        st.select(sel_visual.or(Some(0)));
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
                Line::raw(format!("Commit, push & open a PR for '{task}'?")),
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
        Modal::QuickSend { task, text, .. } => {
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
        Modal::RepoPick { repos, filter, sel, .. } => {
            let area = centered(58, 16, f.area());
            f.render_widget(Clear, area);
            let matches: Vec<&(String, PathBuf)> = repos
                .iter()
                .filter(|(name, _)| filter.is_empty() || name.to_lowercase().contains(&filter.to_lowercase()))
                .collect();
            let mut lines = vec![Line::from(vec![
                Span::styled("repo: ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{filter}▏")),
            ])];
            let h = area.height.saturating_sub(3) as usize;
            if matches.is_empty() {
                lines.push(Line::styled("  no matching repos", Style::default().fg(Color::DarkGray)));
            } else {
                let start = if *sel >= h { *sel + 1 - h } else { 0 };
                for (off, (name, _)) in matches.iter().skip(start).take(h).enumerate() {
                    let idx = start + off;
                    let style = if idx == *sel {
                        Style::default().bg(SEL_BG).fg(SEL_FG).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(GREEN_SOFT)
                    };
                    lines.push(Line::styled(format!(" {name}"), style));
                }
            }
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(HL))
                        .title(" new agent — pick a repo (↑↓ Enter, Esc) "),
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
            repo: "t".into(),
            root: PathBuf::from("/tmp"),
            repo_name: "tmp".into(),
            base: "main".into(),
            status: s,
            added: a,
            removed: d,
            session: tmux::session_name("t", task),
            alive,
            path: Some(PathBuf::from("/tmp/x")),
            cost: crate::cost::Usage::default(),
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
        assert!(screen.contains("Ꮧ main")); // the base branch, not the redundant agent/<task>
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
        // current (Claude Code 2.1.x) dialog wording — wrapped across lines
        let current = "Quick safety check: Is this a project you created\n or one you trust?\n\
             1. Yes, I trust this folder\n 2. No, exit Claude Code";
        assert!(is_trust_prompt(current));
        // the "pre-approves" variant must NOT be auto-accepted (grants allow rules)
        assert!(!is_trust_prompt(
            "Is this a directory you created or one you trust? This folder pre-approves \
             tools. 1. Yes, I trust this folder 2. No, exit"
        ));
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
        let (a, _d) = numstat(&dir, "main");
        assert!(a >= 2, "untracked lines counted in stats, got {a}");
        assert!(full_diff(&dir, "main").contains("new.rs"), "untracked file shown in diff");
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
        let session = app.rows[0].session.clone(); // checks are keyed by session
        spawn_check(&mut app, &session, &dir, &dir); // (session, root, worktree)
        assert!(matches!(app.checks.get(&session), Some(Check::Running { .. })), "check is running async");

        // poll until it finishes (non-blocking) — should not hang the caller
        let mut done = false;
        for _ in 0..300 {
            poll_checks(&mut app);
            if matches!(app.checks.get(&session), Some(Check::Done { .. })) {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done, "check completed");
        match app.checks.get(&session) {
            Some(Check::Done { code }) => assert_eq!(*code, 3),
            _ => panic!("expected Done"),
        }
        // failure surfaced for the selected agent
        assert!(app.msg.as_ref().map(|(t, err, _)| *err && t.contains("failed")).unwrap_or(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_dash_groups_agents_by_repo() {
        let mut app = App::new();
        app.global = true;
        let mut a = row("auth", Status::Ready, true, 1, 0);
        a.repo = "r1".into();
        a.repo_name = "alpha".into();
        a.root = PathBuf::from("/tmp/alpha");
        let mut b = row("payments", Status::Running, true, 2, 1);
        b.repo = "r2".into();
        b.repo_name = "beta".into();
        b.root = PathBuf::from("/tmp/beta");
        app.rows = vec![a, b];
        app.sel = 1;
        let screen = render_to_string(&mut app, 100, 22);
        // both repo headers + both agents appear in the one tree
        assert!(screen.contains("alpha"), "repo alpha header");
        assert!(screen.contains("beta"), "repo beta header");
        assert!(screen.contains("auth") && screen.contains("payments"), "agents shown");
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
        assert!(!is_merged(&dir, "main", "agent/fresh"), "unworked branch is not 'merged'");
        // a branch with a commit, not yet in main → NOT merged
        git(&["checkout", "-qb", "agent/work"]);
        std::fs::write(dir.join("b.txt"), "2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "work"]);
        assert!(!is_merged(&dir, "main", "agent/work"), "unmerged work is not 'merged'");
        // merge it into main (no-ff) → now merged
        git(&["checkout", "-q", "main"]);
        git(&["merge", "--no-ff", "-q", "-m", "merge", "agent/work"]);
        assert!(is_merged(&dir, "main", "agent/work"), "landed branch reads as merged");
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
