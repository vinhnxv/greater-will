//! Phase navigation — infer prev/current/next phase from checkpoint data.
//!
//! This module provides ground-truth phase tracking by scanning the `phases`
//! map in checkpoint.json, rather than relying on the `phase_sequence` field
//! which Rune often fails to update.
//!
//! # Three States
//!
//! The monitor needs to distinguish:
//! 1. **Running**: A phase has `status: "in_progress"` — apply phase timeout
//! 2. **Transitioning**: No `in_progress` phase, but completed phases exist — apply transition timeout
//! 3. **Waiting**: Nothing started yet — wait patiently
//!
//! # Transition Gap Detection
//!
//! Between phases, there's a natural gap where Rune finishes one phase and
//! starts the next. This gap should be short (< 3 minutes). If it exceeds
//! the transition timeout, the arc may be stuck between phases.

use crate::checkpoint::schema::{Checkpoint, PhasePosition};
use chrono::{DateTime, Utc};

/// Default transition gap timeout in seconds.
/// If no phase is `in_progress` for longer than this after the last phase
/// completed, the arc is likely stuck between phases.
pub const DEFAULT_TRANSITION_TIMEOUT_SECS: u64 = 180; // 3 minutes

/// Navigation context: where the pipeline was, is, and will be.
#[derive(Debug, Clone)]
pub struct PhaseNavigation {
    /// Previous completed phase with its duration.
    pub prev: Option<PhaseInfo>,
    /// Currently active phase (in_progress) with elapsed time.
    pub current: Option<PhaseInfo>,
    /// Next pending phase name.
    pub next: Option<&'static str>,
    /// The underlying position state.
    pub position: PhasePosition,
}

/// Info about a single phase: name + timing.
#[derive(Debug, Clone)]
pub struct PhaseInfo {
    pub name: &'static str,
    /// For completed phases: duration in seconds.
    /// For in_progress phases: elapsed since started_at.
    pub duration_secs: Option<i64>,
}

impl PhaseNavigation {
    /// Returns true if in transition gap (between phases, nothing running).
    pub fn is_transitioning(&self) -> bool {
        self.position.is_transitioning()
    }

    /// Get the effective phase name for profile lookup and display.
    pub fn effective_phase_name(&self) -> Option<&'static str> {
        self.position.effective_phase()
    }

    /// Compute transition gap duration in seconds.
    /// Only meaningful when `is_transitioning()` is true.
    pub fn transition_gap_secs(&self) -> Option<u64> {
        self.position.transition_gap_secs()
    }
}

/// Compute phase navigation from a checkpoint.
///
/// Scans the `phases` map against `PHASE_ORDER` to determine
/// previous, current, and next phase with timing information.
pub fn compute_phase_navigation(checkpoint: &Checkpoint) -> PhaseNavigation {
    use crate::checkpoint::phase_order::PHASE_ORDER;

    let position = checkpoint.infer_phase_position();

    match &position {
        PhasePosition::Running { phase, started_at } => {
            let elapsed = started_at.as_ref().and_then(|ts| {
                DateTime::parse_from_rfc3339(ts).ok().map(|dt| {
                    Utc::now()
                        .signed_duration_since(dt.with_timezone(&Utc))
                        .num_seconds()
                })
            });

            let current = Some(PhaseInfo {
                name: phase,
                duration_secs: elapsed,
            });

            // Find previous completed phase (scan backwards from current)
            let current_idx = PHASE_ORDER.iter().position(|&p| p == *phase);
            let prev = current_idx.and_then(|ci| {
                find_prev_completed(checkpoint, ci)
            });

            // Find next pending phase (scan forward from current)
            let next = current_idx.and_then(|ci| {
                find_next_pending(checkpoint, ci + 1)
            });

            PhaseNavigation { prev, current, next, position }
        }

        PhasePosition::Transitioning { last_completed, last_completed_at, next_pending } => {
            let prev_duration = last_completed_at.as_ref().and_then(|completed_ts| {
                let phase = checkpoint.phases.get(*last_completed)?;
                let started_ts = phase.started_at.as_ref()?;
                let start = DateTime::parse_from_rfc3339(started_ts).ok()?;
                let end = DateTime::parse_from_rfc3339(completed_ts).ok()?;
                Some(end.signed_duration_since(start).num_seconds())
            });

            let prev = Some(PhaseInfo {
                name: last_completed,
                duration_secs: prev_duration,
            });

            // next_pending may be empty string if no more pending phases
            let next = if next_pending.is_empty() { None } else { Some(*next_pending) };

            PhaseNavigation {
                prev,
                current: None, // Nothing running — this IS the transition gap
                next,
                position,
            }
        }

        PhasePosition::WaitingToStart { first_pending } => {
            PhaseNavigation {
                prev: None,
                current: None,
                next: Some(first_pending),
                position,
            }
        }

        PhasePosition::AllDone => {
            // Find the last completed phase for prev
            let prev = PHASE_ORDER.iter().rev().find_map(|&p| {
                let phase = checkpoint.phases.get(p)?;
                if phase.status == "completed" {
                    let duration = compute_phase_duration(phase);
                    Some(PhaseInfo { name: p, duration_secs: duration })
                } else {
                    None
                }
            });

            PhaseNavigation {
                prev,
                current: None,
                next: None,
                position,
            }
        }

        PhasePosition::Unknown => {
            PhaseNavigation {
                prev: None,
                current: None,
                next: None,
                position,
            }
        }
    }
}

