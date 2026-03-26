//! Batch queue runner with circuit breaker protection.
//!
//! The `BatchRunner` manages sequential execution of multiple plans using a
//! VecDeque for FIFO ordering. Features include:
//!
//! - **Circuit breaker**: 3 consecutive failures → skip remaining plans
//! - **Atomic state persistence**: crash-safe state at `.gw/batch-state.json`
//! - **Resume support**: continue from where a previous batch left off
//! - **Pre-plan checks**: disk space verification before each plan
//! - **Inter-plan cleanup**: reuses `startup_cleanup()` between plans
//! - **Configurable timeout**: via `GW_PLAN_TIMEOUT` env var (default 3h)

use crate::batch::lock::InstanceLock;
use crate::batch::state::{BatchState, PlanOutcome, PlanResult};
use crate::cleanup::startup_cleanup;
use crate::config::phase_config::PhaseConfig;
use crate::engine::phase_executor::{ExecutorConfig, PhaseGroupExecutor, PhaseGroupState};
use chrono::Utc;
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::collections::VecDeque;
use std::env;
use std::path::Path;
use std::time::{Duration, Instant};

/// Minimum disk space required before starting a plan (2 GB in bytes).
const MIN_DISK_SPACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default plan timeout: 3 hours.
const DEFAULT_PLAN_TIMEOUT_SECS: u64 = 3 * 60 * 60;

/// Summary of a batch execution.
#[derive(Debug)]
pub struct BatchSummary {
    /// Batch identifier.
    pub batch_id: String,
    /// Number of plans that passed.
    pub passed: usize,
    /// Number of plans that failed.
    pub failed: usize,
    /// Number of plans that were skipped (circuit breaker).
    pub skipped: usize,
    /// Total duration of the batch run.
    pub duration: Duration,
    /// Per-plan results.
    pub results: Vec<PlanResult>,
}

impl std::fmt::Display for BatchSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== Batch Summary ({}) ===", self.batch_id)?;
        writeln!(
            f,
            "  Passed:  {} | Failed: {} | Skipped: {}",
            self.passed, self.failed, self.skipped
        )?;
        writeln!(f, "  Duration: {:.1}s", self.duration.as_secs_f64())?;
        writeln!(f)?;
        for result in &self.results {
            let icon = match result.outcome {
                PlanOutcome::Passed => "✓",
                PlanOutcome::Failed => "✗",
                PlanOutcome::Skipped => "⊘",
            };
            writeln!(
                f,
                "  {} {} ({:.1}s) — {}",
                icon, result.plan, result.duration_secs, result.outcome
            )?;
            if let Some(ref err) = result.error {
                writeln!(f, "    Error: {}", err)?;
            }
        }
        Ok(())
    }
}

/// Batch queue runner for executing multiple plans sequentially.
pub struct BatchRunner {
    /// FIFO queue of plan paths to execute.
    pub queue: VecDeque<String>,
    /// Persistent batch state.
    pub batch_state: BatchState,
    /// Instance lock guard (released on drop).
    pub lock_guard: InstanceLock,
    /// Per-plan timeout.
    plan_timeout: Duration,
}

impl BatchRunner {
    /// Create a new batch runner for the given plans.
    ///
    /// Acquires the instance lock and initializes batch state.
    pub fn new(plans: Vec<String>) -> Result<Self> {
        // Acquire instance lock (prevents concurrent gw instances)
        let lock_guard = InstanceLock::acquire()
            .wrap_err("Failed to acquire instance lock")?;

        let batch_state = BatchState::new(plans.clone());
        let queue = VecDeque::from(plans);

        let plan_timeout = parse_plan_timeout();

        tracing::info!(
            batch_id = %batch_state.batch_id,
            plan_count = queue.len(),
            timeout_secs = plan_timeout.as_secs(),
            "Created batch runner"
        );

        Ok(Self {
            queue,
            batch_state,
            lock_guard,
            plan_timeout,
        })
    }

