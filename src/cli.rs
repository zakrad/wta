use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "wta",
    version,
    about = "worktree task agents — a harness for parallel AI coding agents"
)]
pub struct Cli {
    /// tmux server: "wta" (default, isolated socket) or "default" (your own tmux)
    #[arg(long, global = true)]
    pub server: Option<String>,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a worktree + branch, copy local context, start the agent in a tmux session
    New {
        task: String,
        /// Base the agent's branch on an existing branch (default: HEAD)
        #[arg(long)]
        base: Option<String>,
        /// Run the agent with no permission prompts (default; claude --dangerously-skip-permissions)
        #[arg(long)]
        yolo: bool,
        /// Keep permission prompts ON for this agent (opt out of the default skip)
        #[arg(long)]
        safe: bool,
        /// Model for this agent (claude --model), e.g. opus-4.8, sonnet-5, haiku-4.5
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort (claude --effort): low | medium | high | xhigh | max
        #[arg(long)]
        effort: Option<String>,
        /// Role to spawn as — picks that role's engine/model/effort from ~/.wta/roles.json
        /// (built-ins: architect, backend, frontend, reviewer, tester; default: worker)
        #[arg(long)]
        role: Option<String>,
        /// Everything after `--` is passed to the agent command (default: claude)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// List wta-managed agents (worktrees) with diffstat vs the base branch
    Ls,
    /// Token usage + estimated $ per agent (from Claude Code transcripts)
    Cost {
        /// a single agent (default: all agents in this repo, with a total)
        task: Option<String>,
        /// spend-over-time: a burn sparkline + model timeline per agent (compare side by side)
        #[arg(long)]
        chart: bool,
        /// chart dollars instead of tokens (Y-axis in estimated $)
        #[arg(long)]
        usd: bool,
        /// chart the cumulative running total instead of the per-bucket rate
        #[arg(long)]
        cumulative: bool,
        /// dump the per-message time series (ts, tokens, $, model) as JSON for analysis
        #[arg(long)]
        json: bool,
    },
    /// Preview which agent branches merge cleanly vs each other + base (no files touched)
    Matrix,
    /// Spawn N agents on the SAME prompt (names <name>-1..N); compare with `matrix`, merge the winner
    Fanout {
        /// name prefix for the agents (creates <name>-1 .. <name>-N)
        name: String,
        /// how many agents to spawn
        #[arg(short = 'n', long, default_value_t = 3)]
        count: u32,
        /// base the agents' branches on an existing branch (default: HEAD)
        #[arg(long)]
        base: Option<String>,
        /// run agents with no permission prompts (default; claude --dangerously-skip-permissions)
        #[arg(long)]
        yolo: bool,
        /// keep permission prompts ON (opt out of the default skip)
        #[arg(long)]
        safe: bool,
        /// model for every agent (claude --model), e.g. opus-4.8, sonnet-5
        #[arg(long)]
        model: Option<String>,
        /// reasoning effort (claude --effort): low | medium | high | xhigh | max
        #[arg(long)]
        effort: Option<String>,
        /// role for every agent — picks that role's engine/model/effort (default: worker)
        #[arg(long)]
        role: Option<String>,
        /// everything after `--` is passed to each agent (e.g. the prompt)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// Attach to an agent's tmux session in the foreground (Ctrl-q to detach)
    Attach { task: String },
    /// Open the agent's worktree in your editor ($WTA_OPEN_CMD or $EDITOR, e.g. nvim, code)
    Open { task: String },
    /// Spawn an independent reviewer agent on <builder>'s branch (maker/checker)
    Review {
        /// the agent whose work to review
        builder: String,
        /// agent CLI for the reviewer (default: $WTA_REVIEW_AGENT_CMD or $WTA_AGENT_CMD)
        #[arg(long)]
        by: Option<String>,
        /// model for the reviewer (claude --model), e.g. sonnet-5, haiku-4.5
        #[arg(long)]
        model: Option<String>,
        /// reasoning effort (claude --effort): low | medium | high | xhigh | max
        #[arg(long)]
        effort: Option<String>,
    },
    /// Scaffold `.wta/` convention stubs (verify.sh, setup.sh, teardown.sh)
    Init,
    /// Show the resolved agent command per role (model/effort from config + flags)
    Roles,
    /// Watch the fleet and escalate stuck / needs-input / crashed agents (read-only, no actions)
    Supervise {
        /// only this repo (default: watch every repo you have agents in)
        #[arg(long)]
        here: bool,
        /// seconds between checks
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// flag an agent that's been idle with no new changes this long (seconds) as stuck
        #[arg(long, default_value_t = 300)]
        stuck_secs: u64,
    },
    /// Migrate <from>'s context into a NEW agent: branch off it + seed a handoff note
    Handoff {
        /// the agent to hand off FROM (its committed work is carried over)
        from: String,
        /// the new agent's task name
        new: String,
        /// initial prompt for the new agent (everything after the names)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Re-prompt an agent with `.wta/verify.sh` output until it passes (or a guard trips)
    Loop {
        /// the agent to drive
        task: String,
        /// guard: give up after this many fix attempts
        #[arg(long, default_value_t = 6)]
        max: u32,
        /// guard: stop if the agent's diff is unchanged this many attempts running (0 = off)
        #[arg(long = "no-progress", default_value_t = 2)]
        no_progress: u32,
        /// guard: overall wall-clock budget in seconds (0 = no limit)
        #[arg(long, default_value_t = 0)]
        timeout: u64,
        /// optional kickoff prompt sent before the first verify (everything after the task)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Lock a command into a permanent regression check every future agent must pass
    Lock {
        /// name of the check (letters/digits/-/_); omit together with --list
        name: Option<String>,
        /// list the repo's locked checks instead of adding one
        #[arg(long)]
        list: bool,
        /// record which agent this check came from (in the check header)
        #[arg(long)]
        from: Option<String>,
        /// a note recorded in the check header
        #[arg(long)]
        note: Option<String>,
        /// the command that must pass (everything after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Remove a locked check added by `wta lock`
    Unlock { name: String },
    /// Scheduled agent dispatch — routines that fire `wta new` on a cadence (work while you sleep)
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
    /// Send a one-line note into another agent's pane (agents can call this too)
    Send {
        /// the agent to message
        task: String,
        /// the message (everything after the task name)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        message: Vec<String>,
    },
    /// Shared coordination board: `wta board` prints it, `wta board "<claim>"` appends
    Board {
        /// a claim to append (omit to print the board)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        entry: Vec<String>,
    },
    /// Stop an agent's session but KEEP its worktree, so it can be resumed later
    Stop { task: String },
    /// Resume a stopped agent — re-spawn its session in the existing worktree
    Resume { task: String },
    /// Commit + push the agent's branch; with --pr, also open a PR via gh
    Push {
        task: String,
        #[arg(long)]
        pr: bool,
    },
    /// Destroy an agent: kill the session AND remove its worktree and branch
    Rm {
        task: String,
        #[arg(long)]
        force: bool,
    },
    /// Emit agent status (called by Claude Code hooks): OSC user-var + ~/.wta/state file
    Status { state: String },
    /// Wire Claude Code hooks (UserPromptSubmit/Notification/Stop) to `wta status`
    InstallHooks {
        #[arg(long)]
        global: bool,
    },
    /// Live dashboard — all repos by default (a tree), or `--here` for the current repo
    Dash {
        /// only the current repo's agents (default is a global tree of every repo)
        #[arg(long)]
        here: bool,
    },
    /// Notify a Telegram chat when an agent needs input / finishes
    /// (set WTA_TELEGRAM_TOKEN + WTA_TELEGRAM_CHAT)
    #[cfg(feature = "telegram")]
    Bridge {
        /// Send one test message to verify config, then exit
        #[arg(long)]
        test: bool,
    },
}

#[derive(Subcommand)]
pub enum CronAction {
    /// Add a routine: fire `wta new` in a repo on a cadence with a prompt
    Add {
        /// routine name (letters/digits/-/_)
        name: String,
        /// how often to fire, e.g. 30m, 2h, 1d
        #[arg(long)]
        every: String,
        /// repo to run in (default: the current repo)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// prompt for each spawned agent (everything after the name)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// List routines and when each is next due
    List,
    /// Remove a routine
    Rm { name: String },
    /// Enable a disabled routine
    Enable { name: String },
    /// Disable a routine (keep it, but stop firing)
    Disable { name: String },
    /// Fire every due routine once, then exit (wire into system cron / launchd)
    Tick,
    /// Run the scheduler in the foreground until Ctrl-C (leave it running)
    Start {
        /// seconds between checks
        #[arg(long, default_value_t = 60)]
        interval: u64,
    },
}
