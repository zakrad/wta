use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "wta",
    version,
    about = "worktree task agents — parallel AI agents in git worktrees + tmux sessions"
)]
pub struct Cli {
    /// tmux server: "wta" (default, isolated socket) or "default" (your own tmux)
    #[arg(long, global = true)]
    pub server: Option<String>,

    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a worktree + branch, copy local context, start the agent in a tmux session
    New {
        task: String,
        /// Base the agent's branch on an existing branch (default: HEAD)
        #[arg(long)]
        base: Option<String>,
        /// Run the agent with no permission prompts (claude --dangerously-skip-permissions)
        #[arg(long)]
        yolo: bool,
        /// Everything after `--` is passed to the agent command (default: claude)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// List wta-managed agents (worktrees) with diffstat vs the base branch
    Ls,
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
        /// run agents with no permission prompts (claude --dangerously-skip-permissions)
        #[arg(long)]
        yolo: bool,
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
    },
    /// Scaffold `.wta/` convention stubs (verify.sh, setup.sh, teardown.sh)
    Init,
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
    /// Live full-screen dashboard of all agents
    Dash,
    /// Notify a Telegram chat when an agent needs input / finishes
    /// (set WTA_TELEGRAM_TOKEN + WTA_TELEGRAM_CHAT)
    #[cfg(feature = "telegram")]
    Bridge {
        /// Send one test message to verify config, then exit
        #[arg(long)]
        test: bool,
    },
}
