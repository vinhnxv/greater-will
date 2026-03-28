#![allow(dead_code)]
//! Phase order constants for Rune arc pipelines.
//!
//! This module defines the canonical order of all 41 arc phases.
//! Greater-will uses this to determine which phases to mark as completed
//! when manipulating checkpoints for session handoffs.

/// Canonical order of all 41 Rune arc phases.
///
/// This order is CRITICAL for checkpoint manipulation:
/// - `phase_sequence` field indexes into this array
/// - `mark_phases_completed_before()` uses this to determine which phases to mark
///
/// Source: Torrent's arc workflow definition
pub const PHASE_ORDER: &[&str] = &[
    // Group A: Planning & Forge
    "forge",
    "forge_qa",
    // Group B: Plan Review
    "plan_review",
    "plan_refine",
    // Group C: Verification
    "verification",
    "semantic_verification",
    // Group D: Design
    "design_extraction",
    "design_prototype",
    // Group E: Work Execution
    "task_decomposition",
    "work",
    "work_qa",
    "drift_review",
    // Group F: Design Verification
    "storybook_verification",
    "design_verification",
    "design_verification_qa",
    "ux_verification",
    // Group G: Gap Analysis
    "gap_analysis",
    "gap_analysis_qa",
    "codex_gap_analysis",
    "gap_remediation",
    // Group H: Inspection
    "inspect",
    "inspect_fix",
    "verify_inspect",
    "goldmask_verification",
    // Group I: Code Review
    "code_review",
    "code_review_qa",
    "goldmask_correlation",
    // Group J: Remediation
    "mend",
    "mend_qa",
    "verify_mend",
    "design_iteration",
    // Group K: Testing
    "test",
    "test_qa",
    "test_coverage_critique",
    // Group L: Deployment
    "deploy_verify",
    "pre_ship_validation",
    "release_quality_check",
    // Group M: Ship & Merge
    "ship",
    "bot_review_wait",
    "pr_comment_resolution",
    "merge",
];

/// Total number of phases in the arc pipeline.
pub const PHASE_COUNT: usize = PHASE_ORDER.len();

/// Fine-grained phase group boundaries for checkpoint manipulation.
///
/// **Important:** These 13 internal groups are used by the checkpoint system
/// (e.g., `mark_phases_completed_before()`) for precise phase status tracking.
/// They are intentionally more granular than the 7 TOML config groups (A-G)
/// which control session-level execution boundaries.
///
/// The two grouping levels serve different purposes:
/// - **PHASE_GROUPS (13):** Checkpoint progress tracking & phase completion logic
/// - **Config groups A-G (7):** Session boundaries — which phases run in one Claude session
///
/// All phases in PHASE_GROUPS must be a subset of PHASE_ORDER, and together
/// must cover all 41 phases exactly once.
///
/// Each tuple is (start_index, end_index_exclusive, group_name).
pub const PHASE_GROUPS: &[(usize, usize, &str)] = &[
    (0, 2, "forge"),           // forge, forge_qa
    (2, 4, "plan_review"),     // plan_review, plan_refine
    (4, 6, "verification"),    // verification, semantic_verification
    (6, 8, "design"),          // design_extraction, design_prototype
    (8, 12, "work"),           // task_decomposition, work, work_qa, drift_review
    (12, 16, "design_verify"), // storybook_verification, design_verification, design_verification_qa, ux_verification
    (16, 20, "gap_analysis"),  // gap_analysis, gap_analysis_qa, codex_gap_analysis, gap_remediation
    (20, 24, "inspect"),       // inspect, inspect_fix, verify_inspect, goldmask_verification
    (24, 27, "code_review"),   // code_review, code_review_qa, goldmask_correlation
    (27, 31, "mend"),          // mend, mend_qa, verify_mend, design_iteration
    (31, 34, "test"),          // test, test_qa, test_coverage_critique
    (34, 37, "deploy"),        // deploy_verify, pre_ship_validation, release_quality_check
    (37, 41, "ship"),          // ship, bot_review_wait, pr_comment_resolution, merge
];

/// Find the index of a phase by name.
///
/// Returns `None` if the phase name is not in `PHASE_ORDER`.
///
/// # Example
/// ```ignore
/// use crate::checkpoint::phase_order::phase_index;
/// assert_eq!(phase_index("work"), Some(9));
/// assert_eq!(phase_index("nonexistent"), None);
/// ```
pub fn phase_index(phase_name: &str) -> Option<usize> {
    PHASE_ORDER.iter().position(|&p| p == phase_name)
}

