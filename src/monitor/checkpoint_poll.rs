#![allow(dead_code)]
//! Checkpoint file watcher for monitoring phase progress.
//!
//! This module provides utilities for watching checkpoint.json files
//! and detecting phase progress during execution.
//!
//! # Polling Strategy
//!
//! We poll the checkpoint file every 3 seconds rather than using
//! filesystem notification (inotify/FSEvents) because:
//!
//! 1. Cross-platform compatibility (works the same on macOS/Linux)
//! 2. Atomic writes mean we always see consistent data
//! 3. 3-second polling is fast enough for user feedback
//!
//! # Example
//!
//! ```ignore
//! use greater_will::monitor::checkpoint_poll::{CheckpointWatcher, CheckpointDiff};
//!
//! let mut watcher = CheckpointWatcher::new(".rune/arc/arc-123/checkpoint.json");
//!
//! loop {
//!     let diff = watcher.poll()?;
//!
//!     match diff {
//!         CheckpointDiff::Progressed { new_phase, old_phase } => {
//!             println!("Phase changed: {} -> {}", old_phase, new_phase);
//!         }
//!         CheckpointDiff::Completed => {
//!             println!("All phases completed");
//!             break;
//!         }
//!         CheckpointDiff::NoChange => {
//!             // Continue waiting
//!         }
//!     }
//!
//!     thread::sleep(Duration::from_secs(3));
//! }
//! ```

use crate::checkpoint::reader::read_checkpoint;
use crate::checkpoint::schema::Checkpoint;
use color_eyre::Result;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Default polling interval in milliseconds.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 3000;

/// Checkpoint watcher for monitoring phase progress.
///
/// Polls the checkpoint file and detects changes in phase status.
#[derive(Debug)]
pub struct CheckpointWatcher {
    /// Path to the checkpoint.json file.
    checkpoint_path: PathBuf,
    /// Last known checkpoint state.
    last_checkpoint: Option<Checkpoint>,
    /// Phase sequence at start of monitoring.
    starting_sequence: Option<u32>,
    /// When monitoring started.
    started_at: Instant,
    /// Number of polls performed.
    poll_count: u64,
}

impl CheckpointWatcher {
    /// Create a new checkpoint watcher.
    pub fn new(checkpoint_path: impl Into<PathBuf>) -> Self {
        Self {
            checkpoint_path: checkpoint_path.into(),
            last_checkpoint: None,
            starting_sequence: None,
            started_at: Instant::now(),
            poll_count: 0,
        }
    }

    /// Get the path being watched.
    pub fn path(&self) -> &std::path::Path {
        &self.checkpoint_path
    }

    /// Get elapsed time since monitoring started.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// Get the number of polls performed.
    pub fn poll_count(&self) -> u64 {
        self.poll_count
    }

    /// Read the current checkpoint.
    pub fn read_current(&self) -> Result<Option<Checkpoint>> {
        if !self.checkpoint_path.exists() {
            return Ok(None);
        }

        match read_checkpoint(&self.checkpoint_path) {
            Ok(cp) => Ok(Some(cp)),
            Err(e) => {
                debug!(error = %e, "Failed to read checkpoint (may not exist yet)");
                Ok(None)
            }
        }
    }

    /// Poll for changes and return the diff.
    ///
    /// This should be called every `DEFAULT_POLL_INTERVAL_MS`.
    pub fn poll(&mut self) -> Result<CheckpointDiff> {
        self.poll_count += 1;

        let current = self.read_current()?;

        match (current, &self.last_checkpoint) {
            (None, None) => {
                // No checkpoint yet
                Ok(CheckpointDiff::NotStarted)
            }
            (Some(current), None) => {
                // First read
                self.starting_sequence = current.phase_sequence;
                self.last_checkpoint = Some(current.clone());
                Ok(CheckpointDiff::Started {
                    arc_id: current.id.clone(),
                    phase: current.current_phase().unwrap_or("unknown").to_string(),
                })
            }
            (None, Some(_)) => {
                // Checkpoint disappeared (shouldn't happen)
                warn!("Checkpoint file disappeared");
                self.last_checkpoint = None;
                Ok(CheckpointDiff::NotFound)
            }
            (Some(current), Some(last)) => {
                // Compare for changes
                let diff = self.compute_diff(last, &current);
                self.last_checkpoint = Some(current);
                Ok(diff)
            }
        }
    }

