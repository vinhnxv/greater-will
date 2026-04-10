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
/// Supports `--running` and `--failed` filters to narrow output.
pub fn execute(all: bool, json_output: bool, running: bool, failed: bool) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let client = DaemonClient::new()?;
    match client.send(Request::ListRuns { all })? {
        Response::RunList { runs } => {
            let filtered = filter_runs(runs, running, failed);
            if filtered.is_empty() && (running || failed) {
                let status = match (running, failed) {
                    (true, true) => "running or failed",
                    (true, false) => "running",
                    (false, true) => "failed",
                    _ => unreachable!(),
                };
                println!("No {} runs found. Run `gw ps --all` to see all.", status);
                return Ok(());
            }
            if json_output {
                print_json(&filtered)?;
            } else {
                print_table(&filtered);
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

/// Filter runs by status flags.
///
/// When both `running` and `failed` are set, returns the union (Running OR Failed).
/// When neither is set, returns all runs unfiltered.
fn filter_runs(runs: Vec<RunInfo>, running: bool, failed: bool) -> Vec<RunInfo> {
    if !running && !failed {
        return runs;
    }
    runs.into_iter()
        .filter(|r| {
            (running && r.status == RunStatus::Running)
                || (failed && r.status == RunStatus::Failed)
        })
        .collect()
}

/// Prepared row data for table rendering (plain text, no ANSI codes for width calc).
struct RowData {
    id: String,
    tmux: String,
    status_plain: String,
    status_styled: String,
    plan: String,
    repo: String,
    phase: String,
    uptime: String,
}

/// Print runs as a formatted table with box-drawing borders and color-coded status.
///
/// Adapts to terminal width: truncates PLAN and REPO columns with "…" when needed,
/// and drops the REPO column entirely if the terminal is narrower than 60 columns.
fn print_table(runs: &[RunInfo]) {
    if runs.is_empty() {
        println!("No runs found.");
        println!("  Submit one with: gw run <plan>");
        return;
    }

    // Detect terminal width, default to 120 if detection fails.
    let term_width = console::Term::stdout()
        .size()
        .1 as usize;
    let term_width = if term_width == 0 { 120 } else { term_width };
    let narrow = term_width < 60;

    // Pre-compute row data
    let rows: Vec<RowData> = runs
        .iter()
        .map(|run| {
            let status_plain = format_status_plain(run.status);
            let status_styled = format_status(run.status);
            RowData {
                id: short_id(&run.run_id).to_string(),
                tmux: run.session_name.clone(),
                status_plain,
                status_styled,
                plan: abbreviate_home(&run.plan_path.to_string_lossy()),
                repo: abbreviate_home(&run.repo_dir.to_string_lossy()),
                phase: run.current_phase.as_deref().unwrap_or("-").to_string(),
                uptime: format_uptime(run.uptime_secs),
            }
        })
        .collect();

    // Fixed-width columns: ID, TMUX, STATUS, PHASE, UPTIME (plus border chars).
    // Each column has 3 chars overhead: space + content + space, plus 1 for the │.
    let id_w = rows.iter().map(|r| r.id.len()).max().unwrap_or(0).max(2);
    let tmux_w = rows.iter().map(|r| r.tmux.len()).max().unwrap_or(0).max(4);
    let status_w = rows.iter().map(|r| r.status_plain.len()).max().unwrap_or(0).max(6);
    let phase_w = rows.iter().map(|r| r.phase.len()).max().unwrap_or(0).max(5);
    let uptime_w = rows.iter().map(|r| r.uptime.len()).max().unwrap_or(0).max(6);

    let num_cols = if narrow { 6 } else { 7 };
    let chrome = (num_cols + 1) + num_cols * 2; // borders + padding
    let fixed = id_w + tmux_w + status_w + phase_w + uptime_w + chrome;
    let flexible = term_width.saturating_sub(fixed);

    let (plan_budget, repo_budget) = if narrow {
        (flexible, 0)
    } else {
        // Split flexible space between PLAN and REPO (60/40 split)
        let plan_b = flexible * 3 / 5;
        let repo_b = flexible.saturating_sub(plan_b);
        (plan_b.max(4), repo_b.max(4))
    };

    // Apply truncation to rows
    let rows: Vec<RowData> = rows
        .into_iter()
        .map(|mut r| {
            r.plan = truncate_with_ellipsis(&r.plan, plan_budget);
            if !narrow {
                r.repo = truncate_with_ellipsis(&r.repo, repo_budget);
            }
            r
        })
        .collect();

    let headers: &[&str] = if narrow {
        &["ID", "TMUX", "STATUS", "PLAN", "PHASE", "UPTIME"]
    } else {
        &["ID", "TMUX", "STATUS", "PLAN", "REPO", "PHASE", "UPTIME"]
    };

    // Calculate column widths from (possibly truncated) content
    let widths: Vec<usize> = if narrow {
        vec![
            rows.iter().map(|r| r.id.len()).max().unwrap_or(0).max(headers[0].len()),
            rows.iter().map(|r| r.tmux.len()).max().unwrap_or(0).max(headers[1].len()),
            rows.iter().map(|r| r.status_plain.len()).max().unwrap_or(0).max(headers[2].len()),
            rows.iter().map(|r| r.plan.len()).max().unwrap_or(0).max(headers[3].len()),
            rows.iter().map(|r| r.phase.len()).max().unwrap_or(0).max(headers[4].len()),
            rows.iter().map(|r| r.uptime.len()).max().unwrap_or(0).max(headers[5].len()),
        ]
    } else {
        vec![
            rows.iter().map(|r| r.id.len()).max().unwrap_or(0).max(headers[0].len()),
            rows.iter().map(|r| r.tmux.len()).max().unwrap_or(0).max(headers[1].len()),
            rows.iter().map(|r| r.status_plain.len()).max().unwrap_or(0).max(headers[2].len()),
            rows.iter().map(|r| r.plan.len()).max().unwrap_or(0).max(headers[3].len()),
            rows.iter().map(|r| r.repo.len()).max().unwrap_or(0).max(headers[4].len()),
            rows.iter().map(|r| r.phase.len()).max().unwrap_or(0).max(headers[5].len()),
            rows.iter().map(|r| r.uptime.len()).max().unwrap_or(0).max(headers[6].len()),
        ]
    };

    let dim = Style::new().dim();

    // Top border: ┌──┬──┬──┐
    print_border_dynamic(&widths, '┌', '┬', '┐', &dim);

    // Header row
    print!("{}", dim.apply_to("│"));
    let header_style = Style::new().bold();
    for (i, h) in headers.iter().enumerate() {
        print!(
            " {:<width$} {}",
            header_style.apply_to(h),
            dim.apply_to("│"),
            width = widths[i]
        );
    }
    println!();

    // Header separator: ├──┼──┼──┤
    print_border_dynamic(&widths, '├', '┼', '┤', &dim);

    // Data rows
    for row in &rows {
        let (cells, plain_lens): (Vec<&String>, Vec<usize>) = if narrow {
            (
                vec![&row.id, &row.tmux, &row.status_styled, &row.plan, &row.phase, &row.uptime],
                vec![row.id.len(), row.tmux.len(), row.status_plain.len(), row.plan.len(), row.phase.len(), row.uptime.len()],
            )
        } else {
            (
                vec![&row.id, &row.tmux, &row.status_styled, &row.plan, &row.repo, &row.phase, &row.uptime],
                vec![row.id.len(), row.tmux.len(), row.status_plain.len(), row.plan.len(), row.repo.len(), row.phase.len(), row.uptime.len()],
            )
        };

        print!("{}", dim.apply_to("│"));
        for (i, cell) in cells.iter().enumerate() {
            let padding = widths[i].saturating_sub(plain_lens[i]);
            print!(
                " {}{} {}",
                cell,
                " ".repeat(padding),
                dim.apply_to("│")
            );
        }
        println!();
    }

    // Bottom border: └──┴──┴──┘
    print_border_dynamic(&widths, '└', '┴', '┘', &dim);

    // Summary
    let running = runs.iter().filter(|r| r.status == RunStatus::Running).count();
    let queued = runs.iter().filter(|r| r.status == RunStatus::Queued).count();
    println!();
    println!(
        "{} total, {} running, {} queued",
        runs.len(),
        running,
        queued
    );
}

/// Truncate a string to fit within `budget` characters, appending "…" if truncated.
fn truncate_with_ellipsis(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        s.to_string()
    } else if budget <= 1 {
        "\u{2026}".to_string()
    } else {
        let mut result = String::with_capacity(budget);
        for ch in s.chars() {
            if result.len() + 1 >= budget {
                result.push('\u{2026}');
                break;
            }
            result.push(ch);
        }
        result
    }
}

/// Print a box-drawing border line: e.g. ┌──────┬──────┐
fn print_border_dynamic(widths: &[usize], left: char, mid: char, right: char, style: &Style) {
    let segments: Vec<String> = widths
        .iter()
        .map(|&w| "─".repeat(w + 2)) // +2 for padding spaces
        .collect();
    println!(
        "{}",
        style.apply_to(format!(
            "{}{}{}",
            left,
            segments.join(&mid.to_string()),
            right
        ))
    );
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

/// Plain-text status label (no ANSI codes) for width calculation.
fn format_status_plain(status: RunStatus) -> String {
    match status {
        RunStatus::Running => "running",
        RunStatus::Queued => "queued",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Stopped => "stopped",
    }
    .to_string()
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

/// Replace home directory prefix with ~ for shorter display.
fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path.starts_with(home_str.as_ref()) {
            return format!("~{}", &path[home_str.len()..]);
        }
    }
    path.to_string()
}
