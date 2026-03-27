//! Checkpoint reader for Rune arc checkpoints.
//!
//! Provides functions to read, validate, and inspect checkpoint.json files.
//! Includes SHA256 hash validation for artifacts.

use crate::checkpoint::phase_order::{phase_index, PHASE_ORDER};
use crate::checkpoint::schema::{Checkpoint, PhaseStatus};
use color_eyre::eyre::{Context, eyre};
use color_eyre::Result;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use tracing::{debug, info, warn};

/// Read a checkpoint from a file path.
///
/// This function:
/// 1. Reads the JSON file
/// 2. Parses into a `Checkpoint` struct
/// 3. Validates the schema version
/// 4. Logs warnings for compatibility issues
///
/// # Errors
///
/// Returns an error if:
/// - File doesn't exist
/// - File cannot be read
/// - JSON is malformed
/// - Required fields are missing (id, plan_file)
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::read_checkpoint;
///
/// let checkpoint = read_checkpoint(".rune/arc/arc-123/checkpoint.json")?;
/// println!("Arc ID: {}", checkpoint.id);
/// ```
pub fn read_checkpoint<P: AsRef<Path>>(path: P) -> Result<Checkpoint> {
    let path = path.as_ref();
    debug!("Reading checkpoint from: {}", path.display());

    // Check file exists
    if !path.exists() {
        return Err(eyre!("Checkpoint file not found: {}", path.display()));
    }

    // Read file contents
    let mut file = File::open(path)
        .wrap_err_with(|| format!("Failed to open checkpoint file: {}", path.display()))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .wrap_err_with(|| format!("Failed to read checkpoint file: {}", path.display()))?;

    // Parse JSON
    let checkpoint: Checkpoint = serde_json::from_str(&contents)
        .wrap_err_with(|| {
            format!(
                "Failed to parse checkpoint JSON from {}. Invalid schema?",
                path.display()
            )
        })?;

    // Validate schema version
    let compat = checkpoint.schema_compat();
    if !compat.is_compatible() {
        if let Some(warning) = compat.warning() {
            warn!("{}", warning);
        }
    }

    // Validate required fields
    if checkpoint.id.is_empty() {
        return Err(eyre!("Checkpoint missing required field: id"));
    }
    if checkpoint.plan_file.is_empty() {
        return Err(eyre!("Checkpoint missing required field: plan_file"));
    }

    debug!(
        "Loaded checkpoint {} (schema v{:?})",
        checkpoint.id, checkpoint.schema_version
    );

    Ok(checkpoint)
}

/// Read a checkpoint, returning None if the file doesn't exist.
///
/// Use this when you want to gracefully handle missing checkpoints
/// (e.g., when starting a new arc).
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::try_read_checkpoint;
///
/// match try_read_checkpoint("checkpoint.json")? {
///     Some(cp) => println!("Found existing arc: {}", cp.id),
///     None => println!("No existing checkpoint, starting new arc"),
/// }
/// ```
pub fn try_read_checkpoint<P: AsRef<Path>>(path: P) -> Result<Option<Checkpoint>> {
    let path = path.as_ref();

    if !path.exists() {
        return Ok(None);
    }

    read_checkpoint(path).map(Some)
}

/// Validate that an artifact's SHA256 hash matches the expected value.
///
/// # Arguments
///
/// * `artifact_path` - Path to the artifact file
/// * `expected_hash` - Expected SHA256 hash (hex encoded, lowercase)
///
/// # Returns
///
/// - `Ok(true)` if hashes match
/// - `Ok(false)` if hashes don't match
/// - `Err` if the file cannot be read
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::validate_artifact_hash;
///
/// let valid = validate_artifact_hash(
///     "tmp/arc/forge-report.md",
///     "a1b2c3d4..."
/// )?;
/// if !valid {
///     warn!("Artifact hash mismatch!");
/// }
/// ```
pub fn validate_artifact_hash<P: AsRef<Path>>(
    artifact_path: P,
    expected_hash: &str,
) -> Result<bool> {
    let path = artifact_path.as_ref();

    // Read file
    let mut file = File::open(path)
        .wrap_err_with(|| format!("Failed to open artifact: {}", path.display()))?;

    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .wrap_err_with(|| format!("Failed to read artifact: {}", path.display()))?;

    // Compute hash
    let hash = Sha256::digest(&contents);
    let hash_hex = format!("{:x}", hash);

    Ok(hash_hex == expected_hash)
}

