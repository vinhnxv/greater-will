//! Schedule management command (`gw schedule`).
//!
//! Manages scheduled plan executions via the daemon. Supports adding,
//! listing, removing, pausing, and resuming schedules.

use crate::client::socket::DaemonClient;
use crate::config::cli_args::ScheduleAction;
use crate::daemon::protocol::{Request, Response, ScheduleInfo, ScheduleKindInfo};
use crate::daemon::schedule::{
    normalize_cron_expression, parse_duration_suffix, ScheduleKind, ScheduleStatus,
};
use crate::output::tags::tag;
use chrono::{DateTime, Utc};
use color_eyre::{eyre::WrapErr, Result};
use console::Style;
use std::path::PathBuf;

/// Execute the `gw schedule` command.
pub fn execute(action: ScheduleAction) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        color_eyre::eyre::bail!("daemon not running");
    }

    match action {
        ScheduleAction::Add {
            plan,
            cron,
            at,
            after,
            label,
            config_dir,
        } => execute_add(plan, cron, at, after, label, config_dir),
        ScheduleAction::List { json } => execute_list(json),
        ScheduleAction::Remove { id } => execute_simple(Request::RemoveSchedule { id }),
        ScheduleAction::Pause { id } => execute_simple(Request::PauseSchedule { id }),
        ScheduleAction::Resume { id } => execute_simple(Request::ResumeSchedule { id }),
    }
}

/// Add a new schedule.
fn execute_add(
    plan: String,
    cron: Option<String>,
    at: Option<String>,
    after: Option<String>,
    label: Option<String>,
    config_dir: Option<PathBuf>,
) -> Result<()> {
    // Resolve plan path to absolute
    let plan_path = std::fs::canonicalize(&plan)
        .wrap_err_with(|| format!("plan file not found: {plan}"))?;

    // Detect repo dir (current working directory)
    let repo_dir = std::env::current_dir().wrap_err("failed to get current directory")?;

    // Determine schedule kind from flags
    let kind = if let Some(expr) = cron {
        let normalized = normalize_cron_expression(&expr)?;
        ScheduleKind::Cron {
            expression: normalized,
        }
    } else if let Some(time_str) = at {
        let at_time: DateTime<Utc> = time_str
            .parse()
            .wrap_err_with(|| format!("invalid ISO 8601 time: {time_str}"))?;
        ScheduleKind::OneShot { at: at_time }
    } else if let Some(delay_str) = after {
        let secs = parse_duration_suffix(&delay_str)?;
        ScheduleKind::Delayed { delay_secs: secs }
    } else {
        return Err(color_eyre::eyre::eyre!(
            "must specify one of --cron, --at, or --after"
        ));
    };

    let client = DaemonClient::new()?;
    let request = Request::AddSchedule {
        plan_path,
        repo_dir,
        kind,
        config_dir,
        verbose: 0,
        label,
    };

    match client.send(request)? {
        Response::ScheduleAdded { id, next_fire } => {
            println!("{} Schedule created: {}", tag("OK"), id);
            if let Some(nf) = next_fire {
                println!("  Next fire: {nf}");
            }
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

/// List all schedules.
fn execute_list(json_output: bool) -> Result<()> {
    let client = DaemonClient::new()?;
    match client.send(Request::ListSchedules)? {
        Response::ScheduleList { schedules } => {
            if json_output {
                let json = serde_json::to_string_pretty(&schedules)?;
                println!("{json}");
            } else {
                print_schedule_table(&schedules);
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

/// Send a simple request and print the result (remove/pause/resume).
fn execute_simple(request: Request) -> Result<()> {
    let client = DaemonClient::new()?;
    match client.send(request)? {
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

/// Print schedules as a formatted table.
fn print_schedule_table(schedules: &[ScheduleInfo]) {
    if schedules.is_empty() {
        println!("No schedules found.");
        println!("  Create one with: gw schedule add <plan> --cron '*/5 * * * *'");
        return;
    }

    let dim = Style::new().dim();
    let bold = Style::new().bold();

    println!(
        "{}{:>8}  {:<10}  {:<12}  {:<20}  {:<5}  {}{}",
        dim.apply_to("│ "),
        bold.apply_to("ID"),
        bold.apply_to("STATUS"),
        bold.apply_to("KIND"),
        bold.apply_to("NEXT FIRE"),
        bold.apply_to("RUNS"),
        bold.apply_to("PLAN"),
        dim.apply_to(" │"),
    );

    for s in schedules {
        let status_str = format_schedule_status(&s.status);
        let kind_str = format_kind_short(&s.kind);
        let next = s.next_fire.as_deref().unwrap_or("-");
        let label_suffix = s
            .label
            .as_ref()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default();

        println!(
            "  {:>8}  {:<10}  {:<12}  {:<20}  {:<5}  {}{}",
            s.id,
            status_str,
            kind_str,
            next,
            s.fire_count,
            s.plan_path.display(),
            label_suffix,
        );
    }

    let active = schedules
        .iter()
        .filter(|s| s.status == ScheduleStatus::Active)
        .count();
    println!();
    println!("{} total, {} active", schedules.len(), active);
}

/// Format schedule status with color.
fn format_schedule_status(status: &ScheduleStatus) -> String {
    let (label, style) = match status {
        ScheduleStatus::Active => ("active", Style::new().green()),
        ScheduleStatus::Paused => ("paused", Style::new().yellow()),
        ScheduleStatus::Completed => ("completed", Style::new().dim()),
        ScheduleStatus::Failed => ("failed", Style::new().red().bold()),
        ScheduleStatus::Rejected => ("rejected", Style::new().red()),
    };
    style.apply_to(label).to_string()
}

/// Short kind description for table display.
fn format_kind_short(kind: &ScheduleKindInfo) -> String {
    match kind {
        ScheduleKindInfo::Cron { expression } => format!("cron:{}", &expression[..expression.len().min(15)]),
        ScheduleKindInfo::OneShot { .. } => "one-shot".into(),
        ScheduleKindInfo::Delayed { .. } => "delayed".into(),
    }
}
