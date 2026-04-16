//! Queue management command (`gw queue`).
//!
//! Manages the pending run queue via the daemon. Supports listing,
//! removing, and clearing queued plans.

use crate::client::socket::DaemonClient;
use crate::config::cli_args::QueueAction;
use crate::daemon::protocol::{QueuedRunInfo, Request, Response};
use crate::output::tags::tag;
use color_eyre::Result;
use console::Style;
use std::path::PathBuf;

/// Execute the `gw queue` command.
pub fn execute(action: QueueAction) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        color_eyre::eyre::bail!("daemon not running");
    }

    match action {
        QueueAction::List { repo, json } => execute_list(repo, json),
        QueueAction::Remove { id } => execute_remove(id),
        QueueAction::Clear { all } => execute_clear(all),
    }
}

/// List queued runs.
fn execute_list(repo: Option<PathBuf>, json_output: bool) -> Result<()> {
    let client = DaemonClient::new()?;
    match client.send(Request::ListQueue)? {
        Response::QueueList { mut entries } => {
            // Client-side repo filter
            if let Some(ref repo_dir) = repo {
                let canonical = repo_dir.canonicalize().unwrap_or_else(|_| repo_dir.clone());
                entries.retain(|e| e.repo_dir == canonical);
            }

            if json_output {
                let json = serde_json::to_string_pretty(&entries)?;
                println!("{json}");
            } else {
                print_queue_table(&entries);
            }
        }
        Response::Error { message, .. } => {
            println!("{} {}", tag("FAIL"), message);
        }
        _ => {
            println!("{} Unexpected response from daemon", tag("WARN"));
        }
    }
    Ok(())
}

/// Remove a queued run by ID.
fn execute_remove(id: String) -> Result<()> {
    let client = DaemonClient::new()?;
    match client.send(Request::RemoveQueued { run_id: id })? {
        Response::Ok { message } => {
            println!("{} {}", tag("OK"), message);
        }
        Response::Error { code, message } => {
            println!("{} {}", tag("FAIL"), message);
            let suggestion = code.suggestion(&message);
            if !suggestion.is_empty() {
                println!("  {suggestion}");
            }
        }
        _ => {
            println!("{} Unexpected response from daemon", tag("WARN"));
        }
    }
    Ok(())
}

/// Clear queued runs.
fn execute_clear(all: bool) -> Result<()> {
    let repo_dir = if all {
        None
    } else {
        Some(std::env::current_dir()?)
    };

    let client = DaemonClient::new()?;
    match client.send(Request::ClearQueue { repo_dir })? {
        Response::Ok { message } => {
            println!("{} {}", tag("OK"), message);
        }
        Response::Error { code, message } => {
            println!("{} {}", tag("FAIL"), message);
            let suggestion = code.suggestion(&message);
            if !suggestion.is_empty() {
                println!("  {suggestion}");
            }
        }
        _ => {
            println!("{} Unexpected response from daemon", tag("WARN"));
        }
    }
    Ok(())
}

/// Print queued runs as a formatted table.
fn print_queue_table(entries: &[QueuedRunInfo]) {
    use crate::commands::util::short_id;

    if entries.is_empty() {
        println!("No queued plans.");
        println!("  Submit one with: gw run <plan>");
        return;
    }

    let dim = Style::new().dim();
    let bold = Style::new().bold();

    println!(
        "{}{:>3}  {:>8}  {:<20}  {:<20}  {}{}",
        dim.apply_to("| "),
        bold.apply_to("#"),
        bold.apply_to("ID"),
        bold.apply_to("QUEUED AT"),
        bold.apply_to("REPO"),
        bold.apply_to("PLAN"),
        dim.apply_to(" |"),
    );

    for entry in entries {
        let repo_short = entry
            .repo_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| entry.repo_dir.display().to_string());

        println!(
            "  {:>3}  {:>8}  {:<20}  {:<20}  {}",
            entry.position,
            short_id(&entry.run_id),
            &entry.queued_at[..entry.queued_at.len().min(19)],
            repo_short,
            entry.plan_path.display(),
        );
    }

    println!();
    println!("{} queued", entries.len());
}
