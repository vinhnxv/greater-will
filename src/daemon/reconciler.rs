//! Startup reconciler for daemon state vs tmux reality.
//!
//! When the daemon starts (or restarts), the in-memory registry may be
//! stale relative to what's actually running in tmux. The reconciler
//! scans tmux sessions matching `gw-*` and cross-references with
//! `~/.gw/runs/*/meta.json` to bring everything into a consistent state.

use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::RunRegistry;
use crate::session::spawn;
use std::collections::HashSet;
use std::process::Command;
use tracing::{debug, info, warn};

/// Report of reconciliation actions taken.
#[derive(Debug, Default)]
pub struct ReconciliationReport {
    /// Runs re-registered (tmux alive + Running state).
    pub recovered: u32,
    /// Sessions killed (tmux alive + terminal state or no state).
    pub cleaned_up: u32,
    /// Runs marked for auto-resume (no tmux + Running + checkpoint exists).
    pub auto_resumed: u32,
    /// Runs marked as Failed (no tmux + Running + no checkpoint).
    pub marked_failed: u32,
    /// Total tmux sessions scanned.
    pub sessions_scanned: u32,
}

impl std::fmt::Display for ReconciliationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconciled: {} scanned, {} recovered, {} cleaned, {} auto-resumed, {} failed",
            self.sessions_scanned,
            self.recovered,
            self.cleaned_up,
            self.auto_resumed,
            self.marked_failed,
        )
    }
}

/// Reconcile daemon registry state against actual tmux sessions.
///
/// This should be called once at daemon startup after loading the registry
/// from disk. It handles four scenarios:
///
/// | tmux alive | registry state      | action                        |
/// |------------|---------------------|-------------------------------|
/// | yes        | Running             | RECOVER: re-register          |
/// | yes        | Completed/Failed    | CLEANUP: kill tmux session    |
/// | yes        | (none)              | ORPHAN: kill tmux session     |
/// | no         | Running             | STALE: check checkpoint       |
pub fn reconcile(registry: &mut RunRegistry) -> ReconciliationReport {
    let mut report = ReconciliationReport::default();

    // Step 1: Discover all gw-* tmux sessions
    let live_sessions = list_gw_tmux_sessions();
    report.sessions_scanned = live_sessions.len() as u32;

    info!(
        count = live_sessions.len(),
        "found gw-* tmux sessions for reconciliation"
    );

    // Step 2: Process live tmux sessions
    for tmux_name in &live_sessions {
        // Find matching registry entry by tmux_session field
        let matching_run = find_run_by_tmux_session(registry, tmux_name);

        match matching_run {
            Some((run_id, status)) => {
                match status {
                    RunStatus::Running | RunStatus::Queued => {
                        // RECOVER: tmux alive + Running → re-register (already tracked)
                        info!(
                            run_id = %run_id,
                            tmux = %tmux_name,
                            "RECOVER: tmux alive with Running state — keeping"
                        );
                        report.recovered += 1;
                    }
                    RunStatus::Succeeded | RunStatus::Failed | RunStatus::Stopped => {
                        // CLEANUP: tmux alive + terminal state → kill session
                        info!(
                            run_id = %run_id,
                            tmux = %tmux_name,
                            status = ?status,
                            "CLEANUP: killing tmux session for completed run"
                        );
                        kill_tmux_session(tmux_name);
                        report.cleaned_up += 1;
                    }
                }
            }
            None => {
                // ORPHAN: tmux alive but no registry entry → kill
                warn!(
                    tmux = %tmux_name,
                    "ORPHAN: killing tmux session with no registry entry"
                );
                kill_tmux_session(tmux_name);
                report.cleaned_up += 1;
            }
        }
    }

    // Step 4: Process stale registry entries (Running but no tmux)
    let live_set: HashSet<&str> = live_sessions.iter().map(|s| s.as_str()).collect();
    let stale_runs = find_stale_runs(registry, &live_set);

    for (run_id, repo_dir, tmux_session) in &stale_runs {
        let has_checkpoint = check_checkpoint(repo_dir);

        if has_checkpoint {
            // STALE with checkpoint → mark for auto-resume.
            //
            // Convention: set `tmux_session` to the canonical `gw-{run_id}` name as a
            // synthetic marker. That session does not exist (we already know tmux is
            // dead for this run), so on the next heartbeat tick the monitor will
            // invoke `handle_dead_session`, see the checkpoint, and call
            // `spawn_recovery`. Leaving `tmux_session = None` would cause the
            // heartbeat to skip the entry entirely, leaving the run permanently stuck.
            info!(
                run_id = %run_id,
                "STALE: tmux dead but checkpoint exists — marking for auto-resume"
            );
            if let Some(entry) = registry.get_mut(run_id) {
                entry.tmux_session = Some(format!("gw-{run_id}"));
            }
            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Running,
                Some("pending-recovery".to_string()),
                None,
            ) {
                warn!(run_id = %run_id, error = %e, "failed to update stale run");
            }
            report.auto_resumed += 1;
        } else {
            // STALE without checkpoint → mark Failed
            warn!(
                run_id = %run_id,
                tmux = ?tmux_session,
                "STALE: tmux dead and no checkpoint — marking failed"
            );
            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Failed,
                None,
                Some("tmux session lost with no checkpoint (reconciler)".to_string()),
            ) {
                warn!(run_id = %run_id, error = %e, "failed to mark stale run as failed");
            }
            report.marked_failed += 1;
        }
    }

    info!(
        sessions_scanned = report.sessions_scanned,
        recovered = report.recovered,
        cleaned = report.cleaned_up,
        auto_resumed = report.auto_resumed,
        marked_failed = report.marked_failed,
        "reconciliation complete"
    );
    report
}

