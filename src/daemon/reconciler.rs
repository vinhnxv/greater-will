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
use crate::monitor::loop_state::read_arc_loop_state;
use crate::session::spawn;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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
    /// Non-canonical ghost entries resolved by the session-name collision guard.
    pub collisions_resolved: u32,
}

impl std::fmt::Display for ReconciliationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconciled: {} scanned, {} recovered, {} adopted, {} cleaned, {} auto-resumed, {} respawned, {} failed, {} collisions resolved",
            self.sessions_scanned,
            self.recovered,
            self.adopted,
            self.cleaned_up,
            self.auto_resumed,
            self.respawned,
            self.marked_failed,
            self.collisions_resolved,
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

    // Step 0: Re-acquire repo locks for Running entries loaded from disk.
    //
    // After `load_from_disk`, the in-memory `repo_locks` HashMap is empty even
    // though the on-disk meta.json still records entries as Running. Without
    // this step, a subsequent `register_run()` for the same repo would succeed
    // — causing a double-run. We idempotently re-acquire via
    // `acquire_repo_lock_for_adopt`, which returns Ok if the lock is already
    // held by this process.
    //
    // NOTE: Queued entries are intentionally excluded. `enqueue_run` creates a
    // RunEntry with status=Queued purely for `gw ps` visibility WITHOUT
    // acquiring a repo lock (see registry.rs `enqueue_run`). If a pending
    // queue entry's queue.json record was lost (or dequeued) but its meta.json
    // remained at Queued, treating it as lock-holding would keep the repo
    // locked forever, preventing any new runs. Genuinely mid-init register_run
    // entries (rare crash-window case) are still handled later by
    // `find_stale_runs`, which will RESUME/RESPAWN/mark-Failed and release or
    // acquire the lock as appropriate.
    //
    // Gathered as (run_id, repo_dir) pairs first so we don't hold an immutable
    // borrow of `registry` across the `&mut self` acquire call.
    let running_repos: Vec<(String, std::path::PathBuf)> = registry
        .list_runs(false)
        .iter()
        .filter(|r| matches!(r.status, RunStatus::Running))
        .map(|r| (r.run_id.clone(), r.repo_dir.clone()))
        .collect();

    for (run_id, repo_dir) in &running_repos {
        match registry.acquire_repo_lock_for_adopt(repo_dir) {
            Ok(()) => {
                debug!(
                    run_id = %run_id,
                    repo = %repo_dir.display(),
                    "re-acquired repo lock on startup"
                );
            }
            Err(e) => {
                warn!(
                    run_id = %run_id,
                    repo = %repo_dir.display(),
                    error = %e,
                    "cannot re-acquire repo lock — another process may have it"
                );
            }
        }
    }

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

    // Step 4: Process stale registry entries (Running but no tmux).
    //
    // A7: Queued entries that are still in pending_queues are legitimate —
    // they never had a tmux session and should NOT be treated as stale.
    // Only Queued entries that are NOT in any pending queue are stale
    // (e.g., a crash between register and the first status update).
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

    // Step 4.5: Resolve session_name collisions.
    //
    // Invariant: at most one non-terminal entry may claim a given
    // `session_name` ("gw-{run_id}"). The old drain-to-running path
    // (pre-fix) created two entries sharing a session_name — one canonical
    // (Queued, id matches the session) and one non-canonical (Running,
    // fresh id but threaded-through session_name). That state survived
    // on disk across daemon restarts; this step cleans it up.
    report.collisions_resolved += resolve_session_name_collisions(registry);

    // Step 5: Validate pending queue entries (A7).
    //
    // Check that each queued run's plan_path and repo_dir still exist on disk.
    // If either was deleted while the daemon was down, remove the stale entry
    // from the queue and mark its RunEntry as Failed.
    let pending_snapshot: Vec<(String, std::path::PathBuf, std::path::PathBuf)> = registry
        .pending_entries()
        .iter()
        .map(|p| (p.run_id.clone(), p.plan_path.clone(), p.repo_dir.clone()))
        .collect();

    for (run_id, plan_path, repo_dir) in &pending_snapshot {
        let plan_ok = plan_path.exists();
        let repo_ok = repo_dir.exists();
        if !plan_ok || !repo_ok {
            warn!(
                run_id = %run_id,
                plan_exists = plan_ok,
                repo_exists = repo_ok,
                "stale pending queue entry — removing"
            );
            registry.remove_pending(run_id);
            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Failed,
                None,
                Some(format!(
                    "removed from queue by reconciler: plan_exists={plan_ok}, repo_exists={repo_ok}",
                )),
            ) {
                warn!(run_id = %run_id, error = %e, "failed to mark stale queue entry as failed");
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
        collisions_resolved = report.collisions_resolved,
        "reconciliation complete"
    );
    report
}

