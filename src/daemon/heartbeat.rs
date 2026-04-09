//! Heartbeat monitor for active daemon runs.
//!
//! Periodically checks tmux session health for each `Running` entry,
//! captures logs, updates phase information, and handles crash recovery
//! by either auto-resuming (if a checkpoint exists) or marking as Failed.

use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::RunRegistry;
use crate::daemon::state::gw_home;
use crate::session::spawn;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Interval between heartbeat checks.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum crash-restart cycles before giving up.
const MAX_CRASH_RESTARTS: u32 = 3;

/// Heartbeat monitor that watches over active runs.
///
/// Spawns a background tokio task that periodically:
/// 1. Checks if each Running entry's tmux session is still alive
/// 2. Captures pane output and appends to session logs
/// 3. Updates the current phase based on pane content
/// 4. Performs crash recovery when a tmux session dies unexpectedly
pub struct HeartbeatMonitor {
    registry: Arc<Mutex<RunRegistry>>,
}

impl HeartbeatMonitor {
    /// Create a new heartbeat monitor wrapping the shared registry.
    pub fn new(registry: Arc<Mutex<RunRegistry>>) -> Self {
        Self { registry }
    }

    /// Start the heartbeat loop as a background tokio task.
    ///
    /// Returns a `JoinHandle` that can be used to await or abort the task.
    /// The task runs until the handle is dropped/aborted or the process exits.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            info!("heartbeat monitor started (interval: {:?})", HEARTBEAT_INTERVAL);

            loop {
                interval.tick().await;
                self.check_all_runs().await;
            }
        })
    }

    /// Check all Running entries in the registry.
    async fn check_all_runs(&self) {
        let mut registry = self.registry.lock().await;

        // Collect run IDs to check (avoid holding lock during tmux calls)
        let running: Vec<(String, Option<String>)> = registry
            .list_runs(false)
            .iter()
            .filter(|r| r.status == RunStatus::Running)
            .map(|r| (r.run_id.clone(), r.session_name.clone().into()))
            .collect();

        drop(registry); // Release lock before I/O

        for (run_id, _session_name) in &running {
            self.check_run(run_id).await;
        }
    }

    /// Check a single run's health.
    async fn check_run(&self, run_id: &str) {
        let tmux_session = {
            let registry = self.registry.lock().await;
            match registry.get(run_id) {
                Some(entry) if entry.status == RunStatus::Running => {
                    entry.tmux_session.clone()
                }
                _ => return, // Entry gone or no longer running
            }
        };

        let tmux_session = match tmux_session {
            Some(s) => s,
            None => {
                debug!(run_id = %run_id, "no tmux session associated — skipping");
                return;
            }
        };

        // Check if tmux session is alive (sync call, but fast)
        let session_alive = spawn::has_session(&tmux_session);

        if session_alive {
            self.handle_alive_session(run_id, &tmux_session).await;
        } else {
            self.handle_dead_session(run_id, &tmux_session).await;
        }
    }

    /// Handle a healthy (alive) tmux session: capture logs and update phase.
    async fn handle_alive_session(&self, run_id: &str, tmux_session: &str) {
        // Capture pane output for log archival
        match spawn::capture_pane(tmux_session) {
            Ok(content) => {
                self.append_session_log(run_id, &content);
                self.update_phase_from_pane(run_id, &content).await;
            }
            Err(e) => {
                debug!(run_id = %run_id, error = %e, "failed to capture pane");
            }
        }
    }

    /// Handle a dead tmux session: attempt crash recovery or mark as Failed.
    async fn handle_dead_session(&self, run_id: &str, tmux_session: &str) {
        warn!(run_id = %run_id, tmux_session = %tmux_session, "tmux session is dead");

        let mut registry = self.registry.lock().await;
        let entry = match registry.get_mut(run_id) {
            Some(e) if e.status == RunStatus::Running => e,
            _ => return,
        };

        // Check if we have a checkpoint for crash recovery
        let checkpoint_exists = self.has_checkpoint(&entry.repo_dir);

        if checkpoint_exists && entry.crash_restarts < MAX_CRASH_RESTARTS {
            info!(
                run_id = %run_id,
                crash_restarts = entry.crash_restarts,
                "checkpoint found — scheduling crash recovery"
            );
            entry.crash_restarts += 1;
            entry.current_phase = Some("recovering".to_string());

            // Clone what we need before dropping the lock
            let repo_dir = entry.repo_dir.clone();
            let plan_path = entry.plan_path.clone();
            let session_name = entry.session_name.clone();
            let run_id_owned = run_id.to_string();

            // Persist the crash-restart count
            if let Err(e) = registry.update_status(
                &run_id_owned,
                RunStatus::Running,
                Some("recovering".to_string()),
                None,
            ) {
                warn!(error = %e, "failed to update crash-restart status");
            }

            drop(registry);

            // Spawn recovery in background (don't block heartbeat)
            let registry_clone = Arc::clone(&self.registry);
            tokio::spawn(async move {
                spawn_recovery(
                    registry_clone,
                    &run_id_owned,
                    &repo_dir,
                    &plan_path,
                    &session_name,
                )
                .await;
            });
        } else {
            let reason = if entry.crash_restarts >= MAX_CRASH_RESTARTS {
                format!(
                    "tmux session died after {} crash restarts — giving up",
                    entry.crash_restarts
                )
            } else {
                "tmux session died with no checkpoint — marking failed".to_string()
            };

            warn!(run_id = %run_id, reason = %reason);

            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Failed,
                None,
                Some(reason),
            ) {
                warn!(run_id = %run_id, error = %e, "failed to mark run as failed");
            }
        }
    }

    /// Check if a checkpoint exists in the repo's .rune directory.
    fn has_checkpoint(&self, repo_dir: &std::path::Path) -> bool {
        let rune_dir = repo_dir.join(".rune").join("arc");
        if !rune_dir.is_dir() {
            return false;
        }
        // Look for any checkpoint.json under .rune/arc/
        std::fs::read_dir(&rune_dir)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.path().join("checkpoint.json").exists())
            })
            .unwrap_or(false)
    }

    /// Append captured pane content to the session log.
    fn append_session_log(&self, run_id: &str, content: &str) {
        let log_dir = gw_home().join("runs").join(run_id).join("logs");
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            debug!(error = %e, "failed to create log directory");
            return;
        }

        let log_path = log_dir.join("session.log");
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "--- heartbeat capture ---");
                let _ = write!(f, "{content}");
            }
            Err(e) => {
                debug!(error = %e, "failed to append to session log");
            }
        }
    }

    /// Update the run's current phase based on pane content heuristics.
    async fn update_phase_from_pane(&self, run_id: &str, pane_content: &str) {
        // Look for phase markers in the last few lines
        let last_lines: Vec<&str> = pane_content.lines().rev().take(20).collect();
        let tail = last_lines.join("\n").to_lowercase();

        let phase = if tail.contains("phase_1") || tail.contains("plan") {
            Some("plan")
        } else if tail.contains("phase_2") || tail.contains("work") {
            Some("work")
        } else if tail.contains("phase_3") || tail.contains("review") {
            Some("review")
        } else if tail.contains("phase_4") || tail.contains("test") {
            Some("test")
        } else if tail.contains("phase_5") || tail.contains("ship") || tail.contains("merge") {
            Some("ship")
        } else {
            None
        };

        if let Some(phase_name) = phase {
            let mut registry = self.registry.lock().await;
            if let Some(entry) = registry.get_mut(run_id) {
                if entry.current_phase.as_deref() != Some(phase_name) {
                    debug!(run_id = %run_id, phase = %phase_name, "phase updated");
                    entry.current_phase = Some(phase_name.to_string());
                }
            }
        }

        // Check for pipeline completion
        if crate::engine::single_session::util::is_pipeline_complete(pane_content) {
            info!(run_id = %run_id, "pipeline completed");
            let mut registry = self.registry.lock().await;
            let _ = registry.update_status(
                run_id,
                RunStatus::Succeeded,
                Some("complete".to_string()),
                None,
            );
        }
    }
}

