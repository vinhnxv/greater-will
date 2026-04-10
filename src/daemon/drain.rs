//! Pre-shutdown drain: snapshot running sessions before daemon exits.
//!
//! When the daemon receives a shutdown signal, `drain_running_sessions`
//! captures each running session's pane content and persists a snapshot
//! file alongside `meta.json`. On the next startup the reconciler can
//! use this snapshot to decide whether to re-spawn the run from scratch.

use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::RunRegistry;
use crate::daemon::state::gw_home;
use crate::session::spawn;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Snapshot of a running session captured during drain.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Run ID this snapshot belongs to.
    pub run_id: String,
    /// Plan file path.
    pub plan_path: PathBuf,
    /// Repository directory.
    pub repo_dir: PathBuf,
    /// Phase the run was in at drain time.
    pub phase: Option<String>,
    /// Whether the tmux session was alive when we captured.
    pub tmux_alive: bool,
    /// Last N lines of pane content (for diagnostics).
    pub pane_tail: Option<String>,
    /// Timestamp of the snapshot.
    pub drained_at: String,
    /// Number of crash restarts so far.
    pub crash_restarts: u32,
}

/// Maximum lines to capture from pane on drain.
const DRAIN_PANE_TAIL_LINES: usize = 100;

/// Drain all running sessions: capture snapshots and persist to disk.
///
/// Called during graceful shutdown, before the daemon process exits.
/// This does NOT kill any tmux sessions — they remain alive for the
/// reconciler to pick up on the next start.
pub async fn drain_running_sessions(registry: Arc<Mutex<RunRegistry>>) -> u32 {
    let reg = registry.lock().await;

    let running: Vec<_> = reg
        .list_runs(false)
        .iter()
        .filter(|r| r.status == RunStatus::Running)
        .map(|r| r.run_id.clone())
        .collect();

    if running.is_empty() {
        debug!("no running sessions to drain");
        return 0;
    }

    info!(count = running.len(), "draining running sessions before shutdown");

    let mut drained = 0u32;

    for run_id in &running {
        if let Some(entry) = reg.get(run_id) {
            let tmux_session = entry.tmux_session.clone().unwrap_or_default();
            let tmux_alive = if tmux_session.is_empty() {
                false
            } else {
                spawn::has_session(&tmux_session)
            };

            // Capture pane tail if session is alive
            let pane_tail = if tmux_alive && !tmux_session.is_empty() {
                match spawn::capture_pane(&tmux_session) {
                    Ok(content) => {
                        let lines: Vec<&str> = content.lines().collect();
                        let start = lines.len().saturating_sub(DRAIN_PANE_TAIL_LINES);
                        Some(lines[start..].join("\n"))
                    }
                    Err(e) => {
                        debug!(run_id = %run_id, error = %e, "failed to capture pane during drain");
                        None
                    }
                }
            } else {
                None
            };

            let snapshot = SessionSnapshot {
                run_id: run_id.clone(),
                plan_path: entry.plan_path.clone(),
                repo_dir: entry.repo_dir.clone(),
                phase: entry.current_phase.clone(),
                tmux_alive,
                pane_tail,
                drained_at: Utc::now().to_rfc3339(),
                crash_restarts: entry.crash_restarts,
            };

            if let Err(e) = write_snapshot(&snapshot) {
                warn!(run_id = %run_id, error = %e, "failed to write drain snapshot");
            } else {
                info!(run_id = %run_id, tmux_alive = tmux_alive, "session snapshot saved");
                drained += 1;
            }
        }
    }

    info!(drained = drained, "drain complete");
    drained
}

/// Write a snapshot to `~/.gw/runs/{run_id}/snapshot.json`.
fn write_snapshot(snapshot: &SessionSnapshot) -> color_eyre::Result<()> {
    let run_dir = gw_home().join("runs").join(&snapshot.run_id);
    std::fs::create_dir_all(&run_dir)?;

    let snapshot_path = run_dir.join("snapshot.json");
    let tmp_path = run_dir.join("snapshot.json.tmp");

    let json = serde_json::to_string_pretty(snapshot)?;
    std::fs::write(&tmp_path, &json)?;

    // Fsync + atomic rename for crash safety
    let f = std::fs::File::open(&tmp_path)?;
    f.sync_all()?;
    std::fs::rename(&tmp_path, &snapshot_path)?;

    Ok(())
}

/// Read a snapshot from disk, if it exists.
pub fn read_snapshot(run_id: &str) -> Option<SessionSnapshot> {
    let path = gw_home().join("runs").join(run_id).join("snapshot.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Remove a snapshot after successful recovery.
pub fn clear_snapshot(run_id: &str) {
    let path = gw_home().join("runs").join(run_id).join("snapshot.json");
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serde_round_trip() {
        // Test serialization without relying on gw_home() (which uses OnceLock)
        let tmp = tempfile::TempDir::new().unwrap();
        let run_dir = tmp.path().join("abc12345");
        std::fs::create_dir_all(&run_dir).unwrap();

        let snapshot = SessionSnapshot {
            run_id: "abc12345".to_string(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            phase: Some("work".to_string()),
            tmux_alive: true,
            pane_tail: Some("last line of output".to_string()),
            drained_at: "2026-01-01T00:00:00Z".to_string(),
            crash_restarts: 0,
        };

        // Write directly to temp path
        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        let snap_path = run_dir.join("snapshot.json");
        std::fs::write(&snap_path, &json).unwrap();

        // Read back and verify
        let content = std::fs::read_to_string(&snap_path).unwrap();
        let loaded: SessionSnapshot = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.run_id, "abc12345");
        assert_eq!(loaded.phase, Some("work".to_string()));
        assert!(loaded.tmux_alive);
        assert_eq!(loaded.pane_tail, Some("last line of output".to_string()));

        // Clear
        std::fs::remove_file(&snap_path).unwrap();
        assert!(!snap_path.exists());
    }
}
