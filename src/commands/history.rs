//! History command implementation (`gw history`).
//!
//! Browses past arc run sessions by reading `~/.gw/runs/*/meta.json`
//! directly from disk. Works without the daemon running.

use crate::checkpoint::reader::read_checkpoint;
use crate::commands::util::short_id;
use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::RunEntry;
use crate::daemon::state::gw_home;
use crate::output::tags::tag;
use chrono::{DateTime, Local, Utc};
use color_eyre::{Result, eyre::eyre};
use console::Style;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Execute the `gw history` command.
pub fn execute(
    limit: usize,
    status: Option<String>,
    repo: Option<String>,
    detail: Option<String>,
    logs: Option<String>,
    pane: bool,
    tail: Option<usize>,
    json_output: bool,
) -> Result<()> {
    // Route to the appropriate mode
    if let Some(ref run_id) = logs {
        return show_logs(run_id, pane, tail);
    }
    if pane {
        println!("{} --pane requires --logs <RUN_ID>. Example: gw history --logs abc123 --pane", tag("WARN"));
        return Ok(());
    }
    if let Some(ref run_id) = detail {
        return show_detail(run_id);
    }
    list_runs(limit, status, repo, json_output)
}

// ── Mode A: List runs ──────────────────────────────────────────────

fn list_runs(
    limit: usize,
    status_filter: Option<String>,
    repo_filter: Option<String>,
    json_output: bool,
) -> Result<()> {
    let mut runs = load_all_runs()?;

    if runs.is_empty() {
        println!("No run history found.");
        println!("  Runs are stored in: {}", gw_home().join("runs").display());
        println!("  Submit a run with: gw run <plan>");
        return Ok(());
    }

    // Apply status filter
    if let Some(ref status_str) = status_filter {
        let target = parse_status_filter(status_str)?;
        runs.retain(|r| r.status == target);
    }

    // Apply repo filter (case-insensitive substring)
    if let Some(ref repo_str) = repo_filter {
        let lower = repo_str.to_lowercase();
        runs.retain(|r| r.repo_dir.to_string_lossy().to_lowercase().contains(&lower));
    }

    // Apply limit
    runs.truncate(limit);

    if runs.is_empty() {
        println!("No runs match the given filters.");
        return Ok(());
    }

    if json_output {
        print_json(&runs)?;
    } else {
        print_table(&runs);
    }

    Ok(())
}

fn parse_status_filter(s: &str) -> Result<RunStatus> {
    match s.to_lowercase().as_str() {
        "succeeded" | "success" | "ok" => Ok(RunStatus::Succeeded),
        "failed" | "fail" | "error" => Ok(RunStatus::Failed),
        "stopped" | "stop" => Ok(RunStatus::Stopped),
        "running" | "run" => Ok(RunStatus::Running),
        "queued" | "queue" => Ok(RunStatus::Queued),
        _ => Err(eyre!(
            "Unknown status '{}'. Use: succeeded, failed, stopped, running, queued",
            s
        )),
    }
}

