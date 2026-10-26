use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
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
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs};
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

/// Dashboard layout: `Split` = the master-detail list + live Preview/Diff (watch one
/// agent); `Table` = a full-width, column-aligned fleet grid (k9s-style — scan many).
#[derive(Clone, Copy, PartialEq)]
enum View {
    Split,
    Table,
    /// Kanban-style columns by state (`B`): Working · Needs you · Ready · Merged · Stopped.
    Board,
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
    Resume { task: String, repo: String, path: PathBuf },
    /// Resume died right after launch (e.g. `claude --continue` with nothing to
    /// continue). Shows WHY and offers: `f` fresh conversation in the same worktree,
    /// `r` recreate — rm the worktree + `wta new` under the same name and base.
    ResumeFailed {
        task: String,
        repo: String,
        path: PathBuf,
        root: PathBuf,
        base: String,
        reason: String,
    },
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
    /// Global settings editor (`,`) over ~/.wta/config.json. Text fields (editor/
    /// agent/model) edit inline; enum fields (effort/hint_bar/open_mode) cycle.
    Settings {
        cfg: crate::config::Config,
        sel: usize,
        /// Some(buffer) while typing into the selected text field.
        editing: Option<String>,
        msg: Option<String>,
    },
    /// Pick which repo a new agent goes in (global dash). Enter → NewTask in `root`.
    RepoPick {
        repos: Vec<(String, PathBuf)>, // (display name, root)
        filter: String,
        sel: usize,
        prompt: bool, // carry N (new-with-prompt) through the picker
    },
    /// Adopt: pick which repo's sessions to browse (global dash only).
    AdoptRepoPick {
        repos: Vec<(String, PathBuf)>,
        filter: String,
        sel: usize,
    },
    /// Adopt: pick a Claude session under the chosen repo. Enter → AdoptName.
    AdoptPick {
        root: PathBuf,
        cands: Vec<worktree::AdoptCandidate>,
        sel: usize,
    },
    /// Adopt: name the adopted agent, then register it.
    AdoptName {
        dir: PathBuf,
        name: String,
    },
    /// `/` — live filter over the agent list (task, repo, base, #tag, state word).
    Filter {
        text: String,
    },
    /// `T` — edit the selected agent's tags (space/comma separated).
    Tags {
        repo: String,
        task: String,
        text: String,
    },
    /// `C` in the Diff tab — a review note on one diff line, queued for the agent.
    Comment {
        session: String,
        task: String,
        loc: String,
        code: String,
        text: String,
    },
    /// `=` — compare siblings (a fanout group, or a parent and its children) and keep one.
    Compare {
        repo: String,
        root: PathBuf,
        group: String,
        sel: usize,
        confirm: bool,
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

#[derive(Clone)]
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
    model: Option<String>,  // most recent model (short label) — for the table's MODEL column
    ac: Option<(u32, u32)>, // .wta/task.md acceptance criteria (checked, total)
    updated_unix: u64,      // last status write — for the table's AGE column
    pinned: bool,           // `P` — sorts to the top of its repo group
    tags: Vec<String>,      // `T` / `wta tag` — shown as #tag, matched by `/`
    parent: Option<String>, // spawner (fanout name / review builder / handoff source)
    sleeping: bool,         // stopped on purpose — dimmed `z` until resumed
    account: Option<String>, // Claude account (`wta account`), if not the default
    depth: u8,              // nesting under a parent (set by `apply_view`)
    header: Option<String>, // a fanout group label drawn above this row (first member)
    hidden: u32,            // children folded away under this row (`z`)
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
    view: View,   // Split (list + preview) or Table (full-width fleet grid)
    pending_g: bool, // saw a `g` — next `g` jumps to the top (vim gg)
    global: bool, // true = show every repo's agents as a tree; false = current repo only
    repo: String, // launch repo id (current-repo mode + the "current" repo default)
    root: PathBuf, // launch repo root
    op_root: PathBuf, // repo root for the pending modal action (kill/push/resume/new)
    op_repo: String,  // repo id for the pending modal action — the EXACT id the row tracks
    checks: HashMap<String, Check>, // session -> verification result/run
    all_rows: Vec<Row>, // every agent, nested + ordered — `rows` is the filtered/folded view
    rows: Vec<Row>,
    sel: usize,
    filter: String,           // `/` filter terms (empty = show all)
    folded: HashSet<String>,  // "<repo>/<parent>" groups folded with `z`
    comments: HashMap<String, Vec<String>>, // session -> diff review notes waiting for the agent to be ready
    diff_cursor: usize,       // Diff tab: the line a `C` comment attaches to (index into diff_text lines)
    tab: Tab,
    modal: Modal,
    preview: String,
    diff_text: String,
    hashes: HashMap<String, u64>,
    diffcache: HashMap<String, (u32, u32)>, // task -> (added, removed), cadence-refreshed
    costcache: HashMap<String, (crate::cost::Usage, Option<String>)>, // session -> (usage, model), cadence-refreshed
    mergedcache: HashMap<String, bool>,     // task -> branch merged into base, cadence-refreshed
    tick: u64,                              // refresh counter driving the diffstat cadence
    trust_seen: HashMap<String, Instant>,   // session -> first time we saw it (trust grace)
    trust_done: HashSet<String>,            // sessions whose trust prompt is handled/disarmed
    spin: usize,
    msg: Option<(String, bool, Instant)>, // (text, is_error, when)
    attach: Option<String>,               // session to attach to after this frame
    copy: Option<(String, String)>,       // (repo, task) to open copy mode on after this frame
    open: Option<(String, PathBuf)>,      // (editor cmd, worktree) to open inline after this frame
    scroll: u16,                          // preview/diff scroll offset
    scrollback: Option<String>,           // Some => Preview scroll mode: full (colored) history snapshot
    prev_status: HashMap<String, Status>, // last-seen status per task, for transition detection
    attention: HashSet<String>,           // agents that finished / need input and haven't been viewed
    chart: Vec<Line<'static>>,            // selected agent's tokens-over-time chart (table overlay)
    chart_session: String,                // which agent `chart` was computed for (lazy recompute)
    // Transcript parsing (cost) runs on a worker thread so a huge session never
    // freezes the UI — jobs go out on cost_tx, results stream back on cost_rx.
    cost_tx: Sender<(String, PathBuf)>,
    cost_rx: Receiver<(String, (crate::cost::Usage, Option<String>))>,
    cost_inflight: HashSet<String>, // sessions currently being parsed (dedupe jobs)
    costmtime: HashMap<String, u64>, // session -> newest transcript mtime last parsed
}

impl App {
    fn new() -> Self {
        // Cost worker: parses transcripts off the UI thread. Dropping the App drops
        // cost_tx, which ends the worker's recv loop — no thread is leaked.
        let (cost_tx, job_rx) = channel::<(String, PathBuf)>();
        let (res_tx, cost_rx) = channel::<(String, (crate::cost::Usage, Option<String>))>();
        std::thread::spawn(move || {
            while let Ok((session, path)) = job_rx.recv() {
                let cm = crate::cost::for_worktree(&path);
                if res_tx.send((session, cm)).is_err() {
                    break; // dashboard gone
                }
            }
        });
        App {
            cost_tx,
            cost_rx,
            cost_inflight: HashSet::new(),
            costmtime: HashMap::new(),
            view: View::Split,
            pending_g: false,
            global: false,
            repo: worktree::repo_id().unwrap_or_default(),
            root: worktree::repo_root().unwrap_or_default(),
            op_root: PathBuf::new(),
            op_repo: String::new(),
            checks: HashMap::new(),
            all_rows: Vec::new(),
            rows: Vec::new(),
            sel: 0,
            filter: String::new(),
            folded: HashSet::new(),
            comments: HashMap::new(),
            diff_cursor: 0,
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
            copy: None,
            open: None,
            scroll: 0,
            scrollback: None,
            prev_status: HashMap::new(),
            attention: HashSet::new(),
            chart: Vec::new(),
            chart_session: String::new(),
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

/// Suspend the TUI, run wta's copy mode over the agent's conversation, resume on `q`.
fn copy_inline(term: &mut Term, repo: &str, task: &str, app: &mut App) {
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen, crossterm::cursor::Show).ok();
    let res = crate::copyview::run(crate::copyview::source_for(repo, task));
    enable_raw_mode().ok();
    execute!(term.backend_mut(), EnterAlternateScreen, crossterm::cursor::Hide).ok();
    let _ = term.clear();
    if let Err(e) = res {
        app.set_err(e);
    }
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
        drain_costs(&mut app);
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

        if let Some((repo, task)) = app.copy.take() {
            copy_inline(term, &repo, &task, &mut app);
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
                    let (repo, root) = (app.op_repo.clone(), app.op_root.clone());
                    match worktree::rm_in(&repo, &root, &task, false) {
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
                    let (repo, root) = (app.op_repo.clone(), app.op_root.clone());
                    if let Err(e) = worktree::rm_in(&repo, &root, &task, true) {
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
        Modal::Resume { task, repo, path } => {
            match key.code {
                KeyCode::Char('y') => {
                    let (task, repo, path) = (task.clone(), repo.clone(), path.clone());
                    app.modal = Modal::None;
                    if let Err(e) = worktree::resume_session(&repo, &task, &path, false) {
                        match e.downcast_ref::<worktree::ResumeDied>() {
                            // died on launch → explain + offer fresh / recreate
                            Some(d) => {
                                let (root, base) = app
                                    .rows
                                    .iter()
                                    .find(|r| r.task == task && r.repo == repo)
                                    .map(|r| (r.root.clone(), r.base.clone()))
                                    .unwrap_or_default();
                                app.modal = Modal::ResumeFailed {
                                    task,
                                    repo,
                                    path,
                                    root,
                                    base,
                                    reason: d.output.clone(),
                                };
                            }
                            None => app.set_err(e),
                        }
                    }
                    refresh(app);
                    load_detail(app);
                }
                KeyCode::Char('n') | KeyCode::Esc => app.modal = Modal::None,
                _ => {}
            }
            return Ok(false);
        }
        Modal::ResumeFailed { task, repo, path, root, base, .. } => {
            match key.code {
                // fresh conversation, same worktree (branch + uncommitted work kept)
                KeyCode::Char('f') => {
                    let (task, repo, path) = (task.clone(), repo.clone(), path.clone());
                    app.modal = Modal::None;
                    match worktree::resume_session(&repo, &task, &path, true) {
                        Ok(_) => app.set_info(format!("'{task}' restarted with a fresh conversation")),
                        Err(e) => app.set_err(e),
                    }
                    refresh(app);
                    load_detail(app);
                }
                // clean slate: rm worktree + branch, then `wta new` with the same name/base.
                // Only when nothing can be lost — otherwise point at D (which warns properly).
                KeyCode::Char('r') => {
                    let (task, repo, path, root, base) =
                        (task.clone(), repo.clone(), path.clone(), root.clone(), base.clone());
                    app.modal = Modal::None;
                    match recreate_guard(&repo, &task, &path, &base) {
                        Err(why) => app.set_err(why),
                        Ok(()) => {
                            // force: the guard proved the worktree is clean and the branch
                            // fully merged, and a plain rm would leave an unmerged branch
                            // behind that makes the `new --base` below bail.
                            let res = (|| -> anyhow::Result<()> {
                                worktree::rm_in(&repo, &root, &task, true).context("rm failed")?;
                                in_repo(&root, || {
                                    if base.is_empty() {
                                        worktree::new(&task, &[])
                                    } else {
                                        worktree::new_with_base(&task, &[], &base)
                                    }
                                })
                            })();
                            match res {
                                Ok(_) => app.set_info(format!("recreated '{task}' from scratch")),
                                Err(e) => app.set_err(format!("{e:#}")),
                            }
                        }
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
        Modal::AdoptRepoPick { repos, filter, sel } => {
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
                KeyCode::Enter => match matches.get(*sel).map(|(_, r)| r.clone()) {
                    Some(root) => open_adopt_pick(app, root),
                    None => app.modal = Modal::None,
                },
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
        Modal::AdoptPick { cands, sel, .. } => {
            match key.code {
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if *sel + 1 < cands.len() {
                        *sel += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some(c) = cands.get(*sel) {
                        app.modal = Modal::AdoptName { dir: c.dir.clone(), name: c.suggested.clone() };
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        Modal::AdoptName { dir, name } => {
            match key.code {
                KeyCode::Enter => {
                    let task = name.trim().to_string();
                    let dir = dir.clone();
                    app.modal = Modal::None;
                    if !task.is_empty() {
                        match worktree::adopt_core(&task, &dir) {
                            Ok(_) => app.set_info(format!("adopted '{task}' — ↵ to resume it")),
                            Err(e) => app.set_err(e),
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
        Modal::Settings { cfg, sel, editing, msg } => {
            let fields = crate::config::FIELDS;
            let key_at = |i: usize| fields[i].0;
            if let Some(buf) = editing {
                // Inline text-editing mode for editor / agent / model.
                match key.code {
                    KeyCode::Enter => {
                        let field = key_at(*sel);
                        let val = std::mem::take(buf);
                        *editing = None;
                        match cfg.set(field, &val).and_then(|_| crate::config::save(cfg)) {
                            Ok(_) => *msg = Some(format!("saved {field}")),
                            Err(e) => *msg = Some(e.to_string()),
                        }
                    }
                    KeyCode::Esc => *editing = None,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => buf.push(c),
                    _ => {}
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char(',') | KeyCode::Char('q') => app.modal = Modal::None,
                KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1) % fields.len(),
                KeyCode::Char('k') | KeyCode::Up => *sel = (*sel + fields.len() - 1) % fields.len(),
                // Clear the selected field back to its default.
                KeyCode::Char('d') | KeyCode::Delete => {
                    let field = key_at(*sel);
                    match cfg.set(field, "default").and_then(|_| crate::config::save(cfg)) {
                        Ok(_) => *msg = Some(format!("{field} → default")),
                        Err(e) => *msg = Some(e.to_string()),
                    }
                }
                // Enter / Space / l → text fields edit, enum fields cycle forward; h cycles back.
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Right
                | KeyCode::Char('h') | KeyCode::Left => {
                    let field = key_at(*sel);
                    match settings_cycle(field) {
                        None => *editing = Some(cfg.get(field).unwrap_or_default()),
                        Some(opts) => {
                            let cur = cfg.get(field).unwrap_or_default();
                            let i = opts.iter().position(|o| *o == cur).unwrap_or(0);
                            let back = matches!(key.code, KeyCode::Char('h') | KeyCode::Left);
                            let n = opts.len();
                            let j = if back { (i + n - 1) % n } else { (i + 1) % n };
                            match cfg.set(field, opts[j]).and_then(|_| crate::config::save(cfg)) {
                                Ok(_) => *msg = Some(format!("saved {field}")),
                                Err(e) => *msg = Some(e.to_string()),
                            }
                        }
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        Modal::Filter { text } => {
            match key.code {
                // Enter keeps the filter applied; Esc clears it
                KeyCode::Enter => app.modal = Modal::None,
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    app.filter.clear();
                    apply_view(app);
                    load_detail(app);
                }
                KeyCode::Backspace => {
                    text.pop();
                    app.filter = text.clone();
                    apply_view(app);
                    load_detail(app);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    text.push(c);
                    app.filter = text.clone();
                    apply_view(app);
                    load_detail(app);
                }
                _ => {}
            }
            return Ok(false);
        }
        Modal::Tags { repo, task, text } => {
            match key.code {
                KeyCode::Enter => {
                    let (repo, task) = (repo.clone(), task.clone());
                    let words: Vec<String> = text.split([',', ' ']).map(String::from).collect();
                    app.modal = Modal::None;
                    let tags = status::edit_tags(&[], &words);
                    match status::update(&repo, &task, |s| s.tags = tags.clone()) {
                        Ok(_) if tags.is_empty() => app.set_info(format!("cleared tags on '{task}'")),
                        Ok(_) => app.set_info(format!("'{task}' tagged #{}", tags.join(" #"))),
                        Err(e) => app.set_err(e),
                    }
                    refresh(app);
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
        Modal::Comment { session, task, loc, code, text } => {
            match key.code {
                KeyCode::Enter => {
                    let note = text.trim().to_string();
                    let (session, task) = (session.clone(), task.clone());
                    let line = comment_line(loc, code, &note);
                    app.modal = Modal::None;
                    if !note.is_empty() {
                        app.comments.entry(session).or_default().push(line);
                        let n = app.comments.values().map(|v| v.len()).sum::<usize>();
                        app.set_info(format!(
                            "comment queued for '{task}' — sent when it is ● ready ({n} pending)"
                        ));
                        deliver_comments(app);
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
        Modal::Compare { repo, root, group, sel, confirm } => {
            let members = compare_members(&app.all_rows, repo, group);
            let n = members.len();
            if *confirm {
                match key.code {
                    KeyCode::Char('y') => {
                        let keep = members.get(*sel).map(|r| r.task.clone()).unwrap_or_default();
                        let drop: Vec<String> =
                            members.iter().map(|r| r.task.clone()).filter(|t| *t != keep).collect();
                        let (repo, root) = (repo.clone(), root.clone());
                        app.modal = Modal::None;
                        let mut failed = Vec::new();
                        for t in &drop {
                            if let Err(e) = worktree::rm_in(&repo, &root, t, false) {
                                failed.push(format!("{t} ({e})"));
                            }
                        }
                        if failed.is_empty() {
                            app.set_info(format!("kept '{keep}', removed {}", drop.len()));
                        } else {
                            app.set_err(format!("kept '{keep}'; could not remove: {}", failed.join(", ")));
                        }
                        refresh(app);
                        load_detail(app);
                    }
                    KeyCode::Char('n') | KeyCode::Esc => *confirm = false,
                    _ => {}
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('=') => app.modal = Modal::None,
                KeyCode::Char('j') | KeyCode::Down if n > 0 => *sel = (*sel + 1) % n,
                KeyCode::Char('k') | KeyCode::Up if n > 0 => *sel = (*sel + n - 1) % n,
                KeyCode::Char('v') => {
                    // run every member's checks (async; results fill the CHECK column)
                    let jobs: Vec<(String, PathBuf, PathBuf)> = members
                        .iter()
                        .filter_map(|r| r.path.clone().map(|p| (r.session.clone(), r.root.clone(), p)))
                        .collect();
                    for (session, root, wt) in jobs {
                        spawn_check(app, &session, &root, &wt);
                    }
                }
                KeyCode::Enter | KeyCode::Char('K') if n > 1 => *confirm = true,
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

    // Board view: h/l move between columns, j/k within one (Enter etc. act as usual).
    if app.view == View::Board {
        let dir = match key.code {
            KeyCode::Char('h') | KeyCode::Left => Some((-1, 0)),
            KeyCode::Char('l') | KeyCode::Right => Some((1, 0)),
            KeyCode::Char('j') | KeyCode::Down => Some((0, 1)),
            KeyCode::Char('k') | KeyCode::Up => Some((0, -1)),
            _ => None,
        };
        if let Some((dx, dy)) = dir {
            if let Some(i) = board_move(app, dx, dy) {
                select_to(app, i);
            }
            return Ok(false);
        }
    }

    // Diff tab: Shift+↑↓ moves a line cursor (a `C` comment attaches to that line).
    if app.tab == Tab::Diff && key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Up => {
                app.diff_cursor = app.diff_cursor.saturating_sub(1);
                return Ok(false);
            }
            KeyCode::Down => {
                let max = app.diff_text.lines().count().saturating_sub(1);
                app.diff_cursor = (app.diff_cursor + 1).min(max);
                return Ok(false);
            }
            _ => {}
        }
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

    // vim `gg`: a `g` arms the next key; any other key disarms it.
    let was_g = std::mem::replace(&mut app.pending_g, false);
    match key.code {
        // `q` closes the table overlay first (layered, like lazygit); only quits at the base.
        KeyCode::Char('q') if app.view != View::Split => app.view = View::Split,
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        // vim navigation: gg/G jump to ends, Ctrl-d/Ctrl-u half-page
        KeyCode::Char('g') => {
            if was_g {
                select_to(app, 0);
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => select_to(app, app.rows.len().saturating_sub(1)),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            select_to(app, app.sel + app.rows.len().max(1) / 2);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            select_to(app, app.sel.saturating_sub(app.rows.len().max(1) / 2));
        }
        // toggle the fleet-table overlay (k9s-style; esc also closes it)
        KeyCode::Char('t') => {
            app.view = if app.view == View::Table { View::Split } else { View::Table };
        }
        // toggle the board (kanban columns by state)
        KeyCode::Char('B') => {
            app.view = if app.view == View::Board { View::Split } else { View::Board };
        }
        KeyCode::Esc if app.view != View::Split => app.view = View::Split,
        KeyCode::Char('/') => {
            app.modal = Modal::Filter { text: app.filter.clone() };
        }
        KeyCode::Esc if !app.filter.is_empty() => {
            app.filter.clear();
            apply_view(app);
            load_detail(app);
        }
        KeyCode::Char('P') => {
            if let Some((repo, task, pinned)) = app.selected().map(|r| (r.repo.clone(), r.task.clone(), r.pinned)) {
                match status::update(&repo, &task, |s| s.pinned = !pinned) {
                    Ok(_) => app.set_info(if pinned { format!("unpinned '{task}'") } else { format!("pinned '{task}'") }),
                    Err(e) => app.set_err(e),
                }
                refresh(app);
            }
        }
        KeyCode::Char('T') => {
            if let Some(r) = app.selected() {
                app.modal = Modal::Tags { repo: r.repo.clone(), task: r.task.clone(), text: r.tags.join(" ") };
            }
        }
        KeyCode::Char('z') => toggle_fold(app),
        KeyCode::Char('=') => open_compare(app),
        KeyCode::Char('C') => open_comment(app),
        KeyCode::Esc if app.scrollback.is_some() => {
            app.scrollback = None; // leave Preview scroll mode, back to live output
            app.scroll = 0;
        }
        KeyCode::Char('?') => app.modal = Modal::Help,
        KeyCode::Char(',') => {
            app.modal = Modal::Settings {
                cfg: crate::config::load(),
                sel: 0,
                editing: None,
                msg: None,
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.rows.is_empty() {
                app.sel = (app.sel + 1) % app.rows.len();
                app.scroll = 0;
                app.diff_cursor = 0;
                app.scrollback = None;
                load_detail(app);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.rows.is_empty() {
                app.sel = (app.sel + app.rows.len() - 1) % app.rows.len();
                app.scroll = 0;
                app.diff_cursor = 0;
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
            app.diff_cursor = 0;
            load_detail(app);
        }
        KeyCode::Enter | KeyCode::Char('o') => {
            if let Some(r) = app.selected() {
                if r.alive {
                    app.attach = Some(r.session.clone());
                } else if let Some(path) = r.path.clone() {
                    // Capture the EXACT agent identity (repo + path), so resume can't
                    // grab another repo's same-named agent or depend on the cwd.
                    app.modal = Modal::Resume {
                        task: r.task.clone(),
                        repo: r.repo.clone(),
                        path,
                    };
                } else {
                    app.set_err("no session and no worktree to resume");
                }
            }
        }
        // wta's own vim-style copy mode over the agent's conversation (any renderer);
        // runs after this frame, like attach, since it takes over the terminal
        KeyCode::Char('c') => {
            if let Some((repo, task)) = app.selected().map(|r| (r.repo.clone(), r.task.clone())) {
                app.copy = Some((repo, task));
            }
        }
        KeyCode::Char('n') => open_new(app, false),
        KeyCode::Char('N') => open_new(app, true),
        KeyCode::Char('a') => open_adopt(app),
        KeyCode::Char('s') => {
            if let Some(r) = app.selected() {
                let (repo, task) = (r.repo.clone(), r.task.clone());
                if let Err(e) = worktree::stop_session(&repo, &task) {
                    app.set_err(e);
                }
                refresh(app);
                load_detail(app);
            }
        }
        KeyCode::Char('D') => {
            if let Some((task, root, repo)) =
                app.selected().map(|r| (r.task.clone(), r.root.clone(), r.repo.clone()))
            {
                app.op_root = root;
                app.op_repo = repo;
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
                    if !worktree::is_gui_editor(&cmd) {
                        // Terminal editor. WTA_OPEN_TMUX / config open_mode decides:
                        // window = pop a NEW tmux window (dash keeps running, switch back
                        // with your tmux keys); inline = suspend the TUI in place; auto =
                        // window when inside tmux, else inline.
                        let mode = worktree::open_mode();
                        let popped = mode != worktree::OpenMode::Inline
                            && worktree::open_editor_window(&cmd, &path);
                        if popped {
                            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("agent");
                            app.set_info(format!(
                                "opened '{name}' in {}",
                                worktree::editor_placement()
                            ));
                        } else {
                            app.open = Some((cmd, path)); // suspend TUI + run after this frame
                        }
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

/// `z` — fold / unfold the selected agent's group: its own children if it has any,
/// else the group it belongs to (its parent's children, or its fanout siblings).
fn toggle_fold(app: &mut App) {
    let Some(r) = app.selected().cloned() else { return };
    let has_kids = app.all_rows.iter().any(|x| x.repo == r.repo && x.parent.as_deref() == Some(r.task.as_str()));
    let key = if has_kids { Some(r.task.clone()) } else { r.parent.clone() };
    let Some(key) = key else {
        app.set_err("nothing to fold — this agent has no children or group");
        return;
    };
    let k = format!("{}/{key}", r.repo);
    let folding = !app.folded.remove(&k);
    if folding {
        app.folded.insert(k);
    }
    apply_view(app);
    // a fold can hide the selected row: land on the parent (or the group's visible member)
    if !app.rows.iter().any(|x| x.session == r.session) {
        if let Some(i) = app
            .rows
            .iter()
            .position(|x| x.repo == r.repo && (x.task == key || x.parent.as_deref() == Some(key.as_str())))
        {
            app.sel = i;
        }
    }
    load_detail(app);
}

/// Members of a compare group: every agent in `repo` whose parent is `group`.
fn compare_members<'a>(rows: &'a [Row], repo: &str, group: &str) -> Vec<&'a Row> {
    rows.iter().filter(|r| r.repo == repo && r.parent.as_deref() == Some(group)).collect()
}

/// `=` — open the compare view for the selected agent's siblings (or its children).
fn open_compare(app: &mut App) {
    let Some(r) = app.selected().cloned() else { return };
    let group = match &r.parent {
        Some(p) => p.clone(),
        None if app.all_rows.iter().any(|x| x.repo == r.repo && x.parent.as_deref() == Some(r.task.as_str())) => {
            r.task.clone()
        }
        None => {
            app.set_err("nothing to compare — use on a fanout agent (`wta fanout`) or a parent");
            return;
        }
    };
    let members = compare_members(&app.all_rows, &r.repo, &group);
    let sel = members.iter().position(|m| m.session == r.session).unwrap_or(0);
    app.modal = Modal::Compare { repo: r.repo.clone(), root: r.root.clone(), group, sel, confirm: false };
}

/// Rank a compare member: checks passed, then acceptance criteria met, then a smaller
/// diff, then fewer tokens. Higher is better. Pure, so it is unit-tested.
fn compare_score(check: Option<i32>, ac: Option<(u32, u32)>, delta: u32, tokens: u64) -> (u8, u32, i64, i64) {
    let c = match check {
        Some(0) => 2,
        None => 1,
        Some(_) => 0,
    };
    let ac_pct = ac.map(|(d, t)| if t == 0 { 0 } else { d * 100 / t }).unwrap_or(0);
    (c, ac_pct, -(delta as i64), -(tokens.min(i64::MAX as u64) as i64))
}

/// Where a diff line lives: `(file, "L42")` for a `+`/context line (new-file numbering),
/// `(file, "old L42")` for a `-` line. `None` on headers / outside any hunk.
fn diff_location(lines: &[&str], idx: usize) -> Option<(String, String)> {
    let mut file: Option<String> = None;
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for (i, l) in lines.iter().enumerate().take(idx + 1) {
        let here = i == idx;
        if l.starts_with("diff --git") {
            in_hunk = false;
            file = None;
            if here {
                return None;
            }
            continue;
        }
        if let Some(f) = l.strip_prefix("+++ ") {
            if f != "/dev/null" {
                file = Some(f.strip_prefix("b/").unwrap_or(f).to_string());
            }
            if here {
                return None;
            }
            continue;
        }
        if l.starts_with("--- ") && !in_hunk {
            if here {
                return None;
            }
            continue;
        }
        if l.starts_with("@@") {
            // @@ -a,b +c,d @@
            let mut it = l.split_whitespace().skip(1);
            let num = |s: Option<&str>| {
                s.and_then(|x| x[1..].split(',').next().and_then(|n| n.parse::<u32>().ok())).unwrap_or(0)
            };
            old = num(it.next());
            new = num(it.next());
            in_hunk = true;
            if here {
                return file.map(|f| (f, format!("L{new}")));
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let f = file.clone().unwrap_or_default();
        if l.starts_with('-') {
            if here {
                return Some((f, format!("old L{old}")));
            }
            old += 1;
        } else if l.starts_with('+') {
            if here {
                return Some((f, format!("L{new}")));
            }
            new += 1;
        } else {
            if here {
                return Some((f, format!("L{new}")));
            }
            old += 1;
            new += 1;
        }
    }
    None
}

/// One review note as the agent will read it.
fn comment_line(loc: &str, code: &str, note: &str) -> String {
    let code: String = code.trim().chars().take(80).collect();
    if code.is_empty() {
        format!("{loc}: {note}")
    } else {
        format!("{loc} (`{code}`): {note}")
    }
}

/// `C` in the Diff tab — open a comment on the cursor line of the selected agent's diff.
fn open_comment(app: &mut App) {
    if app.tab != Tab::Diff {
        app.set_err("comments attach to a diff line — press tab for the Diff, Shift+↑↓ to pick a line");
        return;
    }
    let Some(r) = app.selected().cloned() else { return };
    let lines: Vec<&str> = app.diff_text.lines().collect();
    let Some((file, at)) = diff_location(&lines, app.diff_cursor) else {
        app.set_err("pick a code line (Shift+↑↓), not a header");
        return;
    };
    let code = lines.get(app.diff_cursor).copied().unwrap_or("").to_string();
    app.modal = Modal::Comment {
        session: r.session.clone(),
        task: r.task.clone(),
        loc: match at.strip_prefix("old L") {
            Some(n) => format!("{file}:{n} (removed line)"),
            None => format!("{file}:{}", at.trim_start_matches('L')),
        },
        code,
        text: String::new(),
    };
}

/// Send queued diff comments to every agent that is ● ready (one message per agent, so
/// the notes arrive together). Agents still working keep theirs queued until ready.
fn deliver_comments(app: &mut App) {
    if app.comments.is_empty() {
        return;
    }
    let ready: Vec<(String, String)> = app
        .all_rows
        .iter()
        .filter(|r| r.alive && r.status == Status::Ready && app.comments.contains_key(&r.session))
        .map(|r| (r.session.clone(), r.task.clone()))
        .collect();
    for (session, task) in ready {
        let notes = app.comments.remove(&session).unwrap_or_default();
        let msg = if notes.len() == 1 {
            format!("Review comment on your diff — {}", notes[0])
        } else {
            let list: Vec<String> = notes.iter().enumerate().map(|(i, n)| format!("({}) {n}", i + 1)).collect();
            format!("Review comments on your diff — {}", list.join("  "))
        };
        match tmux::send_text(&session, &msg) {
            Ok(_) => {
                if let Some(r) = app.all_rows.iter().find(|r| r.session == session) {
                    status::mark_prompt_sent(&r.repo, &r.task);
                }
                app.set_info(format!("sent {} comment(s) → {task}", notes.len()));
            }
            Err(e) => {
                app.comments.insert(session, notes); // keep them; retry next refresh
                app.set_err(format!("comments for '{task}' not sent yet: {e}"));
            }
        }
    }
    // an agent that is gone can't receive them
    let live: HashSet<&str> = app.all_rows.iter().map(|r| r.session.as_str()).collect();
    app.comments.retain(|k, _| live.contains(k.as_str()));
}

/// Board column for a row: 0 working · 1 needs you · 2 ready · 3 merged · 4 stopped.
fn board_col(r: &Row) -> usize {
    match r.status {
        Status::Running => 0,
        Status::NeedsInput => 1,
        Status::Ready => 2,
        Status::Merged => 3,
        Status::Exited | Status::Idle => 4,
    }
}
const BOARD_COLS: [&str; 5] = ["Working", "Needs you", "Ready", "Merged", "Stopped"];

/// Next selection on the board: `dy` moves within the column, `dx` jumps to the nearest
/// non-empty column that way (keeping the row position where it can).
fn board_move(app: &App, dx: i32, dy: i32) -> Option<usize> {
    let cur = app.selected()?;
    let col = board_col(cur);
    let in_col = |c: usize| -> Vec<usize> {
        app.rows.iter().enumerate().filter(|(_, r)| board_col(r) == c).map(|(i, _)| i).collect()
    };
    let here = in_col(col);
    let pos = here.iter().position(|&i| i == app.sel).unwrap_or(0);
    if dy != 0 {
        let n = here.len() as i32;
        let p = (pos as i32 + dy).rem_euclid(n.max(1)) as usize;
        return here.get(p).copied();
    }
    let mut c = col as i32 + dx;
    while (0..BOARD_COLS.len() as i32).contains(&c) {
        let v = in_col(c as usize);
        if !v.is_empty() {
            return v.get(pos.min(v.len() - 1)).copied();
        }
        c += dx;
    }
    None
}

/// Build the colored mergeability grid for the `m` overlay (calls git merge-tree
/// pairwise; read-only, touches no working tree).
/// Trim an ISO timestamp to `MM-DD HH:MM` for the chart's x-axis labels.
fn short_ts(ts: &str) -> String {
    ts.get(5..16).map(|s| s.replace('T', " ")).unwrap_or_else(|| ts.to_string())
}

/// Build the selected agent's tokens-over-time chart as overlay lines — computed on
/// demand (one agent, one transcript pass) so the live dashboard stays cheap.
fn chart_lines(wt: &Path) -> Vec<Line<'static>> {
    let tl = crate::cost::timeline(wt);
    if tl.is_empty() {
        return vec![Line::styled("no usage recorded yet", Style::default().fg(Color::DarkGray))];
    }
    let vals: Vec<f64> = tl.iter().map(|s| s.delta_tokens as f64).collect();
    let (bars, max) = crate::cost::barchart(&vals, 92, 8, false);
    let total: u64 = tl.iter().map(|s| s.delta_tokens).sum();
    let mut out: Vec<Line<'static>> = vec![Line::styled(
        format!(
            "tokens/bucket · peak {} · total {}",
            crate::cost::human_tokens(max as u64),
            crate::cost::human_tokens(total)
        ),
        Style::default().fg(Color::DarkGray),
    )];
    for b in bars {
        out.push(Line::styled(b, Style::default().fg(GREEN)));
    }
    out.push(Line::styled(
        format!("{}  →  {}", short_ts(&tl.first().unwrap().ts), short_ts(&tl.last().unwrap().ts)),
        Style::default().fg(Color::DarkGray),
    ));
    out
}

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

/// `a` — adopt an existing Claude session as an agent. Pick a repo (global dash),
/// then a session under it. Mirrors `open_new`'s repo-selection flow.
fn open_adopt(app: &mut App) {
    let mut list: Vec<(String, PathBuf)> = worktree::discover_repos()
        .iter()
        .map(|(_, root)| (worktree::repo_name(root), root.clone()))
        .collect();
    if !app.root.as_os_str().is_empty() && !list.iter().any(|(_, r)| r == &app.root) {
        list.insert(0, (worktree::repo_name(&app.root), app.root.clone()));
    }
    if app.global && list.len() > 1 {
        let sel = list.iter().position(|(_, r)| r == &app.root).unwrap_or(0);
        app.modal = Modal::AdoptRepoPick { repos: list, filter: String::new(), sel };
    } else {
        let root = list
            .first()
            .map(|(_, r)| r.clone())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_default();
        if root.as_os_str().is_empty() {
            app.set_err("cd into a repo to adopt a session");
            return;
        }
        open_adopt_pick(app, root);
    }
}

/// Scan `root` for adoptable Claude sessions and open the session picker (or warn).
fn open_adopt_pick(app: &mut App, root: PathBuf) {
    let cands = worktree::adopt_candidates(&root);
    if cands.is_empty() {
        app.modal = Modal::None;
        app.set_err(format!(
            "no un-adopted Claude sessions found in {}",
            worktree::repo_name(&root)
        ));
        return;
    }
    app.modal = Modal::AdoptPick { root, cands, sel: 0 };
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
/// Is "recreate from scratch" safe for this agent? Refuses (with the reason) when it
/// would destroy anything: an adopted agent (its dir isn't a wta worktree — `rm` only
/// untracks it, and `new` would build an unrelated one), uncommitted changes, or
/// commits not yet in the base branch.
fn recreate_guard(repo: &str, task: &str, path: &Path, base: &str) -> Result<(), String> {
    if crate::status::read_state(repo, task).map(|s| s.adopted).unwrap_or(false) {
        return Err(format!(
            "'{task}' is an adopted session (not a wta worktree) — use f, or rm + new manually"
        ));
    }
    if !path.exists() {
        return Err(format!("no worktree at {} — press D, then n", path.display()));
    }
    let dirty = git_in(path, &["status", "--porcelain"]).map(|s| !s.trim().is_empty()).unwrap_or(true);
    if dirty {
        return Err(format!("'{task}' has uncommitted changes — kill it with D first, then n"));
    }
    if !base.is_empty() {
        let range = format!("{base}..HEAD");
        let ahead: u32 = git_in(path, &["rev-list", "--count", &range])
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if ahead > 0 {
            return Err(format!(
                "'{task}' has {ahead} commit(s) not in {base} — push/land them, or D to discard"
            ));
        }
    }
    Ok(())
}

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
fn repo_rows(app: &mut App, repo: &str, root: &Path, live: &HashSet<String>, out: &mut Vec<Row>) {
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
        // wta-managed worktrees live under `.agents/`; ADOPTED agents (`wta adopt`) can
        // live anywhere (the user's own dir) — include them by their recorded cwd.
        if (st.adopted || p.starts_with(&agents_dir)) && p.exists() {
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

        // Token usage + model, cadence-refreshed like the diffstat (parsing transcripts
        // is heavy, so only on the periodic full sweep or for the selected agent).
        // Cost is parsed on the worker thread (transcripts can be hundreds of MB).
        // A cheap stat (newest transcript mtime) gates it: only when the agent has
        // written new output since we last parsed do we enqueue a fresh parse, so an
        // idle fleet costs nothing and a paint never blocks on transcript reads.
        let (cost, model) = match path.as_deref() {
            Some(p) => {
                let (_, mtime) = crate::cost::sessions_for(p);
                let stale = app.costmtime.get(&session).copied() != Some(mtime);
                if stale && app.cost_inflight.insert(session.clone()) {
                    app.costmtime.insert(session.clone(), mtime);
                    let _ = app.cost_tx.send((session.clone(), p.to_path_buf()));
                }
                app.costcache.get(&session).cloned().unwrap_or_default()
            }
            None => (crate::cost::Usage::default(), None),
        };

        // Diffstat / merge check are cheap git calls; keep them synchronous but only
        // on the periodic sweep or for the selected agent, so launch isn't a burst of
        // git across the whole fleet (the first paint shows cached-or-zero instead).
        let (added, removed) = match path.as_deref() {
            Some(p) => {
                if full_sweep || is_sel {
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
                if full_sweep || is_sel {
                    let v = is_merged(p, &base, &branch);
                    app.mergedcache.insert(session.clone(), v);
                    v
                } else {
                    app.mergedcache.get(&session).copied().unwrap_or(false)
                }
            }
            None => false,
        };

        let alive = live.contains(&session);
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
            let st = states.get(&task);
            match st.map(|s| s.status.as_str()) {
                // Claude's Notification hook is the authoritative "needs you" signal.
                Some("needs_input") => Status::NeedsInput,
                // No authoritative hook signal → classify the pane by screen manifest so
                // NON-Claude agents (codex/gemini/…) still report needs-input / working
                // without hooks. `None` = no opinion → fall back to the pane-hash heuristic.
                _ => {
                    let engine = st.and_then(|s| s.engine.as_deref());
                    match crate::detect::classify(engine, &text) {
                        // a terminal error (auth/quota) needs you as much as a prompt does
                        Some(crate::detect::PaneState::Error) => Status::NeedsInput,
                        Some(crate::detect::PaneState::NeedsInput) => Status::NeedsInput,
                        Some(crate::detect::PaneState::Working) => Status::Running,
                        None if changed => Status::Running,
                        None => Status::Ready,
                    }
                }
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

        let ac = path
            .as_deref()
            .and_then(crate::worktree::read_task_spec)
            .map(|s| (s.checked_ac, s.total_ac));
        let st = states.get(&task);
        let updated_unix = st.map(|s| s.updated_unix).unwrap_or(0);
        let pinned = st.map(|s| s.pinned).unwrap_or(false);
        let tags = st.map(|s| s.tags.clone()).unwrap_or_default();
        let parent = st.and_then(|s| s.parent.clone()).filter(|p| !p.is_empty() && *p != task);
        let sleeping = status == Status::Exited && st.map(|s| s.sleeping).unwrap_or(false);
        let account = st.and_then(|s| s.account.clone());
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
            model,
            ac,
            updated_unix,
            pinned,
            tags,
            parent,
            sleeping,
            account,
            depth: 0,
            header: None,
            hidden: 0,
        });
    }
}

/// Order one repo's rows into a tree: pinned units first, each child right under its
/// parent (indented), and fanout siblings — whose parent is a NAME, not an agent —
/// gathered under one group label. `folded` holds the parent keys collapsed with `z`:
/// a real parent hides its children; a fanout group shows only its first member.
fn nest(rows: Vec<Row>, folded: &dyn Fn(&str) -> bool) -> Vec<Row> {
    let tasks: HashSet<String> = rows.iter().map(|r| r.task.clone()).collect();
    let mut kids: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(p) = &r.parent {
            kids.entry(p.clone()).or_default().push(i);
        }
    }
    // top-level units, in list order: a lone/parent agent, or a virtual fanout group
    enum Unit {
        Agent(usize),
        Group(String),
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut seen_groups = HashSet::new();
    for (i, r) in rows.iter().enumerate() {
        match &r.parent {
            Some(p) if tasks.contains(p) => {} // drawn under its parent
            Some(p) => {
                if seen_groups.insert(p.clone()) {
                    units.push(Unit::Group(p.clone()));
                }
            }
            None => units.push(Unit::Agent(i)),
        }
    }
    let unit_pinned = |u: &Unit| match u {
        Unit::Agent(i) => rows[*i].pinned,
        Unit::Group(g) => kids.get(g).map(|v| v.iter().any(|&i| rows[i].pinned)).unwrap_or(false),
    };
    units.sort_by_key(|u| !unit_pinned(u)); // stable: pinned first, list order kept

    fn emit(
        i: usize,
        depth: u8,
        rows: &[Row],
        kids: &HashMap<String, Vec<usize>>,
        folded: &dyn Fn(&str) -> bool,
        done: &mut HashSet<usize>,
        out: &mut Vec<Row>,
    ) {
        if !done.insert(i) {
            return; // a parent cycle — never loop
        }
        let mut r = rows[i].clone();
        r.depth = depth;
        let children = kids.get(&r.task).cloned().unwrap_or_default();
        let fold = !children.is_empty() && folded(&r.task);
        if fold {
            r.hidden = children.len() as u32;
        }
        out.push(r);
        if !fold && depth < 3 {
            for c in children {
                emit(c, depth + 1, rows, kids, folded, done, out);
            }
        }
    }

    let mut out = Vec::with_capacity(rows.len());
    let mut done = HashSet::new();
    for u in units {
        match u {
            Unit::Agent(i) => emit(i, 0, &rows, &kids, folded, &mut done, &mut out),
            Unit::Group(g) => {
                let members = kids.get(&g).cloned().unwrap_or_default();
                let fold = folded(&g);
                let start = out.len();
                for (n, &m) in members.iter().enumerate() {
                    if fold && n > 0 {
                        break;
                    }
                    emit(m, 1, &rows, &kids, folded, &mut done, &mut out);
                }
                if let Some(first) = out.get_mut(start) {
                    first.header = Some(g.clone());
                    if fold {
                        first.hidden = members.len().saturating_sub(1) as u32;
                    }
                }
            }
        }
    }
    // anything unreachable (shouldn't happen) still shows, flat
    for i in 0..rows.len() {
        if !done.contains(&i) && !(rows[i].parent.is_some()) {
            out.push(rows[i].clone());
        }
    }
    out
}

/// The status word a filter term can match (`running`, `needs`, `sleeping`, …).
fn status_word(r: &Row) -> &'static str {
    if r.sleeping {
        return "sleeping";
    }
    match r.status {
        Status::Running => "running",
        Status::Ready => "ready",
        Status::NeedsInput => "needs-input",
        Status::Merged => "merged",
        Status::Exited => "exited",
        Status::Idle => "idle",
    }
}

/// Does `r` match EVERY whitespace-separated term of `filter`? `#x` matches a tag
/// prefix; `is:pinned` / `is:<state>` match flags; anything else is a case-insensitive
/// substring of the task, repo, base, a tag, the state word, or the account.
fn row_matches(r: &Row, filter: &str) -> bool {
    filter.split_whitespace().all(|term| {
        let t = term.to_lowercase();
        if let Some(tag) = t.strip_prefix('#') {
            return r.tags.iter().any(|x| x.starts_with(tag));
        }
        if let Some(flag) = t.strip_prefix("is:") {
            return match flag {
                "pinned" | "pin" => r.pinned,
                "child" => r.parent.is_some(),
                f => status_word(r).starts_with(f),
            };
        }
        r.task.to_lowercase().contains(&t)
            || r.repo_name.to_lowercase().contains(&t)
            || r.base.to_lowercase().contains(&t)
            || r.tags.iter().any(|x| x.contains(&t))
            || status_word(r).starts_with(&t)
            || r.account.as_deref().map(|a| a.to_lowercase().contains(&t)).unwrap_or(false)
    })
}

/// Rebuild the visible `rows` from `all_rows`: nest + fold each repo group, then apply
/// the `/` filter (a filter shows matches even inside folded groups). Cheap — no git
/// or tmux — so typing a filter or folding re-renders instantly. Keeps the selection.
fn apply_view(app: &mut App) {
    let prev = app.selected().map(|r| r.session.clone());
    let filtering = !app.filter.trim().is_empty();
    let mut out: Vec<Row> = Vec::with_capacity(app.all_rows.len());
    let mut i = 0;
    while i < app.all_rows.len() {
        let repo = app.all_rows[i].repo.clone();
        let mut j = i;
        while j < app.all_rows.len() && app.all_rows[j].repo == repo {
            j += 1;
        }
        let group: Vec<Row> = app.all_rows[i..j].to_vec();
        let folded = &app.folded;
        let is_folded = |k: &str| !filtering && folded.contains(&format!("{repo}/{k}"));
        out.extend(nest(group, &is_folded));
        i = j;
    }
    if filtering {
        out.retain(|r| row_matches(r, &app.filter));
    }
    app.rows = out;
    app.sel = prev
        .and_then(|s| app.rows.iter().position(|r| r.session == s))
        .unwrap_or(app.sel)
        .min(app.rows.len().saturating_sub(1));
}

/// Fold in any cost results the worker finished since the last frame, updating both
/// the cache and the already-built rows so the numbers appear without a full refresh.
fn drain_costs(app: &mut App) {
    let mut updated = false;
    while let Ok((session, cm)) = app.cost_rx.try_recv() {
        app.cost_inflight.remove(&session);
        app.costcache.insert(session, cm);
        updated = true;
    }
    if updated {
        for r in app.rows.iter_mut().chain(app.all_rows.iter_mut()) {
            if let Some((u, m)) = app.costcache.get(&r.session) {
                r.cost = u.clone();
                r.model = m.clone();
            }
        }
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

    // One `tmux list-sessions` for the whole fleet instead of a `has-session` spawn
    // per agent every refresh — liveness is then a set lookup.
    let live: HashSet<String> = tmux::list_sessions().into_iter().collect();

    let mut rows = Vec::new();
    for (repo, root) in &targets {
        repo_rows(app, repo, root, &live, &mut rows);
    }
    app.all_rows = rows;
    apply_view(app);
    app.sel = prev_sel
        .and_then(|s| app.rows.iter().position(|r| r.session == s))
        .unwrap_or(app.sel)
        .min(app.rows.len().saturating_sub(1));

    // caches are keyed by session (globally unique) — over ALL agents, not just the
    // filtered view, so a filter never throws away a hidden agent's cached numbers
    let live: HashSet<String> = app.all_rows.iter().map(|r| r.session.clone()).collect();
    app.hashes.retain(|k, _| live.contains(k));
    app.diffcache.retain(|k, _| live.contains(k));
    app.costcache.retain(|k, _| live.contains(k));
    app.mergedcache.retain(|k, _| live.contains(k));
    app.cost_inflight.retain(|k| live.contains(k));
    app.costmtime.retain(|k, _| live.contains(k));
    app.trust_seen.retain(|k, _| live.contains(k));
    app.trust_done.retain(|k| live.contains(k));

    let sel_now = app.rows.get(app.sel).map(|r| r.session.clone());
    let mut to_check: Vec<(String, PathBuf, PathBuf)> = Vec::new(); // (session, root, worktree)
    let mut invalidate: Vec<String> = Vec::new();
    for r in &app.all_rows {
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
    // mirror status changes into the attached status-bar chip (only on change, so a
    // quiet fleet costs no tmux calls)
    for r in app.all_rows.iter().filter(|r| r.alive) {
        if app.prev_status.get(&r.session) != Some(&r.status) {
            let s = match r.status {
                Status::Running => "running",
                Status::Ready => "ready",
                Status::NeedsInput => "needs_input",
                Status::Merged => "merged",
                Status::Exited => "exited",
                Status::Idle => "idle",
            };
            tmux::set_status_chip(&r.session, s);
        }
    }
    app.prev_status = app.all_rows.iter().map(|r| (r.session.clone(), r.status)).collect();
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
    deliver_comments(app);
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

/// Move the selection to `idx` (clamped) and refresh the detail pane — shared by the
/// vim jumps (gg/G/Ctrl-d/Ctrl-u) and j/k.
fn select_to(app: &mut App, idx: usize) {
    if app.rows.is_empty() {
        return;
    }
    app.sel = idx.min(app.rows.len() - 1);
    app.scroll = 0;
    app.diff_cursor = 0;
    app.scrollback = None;
    load_detail(app);
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
            Constraint::Length(1), // top summary band (k9s-style)
            Constraint::Min(1),
            Constraint::Length(1), // keybar
            Constraint::Length(1), // status/err line
        ])
        .split(f.area());
    render_header(f, app, root[0]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(root[1]);
    render_list(f, app, cols[0]);
    render_right(f, app, cols[1]);
    render_menu(f, app, root[2]);
    render_err(f, app, root[3]);
    // The fleet table is an OVERLAY over the split (lazygit-style; esc/q/t close it),
    // sized to its content, with the selected agent's tokens-over-time chart BELOW it
    // (btop-style). The chart is recomputed lazily only when the selection changes.
    if app.view == View::Table {
        if let Some(r) = app.rows.get(app.sel) {
            if app.chart_session != r.session {
                app.chart_session = r.session.clone();
                app.chart = r.path.clone().map(|p| chart_lines(&p)).unwrap_or_default();
            }
        }
        let ta = root[1];
        let chart_h = 12u16;
        let table_h = app.rows.len() as u16 + 3; // header + 2 borders + rows
        let w = 101u16.min(ta.width.saturating_sub(2));
        let h = (table_h + chart_h).min(ta.height);
        let area = centered(w, h, ta);
        f.render_widget(Clear, area);
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(chart_h)])
            .split(area);
        render_table(f, app, parts[0]);
        render_chart_panel(f, app, parts[1]);
    }
    if app.view == View::Board {
        render_board(f, app, root[1]);
    }
    render_modal(f, app);
}

/// Agent counts by status: `[running, ready, needs-input, merged, exited]` (idle ignored).
fn status_counts<'a>(rows: impl Iterator<Item = &'a Row>) -> [u32; 5] {
    let mut c = [0u32; 5];
    for r in rows {
        match r.status {
            Status::Running => c[0] += 1,
            Status::Ready => c[1] += 1,
            Status::NeedsInput => c[2] += 1,
            Status::Merged => c[3] += 1,
            Status::Exited => c[4] += 1,
            Status::Idle => {}
        }
    }
    c
}

/// A compact glyph rollup by status (`◐3 ●2 ▲1 ✗1`) — the fleet's at-a-glance health
/// line. Only non-zero buckets are shown; empty when there are none.
fn status_summary<'a>(rows: impl Iterator<Item = &'a Row>) -> String {
    let c = status_counts(rows);
    [(c[0], '◐'), (c[1], '●'), (c[2], '▲'), (c[3], '✓'), (c[4], '✗')]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, g)| format!("{g}{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A tiny in-row bar (`████░░`) for a 0..1 ratio — used to show each agent's token
/// burn relative to the fleet's busiest, so the big spenders read at a glance.
fn inline_bar(ratio: f64, width: usize) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    (0..width).map(|i| if i < filled { '█' } else { '░' }).collect()
}

/// k9s-style top band: scope, agent count, a colored status rollup, and fleet spend.
fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let muted = Style::default().fg(Color::DarkGray);
    let scope = if app.global {
        "all repos".to_string()
    } else {
        app.rows.first().map(|r| r.repo_name.clone()).unwrap_or_else(|| "this repo".into())
    };
    let c = status_counts(app.rows.iter());
    let tok: u64 = app.rows.iter().map(|r| r.cost.tokens()).sum();
    let mut spans = vec![
        Span::styled("▌ wta ", Style::default().fg(GREEN_BRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("· {scope} · "), muted),
        Span::styled(format!("{} agents", app.rows.len()), Style::default().fg(Color::Reset)),
    ];
    for (n, glyph, color) in [
        (c[0], '◐', Color::Reset),
        (c[1], '●', GREEN),
        (c[2], '▲', Color::Yellow),
        (c[3], '✓', Color::Cyan),
        (c[4], '✗', RED),
    ] {
        if n > 0 {
            spans.push(Span::styled(format!("  {glyph}{n}"), Style::default().fg(color)));
        }
    }
    if tok > 0 {
        spans.push(Span::styled(
            format!("   Σ {} tok", crate::cost::human_tokens(tok)),
            Style::default().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compact "time since last activity" for the table AGE column (`3h`, `20m`, `2d`).
fn short_age(updated: u64) -> String {
    if updated == 0 {
        return "-".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = now.saturating_sub(updated);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// k9s-style full-width fleet grid — one row per agent, aligned sortable-looking
/// columns, for scanning many agents at once (toggle with `t`).
fn render_table(f: &mut Frame, app: &App, area: Rect) {
    use ratatui::widgets::{Cell, Row as TRow, Table, TableState};
    f.render_widget(Clear, area); // overlay: hide the dashboard behind it
    let max_tok = app.rows.iter().map(|r| r.cost.tokens()).max().unwrap_or(0);
    let dim = Style::default().fg(Color::DarkGray);
    let header = TRow::new(["TASK", "REPO", "", "STATE", "MODEL", "BASE", "TOK", "BURN", "Δ", "AC", "AGE"])
        .style(Style::default().fg(GREEN).add_modifier(Modifier::BOLD));
    let rows: Vec<TRow> = app
        .rows
        .iter()
        .map(|r| {
            let (glyph, word, gc) = match r.status {
                Status::Running => ('◐', "running", Color::Reset),
                Status::Ready => ('●', "ready", GREEN),
                Status::NeedsInput => ('▲', "needs-you", Color::Yellow),
                Status::Merged => ('✓', "merged", Color::Cyan),
                Status::Exited => ('✗', "exited", RED),
                Status::Idle => ('·', "idle", Color::DarkGray),
            };
            let ratio = if max_tok > 0 { r.cost.tokens() as f64 / max_tok as f64 } else { 0.0 };
            let (burn, bcol) = if r.cost.tokens() > 0 {
                let c = if ratio >= 0.8 { RED } else if ratio >= 0.5 { Color::Yellow } else { GREEN };
                (inline_bar(ratio, 6), c)
            } else {
                (String::new(), Color::DarkGray)
            };
            let tok = if r.cost.tokens() > 0 { crate::cost::human_tokens(r.cost.tokens()) } else { String::new() };
            let ac = r.ac.map(|(c, t)| format!("{c}/{t}")).unwrap_or_else(|| "-".into());
            TRow::new(vec![
                Cell::from(r.task.clone()),
                Cell::from(r.repo_name.clone()).style(dim),
                Cell::from(glyph.to_string()).style(Style::default().fg(gc)),
                Cell::from(word).style(Style::default().fg(gc)),
                Cell::from(r.model.clone().unwrap_or_else(|| "-".into())).style(dim),
                Cell::from(r.base.clone()).style(dim),
                Cell::from(tok).style(Style::default().fg(Color::Yellow)),
                Cell::from(burn).style(Style::default().fg(bcol)),
                Cell::from(format!("+{}/-{}", r.added, r.removed)),
                Cell::from(ac),
                Cell::from(short_age(r.updated_unix)).style(dim),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(18), // TASK
        Constraint::Length(12), // REPO
        Constraint::Length(2),  // glyph
        Constraint::Length(9),  // STATE
        Constraint::Length(10), // MODEL
        Constraint::Length(10), // BASE
        Constraint::Length(6),  // TOK
        Constraint::Length(6),  // BURN
        Constraint::Length(9),  // Δ
        Constraint::Length(5),  // AC
        Constraint::Length(4),  // AGE
    ];
    let sum = status_summary(app.rows.iter());
    let title = format!(" Agents · {} · esc to close ", if sum.is_empty() { "—" } else { &sum });
    let table = Table::new(rows, widths).header(header).column_spacing(1).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(HL))
            .title(title)
            .title_style(Style::default().bg(GREEN).fg(SEL_FG).add_modifier(Modifier::BOLD)),
    );
    let table = table.row_highlight_style(
        Style::default().bg(SEL_BG).fg(SEL_FG).add_modifier(Modifier::BOLD),
    );
    let mut st = TableState::default();
    if !app.rows.is_empty() {
        st.select(Some(app.sel.min(app.rows.len().saturating_sub(1))));
    }
    f.render_stateful_widget(table, area, &mut st);
}

/// The chart panel beneath the fleet table — the selected agent's tokens-over-time
/// bars (precomputed in `app.chart`, recomputed on selection change).
fn render_chart_panel(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Clear, area);
    let title = app
        .rows
        .get(app.sel)
        .map(|r| format!(" {} · tokens over time ", r.task))
        .unwrap_or_else(|| " tokens over time ".to_string());
    let body = if app.chart.is_empty() {
        vec![Line::styled("  no usage recorded yet", Style::default().fg(Color::DarkGray))]
    } else {
        app.chart.clone()
    };
    f.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(HL))
                .title(title),
        ),
        area,
    );
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let base = if app.global { "Agents · all repos" } else { "Instances" };
    let sum = status_summary(app.rows.iter());
    let mut title = if sum.is_empty() { format!(" {base} ") } else { format!(" {base} · {sum} ") };
    if !app.filter.trim().is_empty() {
        title.push_str(&format!("· /{} ({}/{}) ", app.filter.trim(), app.rows.len(), app.all_rows.len()));
    }
    let block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(HL))
        .title(title)
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
            let sub = status_summary(app.rows.iter().filter(|x| x.repo == r.repo));
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("▸ {} ", r.repo_name), Style::default().fg(GREEN_BRIGHT).add_modifier(Modifier::BOLD)),
                Span::styled(format!("({sub})"), Style::default().fg(Color::DarkGray)),
            ])));
            last_repo = Some(r.repo.clone());
        }
        let base_indent = if app.global { "   " } else { " " };
        // a fanout group label above its first member: `⑂ name ×N`
        if let Some(g) = &r.header {
            let n = app.all_rows.iter().filter(|x| x.repo == r.repo && x.parent.as_deref() == Some(g.as_str())).count();
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{base_indent}⑂ {g} "), Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
                Span::styled(format!("×{n}  = compare"), Style::default().fg(Color::DarkGray)),
            ])));
        }
        // finished-but-unviewed agents get a bright review glyph; needs-input keeps ▲;
        // an agent stopped on purpose (`s`) sleeps: a dim `z` instead of the red ✗
        let glyph = if app.attention.contains(&r.session) && r.status != Status::NeedsInput {
            Span::styled("◆ ".to_string(), Style::default().fg(Color::LightYellow))
        } else if r.sleeping {
            Span::styled("z ".to_string(), Style::default().fg(Color::DarkGray))
        } else {
            status_glyph(app, r.status)
        };
        // optional verification-check indicator, shown just left of the status glyph
        let check: Option<Span> = app.checks.get(&r.session).map(|c| match c {
            Check::Running { .. } => Span::styled("⟳ ".to_string(), Style::default().fg(Color::DarkGray)),
            Check::Done { code: 0, .. } => Span::styled("✓ ".to_string(), Style::default().fg(GREEN_BRIGHT)),
            Check::Done { .. } => Span::styled("✗ ".to_string(), Style::default().fg(RED)),
        });
        // queued diff comments waiting for this agent to be ready
        let pending = app.comments.get(&r.session).map(|v| v.len()).unwrap_or(0);
        let note: Option<Span> =
            (pending > 0).then(|| Span::styled(format!("✎{pending} "), Style::default().fg(Color::Yellow)));
        let fold: Option<Span> =
            (r.hidden > 0).then(|| Span::styled(format!("+{} ", r.hidden), Style::default().fg(Color::DarkGray)));
        let extra = if check.is_some() { 2 } else { 0 }
            + note.as_ref().map(|s| s.content.width()).unwrap_or(0)
            + fold.as_ref().map(|s| s.content.width()).unwrap_or(0);
        // indent agents under the repo header in the global tree, children under parents
        let tree = if r.depth > 0 { format!("{}└ ", "  ".repeat(r.depth as usize - 1)) } else { String::new() };
        let pin = if r.pinned { "★ " } else { "" };
        let head_txt = if app.global {
            format!("{base_indent}{tree}{pin}{}", r.task)
        } else {
            format!(" {}. {tree}{pin}{}", i + 1, r.task)
        };
        let head = truncate_cols(&head_txt, inner_w.saturating_sub(2 + extra));
        let pad1 = inner_w.saturating_sub(head.width() + 2 + extra);
        let head_style = if r.sleeping { Style::default().fg(Color::DarkGray) } else { Style::default() };
        let mut spans1 = vec![Span::styled(head, head_style), Span::raw(" ".repeat(pad1))];
        spans1.extend(fold);
        spans1.extend(note);
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
        let indent = format!("{}{}", if app.global { "     " } else { "   " }, "  ".repeat(r.depth as usize));
        let tags = if r.tags.is_empty() { String::new() } else { format!("  #{}", r.tags.join(" #")) };
        let acct = r.account.as_deref().map(|a| format!("  @{a}")).unwrap_or_default();
        let bhead = truncate_cols(&format!("{indent}Ꮧ {}{tags}{acct}", r.base), inner_w.saturating_sub(counts_len));
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
    if items.is_empty() && !app.filter.trim().is_empty() {
        items.push(ListItem::new(Line::styled(
            format!("  nothing matches /{} — esc clears", app.filter.trim()),
            Style::default().fg(Color::DarkGray),
        )));
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
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(HL))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let tabs = Tabs::new(vec!["Preview", if app.tab == Tab::Diff { "Diff · ⇧↑↓ line · C comment" } else { "Diff" }])
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
            let n = app.diff_text.lines().count();
            app.diff_cursor = app.diff_cursor.min(n.saturating_sub(1));
            let has_diff = n > 0 && app.diff_text != "No changes." && app.diff_text != "No worktree.";
            for (i, l) in app.diff_text.lines().enumerate() {
                let mut line = colorize_diff_line(l);
                if has_diff && i == app.diff_cursor {
                    line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
                }
                lines.push(line);
            }
            let max_off = lines.len().saturating_sub(body_h) as u16;
            // keep the cursor line (+1 for the counts header) inside the window
            let cur = app.diff_cursor as u16 + 1;
            if cur < app.scroll {
                app.scroll = cur;
            } else if body_h > 0 && cur >= app.scroll + body_h as u16 {
                app.scroll = cur + 1 - body_h as u16;
            }
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
            Span::styled("t/B", action),
            Span::styled(" table/board  ", muted),
            Span::styled("/", action),
            Span::styled(" filter  ", muted),
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

/// Allowed values for an enum setting (index 0 is "" = default), or None for a
/// free-text field that should be typed instead of cycled.
fn settings_cycle(key: &str) -> Option<Vec<&'static str>> {
    Some(match key {
        "effort" => vec!["", "low", "medium", "high", "xhigh", "max"],
        "hint_bar" => vec!["", "on", "off"],
        "open_mode" => vec!["", "auto", "window", "inline"],
        _ => return None,
    })
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
                    .borders(Borders::ALL).border_type(BorderType::Rounded)
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(RED))
                        .title(" force kill "),
                ),
                area,
            );
        }
        Modal::Resume { task, .. } => {
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HL))
                        .title(" resume "),
                ),
                area,
            );
        }
        Modal::ResumeFailed { task, reason, .. } => {
            let why: Vec<&str> = reason.lines().filter(|l| !l.trim().is_empty()).collect();
            let width = 72.min(f.area().width.saturating_sub(2)).max(40);
            let inner = width.saturating_sub(4) as usize;
            let mut body = vec![Line::styled(
                format!("'{task}' exited right after launch:"),
                Style::default().fg(RED),
            )];
            for l in why.iter().take(4) {
                let mut t: String = l.trim().chars().take(inner).collect();
                if l.trim().chars().count() > inner {
                    t.push('…');
                }
                body.push(Line::styled(format!("  {t}"), Style::default().fg(Color::DarkGray)));
            }
            if why.is_empty() {
                body.push(Line::styled("  (no output)", Style::default().fg(Color::DarkGray)));
            }
            body.push(Line::from(""));
            body.push(Line::from(vec![
                Span::styled("f", Style::default().fg(GREEN)),
                Span::raw("  fresh conversation in the same worktree (keeps branch + changes)"),
            ]));
            body.push(Line::from(vec![
                Span::styled("r", Style::default().fg(GREEN)),
                Span::raw("  recreate from scratch (rm worktree + new agent, same name)"),
            ]));
            body.push(Line::from(vec![
                Span::styled("esc", Style::default().fg(RED)),
                Span::raw("  leave it"),
            ]));
            let area = centered(width, body.len() as u16 + 2, f.area());
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(RED))
                        .title(" resume failed "),
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
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
                            .borders(Borders::ALL).border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(GREEN))
                            .title(format!(" send to '{task}' (Enter sends, Esc cancels) ")),
                    ),
                area,
            );
        }
        Modal::Settings { cfg, sel, editing, msg } => {
            let fields = crate::config::FIELDS;
            let area = centered(72, (fields.len() + 5) as u16, f.area());
            f.render_widget(Clear, area);
            let mut lines: Vec<Line> = Vec::new();
            for (i, (key, help)) in fields.iter().enumerate() {
                let selected = i == *sel;
                let editing_here = selected && editing.is_some();
                let value = if editing_here {
                    format!("{}▏", editing.as_deref().unwrap_or(""))
                } else {
                    let v = cfg.get(key).unwrap_or_default();
                    if v.is_empty() { "(default)".to_string() } else { v }
                };
                let marker = if selected { "›" } else { " " };
                let name_style = if selected {
                    Style::default().fg(SEL_FG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(GREEN_SOFT)
                };
                let val_style = if editing_here {
                    Style::default().fg(SEL_FG).add_modifier(Modifier::BOLD)
                } else if cfg.get(key).unwrap_or_default().is_empty() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(GREEN)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {marker} "), name_style),
                    Span::styled(format!("{key:<10}"), name_style),
                    Span::styled(format!("{value:<18}"), val_style),
                    Span::styled(help.to_string(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            lines.push(Line::from(""));
            let foot = match msg {
                Some(m) => Span::styled(format!(" {m}"), Style::default().fg(GREEN)),
                None => Span::styled(
                    " env vars override these · saved to ~/.wta/config.json",
                    Style::default().fg(Color::DarkGray),
                ),
            };
            lines.push(Line::from(foot));
            let title = if editing.is_some() {
                " settings — type value · Enter save · Esc cancel "
            } else {
                " settings — ↑↓ move · Enter edit/cycle · d default · Esc close "
            };
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(GREEN))
                        .title(title),
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HL))
                        .title(" new agent — pick a repo (↑↓ Enter, Esc) "),
                ),
                area,
            );
        }
        Modal::AdoptRepoPick { repos, filter, sel } => {
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HL))
                        .title(" adopt — pick a repo (↑↓ Enter, Esc) "),
                ),
                area,
            );
        }
        Modal::AdoptPick { root, cands, sel } => {
            let area = centered(66, 16, f.area());
            f.render_widget(Clear, area);
            let mut lines = vec![Line::from(vec![
                Span::styled("sessions in ", Style::default().fg(Color::DarkGray)),
                Span::styled(worktree::repo_name(root), Style::default().fg(GREEN_SOFT)),
            ])];
            let h = area.height.saturating_sub(3) as usize;
            let start = if *sel >= h { *sel + 1 - h } else { 0 };
            for (off, c) in cands.iter().skip(start).take(h).enumerate() {
                let idx = start + off;
                let style = if idx == *sel {
                    Style::default().bg(SEL_BG).fg(SEL_FG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(GREEN_SOFT)
                };
                let plural = if c.sessions == 1 { "" } else { "s" };
                let meta = format!("{} session{} · {} ago", c.sessions, plural, short_age(c.last_unix));
                lines.push(Line::from(vec![
                    Span::styled(format!(" {:<30}", c.label), style),
                    Span::styled(format!("  {meta}"), Style::default().fg(Color::DarkGray)),
                ]));
            }
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HL))
                        .title(" adopt a Claude session (↑↓ Enter, Esc) "),
                ),
                area,
            );
        }
        Modal::AdoptName { name, .. } => {
            let area = centered(50, 3, f.area());
            f.render_widget(Clear, area);
            let p = Paragraph::new(format!("{name}▏")).block(
                Block::default()
                    .borders(Borders::ALL).border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(GREEN))
                    .title(" name this agent (Enter adopts, Esc cancels) "),
            );
            f.render_widget(p, area);
        }
        Modal::Filter { text } => {
            let area = centered(64, 4, f.area());
            f.render_widget(Clear, area);
            let body = vec![
                Line::raw(format!("{text}▏")),
                Line::styled(
                    format!("{} shown · #tag · is:pinned · is:needs · Enter keep · Esc clear", app.rows.len()),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(GREEN))
                        .title(" filter agents "),
                ),
                area,
            );
        }
        Modal::Tags { task, text, .. } => {
            let area = centered(60, 4, f.area());
            f.render_widget(Clear, area);
            let body = vec![
                Line::raw(format!("{text}▏")),
                Line::styled("space-separated · empty clears · Enter save · Esc cancel", Style::default().fg(Color::DarkGray)),
            ];
            f.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(GREEN))
                        .title(format!(" tags for '{task}' ")),
                ),
                area,
            );
        }
        Modal::Comment { task, loc, code, text, .. } => {
            let area = centered(72, 7, f.area());
            f.render_widget(Clear, area);
            let inner = area.width.saturating_sub(4) as usize;
            let body = vec![
                Line::styled(truncate_cols(loc, inner), Style::default().fg(GREEN)),
                Line::styled(truncate_cols(code.trim_end(), inner), Style::default().fg(Color::DarkGray)),
                Line::raw(""),
                Line::raw(format!("{text}▏")),
            ];
            f.render_widget(
                Paragraph::new(body).wrap(ratatui::widgets::Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(GREEN))
                        .title(format!(" comment for '{task}' (Enter queues · sent when ready · Esc) ")),
                ),
                area,
            );
        }
        Modal::Compare { repo, group, sel, confirm, .. } => {
            render_compare(f, app, repo, group, *sel, *confirm);
        }
        Modal::Matrix(lines) => {
            let h = (lines.len() as u16 + 2).min(f.area().height);
            let w = 78u16.min(f.area().width);
            let area = centered(w, h, f.area());
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines.clone()).block(
                    Block::default()
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HL))
                        .title(" mergeability — do branches conflict? (any key closes) "),
                ),
                area,
            );
        }
        Modal::Help => {
            let area = centered(64, 34, f.area());
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
                k("gg / G", "jump to top / bottom"),
                k("Ctrl-d/u", "half-page down / up"),
                k("J / K", "reorder down / up"),
                k("t", "fleet table + per-agent chart (esc/q closes)"),
                k("B", "board: columns by state (h/l/j/k move)"),
                k("/", "filter: name, repo, #tag, is:pinned, is:needs…"),
                k("P / T", "pin to top / edit tags"),
                k("z", "fold / unfold a parent's children or a fanout group"),
                k("=", "compare a fanout group; keep the best one"),
                k("C", "Diff tab: comment on the line (⇧↑↓), sent when ready"),
                k("m", "mergeability matrix (conflict preview)"),
                k("↵ / o", "attach into the agent (type here)"),
                k("i", "send one line to the agent (when ● ready)"),
                k("c", "copy mode: vi-style scroll/search/select/yank of the conversation (Alt-y while attached)"),
                k("v", "run .wta/verify.sh checks (auto-runs when an agent finishes)"),
                k("e", "open the worktree in nvim (split beside the dash) / $EDITOR"),
                k(",", "settings: editor, default agent/model/effort, hint bar"),
                k("Ctrl-q", "detach back to wta (while attached)"),
                k("Alt-] / [", "while attached: jump to next / previous agent (no dashboard)"),
                k("Alt-o", "while attached: jump to the last agent (toggle back & forth)"),
                k("tab", "switch Preview / Diff"),
                k("Shift+↑↓", "scroll Preview / Diff"),
                k("n", "new agent"),
                k("N", "new agent with an initial prompt"),
                k("a", "adopt an existing Claude session as an agent"),
                k("s", "stop (keep worktree — resume later)"),
                k("glyphs", "⠋ running · ● ready · ▲ needs input · ✓ merged · ✗ exited · z asleep"),
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
                        .borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HL))
                        .title(" wta — keys "),
                ),
                area,
            );
        }
        Modal::None => {}
    }
}