    /// Resume a batch from a saved state file.
    ///
    /// Loads the state, rebuilds the queue from the current index,
    /// and continues execution.
    pub fn resume() -> Result<Self> {
        let state_path = BatchState::state_path();
        if !state_path.exists() {
            eyre::bail!(
                "No batch state found at {}. Nothing to resume.",
                state_path.display()
            );
        }

        let lock_guard = InstanceLock::acquire()
            .wrap_err("Failed to acquire instance lock for resume")?;

        let batch_state = BatchState::load(&state_path)
            .wrap_err("Failed to load batch state for resume")?;

        // Rebuild queue from remaining plans
        let remaining: VecDeque<String> = batch_state
            .plans
            .iter()
            .skip(batch_state.current_index)
            .cloned()
            .collect();

        let plan_timeout = parse_plan_timeout();

        tracing::info!(
            batch_id = %batch_state.batch_id,
            resumed_from = batch_state.current_index,
            remaining = remaining.len(),
            "Resuming batch"
        );

        Ok(Self {
            queue: remaining,
            batch_state,
            lock_guard,
            plan_timeout,
        })
    }

    /// Execute all plans in the batch queue.
    pub fn run(
        &mut self,
        config: &PhaseConfig,
        exec_config: &ExecutorConfig,
    ) -> Result<BatchSummary> {
        let batch_start = Instant::now();

        // Save initial state
        self.batch_state.save()?;

        println!(
            "=== Batch Run: {} ({} plans) ===",
            self.batch_state.batch_id,
            self.queue.len()
        );
        println!();

        while let Some(plan) = self.queue.pop_front() {
            // Check circuit breaker
            if self.batch_state.is_circuit_broken() {
                tracing::warn!(plan = %plan, "Circuit breaker tripped — skipping remaining plans");
                let result = PlanResult {
                    plan: plan.clone(),
                    outcome: PlanOutcome::Skipped,
                    duration_secs: 0.0,
                    completed_at: Utc::now(),
                    error: Some("Circuit breaker tripped".to_string()),
                };
                self.batch_state.record_result(result);
                self.batch_state.save()?;
                // Skip remaining plans too
                while let Some(remaining) = self.queue.pop_front() {
                    let skip_result = PlanResult {
                        plan: remaining,
                        outcome: PlanOutcome::Skipped,
                        duration_secs: 0.0,
                        completed_at: Utc::now(),
                        error: Some("Circuit breaker tripped".to_string()),
                    };
                    self.batch_state.record_result(skip_result);
                }
                self.batch_state.save()?;
                break;
            }

            // Pre-plan disk space check
            if let Err(e) = self.check_disk_space() {
                tracing::error!(error = %e, "Disk space check failed");
                let result = PlanResult {
                    plan: plan.clone(),
                    outcome: PlanOutcome::Failed,
                    duration_secs: 0.0,
                    completed_at: Utc::now(),
                    error: Some(format!("Disk space check failed: {}", e)),
                };
                self.batch_state.record_result(result);
                self.batch_state.save()?;
                continue;
            }

            // Inter-plan cleanup (skip for first plan)
            if !self.batch_state.results.is_empty() {
                self.inter_plan_cleanup()?;
            }

            // Execute the plan
            let plan_idx = self.batch_state.current_index + 1;
            let total = self.batch_state.plans.len();
            println!("--- [{}/{}] Executing: {} ---", plan_idx, total, plan);

            let result = self.execute_plan(&plan, config, exec_config);
            self.batch_state.record_result(result);
            self.batch_state.save()?;
        }

        // Build summary
        let summary = self.build_summary(batch_start.elapsed());

        // Release lock explicitly
        self.lock_guard.release()?;

        Ok(summary)
    }

