#![allow(dead_code)]
//! Persistent batch state with atomic writes.
//!
//! State is written to `.gw/batch-state.json` using the write-to-tmp + rename
//! pattern, ensuring crash safety — `rename()` is atomic on POSIX, so the
//! state file is never partially written.

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context;
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Persistent batch execution state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchState {
    /// Unique batch run identifier.
    pub batch_id: String,
    /// All plan paths queued for execution.
    pub plans: Vec<String>,
    /// Index of the plan currently being executed (or next to execute).
    pub current_index: usize,
    /// Results for completed plans.
    pub results: Vec<PlanResult>,
    /// Circuit breaker state.
    pub circuit_breaker: CircuitBreakerState,
    /// Timestamp when this batch started.
    pub started_at: DateTime<Utc>,
    /// Timestamp of last state update.
    pub updated_at: DateTime<Utc>,
}

/// Result of a single plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    /// Plan file path.
    pub plan: String,
    /// Execution outcome.
    pub outcome: PlanOutcome,
    /// Duration in seconds.
    pub duration_secs: f64,
    /// Timestamp when this plan completed.
    pub completed_at: DateTime<Utc>,
    /// Error message if failed.
    pub error: Option<String>,
    /// Whether this failure was transient (API overload, network error).
    /// Transient failures do NOT count toward the circuit breaker threshold,
    /// since they may resolve on their own without indicating a systemic problem.
    #[serde(default)]
    pub transient: bool,
}

/// Outcome of a single plan execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanOutcome {
    /// Plan completed successfully.
    Passed,
    /// Plan failed during execution.
    Failed,
    /// Plan was skipped (circuit breaker tripped).
    Skipped,
}

/// Persisted circuit breaker state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerState {
    /// Number of consecutive failures.
    pub consecutive_failures: u32,
    /// Maximum consecutive failures before tripping.
    pub max_failures: u32,
    /// Whether the circuit breaker is currently tripped.
    pub tripped: bool,
}

impl BatchState {
    /// Create a new batch state for the given plans.
    pub fn new(plans: Vec<String>) -> Self {
        let now = Utc::now();
        let batch_id = format!("batch-{}", now.format("%Y%m%d-%H%M%S"));
        Self {
            batch_id,
            plans,
            current_index: 0,
            results: Vec::new(),
            circuit_breaker: CircuitBreakerState {
                consecutive_failures: 0,
                max_failures: 3,
                tripped: false,
            },
            started_at: now,
            updated_at: now,
        }
    }

    /// Set the maximum consecutive failures before the circuit breaker trips.
    /// A value of 0 disables the circuit breaker.
    pub fn with_max_failures(mut self, max_failures: u32) -> Self {
        self.circuit_breaker.max_failures = max_failures;
        self
    }

    /// Path to the batch state file.
    pub fn state_path() -> PathBuf {
        PathBuf::from(".gw/batch-state.json")
    }

    /// Atomically write state to disk (write-to-tmp + rename).
    pub fn save(&mut self) -> Result<()> {
        self.updated_at = Utc::now();

        let state_path = Self::state_path();

        // Ensure .gw directory exists
        if let Some(parent) = state_path.parent() {
            fs::create_dir_all(parent)
                .wrap_err("Failed to create .gw directory")?;
        }

        let tmp_path = state_path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .wrap_err("Failed to serialize batch state")?;

        // Write to tmp file first
        fs::write(&tmp_path, &json)
            .wrap_err_with(|| format!("Failed to write tmp state: {}", tmp_path.display()))?;

        // Fsync to ensure data is on disk before rename
        let f = fs::File::open(&tmp_path)
            .wrap_err_with(|| format!("Failed to open tmp file for fsync: {}", tmp_path.display()))?;
        f.sync_all()
            .wrap_err("Failed to fsync batch state to disk")?;

        // Atomic rename
        fs::rename(&tmp_path, &state_path)
            .wrap_err("Failed to atomically rename batch state")?;

        tracing::debug!(path = %state_path.display(), "Saved batch state");
        Ok(())
    }

