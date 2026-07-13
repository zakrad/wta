#[cfg(feature = "telegram")]
mod bridge;
mod cli;
mod cron;
mod dash;
mod notify;
mod roles;
mod status;
mod supervise;
mod tmux;
mod worktree;

use anyhow::Context;
use clap::Parser;
use cli::{Cli, Command, CronAction};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // `--server default` (or WTA_TMUX_SOCKET) picks the tmux server for this run.
    if let Some(server) = &cli.server {
        std::env::set_var("WTA_TMUX_SOCKET", server);
    }
    // Remember which tmux the user is driving so an agent's hook can pop a
    // notification onto it later (no-op outside tmux / inside an agent).
    notify::record_user_tmux();
    // bare `wta` opens the (global) dashboard
    let cmd = cli.cmd.unwrap_or(Command::Dash { here: false });
    match cmd {
        Command::New {
            task,
            base,
            yolo,
            safe,
            model,
            effort,
            agent_args,
        } => {
            if safe {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "0");
            } else if yolo {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "1");
            }
            worktree::apply_worker_role(model.as_deref(), effort.as_deref());
            match base {
                Some(b) => worktree::new_with_base(&task, &agent_args, &b)?,
                None => worktree::new(&task, &agent_args)?,
            }
            println!("started agent '{task}' — attach with `wta attach {task}` (or `wta dash`)");
            if let Some(hint) = worktree::instructions_hint() {
                eprintln!("{hint}");
            }
        }
        Command::Ls => worktree::ls()?,
        Command::Matrix => worktree::matrix()?,
        Command::Fanout {
            name,
            count,
            base,
            yolo,
            safe,
            model,
            effort,
            agent_args,
        } => {
            if safe {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "0");
            } else if yolo {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "1");
            }
            worktree::apply_worker_role(model.as_deref(), effort.as_deref());
            worktree::fanout(&name, count, base.as_deref(), &agent_args)?
        }
        Command::Attach { task } => worktree::attach(&task)?,
        Command::Open { task } => worktree::open(&task)?,
        Command::Review { builder, by, model, effort } => {
            worktree::review(&builder, by.as_deref(), model.as_deref(), effort.as_deref())?
        }
        Command::Init => worktree::init()?,
        Command::Roles => roles::print_roles(worktree::repo_root().ok().as_deref()),
        Command::Supervise { here, interval, stuck_secs } => {
            supervise::supervise(!here, interval, stuck_secs)?
        }
        Command::Handoff { from, new, prompt } => worktree::handoff(&from, &new, &prompt)?,
        Command::Loop { task, max, no_progress, timeout, prompt } => {
            worktree::loop_verify(&task, max, no_progress, timeout, &prompt)?
        }
        Command::Lock { name, list, from, note, command } => {
            if list {
                worktree::list_locks()?;
            } else {
                let name = name.context("give a check name (or --list to list)")?;
                worktree::lock(&name, from.as_deref(), note.as_deref(), &command)?;
            }
        }
        Command::Unlock { name } => worktree::unlock(&name)?,
        Command::Cron { action } => match action {
            CronAction::Add { name, every, repo, prompt } => cron::add(&name, &every, repo, &prompt)?,
            CronAction::List => cron::list()?,
            CronAction::Rm { name } => cron::rm(&name)?,
            CronAction::Enable { name } => cron::set_enabled(&name, true)?,
            CronAction::Disable { name } => cron::set_enabled(&name, false)?,
            CronAction::Tick => {
                cron::tick()?;
            }
            CronAction::Start { interval } => cron::start(interval)?,
        },
        Command::Send { task, message } => worktree::send(&task, &message.join(" "))?,
        Command::Board { entry } => {
            let joined = entry.join(" ");
            worktree::board(if joined.trim().is_empty() { None } else { Some(joined.as_str()) })?
        }
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
        Command::Dash { here } => dash::run(here)?,
        #[cfg(feature = "telegram")]
        Command::Bridge { test } => bridge::run(test)?,
    }
    Ok(())
}