/// The `=` compare overlay: one line per sibling with its checks, acceptance criteria,
/// diff size, tokens and estimated $, best-ranked marked ★. Enter keeps one.
fn render_compare(f: &mut Frame, app: &App, repo: &str, group: &str, sel: usize, confirm: bool) {
    let members = compare_members(&app.all_rows, repo, group);
    let check_of = |r: &Row| match app.checks.get(&r.session) {
        Some(Check::Done { code }) => Some(*code),
        _ => None,
    };
    let best = members
        .iter()
        .enumerate()
        .max_by_key(|(_, r)| compare_score(check_of(r), r.ac, r.added + r.removed, r.cost.tokens()))
        .map(|(i, _)| i);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::styled(
        format!("  {:<22} {:<6} {:<6} {:>12} {:>7} {:>8}  STATE", "AGENT", "CHECK", "AC", "Δ", "TOK", "~$"),
        Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
    )];
    for (i, r) in members.iter().enumerate() {
        let check = match app.checks.get(&r.session) {
            Some(Check::Running { .. }) => "⟳".to_string(),
            Some(Check::Done { code: 0 }) => "✓ pass".to_string(),
            Some(Check::Done { .. }) => "✗ fail".to_string(),
            None => "-".to_string(),
        };
        let ac = r.ac.map(|(c, t)| format!("{c}/{t}")).unwrap_or_else(|| "-".into());
        let mark = if Some(i) == best && members.len() > 1 { "★" } else { " " };
        let line = format!(
            "{mark} {:<22} {:<6} {:<6} {:>12} {:>7} {:>8}  {}",
            truncate_cols(&r.task, 22),
            check,
            ac,
            format!("+{}/-{}", r.added, r.removed),
            crate::cost::human_tokens(r.cost.tokens()),
            format!("{:.2}", r.cost.est_usd),
            status_word(r),
        );
        let style = if i == sel {
            Style::default().bg(SEL_BG).fg(SEL_FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(GREEN_SOFT)
        };
        lines.push(Line::styled(line, style));
    }
    if members.is_empty() {
        lines.push(Line::styled("  (no members left)", dim));
    }
    lines.push(Line::raw(""));
    if confirm {
        let keep = members.get(sel).map(|r| r.task.as_str()).unwrap_or("");
        lines.push(Line::from(vec![
            Span::styled(format!(" keep '{keep}' and remove the other {}? ", members.len().saturating_sub(1)), Style::default().fg(Color::Yellow)),
            Span::styled("y", Style::default().fg(RED)),
            Span::raw(" / "),
            Span::styled("n", Style::default().fg(GREEN)),
            Span::styled("  (agents with uncommitted work are kept)", dim),
        ]));
    } else {
        lines.push(Line::styled(
            " ★ best: checks pass › AC met › smaller diff › fewer tokens · v run checks · ↵ keep one · esc",
            dim,
        ));
    }
    let w = 92u16.min(f.area().width);
    let area = centered(w, lines.len() as u16 + 2, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL).border_type(BorderType::Rounded)
                .border_style(Style::default().fg(HL))
                .title(format!(" compare '{group}' ")),
        ),
        area,
    );
}