    /// Execute a single plan through the PhaseGroupExecutor.
    fn execute_plan(
        &self,
        plan: &str,
        config: &PhaseConfig,
        exec_config: &ExecutorConfig,
    ) -> PlanResult {
        let plan_start = Instant::now();
        let plan_path = Path::new(plan);

        // Validate plan exists
        if !plan_path.exists() {
            return PlanResult {
                plan: plan.to_string(),
                outcome: PlanOutcome::Failed,
                duration_secs: 0.0,
                completed_at: Utc::now(),
                error: Some(format!("Plan file not found: {}", plan)),
            };
        }

        // Create executor and run with timeout awareness
        let mut executor = PhaseGroupExecutor::new(config.clone());
        match executor.execute_plan(plan_path, exec_config) {
            Ok(results) => {
                let duration = plan_start.elapsed();
                let any_failed = results
                    .iter()
                    .any(|r| matches!(r.state, PhaseGroupState::Failed { .. }));

                if any_failed {
                    let errors: Vec<String> = results
                        .iter()
                        .filter_map(|r| r.error_message.clone())
                        .collect();
                    PlanResult {
                        plan: plan.to_string(),
                        outcome: PlanOutcome::Failed,
                        duration_secs: duration.as_secs_f64(),
                        completed_at: Utc::now(),
                        error: Some(errors.join("; ")),
                    }
                } else {
                    tracing::info!(plan = %plan, duration_secs = duration.as_secs_f64(), "Plan completed successfully");
                    PlanResult {
                        plan: plan.to_string(),
                        outcome: PlanOutcome::Passed,
                        duration_secs: duration.as_secs_f64(),
                        completed_at: Utc::now(),
                        error: None,
                    }
                }
            }
            Err(e) => {
                let duration = plan_start.elapsed();
                tracing::error!(plan = %plan, error = %e, "Plan execution failed");

                // Check if timeout exceeded
                let error_msg = if duration >= self.plan_timeout {
                    format!("Plan timed out after {:.0}s: {}", duration.as_secs_f64(), e)
                } else {
                    format!("{}", e)
                };

                PlanResult {
                    plan: plan.to_string(),
                    outcome: PlanOutcome::Failed,
                    duration_secs: duration.as_secs_f64(),
                    completed_at: Utc::now(),
                    error: Some(error_msg),
                }
            }
        }
    }

    /// Run cleanup between plans (reuse startup_cleanup).
    fn inter_plan_cleanup(&self) -> Result<()> {
        tracing::info!("Running inter-plan cleanup");
        startup_cleanup().wrap_err("Inter-plan cleanup failed")?;
        Ok(())
    }

    /// Check that minimum disk space is available.
    fn check_disk_space(&self) -> Result<()> {
        use sysinfo::Disks;

        let disks = Disks::new_with_refreshed_list();

        // Find the disk for the current working directory
        let cwd = env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        // Find the best-matching mount point
        let available = disks
            .iter()
            .filter(|d| cwd.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len())
            .map(|d| d.available_space());

        match available {
            Some(space) if space < MIN_DISK_SPACE_BYTES => {
                let gb = space as f64 / (1024.0 * 1024.0 * 1024.0);
                eyre::bail!(
                    "Insufficient disk space: {:.2} GB available, {:.0} GB required",
                    gb,
                    MIN_DISK_SPACE_BYTES as f64 / (1024.0 * 1024.0 * 1024.0)
                );
            }
            Some(space) => {
                let gb = space as f64 / (1024.0 * 1024.0 * 1024.0);
                tracing::debug!(available_gb = format!("{:.2}", gb), "Disk space check passed");
                Ok(())
            }
            None => {
                tracing::warn!("Could not determine disk space — proceeding anyway");
                Ok(())
            }
        }
    }