/// Find the previous completed phase scanning backwards from `before_idx`.
fn find_prev_completed(checkpoint: &Checkpoint, before_idx: usize) -> Option<PhaseInfo> {
    use crate::checkpoint::phase_order::PHASE_ORDER;

    (0..before_idx).rev().find_map(|i| {
        let phase_name = PHASE_ORDER[i];
        let phase = checkpoint.phases.get(phase_name)?;
        if phase.status == "completed" {
            let duration = compute_phase_duration(phase);
            Some(PhaseInfo { name: phase_name, duration_secs: duration })
        } else {
            None
        }
    })
}

/// Find the next pending phase scanning forward from `start_idx`.
fn find_next_pending(checkpoint: &Checkpoint, start_idx: usize) -> Option<&'static str> {
    use crate::checkpoint::phase_order::PHASE_ORDER;

    PHASE_ORDER.iter().skip(start_idx).find_map(|&p| {
        let phase = checkpoint.phases.get(p)?;
        if phase.status == "pending" || phase.status.is_empty() {
            Some(p)
        } else {
            None
        }
    })
}

/// Compute duration of a completed phase from started_at and completed_at.
fn compute_phase_duration(phase: &crate::checkpoint::schema::PhaseStatus) -> Option<i64> {
    let started = phase.started_at.as_ref()?;
    let completed = phase.completed_at.as_ref()?;
    let start = DateTime::parse_from_rfc3339(started).ok()?;
    let end = DateTime::parse_from_rfc3339(completed).ok()?;
    Some(end.signed_duration_since(start).num_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::schema::{Checkpoint, PhaseStatus};
    #[allow(unused_imports)]
    use std::collections::HashMap;

    fn make_checkpoint(phases: HashMap<String, PhaseStatus>) -> Checkpoint {
        let mut cp = Checkpoint::default();
        cp.id = "arc-test".into();
        cp.schema_version = Some(27);
        cp.plan_file = "plan.md".into();
        cp.started_at = "2026-03-20T00:00:00Z".into();
        cp.phase_sequence = Some(1); // intentionally stale!
        cp.phases = phases;
        cp
    }

    #[test]
    fn test_running_phase_detected() {
        let mut phases = HashMap::new();
        phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:05:00Z".into()),
            completed_at: Some("2026-03-20T00:08:00Z".into()),
            ..Default::default()
        });
        phases.insert("plan_review".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:08:30Z".into()),
            ..Default::default()
        });
        phases.insert("plan_refine".into(), PhaseStatus::pending());
        phases.insert("verification".into(), PhaseStatus::pending());

        let cp = make_checkpoint(phases);
        let nav = compute_phase_navigation(&cp);

        assert!(nav.current.is_some());
        assert_eq!(nav.current.as_ref().unwrap().name, "plan_review");
        assert_eq!(nav.prev.as_ref().unwrap().name, "forge_qa");
        assert_eq!(nav.next, Some("plan_refine"));
        assert!(!nav.is_transitioning());
    }

    #[test]
    fn test_transition_detected() {
        // forge_qa completed, plan_review pending, nothing in_progress
        let mut phases = HashMap::new();
        phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:05:00Z".into()),
            completed_at: Some("2026-03-20T00:08:00Z".into()),
            ..Default::default()
        });
        phases.insert("plan_review".into(), PhaseStatus::pending());
        phases.insert("plan_refine".into(), PhaseStatus::pending());

        let cp = make_checkpoint(phases);
        let nav = compute_phase_navigation(&cp);

        assert!(nav.is_transitioning());
        assert!(nav.current.is_none());
        assert_eq!(nav.prev.as_ref().unwrap().name, "forge_qa");
        assert_eq!(nav.next, Some("plan_review"));
        assert_eq!(nav.effective_phase_name(), Some("plan_review"));
    }

    #[test]
    fn test_skipped_phases_handled() {
        let mut phases = HashMap::new();
        phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        phases.insert("forge_qa".into(), PhaseStatus {
            status: "skipped".into(),
            ..Default::default()
        });
        phases.insert("plan_review".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:05:30Z".into()),
            ..Default::default()
        });
        phases.insert("plan_refine".into(), PhaseStatus::pending());

        let cp = make_checkpoint(phases);
        let nav = compute_phase_navigation(&cp);

        assert_eq!(nav.current.as_ref().unwrap().name, "plan_review");
        // prev should be forge (not forge_qa which was skipped)
        assert_eq!(nav.prev.as_ref().unwrap().name, "forge");
    }

    #[test]
    fn test_waiting_to_start() {
        let mut phases = HashMap::new();
        phases.insert("forge".into(), PhaseStatus::pending());
        phases.insert("forge_qa".into(), PhaseStatus::pending());

        let cp = make_checkpoint(phases);
        let nav = compute_phase_navigation(&cp);

        assert!(nav.prev.is_none());
        assert!(nav.current.is_none());
        assert_eq!(nav.next, Some("forge"));
        assert_eq!(nav.effective_phase_name(), Some("forge"));
    }

    #[test]
    fn test_stale_phase_sequence_ignored() {
        // Simulates the real bug: phase_sequence=1 (forge_qa) but
        // pipeline has progressed to plan_review being in_progress
        let mut phases = HashMap::new();
        phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:05:00Z".into()),
            completed_at: Some("2026-03-20T00:08:00Z".into()),
            ..Default::default()
        });
        phases.insert("plan_review".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:08:00Z".into()),
            completed_at: Some("2026-03-20T00:12:00Z".into()),
            ..Default::default()
        });
        phases.insert("plan_refine".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:12:00Z".into()),
            completed_at: Some("2026-03-20T00:12:01Z".into()),
            ..Default::default()
        });
        phases.insert("verification".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:13:00Z".into()),
            ..Default::default()
        });
        phases.insert("work".into(), PhaseStatus::pending());

        let mut cp = make_checkpoint(phases);
        cp.phase_sequence = Some(1); // stale! points to forge_qa

        let nav = compute_phase_navigation(&cp);

        // Should detect verification as current, NOT forge_qa
        assert_eq!(nav.current.as_ref().unwrap().name, "verification");
        assert_eq!(nav.prev.as_ref().unwrap().name, "plan_refine");
        assert_eq!(nav.next, Some("work"));
        assert_eq!(nav.effective_phase_name(), Some("verification"));
    }

    #[test]
    fn test_prev_phase_duration() {
        let mut phases = HashMap::new();
        phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()), // 5 min
            ..Default::default()
        });
        phases.insert("forge_qa".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:05:30Z".into()),
            ..Default::default()
        });

        let cp = make_checkpoint(phases);
        let nav = compute_phase_navigation(&cp);

        assert_eq!(nav.prev.as_ref().unwrap().duration_secs, Some(300)); // 5 min
    }
}