/// Compute SHA256 hash of a file.
///
/// Returns the hex-encoded hash string (lowercase).
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::compute_file_hash;
///
/// let hash = compute_file_hash("report.md")?;
/// println!("SHA256: {}", hash);
/// ```
pub fn compute_file_hash<P: AsRef<Path>>(path: P) -> Result<String> {
    let path = path.as_ref();

    let mut file = File::open(path)
        .wrap_err_with(|| format!("Failed to open file: {}", path.display()))?;

    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .wrap_err_with(|| format!("Failed to read file: {}", path.display()))?;

    let hash = Sha256::digest(&contents);
    Ok(format!("{:x}", hash))
}

/// Validate all artifact hashes in a checkpoint.
///
/// Returns a list of validation failures (phase name, artifact path, issue).
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::validate_all_artifacts;
///
/// let failures = validate_all_artifacts(&checkpoint, ".rune/arc/arc-123")?;
/// for (phase, path, issue) in &failures {
///     warn!("Artifact validation failed for {}: {} - {}", phase, path, issue);
/// }
/// ```
pub fn validate_all_artifacts<P: AsRef<Path>>(
    checkpoint: &Checkpoint,
    arc_dir: P,
) -> Result<Vec<(String, String, String)>> {
    let arc_dir = arc_dir.as_ref();
    let mut failures = Vec::new();

    for (phase_name, phase_status) in &checkpoint.phases {
        // Only validate completed phases with artifacts
        if phase_status.status != "completed" {
            continue;
        }

        if let Some(artifact) = &phase_status.artifact {
            let artifact_path = arc_dir.join(artifact);

            // Check artifact exists
            if !artifact_path.exists() {
                failures.push((
                    phase_name.clone(),
                    artifact.clone(),
                    "Artifact file not found".into(),
                ));
                continue;
            }

            // Check hash if present
            if let Some(expected_hash) = &phase_status.artifact_hash {
                match validate_artifact_hash(&artifact_path, expected_hash) {
                    Ok(true) => {} // Hash matches
                    Ok(false) => {
                        failures.push((
                            phase_name.clone(),
                            artifact.clone(),
                            "Hash mismatch - artifact may have been modified".into(),
                        ));
                    }
                    Err(e) => {
                        failures.push((
                            phase_name.clone(),
                            artifact.clone(),
                            format!("Failed to validate hash: {}", e),
                        ));
                    }
                }
            }
        }
    }

    Ok(failures)
}

/// Check if two plan file references match.
/// Uses Path::ends_with() for component-aware matching (avoids false positives).
fn plan_matches(checkpoint_plan: &str, target_plan: &str) -> bool {
    let cp = Path::new(checkpoint_plan.trim_start_matches("./"));
    let target = Path::new(target_plan.trim_start_matches("./"));
    cp == target || cp.ends_with(target) || target.ends_with(cp)
}

/// Find the most recent checkpoint for a given plan file.
/// Scans `.rune/arc/arc-*/checkpoint.json` and matches by plan_file metadata.
pub fn find_checkpoint_for_plan(plan_path: &Path, cwd: &Path) -> Result<Option<std::path::PathBuf>> {
    let arc_base = cwd.join(".rune").join("arc");
    if !arc_base.exists() {
        return Ok(None);
    }

    let plan_str = plan_path.to_string_lossy();
    let mut best: Option<(std::path::PathBuf, String)> = None;

    let entries = std::fs::read_dir(&arc_base)
        .wrap_err_with(|| format!("Failed to read arc directory: {}", arc_base.display()))?;

    for entry in entries {
        let entry = entry?;
        let dir_name = entry.file_name();
        let dir_name_str = dir_name.to_string_lossy();
        if !dir_name_str.starts_with("arc-") || !entry.path().is_dir() {
            continue;
        }
        let cp_path = entry.path().join("checkpoint.json");
        if !cp_path.exists() {
            continue;
        }
        // IMPORTANT: Use .ok().flatten() — NOT ? — to skip corrupted checkpoints gracefully
        match try_read_checkpoint(&cp_path).ok().flatten() {
            Some(cp) if plan_matches(&cp.plan_file, &plan_str) => {
                let started = cp.started_at.clone();
                if started.is_empty() { continue; }  // Skip checkpoints without started_at
                match &best {
                    None => best = Some((cp_path, started)),
                    Some((_, prev_started)) if started > *prev_started => {
                        best = Some((cp_path, started));
                    }
                    _ => {}
                }
            }
            _ => continue,
        }
    }

    Ok(best.map(|(path, _)| path))
}

