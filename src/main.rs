//! Greater-Will: External Arc Controller for Rune
//!
//! Runs each Rune arc phase group in a fresh Claude Code session.
//!
//! # Commands
//!
//! - `run`: Execute arc phases for plan files
//! - `status`: Show status of active/recent runs
//! - `replay`: Resume from a checkpoint
//! - `clean`: Clean up temporary files and sessions

mod batch;
mod checkpoint;
mod cleanup;
mod commands;
mod config;
mod engine;
mod github;
mod log;
mod monitor;
mod output;
mod session;

mod client;
mod daemon;

use clap::{CommandFactory, Parser};
use color_eyre::Result;

use crate::config::cli_args::Cli;

fn main() -> Result<()> {
    // 1. Install panic hooks FIRST (before any other initialization)
    // This ensures beautiful error traces for panics during startup
    color_eyre::install()?;

    // 2. Parse CLI args (may exit on --help/--version)
    let cli = Cli::parse();

    // 3. Initialize tracing with verbosity level
    crate::log::init(cli.verbose);

    // 4. Dispatch to subcommand
    match cli.command {
        commands::Commands::Run {
            plans,
            dry_run,
            mock,
            group,
            config_dir,
            resume,
            multi_group,
            allow_dirty,
            foreground,
        } => commands::run::execute(plans, dry_run, mock, group, config_dir, resume, multi_group, allow_dirty, foreground, cli.verbose),
        commands::Commands::Elden { install, uninstall, status, event } => {
            if install {
                commands::elden::install()
            } else if uninstall {
                commands::elden::uninstall()
            } else if status {
                commands::elden::hook_status()
            } else if let Some(event_name) = event {
                commands::elden::execute_event(&event_name)
            } else {
                commands::elden::execute()
            }
        }
        commands::Commands::Status => commands::status::execute(),
        commands::Commands::Replay { checkpoint, resume, force } => {
            commands::replay::execute(checkpoint, resume, force)
        }
        commands::Commands::Daemon { action } => commands::daemon::execute(action),
        commands::Commands::Ps { all, json, running, failed } => commands::ps::execute(all, json, running, failed),
        commands::Commands::Logs { run_id, follow, tail, pane } => commands::logs::execute(run_id, follow, tail, pane),
        commands::Commands::Stop { run_id, force, detach } => commands::stop::execute(run_id, force, detach),
        commands::Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "gw", &mut std::io::stdout());
            Ok(())
        }
        commands::Commands::Clean => commands::clean::execute(),
        commands::Commands::History {
            limit,
            status,
            repo,
            detail,
            logs,
            pane,
            tail,
            json,
        } => commands::history::execute(limit, status, repo, detail, logs, pane, tail, json),
    }
}