fn print_table(runs: &[RunEntry]) {
    let dim = Style::new().dim();
    let bold = Style::new().bold();

    // Detect terminal width
    let term_width = console::Term::stdout().size().1 as usize;
    let term_width = if term_width == 0 { 120 } else { term_width };

    // Prepare row data
    struct Row {
        id: String,
        status_plain: String,
        status_styled: String,
        plan: String,
        duration: String,
        started: String,
        crashes: String,
        error: String,
    }

    let rows: Vec<Row> = runs
        .iter()
        .map(|run| {
            let duration = format_duration(run);
            let started = format_started(&run.started_at);
            let crashes_plain = run.crash_restarts.to_string();
            let error = run
                .error_message
                .as_deref()
                .map(|e| truncate(e, 40))
                .unwrap_or_else(|| "-".to_string());
            let plan = abbreviate_path(&run.plan_path.to_string_lossy());

            Row {
                id: short_id(&run.run_id).to_string(),
                status_plain: format_status_plain(run.status),
                status_styled: format_status(run.status),
                plan,
                duration,
                started,
                crashes: crashes_plain,
                error,
            }
        })
        .collect();

    // Column headers and widths
    let headers = ["ID", "STATUS", "PLAN", "DURATION", "STARTED", "CRASHES", "ERROR"];
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();

    for row in &rows {
        widths[0] = widths[0].max(row.id.len());
        widths[1] = widths[1].max(row.status_plain.len());
        widths[2] = widths[2].max(row.plan.len());
        widths[3] = widths[3].max(row.duration.len());
        widths[4] = widths[4].max(row.started.len());
        widths[5] = widths[5].max(row.crashes.len());
        widths[6] = widths[6].max(row.error.len());
    }

    // Budget for plan and error columns based on terminal width
    let fixed_cols_width: usize = widths[0] + widths[1] + widths[3] + widths[4] + widths[5];
    let chrome = (headers.len() + 1) + headers.len() * 2; // borders + padding
    let flexible = term_width.saturating_sub(fixed_cols_width + chrome);
    let plan_budget = (flexible * 3 / 5).max(8);
    let error_budget = flexible.saturating_sub(plan_budget).max(4);

    widths[2] = widths[2].min(plan_budget);
    widths[6] = widths[6].min(error_budget);

    // Print table
    print_border(&widths, '┌', '┬', '┐', &dim);

    // Header
    print!("{}", dim.apply_to("│"));
    for (i, h) in headers.iter().enumerate() {
        print!(
            " {:<width$} {}",
            bold.apply_to(h),
            dim.apply_to("│"),
            width = widths[i]
        );
    }
    println!();

    print_border(&widths, '├', '┼', '┤', &dim);

    // Rebuild styled crashes per row for display
    for (ri, row) in rows.iter().enumerate() {
        let run = &runs[ri];
        let crashes_styled = if run.crash_restarts == 0 {
            dim.apply_to("0").to_string()
        } else {
            Style::new()
                .yellow()
                .apply_to(run.crash_restarts.to_string())
                .to_string()
        };

        let plan_display = truncate(&row.plan, widths[2]);
        let error_display = truncate(&row.error, widths[6]);

        let cells = [
            (&row.id, row.id.len()),
            (&row.status_styled, row.status_plain.len()),
            (&plan_display, visible_len(&plan_display)),
            (&row.duration, row.duration.len()),
            (&row.started, row.started.len()),
            (&crashes_styled, row.crashes.len()),
            (&error_display, visible_len(&error_display)),
        ];

        print!("{}", dim.apply_to("│"));
        for (i, (text, plain_len)) in cells.iter().enumerate() {
            let padding = widths[i].saturating_sub(*plain_len);
            print!(" {}{} {}", text, " ".repeat(padding), dim.apply_to("│"));
        }
        println!();
    }

    print_border(&widths, '└', '┴', '┘', &dim);

    // Summary
    let succeeded = runs.iter().filter(|r| r.status == RunStatus::Succeeded).count();
    let failed = runs.iter().filter(|r| r.status == RunStatus::Failed).count();
    let stopped = runs.iter().filter(|r| r.status == RunStatus::Stopped).count();
    println!();
    println!(
        "{} total, {} succeeded, {} failed, {} stopped",
        runs.len(),
        succeeded,
        failed,
        stopped
    );
}

fn print_border(widths: &[usize], left: char, mid: char, right: char, style: &Style) {
    let segments: Vec<String> = widths.iter().map(|&w| "─".repeat(w + 2)).collect();
    println!(
        "{}",
        style.apply_to(format!("{}{}{}", left, segments.join(&mid.to_string()), right))
    );
}

fn print_json(runs: &[RunEntry]) -> Result<()> {
    let json = serde_json::to_string_pretty(runs)?;
    println!("{}", json);
    Ok(())
}

// ── Mode B: Detail view ────────────────────────────────────────────

