//! Status command implementation.
//!
//! Shows status of active or recent arc runs by reading
//! `.gw/batch-state.json` and displaying progress, machine health,
//! and pass/fail/remaining counts.

use crate::output::tags::tag;
use color_eyre::Result;
use serde::Deserialize;
use std::path::Path;

/// Batch state as written by the run command.
#[derive(Debug, Deserialize)]
struct BatchState {
    batch_id: String,
    #[serde(default)]
    plans: Vec<PlanEntry>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    current_plan_index: usize,
    #[serde(default)]
    current_group: Option<String>,
}

/// A single plan entry in the batch state.
#[derive(Debug, Deserialize)]
struct PlanEntry {
    path: String,
    status: String,
    #[serde(default)]
    groups_completed: u32,
    #[serde(default)]
    groups_total: u32,
}

/// Execute the status command.
///
/// Reads `.gw/batch-state.json` atomically and displays:
/// - Current plan progress (N/M)
/// - Current group being executed
/// - Time elapsed
/// - Machine health (CPU, RAM, disk)
/// - Pass/Fail/Remaining counts
pub fn execute() -> Result<()> {
    let state_path = Path::new(".gw/batch-state.json");

    if !state_path.exists() {
        println!("{} No active batch found (.gw/batch-state.json not present)", tag("WARN"));
        println!("  Run `gw run <plans>` to start a batch.");
        return Ok(());
    }

    // Read atomically (read entire file then parse)
    let content = std::fs::read_to_string(state_path)?;
    let state: BatchState = serde_json::from_str(&content)?;

    print_header(&state);
    print_plan_progress(&state);
    print_machine_health();

    Ok(())
}

fn print_header(state: &BatchState) {
    println!();
    println!("  Batch: {}", state.batch_id);
    if let Some(ref started) = state.started_at {
        // Calculate elapsed time
        if let Ok(start) = chrono::DateTime::parse_from_rfc3339(started) {
            let elapsed = chrono::Utc::now().signed_duration_since(start);
            let hours = elapsed.num_hours();
            let mins = elapsed.num_minutes() % 60;
            let secs = elapsed.num_seconds() % 60;
            println!("  Elapsed: {:02}:{:02}:{:02}", hours, mins, secs);
        }
    }
    println!();
}

fn print_plan_progress(state: &BatchState) {
    if state.plans.is_empty() {
        println!("  No plans in batch.");
        return;
    }

    let total = state.plans.len();
    let passed = state.plans.iter().filter(|p| p.status == "completed").count();
    let failed = state.plans.iter().filter(|p| p.status == "failed").count();
    let remaining = total - passed - failed;

    println!(
        "  Plans: {}/{} {} passed  {} failed  {} remaining",
        state.current_plan_index + 1,
        total,
        tag("OK"),
        tag("FAIL"),
        remaining,
    );
    println!(
        "    {} {}  {} {}  remaining {}",
        tag("OK"), passed,
        tag("FAIL"), failed,
        remaining,
    );

    // Show current plan details
    if let Some(current) = state.plans.get(state.current_plan_index) {
        println!();
        println!(
            "  {} {}",
            tag("RUN"),
            current.path,
        );
        if current.groups_total > 0 {
            println!(
                "    Groups: {}/{}",
                current.groups_completed, current.groups_total,
            );
        }
    }

    if let Some(ref group) = state.current_group {
        println!("    Current group: {}", group);
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
