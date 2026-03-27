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
    /// Returns `None` only if `phase_sequence` is not set.
    /// When `phase_sequence` is past the end of PHASE_ORDER (arc completed),
    /// returns the last phase in the pipeline instead of `None`.
    pub fn current_phase(&self) -> Option<&'static str> {
        let idx = self.phase_sequence? as usize;
        crate::checkpoint::phase_order::phase_at(idx).or_else(|| {
            // phase_sequence past end means arc completed — return last phase
            let count = crate::checkpoint::phase_order::PHASE_COUNT;
            if count > 0 && idx >= count {
                crate::checkpoint::phase_order::phase_at(count - 1)
            } else {
                None
            }
        })
    }

    /// Check if the arc has completed (all phases done).
    ///
    /// An arc is complete if:
    /// - `completed_at` is set, OR
    /// - All phases are "completed" or "skipped"
    pub fn is_complete(&self) -> bool {
        if self.completed_at.is_some() {
            return true;
        }

        !self.phases.is_empty()
            && self.phases.values().all(|p| {
                p.status == "completed" || p.status == "skipped"
            })
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
        assert_eq!(cp.current_phase(), Some("merge")); // past end → last phase

        cp.phase_sequence = None;
        assert_eq!(cp.current_phase(), None);
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
}