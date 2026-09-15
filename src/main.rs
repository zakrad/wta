#[cfg(feature = "telegram")]
mod bridge;
mod cli;
mod config;
mod copyview;
mod cost;
mod cron;
mod dash;
mod detect;
mod guard;
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
    // Seed backing env vars from ~/.wta/config.json where the user hasn't set them
    // (env wins over the file). Must run before any subcommand reads those vars.
    config::apply_env_defaults();
    // Remember which tmux the user is driving so an agent's hook can pop a
    // notification onto it later (no-op outside tmux / inside an agent).
    notify::record_user_tmux();
    // bare `wta` opens the (global) dashboard
    let cmd = cli.cmd.unwrap_or(Command::Dash { here: false });
    match cmd {
        Command::New {
            task,
            base,
            into,
            yolo,
            safe,
            model,
            effort,
            role,
            agent_args,
        } => {
            if safe {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "0");
            } else if yolo {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "1");
            }
            let cfg = config::load();
            let model = model.or_else(|| cfg.model.clone());
            let effort = effort.or_else(|| cfg.effort.clone());
            worktree::apply_role(role.as_deref().unwrap_or("worker"), model.as_deref(), effort.as_deref());
            // `--into <target>` is the shared-branch workflow: branch off <target>, which
            // is persisted as the base so diffs/PR/land all target it (same start point as
            // --base, named for the "several agents converge here" intent).
            match into.or(base) {
                Some(b) => worktree::new_with_base(&task, &agent_args, &b)?,
                None => worktree::new(&task, &agent_args)?,
            }
            println!("started agent '{task}' — attach with `wta attach {task}` (or `wta dash`)");
            if let Some(hint) = worktree::instructions_hint() {
                eprintln!("{hint}");
            }
        }
        Command::Adopt { task, dir } => worktree::adopt(&task, dir.as_deref())?,
        Command::Ls { json } => worktree::ls(json)?,
        Command::Cost { task, chart, usd, cumulative, json } => {
            worktree::show_cost(task.as_deref(), chart, json, usd, cumulative)?
        }
        Command::Matrix { json } => worktree::matrix(json)?,
        Command::Fanout {
            name,
            count,
            base,
            into,
            yolo,
            safe,
            model,
            effort,
            role,
            agent_args,
        } => {
            if safe {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "0");
            } else if yolo {
                std::env::set_var("WTA_SKIP_PERMISSIONS", "1");
            }
            let cfg = config::load();
            let model = model.or_else(|| cfg.model.clone());
            let effort = effort.or_else(|| cfg.effort.clone());
            worktree::apply_role(role.as_deref().unwrap_or("worker"), model.as_deref(), effort.as_deref());
            let tgt = into.or(base);
            worktree::fanout(&name, count, tgt.as_deref(), &agent_args)?
        }
        Command::Attach { task } => worktree::attach(&task)?,
        Command::Open { task } => worktree::open(&task)?,
        Command::Review { builder, by, model, effort } => {
            worktree::review(&builder, by.as_deref(), model.as_deref(), effort.as_deref())?
        }
        Command::Task { task, new, json } => worktree::task_cmd(task.as_deref(), new, json)?,
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
        Command::Send { task, json, message } => worktree::send(&task, &message.join(" "), json)?,
        Command::Board { entry } => {
            let joined = entry.join(" ");
            worktree::board(if joined.trim().is_empty() { None } else { Some(joined.as_str()) })?
        }
        Command::Doctor => worktree::doctor()?,
        Command::Guard { action } => {
            let root = worktree::repo_root()?;
            match action.unwrap_or(cli::GuardAction::Status) {
                cli::GuardAction::On => guard::on(&root)?,
                cli::GuardAction::Off => guard::off(&root)?,
                cli::GuardAction::Status => guard::status(&root)?,
                cli::GuardAction::Test { command } => guard::test(&command.join(" "))?,
            }
        }
        Command::GuardCheck => guard::run_check()?,
        Command::Detect { task } => worktree::detect(&task)?,
        Command::Wait { tasks, until, any, timeout, poll, quiet } => {
            worktree::wait(&tasks, &until, any, &timeout, &poll, quiet)?
        }
        Command::Stop { task } => {
            worktree::stop(&task)?;
            println!("stopped '{task}' — worktree kept; resume with `wta resume {task}`");
        }
        Command::Copy { task, session } => copyview::run_cli(task.as_deref(), session.as_deref())?,
        Command::Switch { client, session, dir } => worktree::switch_session(&client, &session, &dir)?,
        Command::Config { key, value } => config_cmd(key, value)?,
        Command::Resume { task, fresh } => {
            worktree::resume(&task, fresh)?;
            let how = if fresh { "fresh conversation" } else { "continued" };
            println!("resumed '{task}' ({how}) — attach with `wta attach {task}`");
        }
        Command::Push { task, pr } => {
            let summary = worktree::push(&task, pr)?;
            println!("{summary}");
        }
        Command::Land { task, rm } => {
            let summary = worktree::land(&task)?;
            println!("{summary}");
            if rm {
                worktree::rm(&task, false)?;
                println!("removed '{task}' (landed)");
            }
        }
        Command::Rm { task, force } => worktree::rm(&task, force)?,
        Command::Status { state } => status::emit(&state)?,
        Command::InstallHooks { global } => status::install_hooks(global)?,
        Command::Dash { here } => dash::run(here)?,
        #[cfg(feature = "telegram")]
        Command::Bridge { test } => bridge::run(test)?,
    }
    Ok(())
}

/// `wta config` — no args lists every setting with its value; `<key>` prints one;
/// `<key> <value>` sets it ("default"/"-" clears). Persists to ~/.wta/config.json.
fn config_cmd(key: Option<String>, value: Option<String>) -> anyhow::Result<()> {
    let mut cfg = config::load();
    match (key, value) {
        (None, _) => {
            let p = config::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.wta/config.json".into());
            println!("settings ({p}) — env vars override these:\n");
            for (k, help) in config::FIELDS {
                let v = cfg.get(k).unwrap_or_default();
                let shown = if v.is_empty() { "(default)".to_string() } else { v };
                println!("  {k:<10} {shown:<16} {help}");
            }
            println!("\nset one:   wta config <key> <value>      clear:  wta config <key> default");
        }
        (Some(k), None) => {
            let v = cfg
                .get(&k)
                .with_context(|| format!("unknown setting '{k}' (see `wta config`)"))?;
            println!("{}", if v.is_empty() { "(default)".into() } else { v });
        }
        (Some(k), Some(v)) => {
            cfg.set(&k, &v)?;
            config::save(&cfg)?;
            let now = cfg.get(&k).unwrap_or_default();
            println!(
                "set {k} = {}",
                if now.is_empty() { "(default)".into() } else { now }
            );
        }
    }
    Ok(())
}
