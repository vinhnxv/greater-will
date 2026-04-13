#![allow(dead_code)]
//! Checkpoint schema definitions for Rune arc checkpoints.
//!
//! This module provides Serde structs that can parse checkpoint.json files
//! from schema versions 20-30, with forward compatibility via `#[serde(flatten)]`.
//!
//! # Key Design Decisions
//!
//! - **Forward Compatibility**: Unknown fields are captured in `extra` HashMap
//! - **Preserve on Write**: All fields (including unknown) are serialized back
//! - **Optional Fields**: Most fields use `Option` or `#[serde(default)]` for flexibility
//!
//! # Example
//!
//! ```ignore
//! use crate::checkpoint::schema::Checkpoint;
//!
//! let json = std::fs::read_to_string("checkpoint.json")?;
//! let checkpoint: Checkpoint = serde_json::from_str(&json)?;
//! println!("Arc ID: {}", checkpoint.id);
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Deserialize a value that may be either a JSON string or a JSON number into a String.
///
/// Checkpoint producers (JavaScript/TypeScript) are inconsistent about whether
/// `owner_pid` is written as `"41111"` (string) or `41111` (number).
/// This helper accepts both forms so we don't fail on valid checkpoints.
fn string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Null => Ok(String::new()),
        other => Ok(other.to_string()),
    }
}

/// Schema version range that greater-will has been tested against.
/// - MIN: oldest checkpoint format we can reliably parse
/// - MAX: newest checkpoint format we've verified
///
/// Versions outside this range trigger warnings but don't hard-fail,
/// since serde ignores unknown fields gracefully.
pub const SCHEMA_VERSION_MIN: u32 = 20;
pub const SCHEMA_VERSION_MAX: u32 = 30;

/// Result of schema version validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCompat {
    /// Version within tested range — safe to use.
    Compatible,
    /// Version newer than tested — may have structural changes.
    Newer { version: u32 },
    /// Version older than tested — may lack expected fields.
    Older { version: u32 },
    /// No schema_version field present — legacy checkpoint.
    Unknown,
}

impl SchemaCompat {
    /// Human-readable warning message, or None if compatible.
    pub fn warning(&self) -> Option<String> {
        match self {
            SchemaCompat::Compatible => None,
            SchemaCompat::Newer { version } => Some(format!(
                "checkpoint schema v{} is newer than tested range (v{}-v{}). \
                 Greater-will preserve unknown fields but may not interpret new semantics.",
                version, SCHEMA_VERSION_MIN, SCHEMA_VERSION_MAX
            )),
            SchemaCompat::Older { version } => Some(format!(
                "checkpoint schema v{} is older than tested range (v{}-v{}). \
                 Phase structure may differ.",
                version, SCHEMA_VERSION_MIN, SCHEMA_VERSION_MAX
            )),
            SchemaCompat::Unknown => Some(
                "checkpoint has no schema_version — legacy format, parsing best-effort.".into(),
            ),
        }
    }

    /// Returns true if the schema is within the compatible range.
    pub fn is_compatible(&self) -> bool {
        matches!(self, SchemaCompat::Compatible)
    }
}

/// Inferred position of the arc pipeline based on actual phase statuses.
///
/// This is the ground-truth phase position derived from scanning the `phases`
/// map against `PHASE_ORDER`, rather than relying on `phase_sequence` which
/// may be stale or not updated by Rune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhasePosition {
    /// A phase is actively running (`status: "in_progress"`).
    Running {
        phase: &'static str,
        started_at: Option<String>,
    },
    /// Between phases — last phase completed, next phase is pending.
    /// The `last_completed_at` timestamp is used to compute transition gap duration.
    Transitioning {
        last_completed: &'static str,
        last_completed_at: Option<String>,
        next_pending: &'static str,
    },
    /// No phases have started yet.
    WaitingToStart {
        first_pending: &'static str,
    },
    /// A phase has failed — arc may be retrying or halted.
    /// The `failed_phase` is the first failed phase in PHASE_ORDER.
    /// `next_pending` is the next actionable phase (if any — arc may retry).
    Failed {
        failed_phase: &'static str,
        next_pending: Option<&'static str>,
    },
    /// All phases are completed or skipped.
    AllDone,
    /// Cannot determine position (empty phases map).
    Unknown,
}

impl PhasePosition {
    /// Returns true if the pipeline is between phases (transition gap).
    pub fn is_transitioning(&self) -> bool {
        matches!(self, PhasePosition::Transitioning { .. })
    }

    /// Returns true if a phase has failed.
    pub fn has_failure(&self) -> bool {
        matches!(self, PhasePosition::Failed { .. })
    }

    /// Returns the effective phase name for display/profile lookup.
    pub fn effective_phase(&self) -> Option<&'static str> {
        match self {
            PhasePosition::Running { phase, .. } => Some(phase),
            PhasePosition::Transitioning { next_pending, .. } if !next_pending.is_empty() => {
                Some(next_pending)
            }
            PhasePosition::Failed { failed_phase, .. } => Some(failed_phase),
            PhasePosition::WaitingToStart { first_pending } => Some(first_pending),
            _ => None,
        }
    }

    /// Compute the transition gap duration in seconds (time since last phase completed).
    ///
    /// Only meaningful for `Transitioning` state. Returns `None` for other states.
    pub fn transition_gap_secs(&self) -> Option<u64> {
        match self {
            PhasePosition::Transitioning { last_completed_at: Some(ts), .. } => {
                chrono::DateTime::parse_from_rfc3339(ts).ok().map(|dt| {
                    let now = chrono::Utc::now();
                    now.signed_duration_since(dt.with_timezone(&chrono::Utc))
                        .num_seconds()
                        .max(0) as u64
                })
            }
            _ => None,
        }
    }
}