/// The board (`B`): one column per state, a card per agent (task, repo, tokens).
fn render_board(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Clear, area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 5); 5])
        .split(area);
    let colors = [Color::Reset, Color::Yellow, GREEN, Color::Cyan, Color::DarkGray];
    for (c, name) in BOARD_COLS.iter().enumerate() {
        let inner_w = cols[c].width.saturating_sub(2) as usize;
        let mut items: Vec<ListItem> = Vec::new();
        let mut sel_item = None;
        for (i, r) in app.rows.iter().enumerate().filter(|(_, r)| board_col(r) == c) {
            if i == app.sel {
                sel_item = Some(items.len());
            }
            let pin = if r.pinned { "★ " } else { "" };
            let attn = if app.attention.contains(&r.session) { "◆ " } else { "" };
            let mut meta = r.repo_name.clone();
            if r.cost.tokens() > 0 {
                meta.push_str(&format!(" · {}", crate::cost::human_tokens(r.cost.tokens())));
            }
            let tags = if r.tags.is_empty() { String::new() } else { format!("#{}", r.tags.join(" #")) };
            items.push(ListItem::new(vec![
                Line::raw(truncate_cols(&format!(" {attn}{pin}{}", r.task), inner_w)),
                Line::styled(truncate_cols(&format!("   {meta}"), inner_w), Style::default().fg(Color::DarkGray)),
                Line::styled(truncate_cols(&format!("   {tags}"), inner_w), Style::default().fg(Color::DarkGray)),
            ]));
        }
        let n = items.len();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if sel_item.is_some() { GREEN_BRIGHT } else { HL }))
                    .title(Span::styled(format!(" {name} {n} "), Style::default().fg(colors[c]).add_modifier(Modifier::BOLD))),
            )
            .highlight_style(Style::default().bg(SEL_BG).fg(SEL_FG).add_modifier(Modifier::BOLD));
        let mut st = ListState::default();
        st.select(sel_item);
        f.render_stateful_widget(list, cols[c], &mut st);
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
            model: None,
            ac: None,
            updated_unix: 0,
            pinned: false,
            tags: Vec::new(),
            parent: None,
            sleeping: false,
            account: None,
            depth: 0,
            header: None,
            hidden: 0,
        }
    }

    fn kid(task: &str, parent: &str) -> Row {
        let mut r = row(task, Status::Ready, true, 0, 0);
        r.parent = Some(parent.into());
        r
    }

    fn tasks(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth as usize), r.task)).collect()
    }

    #[test]
    fn nest_puts_children_under_parents_and_groups_fanouts() {
        let mut pinned = row("late", Status::Ready, true, 0, 0);
        pinned.pinned = true;
        let rows = vec![
            row("builder", Status::Ready, true, 0, 0),
            kid("fix-1", "fix"),
            row("solo", Status::Ready, true, 0, 0),
            kid("review-builder", "builder"),
            kid("fix-2", "fix"),
            pinned,
        ];
        let none = |_: &str| false;
        let out = nest(rows.clone(), &none);
        assert_eq!(
            tasks(&out),
            vec!["late", "builder", "  review-builder", "  fix-1", "  fix-2", "solo"]
        );
        assert_eq!(out[3].header.as_deref(), Some("fix"));
        // folding: a real parent hides its children; a fanout group keeps its first member
        let folded = |k: &str| k == "builder" || k == "fix";
        let out = nest(rows, &folded);
        assert_eq!(tasks(&out), vec!["late", "builder", "  fix-1", "solo"]);
        assert_eq!(out[1].hidden, 1);
        assert_eq!(out[2].hidden, 1);
    }

    #[test]
    fn nest_survives_a_parent_cycle() {
        let rows = vec![kid("a", "b"), kid("b", "a")];
        let out = nest(rows, &|_: &str| false);
        assert!(out.len() <= 2);
    }

    #[test]
    fn filter_matches_terms_tags_and_flags() {
        let mut r = row("fix-auth", Status::NeedsInput, true, 0, 0);
        r.tags = vec!["api".into(), "backend".into()];
        r.pinned = true;
        assert!(row_matches(&r, "auth"));
        assert!(row_matches(&r, "#back"));
        assert!(row_matches(&r, "is:pinned AUTH"));
        assert!(row_matches(&r, "is:needs"));
        assert!(row_matches(&r, "needs"));
        assert!(!row_matches(&r, "#web"));
        assert!(!row_matches(&r, "auth is:running"));
        let mut z = row("old", Status::Exited, false, 0, 0);
        z.sleeping = true;
        assert!(row_matches(&z, "is:sleeping"));
    }

    #[test]
    fn diff_location_numbers_new_and_old_lines() {
        let d = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -10,3 +10,4 @@ fn x\n ctx\n-gone\n+added\n+more\n ctx2";
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(diff_location(&lines, 0), None);
        assert_eq!(diff_location(&lines, 2), None);
        assert_eq!(diff_location(&lines, 4), Some(("src/a.rs".into(), "L10".into())));
        assert_eq!(diff_location(&lines, 5), Some(("src/a.rs".into(), "old L11".into())));
        assert_eq!(diff_location(&lines, 6), Some(("src/a.rs".into(), "L11".into())));
        assert_eq!(diff_location(&lines, 7), Some(("src/a.rs".into(), "L12".into())));
        assert_eq!(diff_location(&lines, 8), Some(("src/a.rs".into(), "L13".into())));
        // an untracked file appended as a /dev/null diff
        let n = "diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n+one\n+two";
        let nl: Vec<&str> = n.lines().collect();
        assert_eq!(diff_location(&nl, 6), Some(("n.txt".into(), "L2".into())));
    }

    #[test]
    fn compare_prefers_passing_checks_then_ac_then_smaller_diff() {
        let pass = compare_score(Some(0), Some((1, 2)), 500, 9);
        let fail = compare_score(Some(1), Some((2, 2)), 5, 1);
        let unknown = compare_score(None, None, 5, 1);
        assert!(pass > unknown && unknown > fail);
        assert!(compare_score(None, Some((2, 2)), 50, 0) > compare_score(None, Some((1, 2)), 5, 0));
        assert!(compare_score(None, None, 5, 0) > compare_score(None, None, 50, 0));
    }

    #[test]
    fn board_renders_columns_and_moves_across() {
        let mut app = App::new();
        app.view = View::Board;
        app.rows = vec![
            row("w1", Status::Running, true, 0, 0),
            row("n1", Status::NeedsInput, true, 0, 0),
            row("w2", Status::Running, true, 0, 0),
            row("r1", Status::Ready, true, 0, 0),
        ];
        let screen = render_to_string(&mut app, 120, 20);
        assert!(screen.contains("Working 2"));
        assert!(screen.contains("Needs you 1"));
        assert!(screen.contains("Stopped 0"));
        app.sel = 0;
        assert_eq!(board_move(&app, 0, 1), Some(2)); // down within Working
        assert_eq!(board_move(&app, 1, 0), Some(1)); // right → Needs you
        app.sel = 1;
        assert_eq!(board_move(&app, 1, 0), Some(3)); // right → Ready
        assert_eq!(board_move(&app, -1, 0), Some(0));
    }

    #[test]
    fn list_shows_tree_pins_tags_and_sleep() {
        let mut app = App::new();
        let mut p = row("builder", Status::Ready, true, 0, 0);
        p.pinned = true;
        p.tags = vec!["api".into()];
        let mut c = kid("review-builder", "builder");
        c.depth = 1;
        let mut z = row("nap", Status::Exited, false, 0, 0);
        z.sleeping = true;
        app.rows = vec![p, c, z];
        let screen = render_to_string(&mut app, 120, 24);
        println!("{screen}");
        assert!(screen.contains("★ builder"));
        assert!(screen.contains("#api"));
        assert!(screen.contains("└ review-builder"));
        assert!(screen.contains("z "));
    }

    #[test]
    fn table_view_renders_columns() {
        let mut app = App::new();
        app.view = View::Table;
        app.rows = vec![
            row("auth", Status::Running, true, 40, 4),
            row("flaky", Status::NeedsInput, true, 0, 0),
        ];
        let screen = render_to_string(&mut app, 100, 22);
        println!("\n{screen}\n");
        assert!(screen.contains("TASK"));
        assert!(screen.contains("tokens over time"));
        assert!(screen.contains("AGE"));
        assert!(screen.contains("auth"));
        assert!(screen.contains("Agents"));
    }

    #[test]
    fn inline_bar_fills_by_ratio() {
        assert_eq!(inline_bar(1.0, 6), "██████");
        assert_eq!(inline_bar(0.0, 6), "░░░░░░");
        assert_eq!(inline_bar(0.5, 6), "███░░░");
        assert_eq!(inline_bar(2.0, 4), "████"); // clamped
    }

    #[test]
    fn status_summary_rolls_up_by_glyph() {
        let rows = vec![
            row("a", Status::Running, true, 0, 0),
            row("b", Status::Running, true, 0, 0),
            row("c", Status::NeedsInput, true, 0, 0),
            row("d", Status::Exited, false, 0, 0),
        ];
        assert_eq!(status_summary(rows.iter()), "◐2 ▲1 ✗1");
        assert_eq!(status_summary(Vec::<Row>::new().iter()), "");
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
    fn resume_failed_modal_shows_reason_and_choices() {
        let mut app = App::new();
        app.rows = vec![row("gone", Status::Exited, false, 1, 0)];
        app.sel = 0;
        app.modal = Modal::ResumeFailed {
            task: "gone".into(),
            repo: "r".into(),
            path: PathBuf::from("/tmp/gone"),
            root: PathBuf::from("/tmp"),
            base: "main".into(),
            reason: "No conversation found to continue".into(),
        };
        let screen = render_to_string(&mut app, 100, 16);
        println!("\n{screen}\n");
        assert!(screen.contains("resume failed"));
        assert!(screen.contains("No conversation found to continue"));
        assert!(screen.contains("fresh conversation"));
        assert!(screen.contains("recreate from scratch"));
    }

    #[test]
    fn recreate_guard_refuses_dirty_or_unmerged_and_allows_clean() {
        let dir = std::env::temp_dir().join(format!("wta-recreate-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["-c", "commit.gpgsign=false", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "base"]);
        g(&["checkout", "-q", "-b", "feat"]);
        // clean + fully merged into main → allowed (task "zz-guard" has no wta state → not adopted)
        assert_eq!(recreate_guard("r", "zz-guard", &dir, "main"), Ok(()));
        // a commit not in main → refused
        g(&["-c", "commit.gpgsign=false", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "work"]);
        let e = recreate_guard("r", "zz-guard", &dir, "main").unwrap_err();
        assert!(e.contains("1 commit(s) not in main"), "{e}");
        // uncommitted file → refused
        g(&["reset", "-q", "--hard", "main"]);
        std::fs::write(dir.join("x.txt"), "x").unwrap();
        let e = recreate_guard("r", "zz-guard", &dir, "main").unwrap_err();
        assert!(e.contains("uncommitted"), "{e}");
        // missing worktree → refused
        let e = recreate_guard("r", "zz-guard", &dir.join("nope"), "main").unwrap_err();
        assert!(e.contains("no worktree"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
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