fn show_detail(run_id_prefix: &str) -> Result<()> {
    let runs = load_all_runs()?;
    let run = find_by_prefix(&runs, run_id_prefix)?;

    let bold = Style::new().bold();
    let dim = Style::new().dim();

    println!("{}", bold.apply_to("─── Run Detail ───────────────────────────────────────"));
    println!();
    print_field("Run ID", &run.run_id, &bold);
    print_field("Status", &format_status(run.status), &bold);
    print_field("Plan", &run.plan_path.to_string_lossy(), &bold);
    print_field("Repository", &run.repo_dir.to_string_lossy(), &bold);
    print_field("Session", &run.session_name, &bold);
    if let Some(ref tmux) = run.tmux_session {
        print_field("Tmux Session", tmux, &bold);
    }
    print_field("Started", &format_datetime(&run.started_at), &bold);
    if let Some(ref finished) = run.finished_at {
        print_field("Finished", &format_datetime(finished), &bold);
    }
    print_field("Duration", &format_duration(run), &bold);
    print_field("Crash Restarts", &run.crash_restarts.to_string(), &bold);
    if let Some(ref phase) = run.current_phase {
        print_field("Last Phase", phase, &bold);
    }
    if let Some(ref err) = run.error_message {
        println!();
        println!("  {} {}", Style::new().red().bold().apply_to("Error:"), err);
    }
    if let Some(ref config) = run.config_dir {
        print_field("Config Dir", &config.to_string_lossy(), &bold);
    }

    // Check for log files
    let run_dir = gw_home().join("runs").join(&run.run_id).join("logs");
    let has_events = run_dir.join("events.jsonl").exists();
    let has_pane = run_dir.join("pane.log").exists();
    if has_events || has_pane {
        println!();
        println!("  {} Available logs:", dim.apply_to("📋"));
        if has_events {
            println!(
                "    gw history --logs {}    {}",
                short_id(&run.run_id),
                dim.apply_to("(structured events)")
            );
        }
        if has_pane {
            println!(
                "    gw history --logs {} --pane    {}",
                short_id(&run.run_id),
                dim.apply_to("(raw pane capture)")
            );
        }
    }

    // Try to find and display checkpoint info
    show_checkpoint_info(run, &dim)?;

    println!();
    println!("{}", dim.apply_to("─────────────────────────────────────────────────────"));

    Ok(())
}

fn show_checkpoint_info(run: &RunEntry, dim: &Style) -> Result<()> {
    // Scan .rune/arc/ in the run's repo_dir for checkpoint files
    let arc_dir = run.repo_dir.join(".rune").join("arc");
    if !arc_dir.exists() {
        return Ok(());
    }

    // Also check archived checkpoints
    let dirs_to_scan: Vec<std::path::PathBuf> = {
        let mut dirs = Vec::new();
        if let Ok(entries) = fs::read_dir(&arc_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let cp = path.join("checkpoint.json");
                    if cp.exists() {
                        dirs.push(cp);
                    }
                }
            }
        }
        dirs
    };

    // Find a checkpoint that matches this run (by session_id or timestamp proximity)
    for cp_path in &dirs_to_scan {
        if let Ok(checkpoint) = read_checkpoint(cp_path) {
            // Match by start time proximity (within 60 seconds)
            let cp_started = DateTime::parse_from_rfc3339(&checkpoint.started_at).ok();
            let run_started = run.started_at;

            let matches = cp_started
                .map(|cs| {
                    let diff = (cs.with_timezone(&Utc) - run_started)
                        .num_seconds()
                        .abs();
                    diff < 60
                })
                .unwrap_or(false);

            if matches {
                println!();
                println!(
                    "  {} Checkpoint: {}",
                    dim.apply_to("📊"),
                    cp_path.display()
                );

                // Count phase statuses
                let mut completed = 0u32;
                let mut failed = 0u32;
                let mut skipped = 0u32;
                let mut pending = 0u32;
                let mut failed_phases = Vec::new();

                for (name, phase) in &checkpoint.phases {
                    match phase.status.as_str() {
                        "completed" => completed += 1,
                        "failed" => {
                            failed += 1;
                            failed_phases.push(name.as_str());
                        }
                        "skipped" => skipped += 1,
                        _ => pending += 1,
                    }
                }

                let total = checkpoint.phases.len();
                println!(
                    "    Phases: {} total, {} completed, {} failed, {} skipped, {} pending",
                    total, completed, failed, skipped, pending
                );

                if !failed_phases.is_empty() {
                    failed_phases.sort();
                    println!(
                        "    Failed: {}",
                        Style::new().red().apply_to(failed_phases.join(", "))
                    );
                }

                if let Some(ref pr) = checkpoint.pr_url {
                    println!("    PR: {}", pr);
                }

                if !checkpoint.commits.is_empty() {
                    let display: Vec<&str> = checkpoint
                        .commits
                        .iter()
                        .map(|c| c.get(..8).unwrap_or(c))
                        .collect();
                    println!("    Commits: {}", display.join(", "));
                }

                return Ok(());
            }
        }
    }

    Ok(())
}

fn print_field(label: &str, value: &str, bold: &Style) {
    println!("  {:<18} {}", bold.apply_to(format!("{}:", label)), value);
}

// ── Mode C: Log viewer ─────────────────────────────────────────────