/// Arc checkpoint — identity and phase progress.
///
/// Read from: `.rune/arc/arc-{id}/checkpoint.json`
///
/// This struct captures all known checkpoint fields plus unknown fields
/// via `#[serde(flatten)]` for forward compatibility.
///
/// # Field Preservation
///
/// When greater-will writes a modified checkpoint, ALL fields are preserved:
/// - Known fields are serialized explicitly
/// - Unknown fields from `extra` are serialized via flatten
///
/// This ensures we don't lose data when round-tripping newer checkpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    // ── Core identity fields ──
    /// Unique arc identifier (e.g., "arc-1739123456789")
    pub id: String,

    /// Schema version of the checkpoint format
    #[serde(default)]
    pub schema_version: Option<u32>,

    /// Path to the plan file this arc is executing
    pub plan_file: String,

    // ── Session tracking fields ──
    /// Configuration directory for the arc
    #[serde(default)]
    pub config_dir: String,

    /// PID of the process that owns this arc
    #[serde(default, deserialize_with = "string_or_number")]
    pub owner_pid: String,

    /// Claude Code session ID running this arc
    #[serde(default)]
    pub session_id: String,

    // ── Phase progress ──
    /// Status of each phase, keyed by phase name
    #[serde(default)]
    pub phases: HashMap<String, PhaseStatus>,

    // ── PR tracking ──
    /// URL of the created PR (if any)
    #[serde(default)]
    pub pr_url: Option<String>,

    /// Commit SHAs created during the arc
    #[serde(default)]
    pub commits: Vec<String>,

    // ── Timestamps ──
    /// When the arc started (RFC3339 format)
    #[serde(default)]
    pub started_at: String,

    /// When the arc completed (RFC3339 format)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,

    // ── Phase sequence control ──
    /// Current phase index (0-indexed into PHASE_ORDER)
    /// CRITICAL: Must be updated when manipulating checkpoint
    #[serde(default)]
    pub phase_sequence: Option<u32>,

    /// Map of phases to skip with reason
    /// Phases here must be marked "skipped" (not "completed")
    #[serde(default)]
    pub skip_map: Option<HashMap<String, String>>,

    // ── Session validation ──
    /// Nonce for session validation (preserve but don't validate)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_nonce: Option<String>,

    // ── Configuration preservation ──
    /// Arc configuration flags (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<serde_json::Value>,

    /// Arc configuration (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_config: Option<serde_json::Value>,

    // ── Convergence tracking ──
    /// QA convergence state (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa: Option<serde_json::Value>,

    /// Overall convergence tracking (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convergence: Option<serde_json::Value>,

    /// Inspect convergence tracking (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect_convergence: Option<serde_json::Value>,

    // ── Worktree support ──
    /// Worktree metadata (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_meta: Option<serde_json::Value>,

    // ── Hierarchical plans ──
    /// Parent plan reference (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan: Option<serde_json::Value>,

    // ── Staleness tracking ──
    /// Freshness metrics (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<serde_json::Value>,

    /// Stagnation metrics (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stagnation: Option<serde_json::Value>,

    // ── Codex integration ──
    /// Codex cascade state (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_cascade: Option<serde_json::Value>,

    // ── Reaction tracking ──
    /// Reaction configuration (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<serde_json::Value>,

    /// Reaction state (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction_state: Option<serde_json::Value>,

    // ── Totals/summary ──
    /// Totals/summary data (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<serde_json::Value>,

    // ── Phase skip logging ──
    /// Log of skipped phases (preserve as opaque JSON)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_skip_log: Option<Vec<serde_json::Value>>,

    // ── Forward compatibility ──
    /// Capture unknown fields so we don't lose them on write-back.
    /// This is CRITICAL for forward compatibility with newer checkpoint versions.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Checkpoint {
    /// Check if this checkpoint's schema version is within tested range.
    pub fn schema_compat(&self) -> SchemaCompat {
        match self.schema_version {
            Some(v) if v < SCHEMA_VERSION_MIN => SchemaCompat::Older { version: v },
            Some(v) if v > SCHEMA_VERSION_MAX => SchemaCompat::Newer { version: v },
            Some(_) => SchemaCompat::Compatible,
            None => SchemaCompat::Unknown,
        }
    }

    /// Get the current phase name based on `phase_sequence`.
    ///
    /// **WARNING:** `phase_sequence` is often stale — Rune frequently fails to
    /// update this field. Prefer [`inferred_phase_name()`](Self::inferred_phase_name)
    /// which derives the phase from the actual `phases` map.
    ///
    /// Returns `None` if `phase_sequence` is not set or is out of valid range.
    /// When `phase_sequence` equals `PHASE_COUNT` (the completed sentinel),
    /// returns the last phase in the pipeline. Values beyond `PHASE_COUNT`
    /// are treated as corrupted data and return `None`.
    pub fn current_phase(&self) -> Option<&'static str> {
        let idx = self.phase_sequence? as usize;
        let count = crate::checkpoint::phase_order::PHASE_COUNT;
        crate::checkpoint::phase_order::phase_at(idx).or_else(|| {
            // Only the exact completed sentinel (idx == count) returns last phase.
            // Values beyond that indicate corrupted data — return None.
            if count > 0 && idx == count {
                crate::checkpoint::phase_order::phase_at(count - 1)
            } else {
                None
            }
        })
    }

    /// Infer the current phase by scanning the `phases` map against `PHASE_ORDER`.
    ///
    /// Unlike `current_phase()` which relies on the `phase_sequence` field
    /// (often stale or not updated by Rune), this method looks at actual
    /// phase statuses to determine where the pipeline is.
    ///
    /// Returns a [`PhasePosition`] describing current state:
    /// - `Running { phase }` — a phase has `status: "in_progress"`
    /// - `Transitioning { last_completed, next_pending }` — between phases
    /// - `WaitingToStart { first_pending }` — nothing started yet
    /// - `AllDone` — every phase is completed or skipped
    /// - `Unknown` — no phases in map or can't determine
    pub fn infer_phase_position(&self) -> PhasePosition {
        use crate::checkpoint::phase_order::PHASE_ORDER;

        if self.phases.is_empty() {
            return PhasePosition::Unknown;
        }

        // 0. If all phases in the map are done, we're done
        if self.is_complete() {
            return PhasePosition::AllDone;
        }

        // 1. Look for an in_progress phase (scan in canonical order)
        for &phase_name in PHASE_ORDER {
            if let Some(ps) = self.phases.get(phase_name) {
                if ps.status == "in_progress" {
                    return PhasePosition::Running {
                        phase: phase_name,
                        started_at: ps.started_at.clone(),
                    };
                }
            }
        }

        // 2. Check for failed phases — arc may be halted or waiting for retry
        for &phase_name in PHASE_ORDER {
            if let Some(ps) = self.phases.get(phase_name) {
                if ps.status == "failed" {
                    let next = self.next_actionable_phase();
                    return PhasePosition::Failed {
                        failed_phase: phase_name,
                        next_pending: next,
                    };
                }
            }
        }

        // 3. No in_progress, no failed — find last completed and next pending
        let mut last_completed: Option<(&'static str, Option<String>)> = None;

        for &phase_name in PHASE_ORDER {
            if let Some(ps) = self.phases.get(phase_name) {
                if ps.status == "completed" {
                    last_completed = Some((phase_name, ps.completed_at.clone()));
                }
            }
        }

        // Find first actionable pending after last completed (skip_map-aware)
        let search_start = last_completed.as_ref()
            .and_then(|(name, _)| PHASE_ORDER.iter().position(|&p| p == *name))
            .map_or(0, |i| i + 1);

        let next_actionable = self.next_actionable_phase_after(search_start);

        match (last_completed, next_actionable) {
            (Some((last, completed_at)), Some(next)) => PhasePosition::Transitioning {
                last_completed: last,
                last_completed_at: completed_at,
                next_pending: next,
            },
            (Some((last, _)), None) => {
                // Last completed but no more pending phases ahead.
                // This includes the case where `merge` (terminal) completed
                // but is_complete() might still return false due to stale
                // pending phases earlier in the pipeline. Treat as AllDone
                // when the last completed phase is at or past the terminal
                // index, or when is_complete() agrees.
                let last_idx = crate::checkpoint::phase_order::phase_index(last).unwrap_or(0);
                let terminal_idx = crate::checkpoint::phase_order::PHASE_COUNT.saturating_sub(1);
                if self.is_complete() || last_idx >= terminal_idx {
                    PhasePosition::AllDone
                } else {
                    // Genuinely mid-pipeline with no actionable next phase
                    // (e.g., remaining phases are all failed).
                    PhasePosition::Transitioning {
                        last_completed: last,
                        last_completed_at: self.phases.get(last).and_then(|p| p.completed_at.clone()),
                        next_pending: "",
                    }
                }
            }
            (None, Some(first_actionable)) => PhasePosition::WaitingToStart {
                first_pending: first_actionable,
            },
            (None, None) => PhasePosition::Unknown,
        }
    }

    /// Convenience: get the effective current phase name from inferred position.
    ///
    /// For `Running` → the in_progress phase name.
    /// For `Transitioning` → the next_pending phase name (what we're transitioning TO).
    /// For `WaitingToStart` → the first pending phase.
    /// For `AllDone` / `Unknown` → None.
    pub fn inferred_phase_name(&self) -> Option<&'static str> {
        self.infer_phase_position().effective_phase()
    }

    /// Get the effective phase sequence index by scanning actual phase statuses.
    ///
    /// Like `inferred_phase_name()`, this ignores the often-stale `phase_sequence`
    /// field and instead computes the index from the inferred phase position.
    /// Falls back to `phase_sequence` if inference returns no result.
    ///
    /// Returns `None` if neither inference nor `phase_sequence` yields a value.
    pub fn effective_phase_sequence(&self) -> Option<u32> {
        self.inferred_phase_name()
            .and_then(crate::checkpoint::phase_order::phase_index)
            .map(|idx| idx as u32)
            .or(self.phase_sequence)
    }

    /// Check if the arc has completed (all phases done).
    ///
    /// An arc is complete if:
    /// - `completed_at` is set, OR
    /// - The terminal phase (`merge`) is "completed" — regardless of other
    ///   phases' statuses, because `merge` is the last phase in `PHASE_ORDER`
    ///   and a completed merge means the pipeline ran to its end, OR
    /// - All phases are "completed" or "skipped"
    pub fn is_complete(&self) -> bool {
        if self.completed_at.is_some() {
            return true;
        }

        // Terminal phase check: if `merge` (last in PHASE_ORDER) is completed,
        // the pipeline is done even if earlier phases are still "pending"
        // (e.g., due to skip_map not being populated, schema version drift,
        // or Rune not marking all skipped phases before exiting).
        if self.is_terminal_phase_completed() {
            return true;
        }

        !self.phases.is_empty()
            && self.phases.values().all(|p| {
                p.status == "completed" || p.status == "skipped"
            })
    }

    /// Check if the terminal phase (last in PHASE_ORDER) is done.
    ///
    /// This is a stronger signal than `completed_at` because Rune doesn't
    /// always set that field. If the last phase ran to completion (or was
    /// intentionally skipped, e.g. `auto_merge=false` skipping `merge`),
    /// the pipeline is definitively done.
    pub fn is_terminal_phase_completed(&self) -> bool {
        let terminal = crate::checkpoint::phase_order::phase_at(
            crate::checkpoint::phase_order::PHASE_COUNT - 1,
        );
        terminal
            .and_then(|name| self.phases.get(name))
            .is_some_and(|p| p.status == "completed" || p.status == "skipped")
    }

    /// Check if a phase is in the skip_map (will be auto-skipped by Rune).
    ///
    /// A phase in skip_map is "pending" in the phases map but will be
    /// immediately skipped when arc reaches it — not actionable.
    pub fn is_in_skip_map(&self, phase_name: &str) -> bool {
        self.skip_map
            .as_ref()
            .is_some_and(|sm| sm.contains_key(phase_name))
    }

    /// Check if a phase should be considered "done" (completed, skipped, or will-be-skipped).
    ///
    /// Returns true for phases that are completed, skipped, or in skip_map.
    pub fn is_phase_done_or_will_skip(&self, phase_name: &str) -> bool {
        if self.is_in_skip_map(phase_name) {
            return true;
        }
        self.phases
            .get(phase_name)
            .is_some_and(|p| p.status == "completed" || p.status == "skipped")
    }

    /// Check if a phase is actionable (pending, in the map, and NOT in skip_map).
    ///
    /// A phase is actionable if:
    /// - It exists in the `phases` map (Rune populated it)
    /// - Its status is "pending" or empty
    /// - It's NOT in `skip_map` (won't be auto-skipped)
    ///
    /// Phases not in the map are NOT considered actionable — in production,
    /// Rune populates all 41 phases. Missing phases indicate incomplete init.
    pub fn is_phase_actionable(&self, phase_name: &str) -> bool {
        if self.is_in_skip_map(phase_name) {
            return false;
        }
        self.phases
            .get(phase_name)
            .is_some_and(|p| p.status == "pending" || p.status.is_empty())
    }

    /// Find the next actionable phase — first pending phase NOT in skip_map.
    ///
    /// Unlike `next_pending_phase()` in reader.rs, this skips phases that
    /// are in skip_map (will be auto-skipped by arc).
    pub fn next_actionable_phase(&self) -> Option<&'static str> {
        use crate::checkpoint::phase_order::PHASE_ORDER;

        PHASE_ORDER.iter().find(|&&p| self.is_phase_actionable(p)).copied()
    }

    /// Find the next actionable phase after a given index in PHASE_ORDER.
    pub fn next_actionable_phase_after(&self, start_idx: usize) -> Option<&'static str> {
        use crate::checkpoint::phase_order::PHASE_ORDER;

        PHASE_ORDER.iter().skip(start_idx).find(|&&p| self.is_phase_actionable(p)).copied()
    }

    /// Count phases by status.
    pub fn count_by_status(&self, status: &str) -> usize {
        self.phases.values().filter(|p| p.status == status).count()
    }

    /// Get the timestamp when the arc started.
    ///
    /// Returns `None` if `started_at` is empty or invalid.
    pub fn started_datetime(&self) -> Option<DateTime<Utc>> {
        if self.started_at.is_empty() {
            return None;
        }
        DateTime::parse_from_rfc3339(&self.started_at)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Get the timestamp when the arc completed.
    ///
    /// Returns `None` if `completed_at` is not set or invalid.
    pub fn completed_datetime(&self) -> Option<DateTime<Utc>> {
        let s = self.completed_at.as_ref()?;
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            id: String::new(),
            schema_version: Some(SCHEMA_VERSION_MAX),
            plan_file: String::new(),
            config_dir: String::new(),
            owner_pid: String::new(),
            session_id: String::new(),
            phases: HashMap::new(),
            pr_url: None,
            commits: Vec::new(),
            started_at: Utc::now().to_rfc3339(),
            completed_at: None,
            phase_sequence: Some(0),
            skip_map: None,
            session_nonce: None,
            flags: None,
            arc_config: None,
            qa: None,
            convergence: None,
            inspect_convergence: None,
            worktree_meta: None,
            parent_plan: None,
            freshness: None,
            stagnation: None,
            codex_cascade: None,
            reactions: None,
            reaction_state: None,
            totals: None,
            phase_skip_log: None,
            extra: HashMap::new(),
        }
    }
}

