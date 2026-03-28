#![allow(dead_code)]
//! Phase-aware profiles for Rune arc monitoring.
//!
//! Each arc phase has different characteristics that affect how Greater-Will
//! should monitor and recover from failures. This module maps phase names to
//! profiles that control timeout thresholds, idle behavior, and recovery strategy.
//!
//! # Phase Categories
//!
//! | Category | Phases | Typical Duration | Agent Teams? | Recovery |
//! |----------|--------|------------------|-------------|----------|
//! | Planning | forge → design_prototype | 5-15 min | No | Fresh or Resume |
//! | Work | task_decomposition → drift_review | 30-90 min | Yes (swarm) | Resume |
//! | Quality | gap_analysis → goldmask_correlation | 10-20 min/phase | Yes (review) | Resume |
//! | Remediation | mend → design_iteration | 5-15 min | Yes (fixers) | Resume |
//! | Testing | test → test_coverage_critique | 10-30 min | Maybe | Resume |
//! | Deploy | deploy_verify → release_quality_check | 2-5 min | No | Resume |
//! | Ship | ship → merge | 2-10 min | No | Resume + check PR |

use crate::checkpoint::phase_order::phase_index;

/// Grace buffer added to each phase's expected max duration (seconds).
/// Rune handles its own timeouts internally. gw's phase timeout is
/// Rune's budget + this buffer, acting as a safety net only when
/// Rune's own timeout handling fails.
const GRACE_BUFFER_SECS: u64 = 5 * 60; // 5 minutes

/// How gw should recover when a phase crashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStrategy {
    /// Run the original command (no --resume). Used when no meaningful
    /// progress has been made yet.
    FreshStart,
    /// Run with --resume. The standard recovery for mid-pipeline crashes.
    Resume,
    /// Run with --resume but verify PR state first. Used for ship/merge
    /// phases where a PR may already have been created or merged.
    ResumeCheckPR,
}

/// Category of arc phases with shared characteristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseCategory {
    /// forge, forge_qa, plan_review, plan_refine, verification,
    /// semantic_verification, design_extraction, design_prototype
    Planning,
    /// task_decomposition, work, work_qa, drift_review
    Work,
    /// gap_analysis → goldmask_correlation (phases 16-26)
    Quality,
    /// mend, mend_qa, verify_mend, design_iteration
    Remediation,
    /// test, test_qa, test_coverage_critique
    Testing,
    /// deploy_verify, pre_ship_validation, release_quality_check
    Deploy,
    /// ship, bot_review_wait, pr_comment_resolution, merge
    Ship,
}

/// Monitoring profile for a phase category.
///
/// Controls how aggressively gw intervenes during this phase.
#[derive(Debug, Clone)]
pub struct PhaseProfile {
    /// Phase category.
    pub category: PhaseCategory,
    /// Override for idle nudge threshold (seconds).
    /// Longer for phases with agent teams (screen appears idle while teammates work).
    pub idle_nudge_secs: u64,
    /// Override for idle kill threshold (seconds).
    pub idle_kill_secs: u64,
    /// Per-phase timeout budget (seconds). **Fallback only** — when a checkpoint
    /// with `totals.phase_times` is available, use `resolve_phase_timeout()` instead.
    ///
    /// This fallback is Rune's estimated max + GRACE_BUFFER_SECS. The real timeout
    /// comes from the checkpoint's phase_times (Rune's own budget) + grace buffer.
    pub phase_timeout_secs: u64,
    /// Whether this phase typically spawns agent teams.
    /// When true, swarm activity is a strong negative signal for error detection.
    pub has_agent_teams: bool,
    /// How to recover if the session crashes during this phase.
    pub recovery: RecoveryStrategy,
    /// Typical duration range for logging (min, max) in minutes.
    /// Used for progress reporting, not enforcement.
    pub typical_duration_min: (u32, u32),
    /// Human-readable description for logging.
    pub description: &'static str,
}

