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

    /// Path to the batch state file.
    ///
    /// Anchored to the current working directory at call time. For reliable
    /// resume, the caller should ensure CWD matches the original batch run.
    pub fn state_path() -> PathBuf {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".gw/batch-state.json")
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
                self.circuit_breaker.consecutive_failures += 1;
                if self.circuit_breaker.consecutive_failures >= self.circuit_breaker.max_failures {
                    self.circuit_breaker.tripped = true;
                    tracing::warn!(
                        failures = self.circuit_breaker.consecutive_failures,
                        "Circuit breaker tripped after {} consecutive failures",
                        self.circuit_breaker.consecutive_failures
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

impl std::fmt::Display for PlanOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanOutcome::Passed => write!(f, "PASSED"),
            PlanOutcome::Failed => write!(f, "FAILED"),
            PlanOutcome::Skipped => write!(f, "SKIPPED"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_passed_result(plan: &str) -> PlanResult {
        PlanResult {
            plan: plan.to_string(),
            outcome: PlanOutcome::Passed,
            duration_secs: 10.0,
            completed_at: Utc::now(),
            error: None,
        }
    }

    fn make_failed_result(plan: &str) -> PlanResult {
        PlanResult {
            plan: plan.to_string(),
            outcome: PlanOutcome::Failed,
            duration_secs: 5.0,
            completed_at: Utc::now(),
            error: Some("test error".to_string()),
        }
    }

    #[test]
    fn test_batch_state_new() {
        let plans = vec!["a.md".into(), "b.md".into(), "c.md".into()];
        let state = BatchState::new(plans.clone());

        assert_eq!(state.plans, plans);
        assert_eq!(state.current_index, 0);
        assert!(state.results.is_empty());
        assert!(!state.is_circuit_broken());
        assert!(state.has_remaining());
        assert_eq!(state.next_plan(), Some("a.md"));
    }

    #[test]
    fn test_circuit_breaker_trips_after_3_failures() {
        let mut state = BatchState::new(vec!["a.md".into(), "b.md".into(), "c.md".into(), "d.md".into()]);

        // 2 failures — not tripped yet
        state.record_result(make_failed_result("a.md"));
        state.record_result(make_failed_result("b.md"));
        assert!(!state.is_circuit_broken());

        // 3rd failure — tripped
        state.record_result(make_failed_result("c.md"));
        assert!(state.is_circuit_broken());
    }

    #[test]
    fn test_circuit_breaker_resets_on_success() {
        let mut state = BatchState::new(vec!["a.md".into(), "b.md".into(), "c.md".into()]);

        state.record_result(make_failed_result("a.md"));
        state.record_result(make_failed_result("b.md"));
        assert_eq!(state.circuit_breaker.consecutive_failures, 2);

        // Success resets
        state.record_result(make_passed_result("c.md"));
        assert_eq!(state.circuit_breaker.consecutive_failures, 0);
        assert!(!state.is_circuit_broken());
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        // Override CWD temporarily for state_path
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let mut state = BatchState::new(vec!["plan1.md".into(), "plan2.md".into()]);
        state.record_result(make_passed_result("plan1.md"));
        state.save().unwrap();

        let loaded = BatchState::load(&BatchState::state_path()).unwrap();
        assert_eq!(loaded.batch_id, state.batch_id);
        assert_eq!(loaded.plans, state.plans);
        assert_eq!(loaded.current_index, 1);
        assert_eq!(loaded.results.len(), 1);
        assert_eq!(loaded.results[0].outcome, PlanOutcome::Passed);

        // Restore CWD
        std::env::set_current_dir(original_dir).unwrap();
    }

    #[test]
    fn test_current_index_advances() {
        let mut state = BatchState::new(vec!["a.md".into(), "b.md".into()]);

        assert_eq!(state.next_plan(), Some("a.md"));
        state.record_result(make_passed_result("a.md"));
        assert_eq!(state.next_plan(), Some("b.md"));
        state.record_result(make_passed_result("b.md"));
        assert_eq!(state.next_plan(), None);
        assert!(!state.has_remaining());
    }

    #[test]
    fn test_plan_outcome_display() {
        assert_eq!(format!("{}", PlanOutcome::Passed), "PASSED");
        assert_eq!(format!("{}", PlanOutcome::Failed), "FAILED");
        assert_eq!(format!("{}", PlanOutcome::Skipped), "SKIPPED");
    }
}