    /// Load batch state from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read batch state: {}", path.display()))?;
        let state: Self = serde_json::from_str(&content)
            .wrap_err("Failed to parse batch state JSON")?;
        Ok(state)
    }

    /// Record a plan result and update circuit breaker.
    pub fn record_result(&mut self, result: PlanResult) {
        match result.outcome {
            PlanOutcome::Passed => {
                // Reset circuit breaker on success
                self.circuit_breaker.consecutive_failures = 0;
                self.circuit_breaker.tripped = false;
            }
            PlanOutcome::Failed => {
                // Transient failures (API overload, network) don't count toward
                // the circuit breaker — they may resolve without intervention.
                // Only deterministic failures (auth, bootstrap, crashes) trip it.
                if !result.transient {
                    self.circuit_breaker.consecutive_failures += 1;
                    if self.circuit_breaker.max_failures > 0
                        && self.circuit_breaker.consecutive_failures >= self.circuit_breaker.max_failures
                    {
                        self.circuit_breaker.tripped = true;
                        tracing::warn!(
                            failures = self.circuit_breaker.consecutive_failures,
                            "Circuit breaker tripped after {} consecutive deterministic failures",
                            self.circuit_breaker.consecutive_failures
                        );
                    }
                } else {
                    tracing::info!(
                        plan = %result.plan,
                        "Transient failure — not counting toward circuit breaker"
                    );
                }
            }
            PlanOutcome::Skipped => {
                // Skipped plans don't affect circuit breaker
            }
        }
        self.results.push(result);
        self.current_index += 1;
    }

    /// Check if the circuit breaker is tripped.
    pub fn is_circuit_broken(&self) -> bool {
        self.circuit_breaker.tripped
    }

    /// Check if there are remaining plans to execute.
    pub fn has_remaining(&self) -> bool {
        self.current_index < self.plans.len()
    }

    /// Get the next plan to execute.
    pub fn next_plan(&self) -> Option<&str> {
        self.plans.get(self.current_index).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(plan: &str, outcome: PlanOutcome, transient: bool) -> PlanResult {
        let error = if matches!(outcome, PlanOutcome::Failed) { Some("err".into()) } else { None };
        PlanResult {
            plan: plan.to_string(),
            outcome,
            duration_secs: 1.0,
            completed_at: Utc::now(),
            error,
            transient,
        }
    }

    #[test]
    fn test_circuit_breaker_trips_on_deterministic_failures() {
        let mut state = BatchState::new(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        state.record_result(make_result("a", PlanOutcome::Failed, false));
        assert!(!state.is_circuit_broken());
        state.record_result(make_result("b", PlanOutcome::Failed, false));
        assert!(!state.is_circuit_broken());
        state.record_result(make_result("c", PlanOutcome::Failed, false));
        assert!(state.is_circuit_broken(), "3 deterministic failures should trip breaker");
    }

    #[test]
    fn test_circuit_breaker_ignores_transient_failures() {
        let mut state = BatchState::new(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        // 3 transient failures should NOT trip the breaker
        state.record_result(make_result("a", PlanOutcome::Failed, true));
        state.record_result(make_result("b", PlanOutcome::Failed, true));
        state.record_result(make_result("c", PlanOutcome::Failed, true));
        assert!(!state.is_circuit_broken(), "Transient failures should not trip breaker");
    }

    #[test]
    fn test_circuit_breaker_mixed_transient_and_deterministic() {
        let mut state = BatchState::new(vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]);
        state.record_result(make_result("a", PlanOutcome::Failed, false)); // 1 det
        state.record_result(make_result("b", PlanOutcome::Failed, true));  // transient — ignored
        state.record_result(make_result("c", PlanOutcome::Failed, false)); // 2 det
        assert!(!state.is_circuit_broken());
        state.record_result(make_result("d", PlanOutcome::Failed, false)); // 3 det
        assert!(state.is_circuit_broken());
    }

    #[test]
    fn test_circuit_breaker_resets_on_success() {
        let mut state = BatchState::new(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        state.record_result(make_result("a", PlanOutcome::Failed, false));
        state.record_result(make_result("b", PlanOutcome::Failed, false));
        state.record_result(make_result("c", PlanOutcome::Passed, false)); // resets
        state.record_result(make_result("d", PlanOutcome::Failed, false));
        assert!(!state.is_circuit_broken(), "Success should reset consecutive counter");
    }

    #[test]
    fn test_transient_field_defaults_to_false() {
        // Ensure old serialized state without transient field works
        let json = r#"{
            "plan": "test.md",
            "outcome": "Failed",
            "duration_secs": 1.0,
            "completed_at": "2026-01-01T00:00:00Z",
            "error": "some error"
        }"#;
        let result: PlanResult = serde_json::from_str(json).unwrap();
        assert!(!result.transient, "Default should be false for backward compat");
    }
}

impl std::fmt::Display for PlanOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanOutcome::Passed => write!(f, "PASSED"),
            PlanOutcome::Failed => write!(f, "FAILED"),
            PlanOutcome::Skipped => write!(f, "SKIPPED"),
        }
    }
}