impl PhaseCategory {
    /// Get the monitoring profile for this category.
    pub fn profile(&self) -> PhaseProfile {
        match self {
            // Timeout formula: Rune's expected max duration + GRACE_BUFFER_SECS.
            // Rune handles its own timeouts; gw is the safety net when Rune fails.
            PhaseCategory::Planning => PhaseProfile {
                category: *self,
                idle_nudge_secs: 180,                             // 3 min
                idle_kill_secs: 600,                              // 10 min
                phase_timeout_secs: 15 * 60 + GRACE_BUFFER_SECS, // Rune 15 min + 5 min grace
                has_agent_teams: false,
                recovery: RecoveryStrategy::Resume,
                typical_duration_min: (5, 15),
                description: "Planning & forge",
            },
            PhaseCategory::Work => PhaseProfile {
                category: *self,
                idle_nudge_secs: 600,                              // 10 min
                idle_kill_secs: 3600,                              // 60 min
                phase_timeout_secs: 90 * 60 + GRACE_BUFFER_SECS,  // Rune 90 min + 5 min grace
                has_agent_teams: true,
                recovery: RecoveryStrategy::Resume,
                typical_duration_min: (30, 90),
                description: "Work execution (swarm)",
            },
            PhaseCategory::Quality => PhaseProfile {
                category: *self,
                idle_nudge_secs: 300,                              // 5 min
                idle_kill_secs: 1800,                              // 30 min
                phase_timeout_secs: 30 * 60 + GRACE_BUFFER_SECS,  // Rune 30 min + 5 min grace
                has_agent_teams: true,
                recovery: RecoveryStrategy::Resume,
                typical_duration_min: (10, 20),
                description: "Quality analysis (agent teams)",
            },
            PhaseCategory::Remediation => PhaseProfile {
                category: *self,
                idle_nudge_secs: 300,                              // 5 min
                idle_kill_secs: 1800,                              // 30 min
                phase_timeout_secs: 20 * 60 + GRACE_BUFFER_SECS,  // Rune 20 min + 5 min grace
                has_agent_teams: true,
                recovery: RecoveryStrategy::Resume,
                typical_duration_min: (5, 15),
                description: "Remediation (mend-fixers)",
            },
            PhaseCategory::Testing => PhaseProfile {
                category: *self,
                idle_nudge_secs: 300,                              // 5 min
                idle_kill_secs: 1800,                              // 30 min
                phase_timeout_secs: 30 * 60 + GRACE_BUFFER_SECS,  // Rune 30 min + 5 min grace
                has_agent_teams: false,
                recovery: RecoveryStrategy::Resume,
                typical_duration_min: (10, 30),
                description: "Testing",
            },
            PhaseCategory::Deploy => PhaseProfile {
                category: *self,
                idle_nudge_secs: 120,                             // 2 min
                idle_kill_secs: 600,                              // 10 min
                phase_timeout_secs: 10 * 60 + GRACE_BUFFER_SECS, // Rune 10 min + 5 min grace
                has_agent_teams: false,
                recovery: RecoveryStrategy::Resume,
                typical_duration_min: (2, 5),
                description: "Deploy verification",
            },
            PhaseCategory::Ship => PhaseProfile {
                category: *self,
                idle_nudge_secs: 300,                              // 5 min
                idle_kill_secs: 1800,                              // 30 min
                phase_timeout_secs: 20 * 60 + GRACE_BUFFER_SECS,  // Rune 20 min + 5 min grace
                has_agent_teams: false,
                recovery: RecoveryStrategy::ResumeCheckPR,
                typical_duration_min: (2, 10),
                description: "Ship & merge",
            },
        }
    }
}

/// Resolve a phase name to its category.
pub fn phase_category(phase_name: &str) -> Option<PhaseCategory> {
    let idx = phase_index(phase_name)?;
    Some(match idx {
        0..=7 => PhaseCategory::Planning,     // forge → design_prototype
        8..=11 => PhaseCategory::Work,         // task_decomposition → drift_review
        12..=26 => PhaseCategory::Quality,     // storybook_verification → goldmask_correlation
        27..=30 => PhaseCategory::Remediation, // mend → design_iteration
        31..=33 => PhaseCategory::Testing,     // test → test_coverage_critique
        34..=36 => PhaseCategory::Deploy,      // deploy_verify → release_quality_check
        37..=40 => PhaseCategory::Ship,        // ship → merge
        _ => return None,
    })
}

/// Get the monitoring profile for a phase name.
pub fn profile_for_phase(phase_name: &str) -> Option<PhaseProfile> {
    phase_category(phase_name).map(|c| c.profile())
}

/// Default profile used when the current phase is unknown.
///
/// Uses conservative thresholds (long idle before kill, assume agent teams).
pub fn default_profile() -> PhaseProfile {
    PhaseProfile {
        category: PhaseCategory::Work, // conservative default
        idle_nudge_secs: 300,
        idle_kill_secs: 1800,
        phase_timeout_secs: 90 * 60 + GRACE_BUFFER_SECS, // 95 min — generous for unknown
        has_agent_teams: true,
        recovery: RecoveryStrategy::Resume,
        typical_duration_min: (10, 60),
        description: "Unknown phase (conservative defaults)",
    }
}

