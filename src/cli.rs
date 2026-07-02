use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "wta",
    version,
    about = "worktree task agents — parallel AI agents in git worktrees + tmux sessions"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a worktree + branch, copy local context, start the agent in a tmux session
    New {
        task: String,
        /// Everything after `--` is passed to the agent command (default: claude)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// List wta-managed agents (worktrees) with diffstat vs the base branch
    Ls,
    /// Attach to an agent's tmux session in the foreground (Ctrl-q to detach)
    Attach { task: String },
    /// Stop an agent's session but KEEP its worktree, so it can be resumed later
    Stop { task: String },
    /// Resume a stopped agent — re-spawn its session in the existing worktree
    Resume { task: String },
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
    Bridge {
        /// Send one test message to verify config, then exit
        #[arg(long)]
        test: bool,
    },
}
