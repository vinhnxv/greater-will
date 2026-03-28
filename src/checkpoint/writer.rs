#![allow(dead_code)]
//! Checkpoint writer for Rune arc checkpoints.
//!
//! Provides atomic write operations for checkpoint.json files.
//! Uses the standard pattern: write to `.tmp` file → fsync → rename.
//!
//! # Atomicity Guarantee
//!
//! The atomic write pattern ensures:
//! 1. Partial writes never corrupt the original file
//! 2. Readers always see either the old or new complete file
//! 3. System crashes leave either old or new file, never partial
//!
//! # Example
//!
//! ```ignore
//! use crate::checkpoint::writer::write_checkpoint;
//!
//! // Modify checkpoint
//! checkpoint.phases.insert("work".into(), PhaseStatus::pending());
//!
//! // Write atomically
//! write_checkpoint(&checkpoint, ".rune/arc/arc-123/checkpoint.json")?;
//! ```

use crate::checkpoint::schema::Checkpoint;
use color_eyre::eyre::Context;
use color_eyre::Result;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Default temporary file suffix for atomic writes.
const TMP_SUFFIX: &str = ".tmp";

/// Write a checkpoint to a file atomically.
///
/// Uses the atomic write pattern:
/// 1. Write to `<path>.tmp`
/// 2. Fsync the temp file
/// 3. Rename temp file to final path
///
/// This ensures readers never see a partially-written checkpoint.
///
/// # Arguments
///
/// * `checkpoint` - The checkpoint to write
/// * `path` - Target file path (will be created if doesn't exist)
///
/// # Errors
///
/// Returns an error if:
/// - Cannot create parent directories
/// - Cannot write to temp file
/// - Cannot fsync or rename
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::writer::write_checkpoint;
///
/// write_checkpoint(&checkpoint, ".rune/arc/arc-123/checkpoint.json")?;
/// ```
pub fn write_checkpoint<P: AsRef<Path>>(checkpoint: &Checkpoint, path: P) -> Result<()> {
    let path = path.as_ref();
    write_checkpoint_with_tmp(checkpoint, path, format!("{}{}", path.display(), TMP_SUFFIX))
}

/// Write a checkpoint atomically with a custom temp file path.
///
/// Use this when you need to control the temp file location,
/// e.g., when writing to a different filesystem.
///
/// # Arguments
///
/// * `checkpoint` - The checkpoint to write
/// * `target_path` - Final destination path
/// * `tmp_path` - Temporary file path to write first
pub fn write_checkpoint_with_tmp<P1: AsRef<Path>, P2: AsRef<Path>>(
    checkpoint: &Checkpoint,
    target_path: P1,
    tmp_path: P2,
) -> Result<()> {
    let target_path = target_path.as_ref();
    let tmp_path = tmp_path.as_ref();

    info!(
        "Writing checkpoint {} to {}",
        checkpoint.id,
        target_path.display()
    );

    // Ensure parent directory exists
    if let Some(parent) = target_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("Failed to create parent directory: {}", parent.display())
            })?;
            debug!("Created parent directory: {}", parent.display());
        }
    }

    // Write to temp file
    write_to_file(checkpoint, tmp_path)?;

    // Fsync the temp file
    fsync_file(tmp_path)?;

    // Atomic rename
    fs::rename(tmp_path, target_path).wrap_err_with(|| {
        format!(
            "Failed to rename {} to {}",
            tmp_path.display(),
            target_path.display()
        )
    })?;

    info!("Checkpoint written atomically to {}", target_path.display());

    Ok(())
}

/// Write checkpoint JSON to a file (non-atomically).
///
/// This is the internal write function. For production use,
/// prefer `write_checkpoint` which provides atomicity.
fn write_to_file<P: AsRef<Path>>(checkpoint: &Checkpoint, path: P) -> Result<()> {
    let path = path.as_ref();

    let file = File::create(path)
        .wrap_err_with(|| format!("Failed to create file: {}", path.display()))?;

    let mut writer = BufWriter::new(file);

    // Serialize to JSON with pretty printing
    serde_json::to_writer_pretty(&mut writer, checkpoint)
        .wrap_err_with(|| "Failed to serialize checkpoint to JSON")?;

    // Add trailing newline
    writer.write_all(b"\n").wrap_err("Failed to write trailing newline")?;

    writer.flush().wrap_err("Failed to flush writer")?;

    debug!("Wrote JSON to {}", path.display());

    Ok(())
}

/// Fsync a file to ensure data is persisted to disk.
fn fsync_file<P: AsRef<Path>>(path: P) -> Result<()> {
    let path = path.as_ref();

    let file = File::open(path).wrap_err_with(|| format!("Failed to open file for fsync: {}", path.display()))?;

    file.sync_all()
        .wrap_err_with(|| format!("Failed to sync file: {}", path.display()))?;

    debug!("fsync completed for {}", path.display());

    Ok(())
}

