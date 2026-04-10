//! Status command implementation.
//!
//! Shows status of active or recent arc runs by reading
//! `.gw/batch-state.json` and displaying progress, machine health,
//! and pass/fail/remaining counts.

use crate::batch::state::{BatchState, PlanOutcome};
use crate::output::tags::tag;
use color_eyre::Result;
use std::path::Path;

/// Execute the status command.
///
/// Reads `.gw/batch-state.json` atomically and displays:
/// - Current plan progress (N/M)
/// - Current group being executed
/// - Time elapsed
/// - Machine health (CPU, RAM, disk)
/// - Pass/Fail/Remaining counts
pub fn execute() -> Result<()> {
    // If the daemon is running, show daemon status + run list instead
    if crate::client::socket::DaemonClient::is_daemon_running() {
        return show_daemon_status();
    }

    let state_path = Path::new(".gw/batch-state.json");

    if !state_path.exists() {
        println!("{} No active batch found (.gw/batch-state.json not present)", tag("WARN"));
        println!("  Run `gw run <plans>` to start a batch.");
        return Ok(());
    }

    // Read atomically (read entire file then parse)
    let state = BatchState::load(state_path)?;

    print_header(&state);
    print_plan_progress(&state);
    print_machine_health();

    Ok(())
}

/// Show status from the daemon, including active runs.
fn show_daemon_status() -> Result<()> {
    use crate::client::socket::DaemonClient;
    use crate::daemon::protocol::{Request, Response, RunStatus};

    println!("{} Daemon mode active", tag("OK"));
    println!();

    let client = DaemonClient::new()?;
    match client.send(Request::DaemonStatus)? {
        Response::Ok { message } => println!("  {}", message),
        _ => {}
    }

    // Show runs like `gw ps`
    let client = DaemonClient::new()?;
    match client.send(Request::ListRuns { all: false })? {
        Response::RunList { runs } => {
            if runs.is_empty() {
                println!("  No active runs.");
            } else {
                println!();
                for run in &runs {
                    let short = crate::commands::util::short_id(&run.run_id);
                    let status_tag = match run.status {
                        RunStatus::Running => tag("RUN"),
                        RunStatus::Queued => tag("WARN"),
                        RunStatus::Succeeded => tag("OK"),
                        RunStatus::Failed => tag("FAIL"),
                        RunStatus::Stopped => tag("SKIP"),
                    };
                    println!("  {} {} {} ({})",
                        status_tag,
                        short,
                        run.plan_path.display(),
                        run.current_phase.as_deref().unwrap_or("pending"),
                    );
                }
            }
        }
        _ => {}
    }

    print_machine_health();
    Ok(())
}

fn print_header(state: &BatchState) {
    println!();
    println!("  Batch: {}", state.batch_id);

    // Calculate elapsed time from started_at
    let elapsed = chrono::Utc::now().signed_duration_since(state.started_at);
    let hours = elapsed.num_hours();
    let mins = elapsed.num_minutes() % 60;
    let secs = elapsed.num_seconds() % 60;
    println!("  Elapsed: {:02}:{:02}:{:02}", hours, mins, secs);
    println!();
}

fn print_plan_progress(state: &BatchState) {
    if state.plans.is_empty() {
        println!("  No plans in batch.");
        return;
    }

    let total = state.plans.len();
    let passed = state.results.iter().filter(|r| r.outcome == PlanOutcome::Passed).count();
    let failed = state.results.iter().filter(|r| r.outcome == PlanOutcome::Failed).count();
    let skipped = state.results.iter().filter(|r| r.outcome == PlanOutcome::Skipped).count();
    let remaining = total - state.results.len();

    println!(
        "  Plans: {}/{} completed",
        state.current_index,
        total,
    );
    println!(
        "    {} {}  {} {}  {} {}  remaining {}",
        tag("OK"), passed,
        tag("FAIL"), failed,
        tag("SKIP"), skipped,
        remaining,
    );

    // Show current plan if still running
    if let Some(current) = state.next_plan() {
        println!();
        println!(
            "  {} {}",
            tag("RUN"),
            current,
        );
    }

    if state.circuit_breaker.tripped {
        println!("  {} Circuit breaker tripped ({} consecutive failures)",
            tag("WARN"),
            state.circuit_breaker.consecutive_failures,
        );
    }
    println!();
}

fn print_machine_health() {
    use crate::log::jsonl::MachineSnapshot;

    let health = MachineSnapshot::capture();

    println!("  Machine Health:");
    println!(
        "    CPU:    {:.1}%",
        health.cpu_percent,
    );
    println!(
        "    Memory: {} / {} MB ({:.0}%)",
        health.memory_used_mb,
        health.memory_total_mb,
        if health.memory_total_mb > 0 {
            health.memory_used_mb as f64 / health.memory_total_mb as f64 * 100.0
        } else {
            0.0
        },
    );
    println!(
        "    Disk:   {:.1} GB free",
        health.disk_free_gb,
    );
    println!();
}