/// Find groups of non-terminal registry entries sharing the same
/// `session_name`. Returns a map from session_name → Vec<run_id> for every
/// colliding group (groups of size 1 are excluded).
///
/// Only considers `Running` and `Queued` entries — terminal states are
/// excluded because they no longer hold a tmux claim. The caller resolves
/// each group by preferring the canonical entry (run_id matches the
/// `gw-<id>` pattern of session_name).
fn find_session_name_collisions(
    registry: &RunRegistry,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for info in registry.list_runs(false) {
        if !matches!(info.status, RunStatus::Running | RunStatus::Queued) {
            continue;
        }
        groups
            .entry(info.session_name.clone())
            .or_default()
            .push(info.run_id.clone());
    }
    groups.retain(|_, ids| ids.len() > 1);
    groups
}

/// Resolve every session_name collision by flipping non-canonical entries
/// to Stopped. Returns the count of entries resolved (for reporting).
///
/// Resolution rule — the **canonical owner** of `gw-<id>` is the entry
/// whose `run_id == <id>`. Non-canonical entries that collide are marked
/// Stopped with an explicit reason — never Failed, because the tmux
/// session they share is still owned by the canonical entry and work is
/// not necessarily lost. If the canonical entry is absent from the group
/// (no run_id matches the session_name pattern), the collision is left
/// untouched and a warning is logged — we cannot safely pick a winner.
///
/// Extracted from `reconcile` so it can be unit-tested without the
/// surrounding steps (repo lock re-acquisition, tmux discovery, pending
/// queue validation) polluting the assertions.
fn resolve_session_name_collisions(registry: &mut RunRegistry) -> u32 {
    let collisions = find_session_name_collisions(registry);
    let mut resolved = 0u32;
    for (session_name, members) in collisions {
        let canonical_id = session_name.strip_prefix("gw-").map(str::to_string);
        let canonical_present = canonical_id
            .as_deref()
            .map(|id| members.iter().any(|m| m == id))
            .unwrap_or(false);

        if !canonical_present {
            warn!(
                session_name = %session_name,
                members = ?members,
                "session_name collision detected but no canonical owner — leaving untouched"
            );
            continue;
        }

        let canonical_id = canonical_id.expect("verified Some via canonical_present above");
        for run_id in &members {
            if *run_id == canonical_id {
                continue;
            }
            warn!(
                canonical = %canonical_id,
                ghost = %run_id,
                session_name = %session_name,
                "session_name collision: marking non-canonical entry Stopped"
            );
            let reason = format!(
                "session_name '{session_name}' collision with canonical owner '{canonical_id}' — resolved by reconciler"
            );
            crate::daemon::heartbeat::append_event(run_id, "collision_stopped", &reason);
            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Stopped,
                None,
                Some(reason),
            ) {
                warn!(run_id = %run_id, error = %e, "failed to mark collision loser");
            } else {
                resolved += 1;
                // Reap the non-canonical entry's dedicated tmux session if it has one
                // separate from the canonical's. Without this, Stopped entries leak
                // their `gw-<ghost_id>` tmux session forever (bug: observed 2026-04-14).
                if let Some(entry) = registry.get(run_id) {
                    if let Some(ghost_tmux) = entry
                        .tmux_session
                        .as_deref()
                        .filter(|t| *t != session_name.as_str())
                    {
                        if spawn::has_session(ghost_tmux) {
                            if let Err(e) = spawn::kill_session(ghost_tmux) {
                                warn!(run_id = %run_id, tmux = %ghost_tmux, error = %e, "failed to reap ghost tmux");
                            } else {
                                debug!(run_id = %run_id, tmux = %ghost_tmux, "reaped ghost tmux on collision");
                            }
                        }
                    }
                }
            }
        }
    }
    resolved
}

// ── Helper functions ────────────────────────────────────────────────

