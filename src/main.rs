#[cfg(feature = "telegram")]
mod bridge;
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
        Command::New { task, agent_args } => {
            worktree::new(&task, &agent_args)?;
            println!("started agent '{task}' — attach with `wta attach {task}` (or `wta dash`)");
        }
        Command::Ls => worktree::ls()?,
        Command::Attach { task } => worktree::attach(&task)?,
        Command::Stop { task } => {
            worktree::stop(&task)?;
            println!("stopped '{task}' — worktree kept; resume with `wta resume {task}`");
        }
        Command::Resume { task } => {
            worktree::resume(&task)?;
            println!("resumed '{task}' — attach with `wta attach {task}`");
        }
        Command::Rm { task, force } => {
            worktree::rm(&task, force)?;
            println!("removed '{task}' (session, worktree and branch)");
        }
        Command::Status { state } => status::emit(&state)?,
        Command::InstallHooks { global } => status::install_hooks(global)?,
        Command::Dash => dash::run()?,
        #[cfg(feature = "telegram")]
        Command::Bridge { test } => bridge::run(test)?,
    }
    Ok(())
}
