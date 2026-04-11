//! Startup reconciler for daemon state vs tmux reality.
//!
//! When the daemon starts (or restarts), the in-memory registry may be
//! stale relative to what's actually running in tmux. The reconciler
//! scans tmux sessions matching `gw-*` and cross-references with
//! `~/.gw/runs/*/meta.json` to bring everything into a consistent state.

use crate::daemon::drain;
use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::{RunEntry, RunRegistry};
use crate::daemon::state::gw_home;
use crate::session::spawn;
use std::collections::HashSet;
use std::process::Command;
use tracing::{debug, info, warn};

/// Report of reconciliation actions taken.
#[derive(Debug, Default)]
pub struct ReconciliationReport {
    /// Runs re-registered (tmux alive + Running state in registry).
    pub recovered: u32,
    /// Orphaned sessions adopted (tmux alive + no registry but disk meta exists).
    pub adopted: u32,
    /// Sessions killed (tmux alive + terminal state or truly unrecoverable orphan).
    pub cleaned_up: u32,
    /// Runs marked for auto-resume (no tmux + Running + checkpoint exists).
    pub auto_resumed: u32,
    /// Runs marked as Failed (no tmux + Running + no checkpoint + not restartable).
    pub marked_failed: u32,
    /// Runs scheduled for fresh re-spawn (no tmux + no checkpoint + restartable).
    pub respawned: u32,
    /// Total tmux sessions scanned.
    pub sessions_scanned: u32,
}

impl std::fmt::Display for ReconciliationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconciled: {} scanned, {} recovered, {} adopted, {} cleaned, {} auto-resumed, {} respawned, {} failed",
            self.sessions_scanned,
            self.recovered,
            self.adopted,
            self.cleaned_up,
            self.auto_resumed,
            self.respawned,
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
/// | yes        | (none)              | ADOPT: re-register from disk  |
/// | yes        | (none, no disk)     | ORPHAN: kill tmux session     |
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
                // tmux alive but no registry entry — try to adopt before killing.
                //
                // Extract run_id from session name (gw-{run_id}), check if
                // meta.json exists on disk. If so, re-load the entry into
                // the registry and continue monitoring. This preserves the
                // running Claude session instead of wastefully killing it.
                if let Some(adopted_id) = try_adopt_orphan(registry, tmux_name) {
                    info!(
                        run_id = %adopted_id,
                        tmux = %tmux_name,
                        "ADOPT: re-registered orphaned session into registry"
                    );
                    report.adopted += 1;
                } else {
                    // Truly orphaned: no meta.json on disk either. Kill it.
                    warn!(
                        tmux = %tmux_name,
                        "ORPHAN: no recoverable meta on disk — killing tmux session"
                    );
                    kill_tmux_session(tmux_name);
                    report.cleaned_up += 1;
                }
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
            // STALE without checkpoint — check if we can re-spawn
            let is_restartable = registry
                .get(run_id)
                .map(|e| e.restartable)
                .unwrap_or(false);
            let has_snapshot = drain::read_snapshot(run_id).is_some();

            if is_restartable {
                // RESPAWN: Mark for fresh re-spawn via heartbeat.
                //
                // Set tmux_session to the canonical name so the heartbeat
                // detects it as "dead" and triggers spawn_recovery. The
                // recovery path will start a fresh arc run (not --resume)
                // since no checkpoint exists.
                info!(
                    run_id = %run_id,
                    has_snapshot = has_snapshot,
                    "STALE: tmux dead, no checkpoint, but restartable — scheduling re-spawn"
                );
                if let Some(entry) = registry.get_mut(run_id) {
                    entry.tmux_session = Some(format!("gw-{run_id}"));
                    // Reset crash counter for a clean re-spawn
                    entry.crash_restarts = 0;
                }
                if let Err(e) = registry.update_status(
                    run_id,
                    RunStatus::Running,
                    Some("pending-respawn".to_string()),
                    None,
                ) {
                    warn!(run_id = %run_id, error = %e, "failed to update for re-spawn");
                }
                // Don't clear snapshot yet — it will be cleared by spawn_recovery
                // after the session is actually recovered. If the daemon crashes
                // before recovery completes, the snapshot is still available.
                // drain::clear_snapshot(run_id); // deferred to spawn_recovery
                report.respawned += 1;
            } else {
                // Not restartable → mark Failed
                warn!(
                    run_id = %run_id,
                    tmux = ?tmux_session,
                    "STALE: tmux dead, no checkpoint, not restartable — marking failed"
                );
                let reason = format!(
                    "tmux session '{}' lost with no checkpoint — marked failed by reconciler on daemon startup",
                    tmux_session.as_deref().unwrap_or("unknown"),
                );
                crate::daemon::heartbeat::append_event(run_id, "session_died", &reason);
                if let Err(e) = registry.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(reason),
                ) {
                    warn!(run_id = %run_id, error = %e, "failed to mark stale run as failed");
                }
                report.marked_failed += 1;
            }
        }
    }

    info!(
        sessions_scanned = report.sessions_scanned,
        recovered = report.recovered,
        adopted = report.adopted,
        cleaned = report.cleaned_up,
        auto_resumed = report.auto_resumed,
        respawned = report.respawned,
        marked_failed = report.marked_failed,
        "reconciliation complete"
    );
    report
}