fn show_logs(run_id_prefix: &str, pane: bool, tail: Option<usize>) -> Result<()> {
    let runs = load_all_runs()?;
    let run = find_by_prefix(&runs, run_id_prefix)?;

    let log_dir = gw_home().join("runs").join(&run.run_id).join("logs");
    let log_file = if pane {
        log_dir.join("pane.log")
    } else {
        log_dir.join("events.jsonl")
    };

    if !log_file.exists() {
        let file_type = if pane { "pane.log" } else { "events.jsonl" };
        println!(
            "{} No {} found for run {}.",
            tag("WARN"),
            file_type,
            short_id(&run.run_id)
        );
        println!("  Logs are only available for daemon-managed runs.");
        println!(
            "  Expected path: {}",
            log_file.display()
        );
        return Ok(());
    }

    if pane {
        show_pane_log(&log_file, tail)?;
    } else {
        show_event_log(&log_file, tail)?;
    }

    Ok(())
}

fn show_event_log(path: &Path, tail: Option<usize>) -> Result<()> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            let formatted = format_event(&event);
            lines.push(formatted);
        } else {
            // Non-JSON line, print as-is
            lines.push(line);
        }
    }

    let display_lines = if let Some(n) = tail {
        let start = lines.len().saturating_sub(n);
        &lines[start..]
    } else {
        &lines[..]
    };

    for line in display_lines {
        println!("{}", line);
    }

    if display_lines.is_empty() {
        println!("(empty log file)");
    }

    Ok(())
}

fn format_event(event: &serde_json::Value) -> String {
    let ts = event
        .get("ts")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string());

    let event_type = event
        .get("event")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let msg = event
        .get("msg")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let tag_str = match event_type {
        "started" | "completed" => Style::new().green().apply_to(format!("{:<20}", event_type)),
        "failed" | "crash_recovery" | "recovery_failed" => {
            Style::new().red().apply_to(format!("{:<20}", event_type))
        }
        "phase_change" => Style::new().cyan().apply_to(format!("{:<20}", event_type)),
        "stopped" | "kill_session" => {
            Style::new().yellow().apply_to(format!("{:<20}", event_type))
        }
        _ => Style::new().dim().apply_to(format!("{:<20}", event_type)),
    };

    format!("{}  {}  {}", ts, tag_str, msg)
}

fn show_pane_log(path: &Path, tail: Option<usize>) -> Result<()> {
    if let Some(n) = tail {
        // For --tail, read from the end to avoid loading the entire file.
        // pane.log can be very large for multi-hour sessions.
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let all_lines: Vec<String> = reader.lines().collect::<std::io::Result<_>>()?;
        let start = all_lines.len().saturating_sub(n);
        for line in &all_lines[start..] {
            println!("{}", line);
        }
        if all_lines.is_empty() {
            println!("(empty pane log)");
        }
    } else {
        // Without --tail, stream the file line-by-line to avoid loading all at once
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut count = 0;
        for line in reader.lines() {
            println!("{}", line?);
            count += 1;
        }
        if count == 0 {
            println!("(empty pane log)");
        }
    }

    Ok(())
}

// ── Data loading ───────────────────────────────────────────────────

/// Load all runs from `~/.gw/runs/*/meta.json` directly from disk.
///
/// No daemon connection required. Silently skips malformed entries.
fn load_all_runs() -> Result<Vec<RunEntry>> {
    let runs_dir = gw_home().join("runs");
    if !runs_dir.exists() {
        return Ok(vec![]);
    }

    let mut runs = Vec::new();
    for entry in fs::read_dir(&runs_dir)?.flatten() {
        let meta = entry.path().join("meta.json");
        if let Ok(content) = fs::read_to_string(&meta) {
            if let Ok(run) = serde_json::from_str::<RunEntry>(&content) {
                runs.push(run);
            }
        }
    }

    // Sort newest first
    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(runs)
}

/// Find a run by ID prefix, similar to git short SHA matching.
fn find_by_prefix<'a>(runs: &'a [RunEntry], prefix: &str) -> Result<&'a RunEntry> {
    if prefix.is_empty() {
        return Err(eyre!("Run ID cannot be empty"));
    }

    let lower = prefix.to_lowercase();
    let matches: Vec<&RunEntry> = runs
        .iter()
        .filter(|r| r.run_id.to_lowercase().starts_with(&lower))
        .collect();

    match matches.len() {
        0 => Err(eyre!(
            "No run found matching '{}'. Run `gw history` to see available runs.",
            prefix
        )),
        1 => Ok(matches[0]),
        n => Err(eyre!(
            "Ambiguous run ID '{}' matches {} runs. Use a longer prefix.",
            prefix,
            n
        )),
    }
}