/// Resolve the phase timeout from checkpoint data.
///
/// Reads `totals.phase_times.{phase_name}` (milliseconds) from the checkpoint
/// and adds GRACE_BUFFER_SECS. Falls back to the profile's hardcoded default
/// when checkpoint data is unavailable.
///
/// # Arguments
/// * `phase_name` - Current phase (e.g., "forge", "work")
/// * `checkpoint` - The checkpoint to read phase_times from
/// * `fallback` - Fallback timeout from PhaseProfile (used when checkpoint has no data)
pub fn resolve_phase_timeout(
    phase_name: &str,
    checkpoint: &crate::checkpoint::schema::Checkpoint,
    fallback: u64,
) -> u64 {
    // Read totals.phase_times.{phase_name} from checkpoint
    let budget_ms = checkpoint.totals.as_ref()
        .and_then(|t| t.get("phase_times"))
        .and_then(|pt| pt.get(phase_name))
        .and_then(|v| v.as_u64());

    match budget_ms {
        Some(ms) => {
            let budget_secs = ms / 1000;
            let timeout = budget_secs + GRACE_BUFFER_SECS;
            tracing::debug!(
                phase = phase_name,
                budget_ms = ms,
                budget_secs = budget_secs,
                grace_secs = GRACE_BUFFER_SECS,
                timeout_secs = timeout,
                "Phase timeout from checkpoint: {}s budget + {}s grace = {}s",
                budget_secs, GRACE_BUFFER_SECS, timeout,
            );
            timeout
        }
        None => {
            tracing::debug!(
                phase = phase_name,
                fallback_secs = fallback,
                "No phase_times in checkpoint — using fallback timeout",
            );
            fallback
        }
    }
}