/// Create a fresh checkpoint for a new arc.
///
/// Use this when starting a new arc (no existing checkpoint).
/// All phases start as "pending".
///
/// # Arguments
///
/// * `id` - Unique arc identifier (e.g., "arc-1739123456789")
/// * `plan_file` - Path to the plan file
/// * `config_dir` - Configuration directory (optional)
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::writer::create_fresh_checkpoint;
///
/// let checkpoint = create_fresh_checkpoint(
///     "arc-1739123456789",
///     "plans/feature.md",
///     Some(".rune")
/// )?;
/// ```
pub fn create_fresh_checkpoint(
    id: impl Into<String>,
    plan_file: impl Into<String>,
    config_dir: Option<&str>,
) -> Checkpoint {
    use crate::checkpoint::phase_order::PHASE_ORDER;
    use crate::checkpoint::schema::{PhaseStatus, SCHEMA_VERSION_MAX};
    use chrono::Utc;

    let mut phases = std::collections::HashMap::new();
    for &phase_name in PHASE_ORDER {
        phases.insert(phase_name.to_string(), PhaseStatus::pending());
    }

    Checkpoint {
        id: id.into(),
        schema_version: Some(SCHEMA_VERSION_MAX),
        plan_file: plan_file.into(),
        config_dir: config_dir.unwrap_or("").to_string(),
        phases,
        started_at: Utc::now().to_rfc3339(),
        phase_sequence: Some(0),
        ..Default::default()
    }
}

/// Backup an existing checkpoint before modification.
///
/// Creates a backup at `<path>.bak`. Useful before making
/// experimental changes.
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::writer::backup_checkpoint;
///
/// backup_checkpoint(".rune/arc/arc-123/checkpoint.json")?;
/// // Backup is at .rune/arc/arc-123/checkpoint.json.bak
/// ```
pub fn backup_checkpoint<P: AsRef<Path>>(path: P) -> Result<PathBuf> {
    let path = path.as_ref();
    let backup_path = path.with_extension("json.bak");

    if path.exists() {
        fs::copy(path, &backup_path).wrap_err_with(|| {
            format!(
                "Failed to backup {} to {}",
                path.display(),
                backup_path.display()
            )
        })?;
        info!("Created backup at {}", backup_path.display());
    } else {
        warn!("Cannot backup: {} does not exist", path.display());
    }

    Ok(backup_path)
}