// ── Formatting helpers ─────────────────────────────────────────────

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

fn format_duration(run: &RunEntry) -> String {
    let end = run.finished_at.unwrap_or_else(Utc::now);
    let secs = end.signed_duration_since(run.started_at).num_seconds().max(0) as u64;

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

fn format_started(dt: &DateTime<Utc>) -> String {
    dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

fn format_datetime(dt: &DateTime<Utc>) -> String {
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn abbreviate_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path.starts_with(home_str.as_ref()) {
            return format!("~{}", &path[home_str.len()..]);
        }
    }
    path.to_string()
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len <= 1 {
        "\u{2026}".to_string()
    } else {
        let mut result = String::with_capacity(max_len);
        for ch in s.chars() {
            if result.len() + 1 >= max_len {
                result.push('\u{2026}');
                break;
            }
            result.push(ch);
        }
        result
    }
}

/// Compute visible length of a string, ignoring ANSI escape sequences.
fn visible_len(s: &str) -> usize {
    console::measure_text_width(s)
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn make_run(id: &str, status: RunStatus, repo: &str, minutes_ago: i64) -> RunEntry {
        let now = Utc::now();
        let started = now - chrono::Duration::minutes(minutes_ago);
        let finished = if matches!(status, RunStatus::Running | RunStatus::Queued) {
            None
        } else {
            Some(now - chrono::Duration::minutes(minutes_ago.saturating_sub(10)))
        };

        RunEntry {
            run_id: id.to_string(),
            plan_path: PathBuf::from(format!("plans/{}.md", id)),
            repo_dir: PathBuf::from(repo),
            session_name: format!("gw-{}", id),
            tmux_session: None,
            status,
            current_phase: None,
            started_at: started,
            finished_at: finished,
            crash_restarts: 0,
            config_dir: None,
            error_message: None,
            restartable: true,
            claude_pid: None,
        }
    }

    #[test]
    fn find_by_prefix_exact_match() {
        let runs = vec![
            make_run("aabb1122", RunStatus::Succeeded, "/repo", 60),
            make_run("ccdd3344", RunStatus::Failed, "/repo", 30),
        ];

        let found = find_by_prefix(&runs, "aabb").unwrap();
        assert_eq!(found.run_id, "aabb1122");
    }

    #[test]
    fn find_by_prefix_ambiguous() {
        let runs = vec![
            make_run("aabb1122", RunStatus::Succeeded, "/repo", 60),
            make_run("aabb3344", RunStatus::Failed, "/repo", 30),
        ];

        assert!(find_by_prefix(&runs, "aabb").is_err());
    }

    #[test]
    fn find_by_prefix_not_found() {
        let runs = vec![make_run("aabb1122", RunStatus::Succeeded, "/repo", 60)];

        assert!(find_by_prefix(&runs, "zzzz").is_err());
    }

    #[test]
    fn parse_status_filter_variants() {
        assert_eq!(parse_status_filter("succeeded").unwrap(), RunStatus::Succeeded);
        assert_eq!(parse_status_filter("FAILED").unwrap(), RunStatus::Failed);
        assert_eq!(parse_status_filter("ok").unwrap(), RunStatus::Succeeded);
        assert_eq!(parse_status_filter("error").unwrap(), RunStatus::Failed);
        assert!(parse_status_filter("nonexistent").is_err());
    }

    #[test]
    fn truncate_strings() {
        assert_eq!(truncate("hello world", 5), "hell\u{2026}");
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("test", 1), "\u{2026}");
    }

    #[test]
    fn format_duration_ranges() {
        let mut run = make_run("test1234", RunStatus::Succeeded, "/repo", 5);
        run.started_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        run.finished_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 45).unwrap());
        assert_eq!(format_duration(&run), "45s");

        run.finished_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 5, 30).unwrap());
        assert_eq!(format_duration(&run), "5m 30s");

        run.finished_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 2, 15, 0).unwrap());
        assert_eq!(format_duration(&run), "2h 15m");
    }

    #[test]
    fn format_event_known_types() {
        let event = serde_json::json!({
            "ts": "2026-01-01T12:00:00Z",
            "event": "phase_change",
            "msg": "Starting phase_2_work"
        });
        let formatted = format_event(&event);
        assert!(formatted.contains("phase_change"));
        assert!(formatted.contains("Starting phase_2_work"));
    }
}