/// Resolve phase timeout from checkpoint, reading the `reactions` config too.
///
/// Priority:
/// 1. `totals.phase_times.{phase}` (Rune's measured/budgeted time) + grace
/// 2. `reactions.{relevant_reaction}.escalate_after_ms` + grace
/// 3. Fallback from PhaseProfile hardcoded default
pub fn resolve_phase_timeout_full(
    phase_name: &str,
    checkpoint: &crate::checkpoint::schema::Checkpoint,
    profile: &PhaseProfile,
) -> u64 {
    // Try phase_times first (most accurate)
    let from_phase_times = checkpoint.totals.as_ref()
        .and_then(|t| t.get("phase_times"))
        .and_then(|pt| pt.get(phase_name))
        .and_then(|v| v.as_u64());

    if let Some(ms) = from_phase_times {
        let timeout = (ms / 1000) + GRACE_BUFFER_SECS;
        tracing::debug!(
            phase = phase_name,
            source = "phase_times",
            timeout_secs = timeout,
            "Phase timeout from checkpoint phase_times"
        );
        return timeout;
    }

    // Try reactions config for phase-specific escalation timeouts
    let reaction_key = match phase_name {
        "work" | "task_decomposition" | "work_qa" | "drift_review" =>
            Some("work_incomplete"),
        "mend" | "mend_qa" | "verify_mend" =>
            Some("mend_findings_exceeded"),
        "code_review" | "code_review_qa" =>
            Some("review_changes_requested"),
        _ => None,
    };

    if let Some(key) = reaction_key {
        let escalate_ms = checkpoint.reactions.as_ref()
            .and_then(|r| r.get(key))
            .and_then(|r| r.get("escalate_after_ms"))
            .and_then(|v| v.as_u64());

        if let Some(ms) = escalate_ms {
            let timeout = (ms / 1000) + GRACE_BUFFER_SECS;
            tracing::debug!(
                phase = phase_name,
                source = format!("reactions.{}.escalate_after_ms", key),
                timeout_secs = timeout,
                "Phase timeout from checkpoint reactions"
            );
            return timeout;
        }
    }

    // Fallback to profile default
    tracing::debug!(
        phase = phase_name,
        source = "fallback",
        timeout_secs = profile.phase_timeout_secs,
        "No checkpoint timing data — using profile default"
    );
    profile.phase_timeout_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_phases_have_category() {
        use crate::checkpoint::phase_order::PHASE_ORDER;
        for phase in PHASE_ORDER {
            assert!(
                phase_category(phase).is_some(),
                "Phase '{}' has no category",
                phase
            );
        }
    }

    #[test]
    fn test_phase_categories() {
        assert_eq!(phase_category("forge"), Some(PhaseCategory::Planning));
        assert_eq!(phase_category("work"), Some(PhaseCategory::Work));
        assert_eq!(phase_category("code_review"), Some(PhaseCategory::Quality));
        assert_eq!(phase_category("mend"), Some(PhaseCategory::Remediation));
        assert_eq!(phase_category("test"), Some(PhaseCategory::Testing));
        assert_eq!(phase_category("pre_ship_validation"), Some(PhaseCategory::Deploy));
        assert_eq!(phase_category("ship"), Some(PhaseCategory::Ship));
        assert_eq!(phase_category("merge"), Some(PhaseCategory::Ship));
    }

    #[test]
    fn test_work_has_long_idle_threshold() {
        let profile = PhaseCategory::Work.profile();
        assert!(profile.idle_kill_secs >= 3600, "Work should allow 60+ min idle");
        assert!(profile.has_agent_teams);
    }

    #[test]
    fn test_ship_uses_resume_check_pr() {
        let profile = PhaseCategory::Ship.profile();
        assert_eq!(profile.recovery, RecoveryStrategy::ResumeCheckPR);
    }

    #[test]
    fn test_planning_has_short_idle() {
        let profile = PhaseCategory::Planning.profile();
        assert!(profile.idle_kill_secs <= 600, "Planning should kill after 10 min idle");
        assert!(!profile.has_agent_teams);
    }

    #[test]
    fn test_unknown_phase_returns_none() {
        assert_eq!(phase_category("nonexistent"), None);
    }

    #[test]
    fn test_default_profile_is_conservative() {
        let profile = default_profile();
        assert!(profile.has_agent_teams);
        assert!(profile.idle_kill_secs >= 1800);
    }

    #[test]
    fn test_profile_for_phase() {
        let profile = profile_for_phase("forge").unwrap();
        assert_eq!(profile.category, PhaseCategory::Planning);
        assert!(profile_for_phase("nonexistent").is_none());
    }

    #[test]
    fn test_all_profiles_have_grace_buffer() {
        // Every profile's timeout should include the grace buffer
        for cat in &[
            PhaseCategory::Planning, PhaseCategory::Work, PhaseCategory::Quality,
            PhaseCategory::Remediation, PhaseCategory::Testing, PhaseCategory::Deploy,
            PhaseCategory::Ship,
        ] {
            let profile = cat.profile();
            assert!(
                profile.phase_timeout_secs >= GRACE_BUFFER_SECS,
                "{:?} timeout {}s is less than grace buffer {}s",
                cat, profile.phase_timeout_secs, GRACE_BUFFER_SECS,
            );
        }
    }

    #[test]
    fn test_resolve_phase_timeout_from_phase_times() {
        use crate::checkpoint::schema::Checkpoint;
        use serde_json::json;

        let mut cp = Checkpoint::default();
        cp.totals = Some(json!({
            "phase_times": {
                "forge": 420000,   // 420s = 7 min
                "work": 1800000,   // 1800s = 30 min
            }
        }));

        // forge: 420s + 300s grace = 720s
        assert_eq!(resolve_phase_timeout("forge", &cp, 9999), 720);
        // work: 1800s + 300s grace = 2100s
        assert_eq!(resolve_phase_timeout("work", &cp, 9999), 2100);
        // unknown phase: falls back to provided fallback
        assert_eq!(resolve_phase_timeout("nonexistent", &cp, 600), 600);
    }

    #[test]
    fn test_resolve_phase_timeout_no_totals() {
        use crate::checkpoint::schema::Checkpoint;

        let cp = Checkpoint::default(); // no totals
        // Should fall back to provided fallback
        assert_eq!(resolve_phase_timeout("forge", &cp, 1200), 1200);
    }

    #[test]
    fn test_resolve_phase_timeout_full_from_reactions() {
        use crate::checkpoint::schema::Checkpoint;
        use serde_json::json;

        let mut cp = Checkpoint::default();
        // No phase_times, but reactions has escalation timeout
        cp.reactions = Some(json!({
            "work_incomplete": {
                "action": "retry",
                "retries": 1,
                "escalate_after_ms": 1800000  // 30 min
            }
        }));

        let profile = PhaseCategory::Work.profile();
        // work: 1800s + 300s grace = 2100s (from reactions)
        assert_eq!(resolve_phase_timeout_full("work", &cp, &profile), 2100);
        // forge: no reaction mapping → fallback to profile default
        let forge_profile = PhaseCategory::Planning.profile();
        assert_eq!(
            resolve_phase_timeout_full("forge", &cp, &forge_profile),
            forge_profile.phase_timeout_secs,
        );
    }

    #[test]
    fn test_resolve_phase_timeout_full_priority() {
        use crate::checkpoint::schema::Checkpoint;
        use serde_json::json;

        let mut cp = Checkpoint::default();
        // Both phase_times AND reactions available — phase_times should win
        cp.totals = Some(json!({
            "phase_times": { "work": 600000 } // 10 min
        }));
        cp.reactions = Some(json!({
            "work_incomplete": {
                "escalate_after_ms": 1800000  // 30 min
            }
        }));

        let profile = PhaseCategory::Work.profile();
        // phase_times wins: 600s + 300s = 900s (not 1800s + 300s from reactions)
        assert_eq!(resolve_phase_timeout_full("work", &cp, &profile), 900);
    }
}
