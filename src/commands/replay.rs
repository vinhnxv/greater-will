//! Replay command implementation.
//!
//! Displays checkpoint status and allows resuming from a checkpoint.

use crate::checkpoint::schema::Checkpoint;
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::path::PathBuf;

/// Execute the replay command.
///
/// # Arguments
///
/// * `checkpoint` - Path to the checkpoint file to resume from
pub fn execute(checkpoint: PathBuf, resume: bool, force: bool) -> Result<()> {
    // Check if checkpoint file exists
    if !checkpoint.exists() {
        eyre::bail!("Checkpoint file not found: {}", checkpoint.display());
    }

    // Load checkpoint
    let contents = std::fs::read_to_string(&checkpoint)
        .wrap_err_with(|| format!("Failed to read checkpoint: {}", checkpoint.display()))?;

    let cp: Checkpoint = serde_json::from_str(&contents)
        .wrap_err_with(|| "Failed to parse checkpoint JSON. Invalid schema?")?;

    // Check schema compatibility and warn if needed
    let compat = cp.schema_compat();
    if let Some(warning) = compat.warning() {
        tracing::warn!("{}", warning);
    }

    // Display checkpoint info
    print_checkpoint_table(&cp);

    if !resume {
        return Ok(());
    }

    // --- Resume path ---
    use crate::checkpoint::reader::{next_actionable_phase, validate_before_resume};

    let next_phase = next_actionable_phase(&cp)
        .ok_or_else(|| eyre::eyre!("All phases are completed or skipped. Nothing to resume."))?;

    println!();
    println!("Next actionable phase: {}", next_phase);

    if !force {
        let arc_dir = checkpoint.parent()
            .ok_or_else(|| eyre::eyre!("Cannot determine arc directory from checkpoint path"))?;
        let validation = validate_before_resume(&cp, arc_dir)?;
        for warning in &validation.warnings {
            println!("  WARN: {}", warning);
        }
        for error in &validation.errors {
            println!("  ERROR: {}", error);
        }
        if !validation.can_resume() {
            println!();
            println!("Resume blocked: {} critical artifact(s) missing.", validation.errors.len());
            println!("Phases to re-run: {:?}", validation.phases_to_reset);
            println!("Use --force to skip validation.");
            eyre::bail!("Pre-resume validation failed");
        }
    }

    let plan_path = std::path::PathBuf::from(&cp.plan_file);
    if !plan_path.exists() {
        eyre::bail!("Plan file not found: {}. Cannot resume.", cp.plan_file);
    }

    println!();
    println!("Resuming from phase: {}", next_phase);

    use crate::engine::single_session::{SingleSessionConfig, run_single_session};
    let cwd = std::env::current_dir()?;
    let ss_config = SingleSessionConfig::new(&cwd).with_resume();
    let result = run_single_session(&plan_path, &ss_config)?;

    if result.success {
        println!("Resume completed successfully ({:.1}s)", result.duration.as_secs_f64());
        Ok(())
    } else {
        eyre::bail!("Resume failed: {}", result.message);
    }
}

/// Print checkpoint in tabular format.
fn print_checkpoint_table(cp: &Checkpoint) {
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║                      CHECKPOINT STATUS                         ║");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║ ID:      {:54} ║", cp.id);
    println!("║ Schema:  {:54} ║", format!("v{}", cp.schema_version.unwrap_or(0)));
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
                // Check skip_map for skip reason
                cp.skip_map
                    .as_ref()
                    .and_then(|m| m.get(name.as_str()))
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
    if !cp.started_at.is_empty() {
        println!("Started:  {}", cp.started_at);
    }
    if let Some(completed) = &cp.completed_at {
        println!("Completed: {}", completed);
    }
}

/// Truncate a string to max length, adding "..." if truncated.
/// Uses char boundaries to avoid panics on multi-byte UTF-8.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}
