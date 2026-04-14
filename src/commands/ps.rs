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
use std::path::Path;

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
    let mut rows: Vec<(u8, RowData)> = runs
        .iter()
        .map(|run| {
            let sort_key = match run.status {
                RunStatus::Running => 0,
                RunStatus::Queued => 1,
                _ => 2,
            };
            let status_plain = if run.waiting_for_network {
                format!("{} (waiting for network)", format_status_plain(run.status))
            } else {
                format_status_plain(run.status)
            };
            let status_styled = if run.waiting_for_network {
                format!("{} (waiting for network)", format_status(run.status))
            } else {
                format_status(run.status)
            };
            let plan_display = if run.schedule_id.is_some() {
                format!("{} (scheduled)", format_plan_path(&run.plan_path, &run.repo_dir))
            } else {
                format_plan_path(&run.plan_path, &run.repo_dir)
            };
            (sort_key, RowData {
                id: short_id(&run.run_id).to_string(),
                tmux: run.session_name.clone(),
                status_plain,
                status_styled,
                plan: plan_display,
                repo: abbreviate_home(&run.repo_dir.to_string_lossy()),
                phase: run.current_phase.as_deref().unwrap_or("-").to_string(),
                uptime: format_uptime(run.uptime_secs),
            })
        })
        .collect();
    rows.sort_by_key(|(k, _)| *k);
    // `list_runs` returns rows in `started_at` DESC order (newest-first). Stable sort
    // above preserves that ordering within each status group. For the Queued group,
    // "newest-first" is misleading because the daemon drains the per-repo VecDeque via
    // `pop_front` (FIFO) — so the run that executes next is the OLDEST queued entry,
    // not the newest. Reverse just the Queued slice so the top of the block matches
    // the execution order users actually care about.
    if let (Some(s), Some(e)) = (
        rows.iter().position(|(k, _)| *k == 1),
        rows.iter().rposition(|(k, _)| *k == 1),
    ) {
        rows[s..=e].reverse();
    }
    let rows: Vec<RowData> = rows.into_iter().map(|(_, r)| r).collect();

    // Fixed-width columns: ID, TMUX, STATUS, PHASE, UPTIME (plus border chars).
    // Each column has 3 chars overhead: space + content + space, plus 1 for the │.
    //
    // Width measurement uses `display_width` (console::measure_text_width) instead
    // of `String::len()` because truncated cells may contain `…` (U+2026) — 3 bytes
    // in UTF-8 but 1 display column. Byte-based widths would over-count those cells
    // and leave the column mis-padded (visible as 1-2 char drift in aligned rows).
    let id_w = rows.iter().map(|r| display_width(&r.id)).max().unwrap_or(0).max(2);
    let tmux_w = rows.iter().map(|r| display_width(&r.tmux)).max().unwrap_or(0).max(4);
    let status_w = rows.iter().map(|r| display_width(&r.status_plain)).max().unwrap_or(0).max(6);
    let phase_w = rows.iter().map(|r| display_width(&r.phase)).max().unwrap_or(0).max(5);
    let uptime_w = rows.iter().map(|r| display_width(&r.uptime)).max().unwrap_or(0).max(6);

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
            // PLAN filenames carry the distinctive slug/date at the TAIL
            // (e.g. `plans/2026-04-15-fix-daemon-schedule-path-validation-plan.md`).
            // Plain end-truncation would drop exactly that tail, so use a
            // middle-ellipsis that keeps both the `plans/` context and the
            // filename end visible. REPO still uses end-truncation since
            // its tail is just the repo folder name and its head is the
            // meaningful `~/path/to/` context.
            r.plan = truncate_middle(&r.plan, plan_budget);
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

    // Calculate column widths from (possibly truncated) content.
    // Headers are ASCII-only so `headers[i].len()` == display width; cells go through
    // `display_width` to handle the `…` case described above.
    let widths: Vec<usize> = if narrow {
        vec![
            rows.iter().map(|r| display_width(&r.id)).max().unwrap_or(0).max(headers[0].len()),
            rows.iter().map(|r| display_width(&r.tmux)).max().unwrap_or(0).max(headers[1].len()),
            rows.iter().map(|r| display_width(&r.status_plain)).max().unwrap_or(0).max(headers[2].len()),
            rows.iter().map(|r| display_width(&r.plan)).max().unwrap_or(0).max(headers[3].len()),
            rows.iter().map(|r| display_width(&r.phase)).max().unwrap_or(0).max(headers[4].len()),
            rows.iter().map(|r| display_width(&r.uptime)).max().unwrap_or(0).max(headers[5].len()),
        ]
    } else {
        vec![
            rows.iter().map(|r| display_width(&r.id)).max().unwrap_or(0).max(headers[0].len()),
            rows.iter().map(|r| display_width(&r.tmux)).max().unwrap_or(0).max(headers[1].len()),
            rows.iter().map(|r| display_width(&r.status_plain)).max().unwrap_or(0).max(headers[2].len()),
            rows.iter().map(|r| display_width(&r.plan)).max().unwrap_or(0).max(headers[3].len()),
            rows.iter().map(|r| display_width(&r.repo)).max().unwrap_or(0).max(headers[4].len()),
            rows.iter().map(|r| display_width(&r.phase)).max().unwrap_or(0).max(headers[5].len()),
            rows.iter().map(|r| display_width(&r.uptime)).max().unwrap_or(0).max(headers[6].len()),
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
        // `plain_lens` carries the *display* width of each cell so padding below
        // produces a visually-aligned column even when the cell contains a `…`
        // or ANSI-styled text. Using `status_plain`/`plan`/etc. (the ANSI-free
        // strings) for measurement while printing `status_styled` etc. keeps
        // colors without inflating the measured width.
        let (cells, plain_lens): (Vec<&String>, Vec<usize>) = if narrow {
            (
                vec![&row.id, &row.tmux, &row.status_styled, &row.plan, &row.phase, &row.uptime],
                vec![
                    display_width(&row.id),
                    display_width(&row.tmux),
                    display_width(&row.status_plain),
                    display_width(&row.plan),
                    display_width(&row.phase),
                    display_width(&row.uptime),
                ],
            )
        } else {
            (
                vec![&row.id, &row.tmux, &row.status_styled, &row.plan, &row.repo, &row.phase, &row.uptime],
                vec![
                    display_width(&row.id),
                    display_width(&row.tmux),
                    display_width(&row.status_plain),
                    display_width(&row.plan),
                    display_width(&row.repo),
                    display_width(&row.phase),
                    display_width(&row.uptime),
                ],
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

/// Display width of a string in terminal columns (strips ANSI escapes, respects
/// unicode widths). Preferred over `String::len()` for any cell-width math since
/// ellipsis and other non-ASCII glyphs have byte len != display columns.
fn display_width(s: &str) -> usize {
    console::measure_text_width(s)
}

/// Truncate a string to fit within `budget` **display columns**, appending "…"
/// if truncated. Previous version compared `String::len()` (bytes) against a
/// column budget — fine for ASCII input but off by 2 columns once a `…` was
/// appended (UTF-8 `…` is 3 bytes, 1 column). That drift is what made `gw ps`
/// rows look mis-aligned when some rows were truncated and others weren't.
fn truncate_with_ellipsis(s: &str, budget: usize) -> String {
    if display_width(s) <= budget {
        return s.to_string();
    }
    if budget <= 1 {
        return "\u{2026}".to_string();
    }
    // Reserve 1 column for the trailing `…`.
    let mut result = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = unicode_display_width(ch);
        if used + w + 1 > budget {
            break;
        }
        result.push(ch);
        used += w;
    }
    result.push('\u{2026}');
    result
}

/// Truncate keeping both head and tail, with an ellipsis in the middle —
/// suitable for plan paths where the tail (filename slug) is the distinctive
/// part. When the string fits the budget it is returned unchanged; otherwise
/// the tail gets ~2/3 of the remaining budget so the filename end stays
/// readable even on narrow terminals.
fn truncate_middle(s: &str, budget: usize) -> String {
    let total = display_width(s);
    if total <= budget {
        return s.to_string();
    }
    if budget <= 1 {
        return "\u{2026}".to_string();
    }
    // Reserve 1 column for the middle `…`.
    let remaining = budget - 1;
    let head_budget = remaining / 3;
    let tail_budget = remaining - head_budget;

    let chars: Vec<char> = s.chars().collect();

    let mut head = String::new();
    let mut head_used = 0usize;
    for ch in &chars {
        let w = unicode_display_width(*ch);
        if head_used + w > head_budget {
            break;
        }
        head.push(*ch);
        head_used += w;
    }

    // Walk from the end for the tail, then reverse the collected chars.
    let mut tail_rev: Vec<char> = Vec::new();
    let mut tail_used = 0usize;
    for ch in chars.iter().rev() {
        let w = unicode_display_width(*ch);
        if tail_used + w > tail_budget {
            break;
        }
        tail_rev.push(*ch);
        tail_used += w;
    }
    let tail: String = tail_rev.into_iter().rev().collect();

    format!("{}\u{2026}{}", head, tail)
}

/// Display width of a single char (delegates to unicode-width via console).
/// Control chars and zero-width marks return 0; CJK-wide chars return 2.
fn unicode_display_width(ch: char) -> usize {
    // `measure_text_width` on a single-char string is the simplest correct
    // way to reuse console's unicode-width logic without pulling in a new dep.
    let mut buf = [0u8; 4];
    console::measure_text_width(ch.encode_utf8(&mut buf))
}

/// Format a plan path for display in the PLAN column.
///
/// The REPO column already surfaces the absolute repo location, so duplicating
/// `~/path/to/repo/` in every PLAN cell just eats the width budget and pushes
/// the filename (the actually-distinctive slug) under the truncation guillotine.
/// Prefer a path relative to `repo_dir`; fall back to home-abbreviation only
/// when the plan lives outside its repo (rare — one-off runs pointing at a
/// plan in `/tmp` or similar).
fn format_plan_path(plan: &Path, repo: &Path) -> String {
    if let Ok(rel) = plan.strip_prefix(repo) {
        rel.to_string_lossy().into_owned()
    } else {
        abbreviate_home(&plan.to_string_lossy())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn truncate_ellipsis_ascii_fits() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn truncate_ellipsis_ascii_truncates_to_budget_in_columns() {
        // Old bug: byte-len math produced 7 display columns for a 6-col budget
        // because the appended `…` is 3 bytes. New impl respects columns.
        let out = truncate_with_ellipsis("abcdefghij", 6);
        assert_eq!(display_width(&out), 6);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let long = "plans/2026-04-15-fix-daemon-schedule-path-validation-plan.md";
        let out = truncate_middle(long, 30);
        assert_eq!(display_width(&out), 30);
        assert!(out.starts_with("plans/"));
        assert!(out.ends_with("-plan.md"));
        assert!(out.contains('\u{2026}'));
    }

    #[test]
    fn truncate_middle_passes_through_when_fits() {
        assert_eq!(truncate_middle("short.md", 20), "short.md");
    }

    #[test]
    fn truncate_middle_tiny_budget_degrades_gracefully() {
        // Budget 1 → just an ellipsis, no panic from slicing an empty head/tail.
        assert_eq!(truncate_middle("anything", 1), "\u{2026}");
    }

    #[test]
    fn format_plan_path_strips_repo_prefix() {
        let repo = PathBuf::from("/home/x/proj");
        let plan = PathBuf::from("/home/x/proj/plans/foo.md");
        assert_eq!(format_plan_path(&plan, &repo), "plans/foo.md");
    }

    #[test]
    fn format_plan_path_falls_back_when_outside_repo() {
        // A plan that lives outside its declared repo (e.g. /tmp one-off) still
        // needs a sensible display — fall back to home abbreviation, never the
        // raw absolute path (which would eat the whole column).
        let repo = PathBuf::from("/home/x/proj");
        let plan = PathBuf::from("/tmp/adhoc-plan.md");
        let out = format_plan_path(&plan, &repo);
        assert_eq!(out, "/tmp/adhoc-plan.md");
    }

    #[test]
    fn display_width_counts_ellipsis_as_one_column() {
        // This is the load-bearing assumption behind every column-width fix
        // in this module — if it ever breaks, padding math drifts by 2 per `…`.
        assert_eq!(display_width("\u{2026}"), 1);
        assert_eq!(display_width("abc\u{2026}"), 4);
    }
}