// ── Helper functions ────────────────────────────────────────────────

/// List all tmux sessions matching the `gw-*` prefix.
fn list_gw_tmux_sessions() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|name| name.starts_with("gw-"))
                .map(|s| s.to_string())
                .collect()
        }
        Ok(_) => {
            debug!("tmux list-sessions returned non-zero (no server running?)");
            Vec::new()
        }
        Err(e) => {
            debug!(error = %e, "failed to list tmux sessions");
            Vec::new()
        }
    }
}

/// Find a run entry by its tmux_session field.
///
/// Returns (run_id, status) if found.
fn find_run_by_tmux_session(
    registry: &RunRegistry,
    tmux_name: &str,
) -> Option<(String, RunStatus)> {
    // Check all runs (including finished) for tmux session match
    for info in registry.list_runs(true) {
        // The tmux session is stored in the RunEntry, not RunInfo.
        // Try matching by session_name or by the gw-{run_id} pattern.
        if info.session_name == tmux_name {
            return Some((info.run_id.clone(), info.status));
        }
        // Also check gw-{run_id} pattern
        if tmux_name == format!("gw-{}", info.run_id) {
            return Some((info.run_id.clone(), info.status));
        }
    }
    None
}

/// Find Running entries whose tmux session is not in the live set.
fn find_stale_runs(
    registry: &RunRegistry,
    live_sessions: &HashSet<&str>,
) -> Vec<(String, std::path::PathBuf, Option<String>)> {
    registry
        .list_runs(false)
        .iter()
        .filter(|r| r.status == RunStatus::Running)
        .filter(|r| {
            // Check if tmux session is alive
            let tmux_name = format!("gw-{}", r.run_id);
            !live_sessions.contains(tmux_name.as_str())
                && !live_sessions.contains(r.session_name.as_str())
        })
        .map(|r| {
            (
                r.run_id.clone(),
                r.repo_dir.clone(),
                Some(format!("gw-{}", r.run_id)),
            )
        })
        .collect()
}

/// Check if a checkpoint exists for a repo.
///
/// Returns `true` if any `.rune/arc/*/checkpoint.json` file exists under
/// `repo_dir`. Shared with the heartbeat monitor to avoid duplication.
pub(crate) fn check_checkpoint(repo_dir: &std::path::Path) -> bool {
    let rune_dir = repo_dir.join(".rune").join("arc");
    if !rune_dir.is_dir() {
        return false;
    }
    std::fs::read_dir(&rune_dir)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().join("checkpoint.json").exists())
        })
        .unwrap_or(false)
}

/// Kill a tmux session by name, logging any errors.
fn kill_tmux_session(name: &str) {
    if let Err(e) = spawn::kill_session(name) {
        warn!(session = %name, error = %e, "failed to kill tmux session during reconciliation");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_report_display() {
        let report = ReconciliationReport {
            recovered: 2,
            cleaned_up: 1,
            auto_resumed: 1,
            marked_failed: 0,
            sessions_scanned: 5,
        };
        let s = format!("{report}");
        assert!(s.contains("5 scanned"));
        assert!(s.contains("2 recovered"));
        assert!(s.contains("1 cleaned"));
    }

    #[test]
    fn list_gw_sessions_handles_no_tmux_server() {
        // When no tmux server is running, should return empty vec
        let sessions = list_gw_tmux_sessions();
        // May or may not be empty depending on test environment,
        // but should not panic
        assert!(sessions.iter().all(|s| s.starts_with("gw-")));
    }
}