/// Archive stale checkpoint directories for a given plan.
///
/// After identifying the active checkpoint (by path), moves all OTHER checkpoint
/// directories that match the same plan into `.rune/arc/_archived/`. This prevents
/// `find_checkpoint_for_plan` from scanning them on future calls.
///
/// Safe to call multiple times — already-archived dirs are skipped.
pub fn archive_stale_checkpoints(
    active_checkpoint_path: &Path,
    plan_path: &Path,
    cwd: &Path,
) -> Result<usize> {
    let arc_base = cwd.join(".rune").join("arc");
    if !arc_base.exists() {
        return Ok(0);
    }

    let plan_str = plan_path.to_string_lossy();
    let active_dir = active_checkpoint_path
        .parent()
        .ok_or_else(|| eyre!("Checkpoint path has no parent dir"))?;

    let entries = std::fs::read_dir(&arc_base)
        .wrap_err_with(|| format!("Failed to read arc directory: {}", arc_base.display()))?;

    let mut archived_count = 0;

    for entry in entries {
        let entry = entry?;
        let dir_path = entry.path();
        let dir_name = entry.file_name();
        let dir_name_str = dir_name.to_string_lossy();

        // Only consider arc-* directories
        if !dir_name_str.starts_with("arc-") || !dir_path.is_dir() {
            continue;
        }

        // Skip the active checkpoint dir
        if dir_path == active_dir {
            continue;
        }

        let cp_path = dir_path.join("checkpoint.json");
        if !cp_path.exists() {
            continue;
        }

        // Check if this checkpoint matches the same plan
        let matches = match try_read_checkpoint(&cp_path).ok().flatten() {
            Some(cp) => plan_matches(&cp.plan_file, &plan_str),
            None => false,
        };

        if !matches {
            continue;
        }

        // Move to _archived/
        let archive_base = arc_base.join("_archived");
        if let Err(e) = std::fs::create_dir_all(&archive_base) {
            warn!(error = %e, "Failed to create archive directory");
            continue;
        }

        let dest = archive_base.join(&dir_name);

        // Handle dest collision: if _archived/arc-{id} already exists,
        // remove it first (it's a leftover from a previous interrupted archive).
        if dest.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dest) {
                warn!(
                    dest = %dest.display(),
                    error = %e,
                    "Failed to remove existing archive dest, skipping"
                );
                continue;
            }
        }

        match std::fs::rename(&dir_path, &dest) {
            Ok(()) => {
                info!(
                    from = %dir_name_str,
                    to = %dest.display(),
                    "Archived stale checkpoint"
                );
                archived_count += 1;

                // Also clean up the corresponding artifact dir (tmp/arc/arc-{id}).
                // These are ephemeral build outputs, safe to remove entirely.
                let artifact_dir = cwd.join("tmp").join("arc").join(&dir_name);
                if artifact_dir.is_dir() {
                    match std::fs::remove_dir_all(&artifact_dir) {
                        Ok(()) => {
                            info!(dir = %dir_name_str, "Removed stale artifact dir");
                        }
                        Err(e) => {
                            debug!(
                                dir = %artifact_dir.display(),
                                error = %e,
                                "Failed to remove stale artifact dir (non-fatal)"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    dir = %dir_name_str,
                    error = %e,
                    "Failed to archive stale checkpoint"
                );
            }
        }
    }

    if archived_count > 0 {
        info!(
            count = archived_count,
            plan = %plan_str,
            "Archived stale checkpoints for plan"
        );
    }

    Ok(archived_count)
}

/// Result of pre-resume validation.
#[derive(Debug)]
pub struct ResumeValidation {
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub phases_to_reset: Vec<String>,
}

impl ResumeValidation {
    pub fn can_resume(&self) -> bool {
        self.errors.is_empty()
    }
}

const CRITICAL_PHASES: &[&str] = &["work", "mend", "test", "code_review", "gap_analysis"];

/// Validate checkpoint integrity before resuming.
///
/// Wraps `validate_all_artifacts` and classifies failures as warnings or errors
/// based on whether the phase is critical. Critical phase failures block resume.
pub fn validate_before_resume(
    checkpoint: &Checkpoint,
    arc_dir: &Path,
) -> Result<ResumeValidation> {
    let mut validation = ResumeValidation {
        warnings: Vec::new(),
        errors: Vec::new(),
        phases_to_reset: Vec::new(),
    };

    let failures = validate_all_artifacts(checkpoint, arc_dir)?;

    for (phase_name, artifact_path, issue) in failures {
        let is_critical = CRITICAL_PHASES.contains(&phase_name.as_str());
        let msg = format!(
            "Phase '{}': artifact '{}' — {}",
            phase_name, artifact_path, issue
        );
        if is_critical {
            validation.errors.push(msg);
            validation.phases_to_reset.push(phase_name);
        } else {
            validation.warnings.push(msg);
        }
    }

    Ok(validation)
}

/// Mark all phases before `target_phase` as completed (or skipped if in skip_map).
///
/// This is the core function greater-will uses to manipulate checkpoints
/// for session handoffs. When starting a new session at `target_phase`,
/// all preceding phases must be marked as completed (or skipped) so that
/// arc's --resume mechanism picks up at the correct phase.
///
/// # Arguments
///
/// * `checkpoint` - Mutable reference to the checkpoint to modify
/// * `target_phase` - Name of the phase to resume at
///
/// # Unknown Phases
///
/// If `target_phase` is not in `PHASE_ORDER`, logs a warning and returns
/// without modifying the checkpoint. This handles forward-compatibility
/// when a checkpoint contains phase names from a newer version.
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::mark_phases_completed_before;
///
/// // Before running session starting at "work":
/// mark_phases_completed_before(&mut checkpoint, "work");
/// // Now forge, forge_qa, plan_review, etc. are marked "completed"
/// ```
pub fn mark_phases_completed_before(checkpoint: &mut Checkpoint, target_phase: &str) {
    let target_idx = match phase_index(target_phase) {
        Some(idx) => idx,
        None => {
            warn!(
                "Unknown phase '{}' not in PHASE_ORDER — skipping mark_phases_completed_before",
                target_phase
            );
            return;
        }
    };

    info!(
        "Marking phases before {} (index {}) as completed",
        target_phase, target_idx
    );

    for &phase_name in &PHASE_ORDER[..target_idx] {
        let status = checkpoint
            .phases
            .entry(phase_name.to_string())
            .or_insert_with(PhaseStatus::pending);

        // If phase is in skip_map, mark as "skipped"; otherwise "completed"
        let is_skipped = checkpoint
            .skip_map
            .as_ref()
            .map_or(false, |m| m.contains_key(phase_name));

        if is_skipped {
            status.status = "skipped".into();
            info!("Phase {} marked as skipped", phase_name);
        } else {
            status.status = "completed".into();
        }

        // Set completed_at timestamp if not already set
        if status.completed_at.is_none() {
            status.completed_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    // Ensure target phase exists and is pending
    checkpoint
        .phases
        .entry(target_phase.to_string())
        .or_insert_with(PhaseStatus::pending);

    // CRITICAL: Update phase_sequence to match the target phase index.
    // Arc uses phase_sequence (0-indexed into PHASE_ORDER) to determine
    // which phase to dispatch next. If this is out of sync with the phases
    // map, arc may dispatch the wrong phase on resume.
    checkpoint.phase_sequence = Some(target_idx as u32);

    info!("Updated phase_sequence to {}", target_idx);
}

/// Get a summary of checkpoint status for display.
///
/// Returns a formatted summary suitable for CLI output.
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::reader::checkpoint_summary;
///
/// let summary = checkpoint_summary(&checkpoint);
/// println!("{}", summary);
/// ```
pub fn checkpoint_summary(checkpoint: &Checkpoint) -> String {
    let completed = checkpoint.count_by_status("completed");
    let skipped = checkpoint.count_by_status("skipped");
    let pending = checkpoint.count_by_status("pending");
    let in_progress = checkpoint.count_by_status("in_progress");
    let failed = checkpoint.count_by_status("failed");

    let total = checkpoint.phases.len();
    let progress_pct = if total > 0 {
        (completed + skipped) * 100 / total
    } else {
        0
    };

    format!(
        "Arc: {} | Schema: {} | Progress: {}% ({}/{} phases)\n\
         Completed: {} | Skipped: {} | In Progress: {} | Pending: {} | Failed: {}",
        checkpoint.id,
        checkpoint.schema_version.map(|v| format!("v{}", v)).unwrap_or_else(|| "unknown".to_string()),
        progress_pct,
        completed + skipped,
        total,
        completed,
        skipped,
        in_progress,
        pending,
        failed
    )
}

/// Find the next pending phase in the checkpoint.
///
/// Returns the name of the next phase that has status "pending",
/// or None if all phases are done.
pub fn next_pending_phase(checkpoint: &Checkpoint) -> Option<String> {
    for &phase in PHASE_ORDER {
        if let Some(status) = checkpoint.phases.get(phase) {
            if status.status == "pending" {
                return Some(phase.to_string());
            }
        } else {
            // Phase not in map means it's pending
            return Some(phase.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::schema::{Checkpoint, PhaseStatus};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_test_checkpoint() -> Checkpoint {
        Checkpoint {
            id: "arc-test".into(),
            schema_version: Some(25),
            plan_file: "plans/test.md".into(),
            phases: HashMap::new(),
            started_at: "2026-03-25T00:00:00Z".into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_mark_phases_completed_before_work() {
        let mut cp = make_test_checkpoint();

        mark_phases_completed_before(&mut cp, "work");

        // Work should be pending
        assert_eq!(cp.phases["work"].status, "pending");

        // Phases before work should be completed
        assert_eq!(cp.phases["forge"].status, "completed");
        assert_eq!(cp.phases["forge_qa"].status, "completed");
        assert_eq!(cp.phases["plan_review"].status, "completed");

        // phase_sequence should point to work (index 9)
        assert_eq!(cp.phase_sequence, Some(9));
    }

    #[test]
    fn test_mark_phases_with_skip_map() {
        let mut cp = make_test_checkpoint();
        let mut skip_map = HashMap::new();
        skip_map.insert("semantic_verification".into(), "not_needed".into());
        cp.skip_map = Some(skip_map);

        mark_phases_completed_before(&mut cp, "design_extraction");

        // semantic_verification is after design_extraction, so it's not affected
        // Let's mark before work to test skip behavior
        let mut cp2 = make_test_checkpoint();
        let mut skip_map2 = HashMap::new();
        skip_map2.insert("verification".into(), "not_needed".into());
        cp2.skip_map = Some(skip_map2);

        mark_phases_completed_before(&mut cp2, "task_decomposition");

        // verification (index 4) should be skipped
        assert_eq!(cp2.phases["verification"].status, "skipped");
        assert_eq!(cp2.phases["forge"].status, "completed");
    }

    #[test]
    fn test_next_pending_phase() {
        let mut cp = make_test_checkpoint();
        cp.phases.insert("forge".into(), PhaseStatus::completed(None));
        cp.phases.insert("forge_qa".into(), PhaseStatus::completed(None));

        let next = next_pending_phase(&cp);
        assert_eq!(next, Some("plan_review".into()));
    }

    #[test]
    fn test_next_pending_phase_all_done() {
        let mut cp = make_test_checkpoint();
        for phase in PHASE_ORDER {
            cp.phases.insert(phase.to_string(), PhaseStatus::completed(None));
        }

        let next = next_pending_phase(&cp);
        assert_eq!(next, None);
    }

    #[test]
    fn test_checkpoint_summary() {
        let mut cp = make_test_checkpoint();
        cp.phases.insert("forge".into(), PhaseStatus::completed(None));
        cp.phases.insert("work".into(), PhaseStatus::pending());

        let summary = checkpoint_summary(&cp);
        assert!(summary.contains("arc-test"));
        assert!(summary.contains("v25"));
    }

    #[test]
    fn test_compute_file_hash() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let hash = compute_file_hash(&file_path).unwrap();
        // SHA256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_validate_artifact_hash() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        // Correct hash
        let valid = validate_artifact_hash(
            &file_path,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        )
        .unwrap();
        assert!(valid);

        // Wrong hash
        let invalid = validate_artifact_hash(&file_path, "wronghash").unwrap();
        assert!(!invalid);
    }

    #[test]
    fn test_validate_before_resume_clean() {
        let dir = TempDir::new().unwrap();
        let cp = make_test_checkpoint();
        let result = validate_before_resume(&cp, dir.path()).unwrap();
        assert!(result.can_resume());
        assert!(result.warnings.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_validate_before_resume_missing_critical() {
        let dir = TempDir::new().unwrap();
        let mut cp = make_test_checkpoint();
        let mut work_phase = PhaseStatus::completed(None);
        work_phase.artifact = Some("nonexistent.md".into());
        cp.phases.insert("work".into(), work_phase);
        let result = validate_before_resume(&cp, dir.path()).unwrap();
        assert!(!result.can_resume());
        assert!(!result.phases_to_reset.is_empty());
    }

    #[test]
    fn test_validate_before_resume_missing_noncritical() {
        let dir = TempDir::new().unwrap();
        let mut cp = make_test_checkpoint();
        let mut forge_phase = PhaseStatus::completed(None);
        forge_phase.artifact = Some("nonexistent.md".into());
        cp.phases.insert("forge".into(), forge_phase);
        let result = validate_before_resume(&cp, dir.path()).unwrap();
        assert!(result.can_resume());
        assert!(!result.warnings.is_empty());
    }

    #[test]
    fn test_plan_matches_exact() {
        assert!(plan_matches("plans/test.md", "plans/test.md"));
    }

    #[test]
    fn test_plan_matches_dot_prefix() {
        assert!(plan_matches("./plans/test.md", "plans/test.md"));
        assert!(plan_matches("plans/test.md", "./plans/test.md"));
    }

    #[test]
    fn test_plan_matches_suffix() {
        // Path::ends_with is component-aware
        assert!(plan_matches("/abs/path/plans/test.md", "plans/test.md"));
    }

    #[test]
    fn test_plan_matches_no_false_positive() {
        assert!(!plan_matches("plans/other.md", "plans/test.md"));
    }

    #[test]
    fn test_find_checkpoint_no_arc_dir() {
        let dir = TempDir::new().unwrap();
        let result = find_checkpoint_for_plan(
            Path::new("plans/test.md"), dir.path()
        ).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_archive_stale_checkpoints() {
        use crate::checkpoint::writer::write_checkpoint;

        let dir = TempDir::new().unwrap();
        let arc_base = dir.path().join(".rune").join("arc");

        // Create 3 checkpoint dirs for the same plan, with different started_at
        let timestamps = ["2026-03-25T01:00:00Z", "2026-03-25T02:00:00Z", "2026-03-25T03:00:00Z"];
        let arc_ids = ["arc-1000", "arc-2000", "arc-3000"];

        for (ts, id) in timestamps.iter().zip(arc_ids.iter()) {
            let arc_dir = arc_base.join(id);
            std::fs::create_dir_all(&arc_dir).unwrap();

            let mut cp = make_test_checkpoint();
            cp.id = id.to_string();
            cp.started_at = ts.to_string();
            cp.plan_file = "plans/test.md".to_string();

            write_checkpoint(&cp, arc_dir.join("checkpoint.json")).unwrap();

            // Also create corresponding artifact dirs
            let artifact_dir = dir.path().join("tmp").join("arc").join(id);
            std::fs::create_dir_all(&artifact_dir).unwrap();
            std::fs::write(artifact_dir.join("some-output.md"), "test").unwrap();
        }

        // The newest checkpoint is arc-3000
        let active_path = arc_base.join("arc-3000").join("checkpoint.json");

        let archived = archive_stale_checkpoints(
            &active_path,
            Path::new("plans/test.md"),
            dir.path(),
        ).unwrap();

        // Should archive 2 stale dirs
        assert_eq!(archived, 2);

        // Active dir still exists
        assert!(arc_base.join("arc-3000").exists());

        // Stale checkpoint dirs moved to _archived/
        assert!(!arc_base.join("arc-1000").exists());
        assert!(!arc_base.join("arc-2000").exists());
        assert!(arc_base.join("_archived").join("arc-1000").exists());
        assert!(arc_base.join("_archived").join("arc-2000").exists());

        // Stale artifact dirs removed
        assert!(!dir.path().join("tmp").join("arc").join("arc-1000").exists());
        assert!(!dir.path().join("tmp").join("arc").join("arc-2000").exists());
        // Active artifact dir preserved
        assert!(dir.path().join("tmp").join("arc").join("arc-3000").exists());

        // Calling again is idempotent
        let archived_again = archive_stale_checkpoints(
            &active_path,
            Path::new("plans/test.md"),
            dir.path(),
        ).unwrap();
        assert_eq!(archived_again, 0);
    }

    #[test]
    fn test_archive_handles_dest_collision() {
        use crate::checkpoint::writer::write_checkpoint;

        let dir = TempDir::new().unwrap();
        let arc_base = dir.path().join(".rune").join("arc");

        // Create stale checkpoint
        let stale_dir = arc_base.join("arc-1000");
        std::fs::create_dir_all(&stale_dir).unwrap();
        let mut cp = make_test_checkpoint();
        cp.id = "arc-1000".to_string();
        cp.started_at = "2026-03-25T01:00:00Z".to_string();
        write_checkpoint(&cp, stale_dir.join("checkpoint.json")).unwrap();

        // Create active checkpoint
        let active_dir = arc_base.join("arc-2000");
        std::fs::create_dir_all(&active_dir).unwrap();
        let mut cp2 = make_test_checkpoint();
        cp2.id = "arc-2000".to_string();
        cp2.started_at = "2026-03-25T02:00:00Z".to_string();
        write_checkpoint(&cp2, active_dir.join("checkpoint.json")).unwrap();

        // Pre-create _archived/arc-1000 to simulate collision
        let collision_dir = arc_base.join("_archived").join("arc-1000");
        std::fs::create_dir_all(&collision_dir).unwrap();
        std::fs::write(collision_dir.join("old-data.txt"), "leftover").unwrap();

        let active_path = active_dir.join("checkpoint.json");
        let archived = archive_stale_checkpoints(
            &active_path,
            Path::new("plans/test.md"),
            dir.path(),
        ).unwrap();

        // Should succeed despite collision
        assert_eq!(archived, 1);
        assert!(!arc_base.join("arc-1000").exists());
        // Old collision data replaced
        assert!(arc_base.join("_archived").join("arc-1000").join("checkpoint.json").exists());
    }

    #[test]
    fn test_archive_preserves_different_plan() {
        use crate::checkpoint::writer::write_checkpoint;

        let dir = TempDir::new().unwrap();
        let arc_base = dir.path().join(".rune").join("arc");

        // Create 2 dirs: one for plan A, one for plan B
        for (id, plan) in [("arc-1000", "plans/a.md"), ("arc-2000", "plans/b.md")] {
            let arc_dir = arc_base.join(id);
            std::fs::create_dir_all(&arc_dir).unwrap();

            let mut cp = make_test_checkpoint();
            cp.id = id.to_string();
            cp.plan_file = plan.to_string();

            write_checkpoint(&cp, arc_dir.join("checkpoint.json")).unwrap();
        }

        let active_path = arc_base.join("arc-1000").join("checkpoint.json");

        let archived = archive_stale_checkpoints(
            &active_path,
            Path::new("plans/a.md"),
            dir.path(),
        ).unwrap();

        // Should not archive arc-2000 (different plan)
        assert_eq!(archived, 0);
        assert!(arc_base.join("arc-2000").exists());
    }
}