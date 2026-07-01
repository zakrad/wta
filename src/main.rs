mod cli;
mod dash;
mod status;
mod tmux;
mod worktree;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::New { task, agent_args } => worktree::new(&task, &agent_args)?,
        Command::Ls => worktree::ls()?,
        Command::Attach { task } => worktree::attach(&task)?,
        Command::Rm { task, force } => worktree::rm(&task, force)?,
        Command::Status { state } => status::emit(&state)?,
        Command::InstallHooks { global } => status::install_hooks(global)?,
        Command::Dash => dash::run()?,
    }
    Ok(())
}
