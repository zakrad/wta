use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "wta", version, about = "wezterm task agents — one git worktree + one WezTerm tab per agent")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a worktree + branch, copy local context, launch the agent in a new WezTerm tab
    New {
        task: String,
        /// Everything after `--` is passed to the agent command (default: claude)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// List wta-managed agents (worktrees) with diffstat vs the base branch
    Ls,
    /// Attach to an agent's tmux session in the foreground (Ctrl-b d to detach)
    Attach { task: String },
    /// Kill the agent's pane(s), remove its worktree and branch
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
}