/// Spawn a crash recovery session for a failed run.
///
/// Creates a new tmux session and runs the arc command with `--resume`.
async fn spawn_recovery(
    registry: Arc<Mutex<RunRegistry>>,
    run_id: &str,
    repo_dir: &std::path::Path,
    plan_path: &std::path::Path,
    session_name: &str,
) {
    let tmux_session = format!("gw-{}", run_id);

    info!(
        run_id = %run_id,
        tmux_session = %tmux_session,
        "starting crash recovery"
    );

    // Build the resume command
    let plan_str = plan_path
        .to_str()
        .unwrap_or("plan.md");
    let cmd = format!(
        "/rune:arc '{}' --resume",
        plan_str
    );

    // Spawn tmux session
    let config = spawn::SpawnConfig {
        session_id: tmux_session.clone(),
        working_dir: repo_dir.to_path_buf(),
        config_dir: None,
        claude_path: "claude".to_string(),
        mock: false,
    };

    match spawn::spawn_claude_session(&config) {
        Ok(_pid) => {
            // Send the resume command
            if let Err(e) = spawn::send_keys_with_workaround(&tmux_session, &cmd) {
                warn!(error = %e, "failed to send resume command");
                let mut reg = registry.lock().await;
                let _ = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!("crash recovery failed: {e}")),
                );
                return;
            }

            // Update registry with new tmux session
            let mut reg = registry.lock().await;
            if let Some(entry) = reg.get_mut(run_id) {
                entry.tmux_session = Some(tmux_session.clone());
                entry.current_phase = Some("resuming".to_string());
            }

            info!(run_id = %run_id, tmux_session = %tmux_session, "crash recovery started");
        }
        Err(e) => {
            warn!(run_id = %run_id, error = %e, "failed to spawn recovery session");
            let mut reg = registry.lock().await;
            let _ = reg.update_status(
                run_id,
                RunStatus::Failed,
                None,
                Some(format!("crash recovery spawn failed: {e}")),
            );
        }
    }
}