/// Compare two plan path references that may use different encodings.
/// Returns true if they resolve to the same plan file under `repo_dir`.
fn plans_match(a: &str, b: &str, repo_dir: &Path) -> bool {
    let norm = |s: &str| -> PathBuf {
        let p = Path::new(s);
        if p.is_absolute() {
            std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
        } else {
            std::fs::canonicalize(repo_dir.join(p))
                .unwrap_or_else(|_| repo_dir.join(p))
        }
    };
    let a_n = norm(a);
    let b_n = norm(b);
    if a_n == b_n {
        return true;
    }
    // Filename fallback: same basename is a weak match but better than
    // rejecting on trivial prefix drift (e.g., symlink farms).
    a_n.file_name() == b_n.file_name() && a_n.file_name().is_some()
}

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

    // Try to load from disk. Two failure modes are treated the same way —
    // `None` so the caller kills the tmux session:
    //
    //   1. meta.json does not exist at all → truly orphaned tmux session
    //      with no recoverable metadata.
    //
    //   2. meta.json exists but is not valid JSON → most likely a torn write
    //      from disk-full or a crash mid-write.
    //
    // FLAW-006: the previous implementation returned `Some(run_id)` on a
    // parse error on the theory that a live Claude session should be
    // preserved rather than killed. But the function also short-circuits
    // BEFORE `registry.adopt(entry)`, so no entry ever enters the registry
    // and no monitor is spawned. The caller treats `Some` as a successful
    // adoption (`report.adopted += 1`) and moves on, leaving the tmux
    // session running with no outcome tracking — it runs to completion and
    // its result is silently lost. Returning `None` is strictly better:
    // the caller kills the session, yielding a clean observable failure
    // instead of a zombie running in the background.
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
                "meta.json corrupt — declining adoption, caller will kill tmux session"
            );
            return None;
        }
    };

    // Plan-match guard: verify the live tmux session is running the plan
    // we're about to adopt it as. Skip if env var override is set.
    if std::env::var("GW_DAEMON_SKIP_PLAN_MATCH").ok().as_deref() != Some("1") {
        let loop_state_read = read_arc_loop_state(&entry.repo_dir);
        if let Some(state) = loop_state_read.active() {
            let live_plan = &state.plan_file;
            let recorded_plan = entry.plan_path.to_string_lossy();
            if !plans_match(live_plan, &recorded_plan, &entry.repo_dir) {
                warn!(
                    run_id = %run_id,
                    recorded = %recorded_plan,
                    live = %live_plan,
                    "refusing to adopt orphan — plan mismatch between meta.json and live arc loop state"
                );
                crate::daemon::heartbeat::append_event(
                    run_id,
                    "adopt_refused_plan_mismatch",
                    &format!("recorded={}, live={}", recorded_plan, live_plan),
                );
                return Some(run_id.to_string());
            }
        }
        // No loop state file → proceed with adoption (pre-init window)
    }

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
            // A7: Skip Queued entries that are legitimately waiting in
            // pending_queues — they never had a tmux session and are not stale.
            if r.status == RunStatus::Queued && registry.is_in_pending_queue(&r.run_id) {
                return false;
            }
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
            collisions_resolved: 0,
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

    // Collision detection / resolution tests. These exercise the
    // find_session_name_collisions helper and the Step 4.5 loop against
    // an in-memory RunRegistry directly, so they don't require a tmux
    // server or real filesystem state beyond a temp GW_HOME.
    use crate::daemon::registry::{PendingRun, RunRegistry};
    use chrono::Utc;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Uses the shared `daemon::state::gw_home_test_mutex` so reconciler
    /// tests serialize against registry/server tests that also mutate
    /// `GW_HOME`. Per-module mutexes would only serialize within one
    /// module, leaving cross-module races.
    fn with_temp_gw_home(f: impl FnOnce(&TempDir)) {
        let _guard = crate::daemon::state::gw_home_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        crate::daemon::state::ensure_gw_home().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&tmp)));
        unsafe { std::env::remove_var("GW_HOME") };
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn find_session_name_collisions_empty_when_no_duplicates() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let _id = reg
                .register_run(
                    PathBuf::from("plans/unique.md"),
                    PathBuf::from("/tmp/coll-unique"),
                    None,
                    None,
                )
                .unwrap();
            let groups = find_session_name_collisions(&reg);
            assert!(groups.is_empty());
        });
    }

    #[test]
    fn find_session_name_collisions_detects_duplicate() {
        // Reproduces the old-bug state: an enqueued ghost entry sharing
        // session_name with a freshly-spawned one. Both must be detected.
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            // Canonical: run_id matches session_name.
            let canonical = PendingRun {
                run_id: "ghost01".into(),
                plan_path: PathBuf::from("/tmp/plan.md"),
                repo_dir: PathBuf::from("/tmp/coll-dup"),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            reg.enqueue_run(canonical).unwrap();
            // Non-canonical: fresh run_id but same session_name "gw-ghost01".
            let id2 = reg
                .register_run(
                    PathBuf::from("/tmp/plan.md"),
                    PathBuf::from("/tmp/coll-dup-2"),
                    Some("gw-ghost01".into()),
                    None,
                )
                .unwrap();

            let groups = find_session_name_collisions(&reg);
            let members = groups
                .get("gw-ghost01")
                .expect("colliding group must be surfaced");
            assert_eq!(members.len(), 2);
            assert!(members.contains(&"ghost01".to_string()));
            assert!(members.contains(&id2));
        });
    }

    #[test]
    fn resolve_collision_flips_non_canonical_to_stopped() {
        // Reproduces the user-reported `gw ps` duplicate: a canonical
        // Queued entry (run_id matches gw-{id}) and a non-canonical
        // Running entry sharing the same session_name. After resolution,
        // the non-canonical entry must be Stopped; the canonical must
        // survive at its pre-resolution status.
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let pending = PendingRun {
                run_id: "ghost02".into(),
                plan_path: PathBuf::from("/tmp/plan.md"),
                repo_dir: PathBuf::from("/tmp/coll-res-a"),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            reg.enqueue_run(pending).unwrap();

            let non_canonical = reg
                .register_run(
                    PathBuf::from("/tmp/plan.md"),
                    PathBuf::from("/tmp/coll-res-b"),
                    Some("gw-ghost02".into()),
                    None,
                )
                .unwrap();
            reg.update_status(&non_canonical, RunStatus::Running, None, None)
                .unwrap();

            let resolved = resolve_session_name_collisions(&mut reg);
            assert_eq!(resolved, 1);

            let canonical = reg.get("ghost02").expect("canonical entry must survive");
            assert_eq!(canonical.status, RunStatus::Queued);

            let loser = reg.get(&non_canonical).expect("loser still in registry");
            assert_eq!(loser.status, RunStatus::Stopped);
            assert!(
                loser
                    .error_message
                    .as_deref()
                    .unwrap_or("")
                    .contains("collision"),
                "error_message must explain why it was stopped"
            );
        });
    }

    #[test]
    fn resolve_collision_skips_when_no_canonical_owner() {
        // Guard: if neither entry's run_id matches the session_name
        // (e.g., "gw-no-canonical" with run_ids "xyz" and "pqr"), the
        // reconciler must NOT pick a winner arbitrarily. Both must
        // survive untouched.
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let a = reg
                .register_run(
                    PathBuf::from("/tmp/plan.md"),
                    PathBuf::from("/tmp/no-canon-a"),
                    Some("gw-no-canonical".into()),
                    None,
                )
                .unwrap();
            let b = reg
                .register_run(
                    PathBuf::from("/tmp/plan.md"),
                    PathBuf::from("/tmp/no-canon-b"),
                    Some("gw-no-canonical".into()),
                    None,
                )
                .unwrap();

            let resolved = resolve_session_name_collisions(&mut reg);
            assert_eq!(resolved, 0);
            assert_eq!(reg.get(&a).unwrap().status, RunStatus::Queued);
            assert_eq!(reg.get(&b).unwrap().status, RunStatus::Queued);
        });
    }

    #[test]
    fn resolve_collision_ignores_terminal_states() {
        // A collision where one entry is already Succeeded/Failed/Stopped
        // is not a real collision — the terminal entry no longer claims
        // a tmux session. Must not be touched or counted.
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let pending = PendingRun {
                run_id: "ghost03".into(),
                plan_path: PathBuf::from("/tmp/plan.md"),
                repo_dir: PathBuf::from("/tmp/coll-term-a"),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            reg.enqueue_run(pending).unwrap();
            let terminal = reg
                .register_run(
                    PathBuf::from("/tmp/plan.md"),
                    PathBuf::from("/tmp/coll-term-b"),
                    Some("gw-ghost03".into()),
                    None,
                )
                .unwrap();
            reg.update_status(&terminal, RunStatus::Succeeded, None, None)
                .unwrap();

            let resolved = resolve_session_name_collisions(&mut reg);
            assert_eq!(
                resolved, 0,
                "a terminal entry does not count as an active collision"
            );
            assert_eq!(reg.get("ghost03").unwrap().status, RunStatus::Queued);
            assert_eq!(reg.get(&terminal).unwrap().status, RunStatus::Succeeded);
        });
    }
}
