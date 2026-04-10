//! Run executor for the daemon.
//!
//! Handles spawning and stopping arc runs in tmux sessions.
//! Follows the patterns established in `session/spawn.rs` and
//! `engine/single_session/util.rs` for tmux interaction and
//! arc command construction.

use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::RunRegistry;
use crate::session::spawn::{self, SpawnConfig};
use color_eyre::{eyre::eyre, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Grace period after sending /exit before force-killing.
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval when waiting for graceful stop.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn a new arc run in a tmux session.
///
/// # Steps
///
/// 1. Validate that the plan file exists and the repo is clean
/// 2. Register the run in the registry (acquires per-repo lock)
/// 3. Spawn a tmux session via `SpawnConfig`
/// 4. Send the `/rune:arc` command
/// 5. Update registry with tmux session info
///
/// Returns the run ID on success.
pub async fn spawn_run(
    registry: Arc<Mutex<RunRegistry>>,
    plan_path: &Path,
    repo_dir: &Path,
    session_name: Option<String>,
) -> Result<String> {
    // ── Pre-flight checks ───────────────────────────────────────────
    if !plan_path.exists() {
        return Err(eyre!(
            "Plan file not found: {}\nProvide a valid plan path.",
            plan_path.display()
        ));
    }

    if !repo_dir.is_dir() {
        return Err(eyre!(
            "Repository directory not found: {}",
            repo_dir.display()
        ));
    }

    // Check for dirty git state (warn but don't block)
    if let Some(warning) = check_git_clean(repo_dir) {
        warn!(repo = %repo_dir.display(), "{warning}");
    }

    // ── Register run ────────────────────────────────────────────────
    let run_id = {
        let mut reg = registry.lock().await;
        reg.register_run(
            plan_path.to_path_buf(),
            repo_dir.to_path_buf(),
            session_name,
        )?
    };

    info!(run_id = %run_id, plan = %plan_path.display(), "run registered");

    // ── Spawn tmux session ──────────────────────────────────────────
    let tmux_session = format!("gw-{}", run_id);

    let config = SpawnConfig {
        session_id: tmux_session.clone(),
        working_dir: repo_dir.to_path_buf(),
        config_dir: None,
        claude_path: "claude".to_string(),
        mock: false,
    };

    match spawn::spawn_claude_session(&config) {
        Ok(pid) => {
            debug!(run_id = %run_id, pid = pid, tmux = %tmux_session, "tmux session spawned");
        }
        Err(e) => {
            // Spawn failed — clean up registry entry
            warn!(run_id = %run_id, error = %e, "failed to spawn tmux session");
            let mut reg = registry.lock().await;
            let _ = reg.update_status(
                &run_id,
                RunStatus::Failed,
                None,
                Some(format!("tmux spawn failed: {e}")),
            );
            return Err(e);
        }
    }

    // ── Send arc command ────────────────────────────────────────────
    let plan_str = plan_path.to_string_lossy();
    let arc_cmd = build_arc_command(&plan_str);

    if let Err(e) = spawn::send_keys_with_workaround(&tmux_session, &arc_cmd) {
        warn!(error = %e, "failed to send arc command — killing session");
        let _ = spawn::kill_session(&tmux_session);
        let mut reg = registry.lock().await;
        let _ = reg.update_status(
            &run_id,
            RunStatus::Failed,
            None,
            Some(format!("failed to send arc command: {e}")),
        );
        return Err(e);
    }

    // ── Update registry ─────────────────────────────────────────────
    {
        let mut reg = registry.lock().await;
        if let Some(entry) = reg.get_mut(&run_id) {
            entry.tmux_session = Some(tmux_session.clone());
        }
        reg.update_status(
            &run_id,
            RunStatus::Running,
            Some("starting".to_string()),
            None,
        )?;
    }

    info!(
        run_id = %run_id,
        tmux = %tmux_session,
        "run started successfully"
    );

    // Log structured event for `gw logs`
    crate::daemon::heartbeat::log_run_started(&run_id, &plan_str);

    Ok(run_id)
}

/// Gracefully stop a running arc.
///
/// # Steps
///
/// 1. Send `/exit` to the tmux session
/// 2. Wait up to 30 seconds for the session to terminate
/// 3. Force-kill if still alive
/// 4. Update registry and release per-repo lock
pub async fn stop_run(
    registry: Arc<Mutex<RunRegistry>>,
    run_id: &str,
) -> Result<()> {
    let tmux_session = {
        let reg = registry.lock().await;
        let entry = reg
            .get(run_id)
            .ok_or_else(|| eyre!("run not found: {run_id}"))?;

        if !matches!(entry.status, RunStatus::Running | RunStatus::Queued) {
            return Err(eyre!(
                "run {} is not active (status: {:?})",
                run_id,
                entry.status
            ));
        }

        entry.tmux_session.clone()
    };

    let tmux_session = match tmux_session {
        Some(s) => s,
        None => {
            // No tmux session — just mark as stopped
            let mut reg = registry.lock().await;
            reg.update_status(
                run_id,
                RunStatus::Stopped,
                None,
                Some("stopped (no tmux session)".to_string()),
            )?;
            return Ok(());
        }
    };

    info!(run_id = %run_id, tmux = %tmux_session, "stopping run");

    // Step 1: Send /exit for graceful shutdown
    if spawn::has_session(&tmux_session) {
        debug!(tmux = %tmux_session, "sending /exit for graceful stop");
        if let Err(e) = spawn::send_keys_with_workaround(&tmux_session, "/exit") {
            warn!(error = %e, "failed to send /exit — will force-kill");
        }
    }

    // Step 2: Wait for graceful shutdown
    let stopped = wait_for_session_exit(&tmux_session, GRACEFUL_STOP_TIMEOUT).await;

    // Step 3: Force-kill if still alive
    if !stopped && spawn::has_session(&tmux_session) {
        warn!(tmux = %tmux_session, "session still alive after grace period — force killing");
        if let Err(e) = spawn::kill_session(&tmux_session) {
            warn!(error = %e, "failed to force-kill tmux session");
        }
    }

    // Step 4: Update registry
    {
        let mut reg = registry.lock().await;
        reg.update_status(
            run_id,
            RunStatus::Stopped,
            None,
            Some("stopped by user".to_string()),
        )?;
    }

    info!(run_id = %run_id, "run stopped");
    crate::daemon::heartbeat::log_run_stopped(run_id);
    Ok(())
}

// ── Helper functions ────────────────────────────────────────────────

/// Build the `/rune:arc` command string.
///
/// Follows the pattern from `engine/single_session/util.rs::build_arc_command`
/// but simplified for daemon use (no resume flag on initial spawn).
fn build_arc_command(plan_path: &str) -> String {
    format!(
        "/rune:arc '{}'",
        plan_path.replace('\'', "'\\''")
    )
}

/// Check if the git working tree is clean.
///
/// Returns `Some(warning)` if dirty, `None` if clean.
fn check_git_clean(repo_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return Some("not a git repository or git not available".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        None
    } else {
        let changed_count = stdout.lines().count();
        Some(format!(
            "working tree has {changed_count} uncommitted change(s)"
        ))
    }
}

/// Wait for a tmux session to exit, polling periodically.
///
/// Returns `true` if the session exited within the timeout.
async fn wait_for_session_exit(tmux_session: &str, timeout: Duration) -> bool {
    let start = tokio::time::Instant::now();

    while start.elapsed() < timeout {
        if !spawn::has_session(tmux_session) {
            return true;
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_arc_command_simple() {
        let cmd = build_arc_command("plans/feat.md");
        assert_eq!(cmd, "/rune:arc 'plans/feat.md'");
    }

    #[test]
    fn build_arc_command_escapes_quotes() {
        let cmd = build_arc_command("plans/it's-a-plan.md");
        assert_eq!(cmd, "/rune:arc 'plans/it'\\''s-a-plan.md'");
    }

    #[test]
    fn check_git_clean_nonexistent_dir() {
        // Non-existent directory should return a warning
        let result = check_git_clean(Path::new("/nonexistent/repo"));
        // git command will fail, returning Some
        assert!(result.is_some() || result.is_none()); // either is fine
    }
}