/// Status of a single arc phase.
///
/// Each of the 41 phases has this shape. Like `Checkpoint`,
/// unknown fields are preserved via `#[serde(flatten)]`.
///
/// # Phase-Specific Fields
///
/// Different phases may have additional fields:
/// - `work.branch`: Git branch name
/// - `work.commits`: List of commit SHAs
/// - `test.pass_rate`: Test pass percentage
/// - These are captured in `extra`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhaseStatus {
    /// Current status: "pending" | "in_progress" | "completed" | "skipped" | "failed"
    #[serde(default)]
    pub status: String,

    /// Path to the phase's output artifact
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,

    /// SHA256 hash of the artifact (hex encoded)
    /// Used for integrity validation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_hash: Option<String>,

    /// When this phase started (RFC3339 format)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    /// When this phase completed (RFC3339 format)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,

    /// Team name for multi-agent phases
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,

    /// Capture phase-specific fields (work.branch, test.pass_rate, etc.)
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl PhaseStatus {
    /// Create a new PhaseStatus with the given status.
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            ..Default::default()
        }
    }

    /// Create a "pending" status.
    pub fn pending() -> Self {
        Self::new("pending")
    }

    /// Create a "completed" status with optional artifact.
    pub fn completed(artifact: Option<String>) -> Self {
        Self {
            status: "completed".into(),
            artifact,
            completed_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        }
    }

    /// Create a "skipped" status.
    pub fn skipped() -> Self {
        Self::new("skipped")
    }

    /// Create an "in_progress" status.
    pub fn in_progress() -> Self {
        Self {
            status: "in_progress".into(),
            started_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        }
    }

    /// Create a "failed" status.
    pub fn failed() -> Self {
        Self::new("failed")
    }

    /// Check if this phase is done (completed or skipped).
    pub fn is_done(&self) -> bool {
        self.status == "completed" || self.status == "skipped"
    }

    /// Set the completed_at timestamp to now.
    pub fn mark_completed(&mut self) {
        self.status = "completed".into();
        self.completed_at = Some(Utc::now().to_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_compat_in_range() {
        let mut cp = Checkpoint::default();
        cp.schema_version = Some(25);
        assert_eq!(cp.schema_compat(), SchemaCompat::Compatible);
        assert!(cp.schema_compat().warning().is_none());
    }

    #[test]
    fn test_schema_compat_at_boundaries() {
        let mut cp_min = Checkpoint::default();
        cp_min.schema_version = Some(SCHEMA_VERSION_MIN);
        assert_eq!(cp_min.schema_compat(), SchemaCompat::Compatible);

        let mut cp_max = Checkpoint::default();
        cp_max.schema_version = Some(SCHEMA_VERSION_MAX);
        assert_eq!(cp_max.schema_compat(), SchemaCompat::Compatible);
    }

    #[test]
    fn test_schema_compat_newer() {
        let mut cp = Checkpoint::default();
        cp.schema_version = Some(35);
        assert_eq!(cp.schema_compat(), SchemaCompat::Newer { version: 35 });
        assert!(cp.schema_compat().warning().unwrap().contains("newer"));
    }

    #[test]
    fn test_schema_compat_older() {
        let mut cp = Checkpoint::default();
        cp.schema_version = Some(15);
        assert_eq!(cp.schema_compat(), SchemaCompat::Older { version: 15 });
        assert!(cp.schema_compat().warning().unwrap().contains("older"));
    }

    #[test]
    fn test_schema_compat_unknown() {
        let mut cp = Checkpoint::default();
        cp.schema_version = None;
        assert_eq!(cp.schema_compat(), SchemaCompat::Unknown);
        assert!(cp.schema_compat().warning().unwrap().contains("legacy"));
    }

    #[test]
    fn test_deserialize_with_unknown_fields() {
        let json = r#"{
            "id": "arc-test",
            "schema_version": 25,
            "plan_file": "plans/test.md",
            "unknown_field": "preserved",
            "another_unknown": {"nested": "value"},
            "phases": {
                "forge": {
                    "status": "completed",
                    "artifact": "forge-report.md",
                    "custom_field": "custom_value"
                }
            },
            "started_at": "2026-03-25T00:00:00Z"
        }"#;

        let cp: Checkpoint = serde_json::from_str(json).unwrap();

        // Basic fields
        assert_eq!(cp.id, "arc-test");
        assert_eq!(cp.schema_version, Some(25));
        assert_eq!(cp.plan_file, "plans/test.md");

        // Unknown fields preserved
        assert_eq!(cp.extra.get("unknown_field").unwrap(), "preserved");
        assert!(cp.extra.contains_key("another_unknown"));

        // Phase with custom field
        let forge = cp.phases.get("forge").unwrap();
        assert_eq!(forge.status, "completed");
        assert_eq!(forge.artifact, Some("forge-report.md".into()));
        assert_eq!(forge.extra.get("custom_field").unwrap(), "custom_value");
    }

    #[test]
    fn test_roundtrip_preserves_all_fields() {
        let json = r#"{
            "id": "arc-roundtrip",
            "schema_version": 27,
            "plan_file": "plans/test.md",
            "config_dir": ".rune",
            "owner_pid": "12345",
            "session_id": "sess-abc",
            "phases": {
                "forge": {"status": "completed"},
                "work": {"status": "pending"}
            },
            "pr_url": "https://github.com/org/repo/pull/1",
            "commits": ["abc123"],
            "started_at": "2026-03-25T00:00:00Z",
            "phase_sequence": 9,
            "skip_map": {"semantic_verification": "not_needed"},
            "custom_field": "custom_value"
        }"#;

        // Parse
        let cp: Checkpoint = serde_json::from_str(json).unwrap();

        // Serialize
        let serialized = serde_json::to_string_pretty(&cp).unwrap();

        // Parse again
        let cp2: Checkpoint = serde_json::from_str(&serialized).unwrap();

        // Verify round-trip
        assert_eq!(cp2.id, cp.id);
        assert_eq!(cp2.schema_version, cp.schema_version);
        assert_eq!(cp2.plan_file, cp.plan_file);
        assert_eq!(cp2.phase_sequence, cp.phase_sequence);
        assert_eq!(cp2.skip_map, cp.skip_map);
        assert_eq!(cp2.extra.get("custom_field").unwrap(), "custom_value");
    }

    #[test]
    fn test_phase_status_helpers() {
        let pending = PhaseStatus::pending();
        assert_eq!(pending.status, "pending");
        assert!(!pending.is_done());

        let completed = PhaseStatus::completed(Some("artifact.md".into()));
        assert_eq!(completed.status, "completed");
        assert!(completed.is_done());
        assert!(completed.completed_at.is_some());

        let skipped = PhaseStatus::skipped();
        assert_eq!(skipped.status, "skipped");
        assert!(skipped.is_done());

        let in_progress = PhaseStatus::in_progress();
        assert_eq!(in_progress.status, "in_progress");
        assert!(in_progress.started_at.is_some());
        assert!(!in_progress.is_done());

        let failed = PhaseStatus::failed();
        assert_eq!(failed.status, "failed");
        assert!(!failed.is_done());
    }

    #[test]
    fn test_checkpoint_is_complete() {
        let mut cp = Checkpoint::default();
        cp.completed_at = Some("2026-03-25T12:00:00Z".into());
        assert!(cp.is_complete());

        cp.completed_at = None;
        cp.phases.insert("forge".into(), PhaseStatus::completed(None));
        cp.phases.insert("work".into(), PhaseStatus::skipped());
        assert!(cp.is_complete());

        cp.phases.insert("merge".into(), PhaseStatus::pending());
        assert!(!cp.is_complete());
    }

    #[test]
    fn test_checkpoint_current_phase() {
        let mut cp = Checkpoint::default();
        cp.phase_sequence = Some(0);
        assert_eq!(cp.current_phase(), Some("forge"));

        cp.phase_sequence = Some(9);
        assert_eq!(cp.current_phase(), Some("work"));

        cp.phase_sequence = Some(41);
        assert_eq!(cp.current_phase(), Some("merge")); // completed sentinel → last phase

        cp.phase_sequence = Some(42);
        assert_eq!(cp.current_phase(), None); // beyond sentinel → corrupted → None

        cp.phase_sequence = Some(u32::MAX);
        assert_eq!(cp.current_phase(), None); // garbage value → None

        cp.phase_sequence = None;
        assert_eq!(cp.current_phase(), None);
    }

    #[test]
    fn test_infer_phase_running() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:05:30Z".into()),
            ..Default::default()
        });
        cp.phases.insert("plan_review".into(), PhaseStatus::pending());

        match cp.infer_phase_position() {
            PhasePosition::Running { phase, .. } => assert_eq!(phase, "forge_qa"),
            other => panic!("Expected Running, got {:?}", other),
        }
        assert_eq!(cp.inferred_phase_name(), Some("forge_qa"));
    }

    #[test]
    fn test_infer_phase_transitioning() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:00:00Z".into()),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(),
            started_at: Some("2026-03-20T00:05:00Z".into()),
            completed_at: Some("2026-03-20T00:08:00Z".into()),
            ..Default::default()
        });
        cp.phases.insert("plan_review".into(), PhaseStatus::pending());

        match cp.infer_phase_position() {
            PhasePosition::Transitioning { last_completed, next_pending, .. } => {
                assert_eq!(last_completed, "forge_qa");
                assert_eq!(next_pending, "plan_review");
            }
            other => panic!("Expected Transitioning, got {:?}", other),
        }
        assert_eq!(cp.inferred_phase_name(), Some("plan_review"));
    }

    #[test]
    fn test_infer_phase_with_stale_sequence() {
        // Simulates the real bug: phase_sequence=1 but pipeline far ahead
        let mut cp = Checkpoint::default();
        cp.phase_sequence = Some(1); // stale!

        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("plan_review".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("plan_refine".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("verification".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:13:00Z".into()),
            ..Default::default()
        });

        // current_phase() returns forge_qa (stale!)
        assert_eq!(cp.current_phase(), Some("forge_qa"));
        // inferred_phase_name() returns verification (correct!)
        assert_eq!(cp.inferred_phase_name(), Some("verification"));
    }

    #[test]
    fn test_infer_phase_waiting_to_start() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus::pending());
        cp.phases.insert("forge_qa".into(), PhaseStatus::pending());

        match cp.infer_phase_position() {
            PhasePosition::WaitingToStart { first_pending } => {
                assert_eq!(first_pending, "forge");
            }
            other => panic!("Expected WaitingToStart, got {:?}", other),
        }
    }

    #[test]
    fn test_infer_phase_all_done() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "skipped".into(),
            ..Default::default()
        });
        // Only 2 phases for test simplicity — is_complete() checks all are done
        assert!(cp.is_complete());
        match cp.infer_phase_position() {
            PhasePosition::AllDone => {}
            other => panic!("Expected AllDone, got {:?}", other),
        }
    }

    #[test]
    fn test_infer_phase_empty() {
        let cp = Checkpoint::default();
        assert_eq!(cp.infer_phase_position(), PhasePosition::Unknown);
    }

    #[test]
    fn test_infer_phase_skipped_phases_skipped() {
        // forge completed, forge_qa skipped, plan_review pending → transitioning to plan_review
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "skipped".into(),
            ..Default::default()
        });
        cp.phases.insert("plan_review".into(), PhaseStatus::pending());

        match cp.infer_phase_position() {
            PhasePosition::Transitioning { last_completed, next_pending, .. } => {
                assert_eq!(last_completed, "forge"); // not forge_qa (skipped)
                assert_eq!(next_pending, "plan_review");
            }
            other => panic!("Expected Transitioning, got {:?}", other),
        }
    }

    #[test]
    fn test_owner_pid_as_number() {
        let json = r#"{
            "id": "arc-123",
            "plan_file": "plans/test.md",
            "owner_pid": 41111,
            "started_at": "2026-03-27T00:00:00Z"
        }"#;

        let cp: Checkpoint = serde_json::from_str(json).unwrap();
        assert_eq!(cp.owner_pid, "41111");
    }

    #[test]
    fn test_owner_pid_as_string() {
        let json = r#"{
            "id": "arc-123",
            "plan_file": "plans/test.md",
            "owner_pid": "41111",
            "started_at": "2026-03-27T00:00:00Z"
        }"#;

        let cp: Checkpoint = serde_json::from_str(json).unwrap();
        assert_eq!(cp.owner_pid, "41111");
    }

    // ── skip_map awareness tests ─────────────────────────

    #[test]
    fn test_is_in_skip_map() {
        let mut cp = Checkpoint::default();
        let mut skip_map = HashMap::new();
        skip_map.insert("semantic_verification".into(), "codex_disabled".into());
        skip_map.insert("design_extraction".into(), "no_figma_urls".into());
        cp.skip_map = Some(skip_map);

        assert!(cp.is_in_skip_map("semantic_verification"));
        assert!(cp.is_in_skip_map("design_extraction"));
        assert!(!cp.is_in_skip_map("forge"));
        assert!(!cp.is_in_skip_map("work"));
    }

    #[test]
    fn test_is_in_skip_map_none() {
        let cp = Checkpoint::default();
        assert!(!cp.is_in_skip_map("anything"));
    }

    #[test]
    fn test_is_phase_actionable() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus::pending());
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("semantic_verification".into(), PhaseStatus::pending());

        let mut skip_map = HashMap::new();
        skip_map.insert("semantic_verification".into(), "codex_disabled".into());
        cp.skip_map = Some(skip_map);

        assert!(cp.is_phase_actionable("forge")); // pending, not in skip_map
        assert!(!cp.is_phase_actionable("forge_qa")); // completed
        assert!(!cp.is_phase_actionable("semantic_verification")); // in skip_map
    }

    #[test]
    fn test_next_actionable_phase() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus::pending());
        cp.phases.insert("plan_review".into(), PhaseStatus::pending());

        // Without skip_map → forge_qa is next
        assert_eq!(cp.next_actionable_phase(), Some("forge_qa"));

        // With skip_map on forge_qa → plan_review is next
        let mut skip_map = HashMap::new();
        skip_map.insert("forge_qa".into(), "auto_skipped".into());
        cp.skip_map = Some(skip_map);
        assert_eq!(cp.next_actionable_phase(), Some("plan_review"));
    }

    #[test]
    fn test_infer_phase_skip_map_aware() {
        // Real-world scenario: forge completed, forge_qa + semantic_verification in skip_map,
        // plan_review pending → should transition to plan_review, NOT forge_qa
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            completed_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus::pending());
        cp.phases.insert("plan_review".into(), PhaseStatus::pending());

        let mut skip_map = HashMap::new();
        skip_map.insert("forge_qa".into(), "auto_skipped".into());
        cp.skip_map = Some(skip_map);

        match cp.infer_phase_position() {
            PhasePosition::Transitioning { last_completed, next_pending, .. } => {
                assert_eq!(last_completed, "forge");
                assert_eq!(next_pending, "plan_review"); // NOT forge_qa
            }
            other => panic!("Expected Transitioning, got {:?}", other),
        }
    }

    #[test]
    fn test_is_phase_done_or_will_skip() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(),
            ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "skipped".into(),
            ..Default::default()
        });
        cp.phases.insert("semantic_verification".into(), PhaseStatus::pending());
        cp.phases.insert("work".into(), PhaseStatus::pending());

        let mut skip_map = HashMap::new();
        skip_map.insert("semantic_verification".into(), "codex_disabled".into());
        cp.skip_map = Some(skip_map);

        assert!(cp.is_phase_done_or_will_skip("forge")); // completed
        assert!(cp.is_phase_done_or_will_skip("forge_qa")); // skipped
        assert!(cp.is_phase_done_or_will_skip("semantic_verification")); // in skip_map
        assert!(!cp.is_phase_done_or_will_skip("work")); // pending, not in skip_map
    }

    // ── failed phase detection tests ─────────────────────

    #[test]
    fn test_infer_phase_failed_detected() {
        // completed → completed → skipped → skipped → completed → failed → completed → pending
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(), ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "completed".into(), ..Default::default()
        });
        cp.phases.insert("plan_review".into(), PhaseStatus {
            status: "skipped".into(), ..Default::default()
        });
        cp.phases.insert("plan_refine".into(), PhaseStatus {
            status: "skipped".into(), ..Default::default()
        });
        cp.phases.insert("verification".into(), PhaseStatus {
            status: "completed".into(), ..Default::default()
        });
        cp.phases.insert("semantic_verification".into(), PhaseStatus {
            status: "failed".into(), ..Default::default()
        });
        cp.phases.insert("design_extraction".into(), PhaseStatus {
            status: "completed".into(), ..Default::default()
        });
        cp.phases.insert("work".into(), PhaseStatus::pending());

        match cp.infer_phase_position() {
            PhasePosition::Failed { failed_phase, next_pending } => {
                assert_eq!(failed_phase, "semantic_verification");
                assert_eq!(next_pending, Some("work"));
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
        // effective phase should be the failed one
        assert_eq!(cp.inferred_phase_name(), Some("semantic_verification"));
    }

    #[test]
    fn test_infer_phase_failed_no_pending() {
        // All done except one failed — no more pending
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "completed".into(), ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "failed".into(), ..Default::default()
        });

        match cp.infer_phase_position() {
            PhasePosition::Failed { failed_phase, next_pending } => {
                assert_eq!(failed_phase, "forge_qa");
                assert_eq!(next_pending, None);
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn test_in_progress_takes_priority_over_failed() {
        // If a failed phase is being retried (another phase is in_progress),
        // Running takes priority
        let mut cp = Checkpoint::default();
        cp.phases.insert("forge".into(), PhaseStatus {
            status: "failed".into(), ..Default::default()
        });
        cp.phases.insert("forge_qa".into(), PhaseStatus {
            status: "in_progress".into(),
            started_at: Some("2026-03-20T00:05:00Z".into()),
            ..Default::default()
        });

        match cp.infer_phase_position() {
            PhasePosition::Running { phase, .. } => {
                assert_eq!(phase, "forge_qa");
            }
            other => panic!("Expected Running, got {:?}", other),
        }
    }

    // ── Terminal phase completion tests ─────────────────────────────

    /// Helper: build a checkpoint with merge completed but some earlier
    /// phases still "pending" — the exact scenario that caused the
    /// post-merge restart loop.
    fn checkpoint_merge_done_but_pending_earlier() -> Checkpoint {
        use crate::checkpoint::phase_order::PHASE_ORDER;
        let mut cp = Checkpoint::default();
        for &phase in PHASE_ORDER {
            let status = match phase {
                // Simulate: some early phases pending (never populated by Rune)
                "design_extraction" | "design_prototype" | "storybook_verification"
                | "design_verification" | "design_verification_qa" | "ux_verification"
                | "design_iteration" => "pending",
                // Everything else completed
                _ => "completed",
            };
            cp.phases.insert(phase.to_string(), PhaseStatus {
                status: status.to_string(),
                ..Default::default()
            });
        }
        cp
    }

    #[test]
    fn test_is_terminal_phase_completed_true() {
        let cp = checkpoint_merge_done_but_pending_earlier();
        assert!(cp.is_terminal_phase_completed());
    }

    #[test]
    fn test_is_terminal_phase_completed_false_when_merge_pending() {
        let mut cp = Checkpoint::default();
        cp.phases.insert("merge".to_string(), PhaseStatus {
            status: "pending".to_string(),
            ..Default::default()
        });
        assert!(!cp.is_terminal_phase_completed());
    }

    #[test]
    fn test_is_terminal_phase_completed_true_when_merge_skipped() {
        // auto_merge=false case: Rune marks merge as "skipped", which is
        // still a successful terminal state — pipeline ran to its end.
        let mut cp = Checkpoint::default();
        cp.phases.insert("merge".to_string(), PhaseStatus {
            status: "skipped".to_string(),
            ..Default::default()
        });
        assert!(
            cp.is_terminal_phase_completed(),
            "skipped merge (auto_merge=false) must count as terminal completion"
        );
    }

    #[test]
    fn test_is_complete_with_terminal_phase_done_despite_pending() {
        // This is THE bug scenario: merge done, some phases pending.
        // Before the fix, is_complete() returned false → crash loop.
        let cp = checkpoint_merge_done_but_pending_earlier();
        assert!(
            cp.is_complete(),
            "is_complete() should return true when merge (terminal) is completed, \
             even if earlier phases are still pending"
        );
    }

    #[test]
    fn test_infer_phase_position_alldone_when_merge_completed() {
        // With merge completed, infer_phase_position should return AllDone
        // (not Transitioning with empty next_pending).
        let cp = checkpoint_merge_done_but_pending_earlier();
        assert!(
            matches!(cp.infer_phase_position(), PhasePosition::AllDone),
            "Expected AllDone when merge is completed, got {:?}",
            cp.infer_phase_position()
        );
    }

    #[test]
    fn test_inferred_phase_name_none_when_merge_completed() {
        // inferred_phase_name should return None (AllDone) — not "work"
        // or any other phase that would trigger a restart.
        let cp = checkpoint_merge_done_but_pending_earlier();
        assert_eq!(
            cp.inferred_phase_name(),
            None,
            "inferred_phase_name should be None when pipeline is AllDone"
        );
    }
}