    /// Build the final batch summary.
    fn build_summary(&self, total_duration: Duration) -> BatchSummary {
        let passed = self
            .batch_state
            .results
            .iter()
            .filter(|r| r.outcome == PlanOutcome::Passed)
            .count();
        let failed = self
            .batch_state
            .results
            .iter()
            .filter(|r| r.outcome == PlanOutcome::Failed)
            .count();
        let skipped = self
            .batch_state
            .results
            .iter()
            .filter(|r| r.outcome == PlanOutcome::Skipped)
            .count();

        BatchSummary {
            batch_id: self.batch_state.batch_id.clone(),
            passed,
            failed,
            skipped,
            duration: total_duration,
            results: self.batch_state.results.clone(),
        }
    }
}

/// Parse the plan timeout from the `GW_PLAN_TIMEOUT` env var.
///
/// Format: integer seconds, or suffixed like "3h", "180m", "10800s".
/// Defaults to 3 hours.
fn parse_plan_timeout() -> Duration {
    match env::var("GW_PLAN_TIMEOUT") {
        Ok(val) => {
            let val = val.trim();
            let (num_str, multiplier) = if val.ends_with('h') {
                (&val[..val.len() - 1], 3600u64)
            } else if val.ends_with('m') {
                (&val[..val.len() - 1], 60u64)
            } else if val.ends_with('s') {
                (&val[..val.len() - 1], 1u64)
            } else {
                (val, 1u64)
            };

            match num_str.parse::<u64>() {
                Ok(n) => {
                    let secs = n * multiplier;
                    tracing::info!(timeout_secs = secs, "Using GW_PLAN_TIMEOUT");
                    Duration::from_secs(secs)
                }
                Err(_) => {
                    tracing::warn!(
                        value = val,
                        "Invalid GW_PLAN_TIMEOUT, using default 3h"
                    );
                    Duration::from_secs(DEFAULT_PLAN_TIMEOUT_SECS)
                }
            }
        }
        Err(_) => Duration::from_secs(DEFAULT_PLAN_TIMEOUT_SECS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All parse_plan_timeout tests in a single sequential test to avoid
    /// env var race conditions between parallel test threads.
    #[test]
    fn test_parse_plan_timeout_all() {
        // Save any existing value
        let saved = std::env::var("GW_PLAN_TIMEOUT").ok();

        // Default (env var not set)
        std::env::remove_var("GW_PLAN_TIMEOUT");
        assert_eq!(parse_plan_timeout(), Duration::from_secs(3 * 3600));

        // Hours
        std::env::set_var("GW_PLAN_TIMEOUT", "2h");
        assert_eq!(parse_plan_timeout(), Duration::from_secs(2 * 3600));

        // Minutes
        std::env::set_var("GW_PLAN_TIMEOUT", "90m");
        assert_eq!(parse_plan_timeout(), Duration::from_secs(90 * 60));

        // Seconds suffix
        std::env::set_var("GW_PLAN_TIMEOUT", "7200s");
        assert_eq!(parse_plan_timeout(), Duration::from_secs(7200));

        // Bare number (seconds)
        std::env::set_var("GW_PLAN_TIMEOUT", "600");
        assert_eq!(parse_plan_timeout(), Duration::from_secs(600));

        // Invalid → default (3h)
        std::env::set_var("GW_PLAN_TIMEOUT", "not_a_number");
        assert_eq!(parse_plan_timeout(), Duration::from_secs(3 * 3600));

        // Restore
        match saved {
            Some(val) => std::env::set_var("GW_PLAN_TIMEOUT", val),
            None => std::env::remove_var("GW_PLAN_TIMEOUT"),
        }
    }

    #[test]
    fn test_batch_summary_display() {
        let summary = BatchSummary {
            batch_id: "test-batch".to_string(),
            passed: 3,
            failed: 1,
            skipped: 0,
            duration: Duration::from_secs(120),
            results: vec![],
        };

        let display = format!("{}", summary);
        assert!(display.contains("test-batch"));
        assert!(display.contains("Passed:  3"));
        assert!(display.contains("Failed: 1"));
    }
}