// ── Helper functions ────────────────────────────────────────────────

/// Try to adopt an orphaned tmux session by loading its meta.json from disk.
///
/// Extracts the run_id from the tmux session name (e.g., `gw-abc12345` → `abc12345`),
/// reads `~/.gw/runs/{run_id}/meta.json`, and re-inserts the entry into the
/// in-memory registry. Returns `Some(run_id)` on success.
///
/// This preserves the running Claude session instead of killing it — the daemon
/// simply re-attaches as a monitor.
fn try_adopt_orphan(registry: &mut RunRegistry, tmux_name: &str) -> Option<String> {
    // Extract run_id from "gw-{run_id}"
    let run_id = tmux_name.strip_prefix("gw-")?;

    // Already in registry (shouldn't happen, but guard)
    if registry.get(run_id).is_some() {
        return Some(run_id.to_string());
    }

    // Try to load from disk. Two failure modes are distinguished:
    //
    //   1. meta.json does not exist at all → truly orphaned tmux session
    //      with no recoverable metadata. Return None so the caller kills it.
    //
    //   2. meta.json exists but is not valid JSON → most likely a torn write
    //      from disk-full or a crash mid-write. A running Claude session is
    //      almost certainly still alive in the tmux session — killing it to
    //      "clean up" would destroy live work. Return Some(run_id) so the
    //      caller preserves the session (untracked, but alive). A future
    //      reconcile pass can re-try adoption if meta.json is repaired.
    let meta_path = gw_home().join("runs").join(run_id).join("meta.json");
    let content = match std::fs::read_to_string(&meta_path) {
        Ok(c) => c,
        Err(e) => {
            debug!(
                run_id = %run_id,
                error = %e,
                "meta.json not readable — treating tmux session as truly orphaned"
            );
            return None;
        }
    };
    let mut entry: RunEntry = match serde_json::from_str(&content) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                run_id = %run_id,
                error = %e,
                "meta.json corrupt — preserving live tmux session, skipping adopt"
            );
            return Some(run_id.to_string());
        }
    };

    // Update the entry to reflect reality: tmux is alive, mark as Running
    entry.tmux_session = Some(tmux_name.to_string());
    entry.status = RunStatus::Running;
    entry.current_phase = Some("adopted".to_string());
    entry.error_message = None;
    entry.finished_at = None;

    // Acquire repo lock before adopting to prevent concurrent runs on same repo
    let repo_dir = entry.repo_dir.clone();
    if let Err(e) = registry.acquire_repo_lock_for_adopt(&repo_dir) {
        warn!(
            run_id = %run_id,
            error = %e,
            "cannot adopt orphan — repo already locked by another run"
        );
        return None;
    }

    // Re-insert into registry (persists to disk)
    registry.adopt(entry);

    Some(run_id.to_string())
}

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

/// Find Running/Queued entries whose tmux session is not in the live set.
///
/// Queued entries are included because a crash between `register_run()`
/// (which writes status=Queued) and the first `update_status(Running)` would
/// otherwise leave the entry invisible to this scan — the run stays Queued
/// with no tmux session forever. Queued entries fall through to the same
/// checkpoint / restartable / fail branches as Running entries in the caller,
/// so no new code path is required here.
fn find_stale_runs(
    registry: &RunRegistry,
    live_sessions: &HashSet<&str>,
) -> Vec<(String, std::path::PathBuf, Option<String>)> {
    registry
        .list_runs(false)
        .iter()
        .filter(|r| matches!(r.status, RunStatus::Running | RunStatus::Queued))
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
                .any(|e| {
                    let cp = e.path().join("checkpoint.json");
                    if !cp.exists() {
                        return false;
                    }
                    // Validate that the checkpoint is actually valid JSON,
                    // not a zero-byte or corrupt file from a partial write
                    match std::fs::read_to_string(&cp) {
                        Ok(content) => {
                            if content.trim().is_empty() {
                                warn!(path = ?cp, "checkpoint.json is empty — ignoring");
                                return false;
                            }
                            match serde_json::from_str::<serde_json::Value>(&content) {
                                Ok(_) => true,
                                Err(e) => {
                                    warn!(path = ?cp, error = %e, "checkpoint.json is invalid JSON — ignoring");
                                    false
                                }
                            }
                        }
                        Err(e) => {
                            warn!(path = ?cp, error = %e, "failed to read checkpoint.json");
                            false
                        }
                    }
                })
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
            adopted: 0,
            cleaned_up: 1,
            auto_resumed: 1,
            marked_failed: 0,
            respawned: 0,
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
