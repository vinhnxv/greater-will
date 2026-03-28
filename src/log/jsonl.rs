#![allow(dead_code)]
//! JSONL structured log writer for run-level telemetry.
//!
//! Each completed plan execution appends a single JSON line to
//! `.gw/run-log.jsonl`. Log rotation kicks in at 10 MB, keeping
//! the 5 most recent rotated files.

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context;
use color_eyre::Result;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum log file size before rotation (10 MB).
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;

/// Number of rotated files to keep.
const MAX_ROTATED_FILES: usize = 5;

/// Snapshot of machine health at a point in time.
#[derive(Debug, Clone, Serialize)]
pub struct MachineSnapshot {
    pub cpu_percent: f32,
    pub memory_used_mb: u64,
    pub memory_total_mb: u64,
    pub disk_free_gb: f64,
}

impl MachineSnapshot {
    /// Capture current machine health using sysinfo.
    pub fn capture() -> Self {
        use sysinfo::System;

        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();

        let cpu_percent = sys.global_cpu_usage();
        let memory_used_mb = sys.used_memory() / (1024 * 1024);
        let memory_total_mb = sys.total_memory() / (1024 * 1024);

        // Disk free space for root filesystem
        let disk_free_gb = {
            use sysinfo::Disks;
            let disks = Disks::new_with_refreshed_list();
            disks
                .iter()
                .find(|d| d.mount_point() == Path::new("/"))
                .map(|d| d.available_space() as f64 / (1024.0 * 1024.0 * 1024.0))
                .unwrap_or(0.0)
        };

        Self {
            cpu_percent,
            memory_used_mb,
            memory_total_mb,
            disk_free_gb,
        }
    }
}

/// A single run log entry in JSONL format.
#[derive(Debug, Clone, Serialize)]
pub struct GwRunLogEntry {
    pub batch_id: String,
    pub timestamp: DateTime<Utc>,
    pub plan: String,
    pub status: String,
    pub groups_completed: u32,
    pub groups_total: u32,
    pub wallclock_seconds: u64,
    pub error: Option<String>,
    pub machine_health: MachineSnapshot,
}

/// Append a run log entry to the JSONL file at the given path.
///
/// Performs log rotation if the file exceeds [`MAX_LOG_SIZE`].
pub fn append_run_log(entry: &GwRunLogEntry, log_dir: &Path) -> Result<()> {
    fs::create_dir_all(log_dir)
        .wrap_err_with(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let log_path = log_dir.join("run-log.jsonl");

    // Rotate if needed
    if let Ok(meta) = fs::metadata(&log_path) {
        if meta.len() >= MAX_LOG_SIZE {
            rotate_log(&log_path)?;
        }
    }

    let json = serde_json::to_string(entry).wrap_err("Failed to serialize run log entry")?;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .wrap_err_with(|| format!("Failed to open run log: {}", log_path.display()))?;

    writeln!(file, "{}", json)
        .wrap_err_with(|| format!("Failed to write to run log: {}", log_path.display()))?;

    Ok(())
}

/// Rotate a log file: rename current to `.1`, shift `.1`→`.2`, etc.
/// Deletes files beyond [`MAX_ROTATED_FILES`].
fn rotate_log(log_path: &Path) -> Result<()> {
    let base = log_path.to_string_lossy().to_string();

    // Delete the oldest if it exists
    let oldest = PathBuf::from(format!("{}.{}", base, MAX_ROTATED_FILES));
    if oldest.exists() {
        if let Err(e) = fs::remove_file(&oldest) {
            tracing::warn!(error = %e, path = %oldest.display(), "Failed to remove oldest log");
        }
    }

    // Shift existing rotated files
    for i in (1..MAX_ROTATED_FILES).rev() {
        let from = PathBuf::from(format!("{}.{}", base, i));
        let to = PathBuf::from(format!("{}.{}", base, i + 1));
        if from.exists() {
            if let Err(e) = fs::rename(&from, &to) {
                tracing::warn!(error = %e, from = %from.display(), "Failed to rotate log file");
            }
        }
    }

    // Rename current to .1
    let first = PathBuf::from(format!("{}.1", base));
    if log_path.exists() {
        if let Err(e) = fs::rename(log_path, &first) {
            tracing::warn!(error = %e, "Failed to rotate current log file");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_append_run_log_creates_file() {
        let dir = TempDir::new().unwrap();
        let entry = GwRunLogEntry {
            batch_id: "batch-001".into(),
            timestamp: Utc::now(),
            plan: "plans/test.md".into(),
            status: "completed".into(),
            groups_completed: 7,
            groups_total: 7,
            wallclock_seconds: 120,
            error: None,
            machine_health: MachineSnapshot {
                cpu_percent: 25.0,
                memory_used_mb: 8192,
                memory_total_mb: 16384,
                disk_free_gb: 100.0,
            },
        };

        append_run_log(&entry, dir.path()).unwrap();

        let content = fs::read_to_string(dir.path().join("run-log.jsonl")).unwrap();
        assert!(content.contains("batch-001"));
        assert!(content.contains("completed"));
    }

    #[test]
    fn test_append_run_log_appends_multiple() {
        let dir = TempDir::new().unwrap();
        let health = MachineSnapshot {
            cpu_percent: 10.0,
            memory_used_mb: 4096,
            memory_total_mb: 16384,
            disk_free_gb: 50.0,
        };

        for i in 0..3 {
            let entry = GwRunLogEntry {
                batch_id: format!("batch-{:03}", i),
                timestamp: Utc::now(),
                plan: format!("plans/plan-{}.md", i),
                status: "completed".into(),
                groups_completed: 7,
                groups_total: 7,
                wallclock_seconds: 60,
                error: None,
                machine_health: health.clone(),
            };
            append_run_log(&entry, dir.path()).unwrap();
        }

        let content = fs::read_to_string(dir.path().join("run-log.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_rotate_log() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("run-log.jsonl");

        // Create a file with content
        fs::write(&log_path, "original\n").unwrap();

        rotate_log(&log_path).unwrap();

        // Original should be gone, .1 should exist
        assert!(!log_path.exists());
        let rotated = dir.path().join("run-log.jsonl.1");
        assert!(rotated.exists());
        assert_eq!(fs::read_to_string(rotated).unwrap(), "original\n");
    }

    #[test]
    fn test_machine_snapshot_capture() {
        let snap = MachineSnapshot::capture();
        // Just verify it doesn't panic and returns reasonable values
        assert!(snap.memory_total_mb > 0);
    }
}