    /// Compute the difference between two checkpoints.
    fn compute_diff(&self, old: &Checkpoint, new: &Checkpoint) -> CheckpointDiff {
        // Check for completion
        if new.is_complete() {
            return CheckpointDiff::Completed;
        }

        // Check for phase sequence change
        let old_seq = old.phase_sequence.unwrap_or(0);
        let new_seq = new.phase_sequence.unwrap_or(0);

        if new_seq > old_seq {
            let old_phase = old.current_phase().unwrap_or("unknown");
            let new_phase = new.current_phase().unwrap_or("unknown");

            info!(
                old_phase = old_phase,
                new_phase = new_phase,
                "Phase sequence advanced"
            );

            return CheckpointDiff::Progressed {
                old_phase: old_phase.to_string(),
                new_phase: new_phase.to_string(),
            };
        }

        // Check for phase status changes
        let changed_phases = self.find_changed_phases(old, new);
        if !changed_phases.is_empty() {
            return CheckpointDiff::PhasesChanged {
                phases: changed_phases,
            };
        }

        // No changes
        CheckpointDiff::NoChange
    }

    /// Find phases that changed status between old and new checkpoints.
    fn find_changed_phases(&self, old: &Checkpoint, new: &Checkpoint) -> Vec<PhaseChange> {
        let mut changes = Vec::new();

        for (phase_name, new_status) in &new.phases {
            let old_status = old.phases.get(phase_name);

            match old_status {
                Some(old) if old.status != new_status.status => {
                    changes.push(PhaseChange {
                        phase: phase_name.clone(),
                        old_status: old.status.clone(),
                        new_status: new_status.status.clone(),
                    });
                }
                None => {
                    // New phase appeared
                    changes.push(PhaseChange {
                        phase: phase_name.clone(),
                        old_status: "none".to_string(),
                        new_status: new_status.status.clone(),
                    });
                }
                _ => {}
            }
        }

        changes
    }

    /// Check if the checkpoint has progressed since monitoring started.
    pub fn has_progressed(&mut self) -> Result<bool> {
        let current = self.read_current()?;

        match (current, self.starting_sequence) {
            (Some(cp), Some(start_seq)) => {
                let current_seq = cp.phase_sequence.unwrap_or(0);
                Ok(current_seq > start_seq)
            }
            _ => Ok(false),
        }
    }

    /// Check if a specific phase has completed.
    pub fn is_phase_complete(&self, phase_name: &str) -> Result<bool> {
        let current = self.read_current()?;

        match current {
            Some(cp) => {
                let status = cp.phases.get(phase_name);
                match status {
                    Some(s) => Ok(s.status == "completed" || s.status == "skipped"),
                    None => Ok(false),
                }
            }
            None => Ok(false),
        }
    }

    /// Check if all phases in a group have completed.
    pub fn is_group_complete(&self, group_phases: &[&str]) -> Result<bool> {
        let current = self.read_current()?;

        match current {
            Some(cp) => {
                let all_complete = group_phases.iter().all(|phase| {
                    cp.phases
                        .get(*phase)
                        .map(|s| s.status == "completed" || s.status == "skipped")
                        .unwrap_or(false)
                });
                Ok(all_complete)
            }
            None => Ok(false),
        }
    }

    /// Get the current phase name.
    pub fn current_phase(&self) -> Result<Option<String>> {
        let current = self.read_current()?;

        match current {
            Some(cp) => Ok(cp.current_phase().map(|s| s.to_string())),
            None => Ok(None),
        }
    }

    /// Get the current phase sequence index.
    pub fn current_sequence(&self) -> Result<Option<u32>> {
        let current = self.read_current()?;

        match current {
            Some(cp) => Ok(cp.phase_sequence),
            None => Ok(None),
        }
    }

    /// Get a summary of the current checkpoint state.
    pub fn summary(&self) -> Result<Option<CheckpointSummary>> {
        let current = self.read_current()?;

        match current {
            Some(cp) => Ok(Some(CheckpointSummary {
                arc_id: cp.id.clone(),
                current_phase: cp.current_phase().map(|s| s.to_string()),
                phase_sequence: cp.phase_sequence,
                completed_count: cp.count_by_status("completed"),
                skipped_count: cp.count_by_status("skipped"),
                pending_count: cp.count_by_status("pending"),
                in_progress_count: cp.count_by_status("in_progress"),
                failed_count: cp.count_by_status("failed"),
            })),
            None => Ok(None),
        }
    }
}