/// Update checkpoint for a completed phase.
///
/// Marks the phase as completed with an optional artifact.
/// Computes the artifact hash if provided.
///
/// # Arguments
///
/// * `checkpoint` - Mutable checkpoint to update
/// * `phase_name` - Name of the completed phase
/// * `artifact_path` - Optional path to artifact file (for hash computation)
///
/// # Example
///
/// ```ignore
/// use crate::checkpoint::writer::mark_phase_completed;
///
/// mark_phase_completed(&mut checkpoint, "forge", Some("tmp/arc/forge-report.md"))?;
/// ```
pub fn mark_phase_completed<P: AsRef<Path>>(
    checkpoint: &mut Checkpoint,
    phase_name: &str,
    artifact_path: Option<P>,
) -> Result<()> {
    use crate::checkpoint::reader::compute_file_hash;
    use chrono::Utc;

    let phase = checkpoint
        .phases
        .entry(phase_name.to_string())
        .or_insert_with(crate::checkpoint::schema::PhaseStatus::pending);

    phase.status = "completed".into();
    phase.completed_at = Some(Utc::now().to_rfc3339());

    if let Some(artifact) = artifact_path {
        let artifact = artifact.as_ref();
        let artifact_str = artifact.to_string_lossy().to_string();

        phase.artifact = Some(artifact_str);

        // Compute hash if file exists
        if artifact.exists() {
            match compute_file_hash(artifact) {
                Ok(hash) => {
                    phase.artifact_hash = Some(hash);
                    debug!("Computed artifact hash for {}", phase_name);
                }
                Err(e) => {
                    warn!("Failed to compute artifact hash: {}", e);
                }
            }
        }
    }

    // Advance phase_sequence
    if let Some(idx) = crate::checkpoint::phase_order::phase_index(phase_name) {
        checkpoint.phase_sequence = Some((idx + 1) as u32);
    }

    info!("Marked phase {} as completed", phase_name);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::phase_order::PHASE_ORDER;
    use crate::checkpoint::schema::PhaseStatus;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_test_checkpoint() -> Checkpoint {
        let mut phases = HashMap::new();
        for &phase in PHASE_ORDER {
            phases.insert(phase.to_string(), PhaseStatus::pending());
        }

        Checkpoint {
            id: "arc-test".into(),
            schema_version: Some(25),
            plan_file: "plans/test.md".into(),
            phases,
            started_at: "2026-03-25T00:00:00Z".into(),
            phase_sequence: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn test_write_and_read_checkpoint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("checkpoint.json");

        let original = make_test_checkpoint();

        // Write
        write_checkpoint(&original, &path).unwrap();

        // Verify file exists
        assert!(path.exists());

        // Verify no temp file left
        assert!(!path.with_extension("json.tmp").exists());

        // Read back
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Checkpoint = serde_json::from_str(&contents).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.schema_version, original.schema_version);
        assert_eq!(parsed.plan_file, original.plan_file);
    }

    #[test]
    fn test_write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/deep/dir/checkpoint.json");

        let checkpoint = make_test_checkpoint();

        write_checkpoint(&checkpoint, &path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn test_create_fresh_checkpoint() {
        let checkpoint = create_fresh_checkpoint(
            "arc-1739123456789",
            "plans/feature.md",
            Some(".rune"),
        );

        assert_eq!(checkpoint.id, "arc-1739123456789");
        assert_eq!(checkpoint.plan_file, "plans/feature.md");
        assert_eq!(checkpoint.config_dir, ".rune");
        assert!(!checkpoint.started_at.is_empty());
        assert_eq!(checkpoint.phase_sequence, Some(0));

        // All phases should be pending
        for phase in PHASE_ORDER {
            assert_eq!(checkpoint.phases[*phase].status, "pending");
        }
    }

    #[test]
    fn test_backup_checkpoint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("checkpoint.json");
        let backup_path = dir.path().join("checkpoint.json.bak");

        // Write original
        let checkpoint = make_test_checkpoint();
        write_checkpoint(&checkpoint, &path).unwrap();

        // Backup
        let result = backup_checkpoint(&path).unwrap();
        assert_eq!(result, backup_path);

        // Verify backup exists
        assert!(backup_path.exists());

        // Verify backup has same content
        let original = std::fs::read_to_string(&path).unwrap();
        let backup = std::fs::read_to_string(&backup_path).unwrap();
        assert_eq!(original, backup);
    }

    #[test]
    fn test_mark_phase_completed() {
        let mut checkpoint = make_test_checkpoint();

        let dir = TempDir::new().unwrap();
        let artifact_path = dir.path().join("artifact.md");
        std::fs::write(&artifact_path, "test artifact content").unwrap();

        mark_phase_completed(&mut checkpoint, "forge", Some(&artifact_path)).unwrap();

        assert_eq!(checkpoint.phases["forge"].status, "completed");
        assert!(checkpoint.phases["forge"].completed_at.is_some());
        assert!(checkpoint.phases["forge"].artifact.is_some());
        assert!(checkpoint.phases["forge"].artifact_hash.is_some());

        // phase_sequence should advance to 1 (next phase)
        assert_eq!(checkpoint.phase_sequence, Some(1));
    }

    #[test]
    fn test_atomic_write_on_simulated_crash() {
        // Simulate what happens if we crash after temp file write
        // but before rename
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("checkpoint.json");
        let tmp_path = dir.path().join("checkpoint.json.tmp");

        // Write original
        let checkpoint = make_test_checkpoint();
        write_checkpoint(&checkpoint, &path).unwrap();

        // Create a temp file that "crashed"
        std::fs::write(&tmp_path, "partial content").unwrap();

        // Original should still be intact
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("arc-test"));

        // Now write again (should clean up old temp file)
        write_checkpoint(&checkpoint, &path).unwrap();

        // Temp file should be gone
        assert!(!tmp_path.exists());
    }

    #[test]
    fn test_roundtrip_preserves_all_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("checkpoint.json");

        let mut original = make_test_checkpoint();
        original.skip_map = Some(HashMap::from([
            ("semantic_verification".into(), "not_needed".into()),
        ]));
        original.pr_url = Some("https://github.com/org/repo/pull/1".into());
        original.commits = vec!["abc123".into(), "def456".into()];
        original.extra.insert("custom_field".into(), serde_json::json!("custom_value"));

        // Write and read back
        write_checkpoint(&original, &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Checkpoint = serde_json::from_str(&contents).unwrap();

        assert_eq!(parsed.skip_map, original.skip_map);
        assert_eq!(parsed.pr_url, original.pr_url);
        assert_eq!(parsed.commits, original.commits);
        assert_eq!(
            parsed.extra.get("custom_field"),
            original.extra.get("custom_field")
        );
    }
}