/// Get the phase name at a given index.
///
/// Returns `None` if the index is out of bounds.
///
/// # Example
/// ```ignore
/// use crate::checkpoint::phase_order::phase_at;
/// assert_eq!(phase_at(0), Some("forge"));
/// assert_eq!(phase_at(100), None);
/// ```
pub fn phase_at(index: usize) -> Option<&'static str> {
    PHASE_ORDER.get(index).copied()
}

/// Find the group name containing a phase.
///
/// Returns `None` if the phase is not found or index is invalid.
///
/// # Example
/// ```ignore
/// use crate::checkpoint::phase_order::group_for_phase;
/// assert_eq!(group_for_phase("work"), Some("work"));
/// assert_eq!(group_for_phase("merge"), Some("ship"));
/// ```
pub fn group_for_phase(phase_name: &str) -> Option<&'static str> {
    let idx = phase_index(phase_name)?;
    PHASE_GROUPS
        .iter()
        .find(|(start, end, _)| idx >= *start && idx < *end)
        .map(|(_, _, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_count() {
        assert_eq!(PHASE_COUNT, 41);
    }

    #[test]
    fn test_phase_index_known_phases() {
        assert_eq!(phase_index("forge"), Some(0));
        assert_eq!(phase_index("work"), Some(9));
        assert_eq!(phase_index("merge"), Some(40));
    }

    #[test]
    fn test_phase_index_unknown_phase() {
        assert_eq!(phase_index("unknown_phase"), None);
    }

    #[test]
    fn test_phase_at_valid_index() {
        assert_eq!(phase_at(0), Some("forge"));
        assert_eq!(phase_at(40), Some("merge"));
    }

    #[test]
    fn test_phase_at_invalid_index() {
        assert_eq!(phase_at(41), None);
        assert_eq!(phase_at(100), None);
    }

    #[test]
    fn test_group_for_phase() {
        assert_eq!(group_for_phase("forge"), Some("forge"));
        assert_eq!(group_for_phase("work"), Some("work"));
        assert_eq!(group_for_phase("merge"), Some("ship"));
    }

    #[test]
    fn test_all_phases_have_groups() {
        for phase in PHASE_ORDER {
            assert!(
                group_for_phase(phase).is_some(),
                "Phase '{}' has no group",
                phase
            );
        }
    }

    #[test]
    fn test_phase_groups_cover_all_phases() {
        let mut covered = vec![false; PHASE_COUNT];
        for (start, end, _) in PHASE_GROUPS {
            for i in *start..*end {
                covered[i] = true;
            }
        }
        assert!(covered.iter().all(|&c| c), "Not all phases are covered by groups");
    }

    #[test]
    fn test_phase_groups_compatible_with_config_groups() {
        // Verify that every phase in PHASE_GROUPS comes from PHASE_ORDER
        // and that PHASE_GROUPS indices are valid and non-overlapping.
        let mut last_end = 0;
        let mut total_phases = 0;
        for (start, end, name) in PHASE_GROUPS {
            assert!(
                *start == last_end,
                "Gap between PHASE_GROUPS: group '{}' starts at {} but previous ended at {}",
                name, start, last_end
            );
            assert!(
                *end > *start,
                "Empty PHASE_GROUP: '{}' has start={} end={}",
                name, start, end
            );
            assert!(
                *end <= PHASE_COUNT,
                "PHASE_GROUP '{}' end ({}) exceeds PHASE_COUNT ({})",
                name, end, PHASE_COUNT
            );
            total_phases += end - start;
            last_end = *end;
        }
        assert_eq!(
            total_phases, PHASE_COUNT,
            "PHASE_GROUPS cover {} phases but PHASE_COUNT is {}",
            total_phases, PHASE_COUNT
        );
    }

    #[test]
    fn test_phase_order_matches_known_phases() {
        // Verify PHASE_ORDER and KNOWN_PHASES (config module) contain the same phases.
        // This catches drift between the checkpoint system and config validation.
        use crate::config::phase_config::KNOWN_PHASES;
        use std::collections::HashSet;

        let order_set: HashSet<&str> = PHASE_ORDER.iter().copied().collect();
        let known_set: HashSet<&str> = KNOWN_PHASES.iter().copied().collect();

        let in_order_not_known: Vec<&&str> = order_set.difference(&known_set).collect();
        let in_known_not_order: Vec<&&str> = known_set.difference(&order_set).collect();

        assert!(
            in_order_not_known.is_empty(),
            "Phases in PHASE_ORDER but not KNOWN_PHASES: {:?}",
            in_order_not_known
        );
        assert!(
            in_known_not_order.is_empty(),
            "Phases in KNOWN_PHASES but not PHASE_ORDER: {:?}",
            in_known_not_order
        );
    }
}