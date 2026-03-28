#![allow(dead_code)]
//! Progress bar utilities using indicatif.
//!
//! Provides a two-level progress display:
//! - **Batch bar**: overall progress across all plans
//! - **Plan bar**: progress within a single plan's phase groups

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Create a multi-progress container with a batch bar and a plan bar.
///
/// Returns `(multi, batch_bar, plan_bar)`.
///
/// # Arguments
///
/// * `total_plans` - Total number of plans in the batch
/// * `total_groups` - Total number of phase groups per plan
pub fn create_progress(total_plans: u64, total_groups: u64) -> (MultiProgress, ProgressBar, ProgressBar) {
    let multi = MultiProgress::new();

    let batch_style = ProgressStyle::with_template(
        "{prefix:.bold} [{bar:30.green/dim}] {pos}/{len} plans  {elapsed_precise}",
    )
    .unwrap()
    .progress_chars("━━─");

    let plan_style = ProgressStyle::with_template(
        "  {prefix:.cyan} [{bar:20.cyan/dim}] {pos}/{len} groups  {msg}",
    )
    .unwrap()
    .progress_chars("━━─");

    let batch_bar = multi.add(ProgressBar::new(total_plans));
    batch_bar.set_style(batch_style);
    batch_bar.set_prefix("Batch");

    let plan_bar = multi.add(ProgressBar::new(total_groups));
    plan_bar.set_style(plan_style);
    plan_bar.set_prefix("Plan");

    (multi, batch_bar, plan_bar)
}

/// Create a simple spinner for indeterminate operations.
///
/// # Arguments
///
/// * `message` - Message to display next to the spinner
pub fn create_spinner(message: &str) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
    );
    spinner.set_message(message.to_string());
    spinner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_progress() {
        let (_, batch_bar, plan_bar) = create_progress(5, 7);
        assert_eq!(batch_bar.length(), Some(5));
        assert_eq!(plan_bar.length(), Some(7));
    }

    #[test]
    fn test_create_spinner() {
        let spinner = create_spinner("Loading...");
        // Just verify it doesn't panic
        spinner.finish_and_clear();
    }
}
