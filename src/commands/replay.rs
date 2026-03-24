//! Replay command implementation.
//!
//! Displays checkpoint status and allows resuming from a checkpoint.

use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Checkpoint file structure (v27 schema).
#[derive(Debug, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub schema_version: u32,
    pub plan_file: String,
    pub session_id: String,
    pub phases: HashMap<String, PhaseState>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub completed_at: Option<String>,
}

/// State of a single phase in the checkpoint.
#[derive(Debug, Deserialize)]
pub struct PhaseState {
    pub status: String,
    pub artifact: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub skip_reason: Option<String>,
}

/// Execute the replay command.
///
/// # Arguments
///
/// * `checkpoint` - Path to the checkpoint file to resume from
pub fn execute(checkpoint: PathBuf) -> Result<()> {
    // Check if checkpoint file exists
    if !checkpoint.exists() {
        eyre::bail!("Checkpoint file not found: {}", checkpoint.display());
    }

    // Load checkpoint
    let contents = std::fs::read_to_string(&checkpoint)
        .wrap_err_with(|| format!("Failed to read checkpoint: {}", checkpoint.display()))?;

    let cp: Checkpoint = serde_json::from_str(&contents)
        .wrap_err_with(|| "Failed to parse checkpoint JSON. Invalid schema?")?;

    // Display checkpoint info
    print_checkpoint_table(&cp);

    Ok(())
}

/// Print checkpoint in tabular format.
fn print_checkpoint_table(cp: &Checkpoint) {
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║                      CHECKPOINT STATUS                         ║");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║ ID:      {:54} ║", cp.id);
    println!("║ Schema:  {:54} ║", format!("v{}", cp.schema_version));
    println!("║ Plan:    {:54} ║", truncate(&cp.plan_file, 54));
    println!("║ Session: {:54} ║", truncate(&cp.session_id, 54));
    println!("╠════════════════════════════════════════════════════════════════╣");

    // Count phases by status
    let mut completed = 0;
    let mut in_progress = 0;
    let mut pending = 0;
    let mut skipped = 0;

    for phase in cp.phases.values() {
        match phase.status.as_str() {
            "completed" => completed += 1,
            "in_progress" => in_progress += 1,
            "pending" => pending += 1,
            "skipped" => skipped += 1,
            _ => {}
        }
    }

    let total = cp.phases.len();
    let progress_pct = if total > 0 {
        (completed * 100) / total
    } else {
        0
    };

    println!("║                                                                ║");
    println!("║ Progress: [{:<50}] {:3}% ║", "█".repeat(progress_pct / 2), progress_pct);
    println!("║                                                                ║");
    println!("║   Completed: {:4}    In Progress: {:4}                      ║", completed, in_progress);
    println!("║   Pending:   {:4}    Skipped:     {:4}                      ║", pending, skipped);
    println!("╠════════════════════════════════════════════════════════════════╣");

    // Print phase table (grouped by status)
    println!("║                      PHASE DETAILS                             ║");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║ {:<20} {:<12} {:<28} ║", "Phase", "Status", "Artifact");
    println!("╠════════════════════════════════════════════════════════════════╣");

    // Sort phases by name for consistent display
    let mut phase_names: Vec<&String> = cp.phases.keys().collect();
    phase_names.sort();

    for name in phase_names {
        let phase = &cp.phases[name];
        let status_icon = match phase.status.as_str() {
            "completed" => "✓",
            "in_progress" => "▶",
            "pending" => "○",
            "skipped" => "⊘",
            _ => "?",
        };

        let artifact_display = phase.artifact.as_ref()
            .map(|a| truncate(a, 28))
            .unwrap_or_else(|| {
                phase.skip_reason.as_ref()
                    .map(|r| format!("skip: {}", truncate(r, 22)))
                    .unwrap_or_default()
            });

        println!(
            "║ {:<20} {:<12} {:<28} ║",
            truncate(name, 20),
            format!("{} {}", status_icon, phase.status),
            artifact_display
        );
    }

    println!("╚════════════════════════════════════════════════════════════════╝");

    // Timestamp info
    if let Some(started) = &cp.started_at {
        println!("Started:  {}", started);
    }
    if let Some(updated) = &cp.updated_at {
        println!("Updated:  {}", updated);
    }
    if let Some(completed) = &cp.completed_at {
        println!("Completed: {}", completed);
    }
}

/// Truncate a string to max length, adding "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s[..max_len].to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}