/// Difference between two checkpoint states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointDiff {
    /// Checkpoint file doesn't exist yet.
    NotStarted,

    /// Checkpoint file disappeared.
    NotFound,

    /// First checkpoint read.
    Started {
        arc_id: String,
        phase: String,
    },

    /// Phase sequence advanced.
    Progressed {
        old_phase: String,
        new_phase: String,
    },

    /// Individual phase statuses changed.
    PhasesChanged {
        phases: Vec<PhaseChange>,
    },

    /// All phases completed.
    Completed,

    /// No changes detected.
    NoChange,
}

/// A change to a single phase's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    /// Phase name.
    pub phase: String,
    /// Previous status.
    pub old_status: String,
    /// New status.
    pub new_status: String,
}

/// Summary of checkpoint state.
#[derive(Debug, Clone)]
pub struct CheckpointSummary {
    /// Arc ID.
    pub arc_id: String,
    /// Current phase name.
    pub current_phase: Option<String>,
    /// Current phase sequence index.
    pub phase_sequence: Option<u32>,
    /// Number of completed phases.
    pub completed_count: usize,
    /// Number of skipped phases.
    pub skipped_count: usize,
    /// Number of pending phases.
    pub pending_count: usize,
    /// Number of in-progress phases.
    pub in_progress_count: usize,
    /// Number of failed phases.
    pub failed_count: usize,
}

impl CheckpointSummary {
    /// Get the total number of phases tracked.
    pub fn total_tracked(&self) -> usize {
        self.completed_count
            + self.skipped_count
            + self.pending_count
            + self.in_progress_count
            + self.failed_count
    }

    /// Get progress percentage.
    pub fn progress_percent(&self) -> f64 {
        let total = self.total_tracked();
        if total == 0 {
            return 0.0;
        }
        (self.completed_count + self.skipped_count) as f64 / total as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_watcher_new() {
        let watcher = CheckpointWatcher::new("/tmp/checkpoint.json");
        assert_eq!(watcher.path().to_str(), Some("/tmp/checkpoint.json"));
        assert_eq!(watcher.poll_count(), 0);
    }

    #[test]
    fn test_checkpoint_watcher_elapsed() {
        let watcher = CheckpointWatcher::new("/tmp/checkpoint.json");
        // Should be very small
        assert!(watcher.elapsed().as_millis() < 100);
    }

    #[test]
    fn test_checkpoint_diff_is_progress() {
        let diff = CheckpointDiff::Progressed {
            old_phase: "forge".to_string(),
            new_phase: "work".to_string(),
        };

        assert!(matches!(diff, CheckpointDiff::Progressed { .. }));
    }

    #[test]
    fn test_phase_change() {
        let change = PhaseChange {
            phase: "forge".to_string(),
            old_status: "in_progress".to_string(),
            new_status: "completed".to_string(),
        };

        assert_eq!(change.phase, "forge");
        assert_eq!(change.old_status, "in_progress");
        assert_eq!(change.new_status, "completed");
    }

    #[test]
    fn test_checkpoint_summary_progress() {
        let summary = CheckpointSummary {
            arc_id: "arc-test".to_string(),
            current_phase: Some("work".to_string()),
            phase_sequence: Some(9),
            completed_count: 5,
            skipped_count: 2,
            pending_count: 30,
            in_progress_count: 1,
            failed_count: 0,
        };

        assert_eq!(summary.total_tracked(), 38);
        assert!((summary.progress_percent() - 18.42).abs() < 0.1);
    }

    #[test]
    fn test_checkpoint_summary_zero_total() {
        let summary = CheckpointSummary {
            arc_id: "arc-test".to_string(),
            current_phase: None,
            phase_sequence: None,
            completed_count: 0,
            skipped_count: 0,
            pending_count: 0,
            in_progress_count: 0,
            failed_count: 0,
        };

        assert_eq!(summary.total_tracked(), 0);
        assert_eq!(summary.progress_percent(), 0.0);
    }
}