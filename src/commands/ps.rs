//! Process listing command (`gw ps`).
//!
//! Lists arc runs tracked by the daemon in a table or JSON format,
//! with color-coded status indicators.

use crate::client::socket::DaemonClient;
use crate::commands::util::short_id;
use crate::daemon::protocol::{Request, Response, RunInfo, RunStatus};
use crate::output::tags::tag;
use color_eyre::Result;
use console::Style;

/// Execute the `gw ps` command.
///
/// Lists runs from the daemon. Falls back gracefully when daemon is not running.
pub fn execute(all: bool, json_output: bool) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let client = DaemonClient::new()?;
    match client.send(Request::ListRuns { all })? {
        Response::RunList { runs } => {
            if json_output {
                print_json(&runs)?;
            } else {
                print_table(&runs);
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

/// Print runs as a formatted table with color-coded status.
fn print_table(runs: &[RunInfo]) {
    if runs.is_empty() {
        println!("No runs found.");
        println!("  Submit one with: gw run <plan>");
        return;
    }

    // Header
    println!(
        "{:<10} {:<10} {:<26} {:<12} {:<10} {}",
        "ID", "STATUS", "PLAN", "REPO", "PHASE", "UPTIME"
    );
    println!("{}", "-".repeat(78));

    for run in runs {
        let id = short_id(&run.run_id);
        let status_str = format_status(run.status);
        let plan = abbreviate_path(&run.plan_path.to_string_lossy(), 24);
        let repo = abbreviate_path(&run.repo_dir.to_string_lossy(), 10);
        let phase = run.current_phase.as_deref().unwrap_or("-");
        let uptime = format_uptime(run.uptime_secs);

        println!(
            "{:<10} {:<10} {:<26} {:<12} {:<10} {}",
            id, status_str, plan, repo, phase, uptime
        );
    }

    // Summary
    let running = runs.iter().filter(|r| r.status == RunStatus::Running).count();
    let queued = runs.iter().filter(|r| r.status == RunStatus::Queued).count();
    println!();
    println!("{} total, {} running, {} queued", runs.len(), running, queued);
}

/// Print runs as JSON for machine consumption.
fn print_json(runs: &[RunInfo]) -> Result<()> {
    let json = serde_json::to_string_pretty(runs)?;
    println!("{}", json);
    Ok(())
}

/// Format a RunStatus with console colors.
fn format_status(status: RunStatus) -> String {
    let (label, style) = match status {
        RunStatus::Running => ("running", Style::new().yellow().bold()),
        RunStatus::Queued => ("queued", Style::new().cyan()),
        RunStatus::Succeeded => ("succeeded", Style::new().green()),
        RunStatus::Failed => ("failed", Style::new().red().bold()),
        RunStatus::Stopped => ("stopped", Style::new().dim()),
    };
    style.apply_to(label).to_string()
}

/// Format seconds into a human-readable uptime string.
fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {:02}m", hours, mins)
    }
}

/// Abbreviate a path for table display, replacing home dir with ~.
fn abbreviate_path(path: &str, max_len: usize) -> String {
    let abbreviated = if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path.starts_with(home_str.as_ref()) {
            format!("~{}", &path[home_str.len()..])
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    if abbreviated.len() > max_len {
        format!("...{}", &abbreviated[abbreviated.len() - (max_len - 3)..])
    } else {
        abbreviated
    }
}
