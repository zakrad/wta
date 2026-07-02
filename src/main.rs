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
    // `--server default` (or WTA_TMUX_SOCKET) picks the tmux server for this run.
    if let Some(server) = &cli.server {
        std::env::set_var("WTA_TMUX_SOCKET", server);
    }
    match cli.cmd {
        Command::New {
            task,
            base,
            agent_args,
        } => {
            match base {
                Some(b) => worktree::new_with_base(&task, &agent_args, &b)?,
                None => worktree::new(&task, &agent_args)?,
            }
            println!("started agent '{task}' — attach with `wta attach {task}` (or `wta dash`)");
        }
        Command::Ls => worktree::ls()?,
        Command::Matrix => worktree::matrix()?,
        Command::Attach { task } => worktree::attach(&task)?,
        Command::Stop { task } => {
            worktree::stop(&task)?;
            println!("stopped '{task}' — worktree kept; resume with `wta resume {task}`");
        }
        Command::Resume { task } => {
            worktree::resume(&task)?;
            println!("resumed '{task}' — attach with `wta attach {task}`");
        }
        Command::Push { task, pr } => {
            let summary = worktree::push(&task, pr)?;
            println!("{summary}");
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
