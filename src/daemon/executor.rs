//! Run executor for the daemon.
//!
//! Handles spawning and stopping arc runs in tmux sessions.
//! Follows the patterns established in `session/spawn.rs` and
//! `engine/single_session/util.rs` for tmux interaction and
//! arc command construction.

use crate::daemon::heartbeat::MonitorHandle;
use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::RunRegistry;
use crate::session::spawn::{self, SpawnConfig};
use color_eyre::{eyre::eyre, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// ── Drain task tracker (BACK-001) ────────────────────────────────────
//
// All fire-and-forget drain spawns are registered here so the shutdown
// path can await them before flushing queue state.
//
// Design: `Vec<JoinHandle<()>>` under a Mutex rather than `JoinSet<()>`.
// This lets the shutdown path drain the Vec under a brief lock, then await
// each handle OUTSIDE the lock.  If we used `JoinSet` and held the Mutex
// during `join_next`, a completing task that tries to add a retry would
// deadlock trying to re-acquire the same Mutex.
static DRAIN_HANDLES: OnceLock<Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>> = OnceLock::new();

/// Initialize the process-wide drain handle registry and return a clone of the Arc.
///
/// Must be called once from `start_daemon` before the heartbeat monitor or
/// server start — both paths may trigger `drain_if_available`.  Subsequent
/// calls return a clone of the already-initialized Arc (idempotent).
pub(crate) fn init_drain_join_set() -> Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> {
    Arc::clone(DRAIN_HANDLES.get_or_init(|| Arc::new(Mutex::new(Vec::new()))))
}

/// Grace period after sending /exit before force-killing.
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval when waiting for graceful stop.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Wait time for Claude Code TUI to initialize before dispatching `/rune:arc`.
///
/// Matches the foreground behavior at `src/engine/single_session/monitor.rs:79-81`.
/// If `/rune:arc` dispatches before stdin is hooked, the command is silently dropped
/// and the session stalls in `starting` state until bootstrap timeout.
pub(crate) const SPAWN_INIT_WAIT_SECS: u64 = 12;

/// Spawn a new arc run in a tmux session.
///
/// # Steps
///
/// 1. Validate that the plan file exists and the repo is clean
/// 2. Register the run in the registry (acquires per-repo lock)
/// 3. Pre-flight: `cleanup::pre_phase_cleanup` + `elden::clear_signals_from`
///    (parity with foreground `monitor::run_session_attempt` — plan GAP A3)
/// 4. Spawn a tmux session via `SpawnConfig`
/// 5. Write `session_owner.json` for adopt-orphaned-session recovery
/// 6. Populate `last_recovery_at` so first-attempt uptime is measured from
///    this spawn, not from daemon boot (plan GAP A5)
/// 7. Send the `/rune:arc` command (persists crash dump on send failure)
/// 8. Update registry with tmux session info
///
/// Returns the run ID on success.
///
/// # Spawn-init wait (C-5)
///
/// The foreground path sleeps 12 s after spawn to let Claude Code finish
/// initializing before dispatching the command (`monitor.rs:79-81`).
/// The daemon now matches this behavior via [`SPAWN_INIT_WAIT_SECS`]
/// using `tokio::time::sleep` so the reactor remains free for heartbeat
/// and IPC progress during the wait.
pub async fn spawn_run(
    registry: Arc<Mutex<RunRegistry>>,
    plan_path: &Path,
    repo_dir: &Path,
    session_name: Option<String>,
    config_dir: Option<PathBuf>,
    verbose: u8,
) -> Result<String> {
    // ── Pre-flight checks ───────────────────────────────────────────
    preflight_checks(plan_path, repo_dir).await?;

    // ── Register run ────────────────────────────────────────────────
    let run_id = {
        let mut reg = registry.lock().await;
        reg.register_run(
            plan_path.to_path_buf(),
            repo_dir.to_path_buf(),
            session_name,
            config_dir.clone(),
        )?
    };

    spawn_after_register(
        registry,
        run_id,
        plan_path.to_path_buf(),
        repo_dir.to_path_buf(),
        config_dir,
        verbose,
    )
    .await
}

/// Spawn a run that was previously enqueued, reusing its registered `run_id`.
///
/// Paired with `RunRegistry::promote_queued`. The drain path calls this
/// instead of `spawn_run` so the Queued→Running transition happens on the
/// existing RunEntry — no second entry with a fresh id, no ghost row in
/// `gw ps`, and `gw logs <id>` / `gw stop <id>` keep working across the
/// queue→run boundary.
pub async fn spawn_queued_run(
    registry: Arc<Mutex<RunRegistry>>,
    pending: crate::daemon::registry::PendingRun,
    verbose: u8,
) -> Result<String> {
    // Pre-flight uses the same gates as fresh runs. Running them BEFORE
    // `promote_queued` avoids acquiring the repo lock only to release it
    // on a disk-space or network failure.
    preflight_checks(&pending.plan_path, &pending.repo_dir).await?;

    // Promote the existing Queued RunEntry: acquires repo lock, resets
    // `started_at` for correct uptime, flips `restartable` to true.
    // Status stays Queued until the tmux send below flips it to Running —
    // matching the fresh-run state machine.
    {
        let mut reg = registry.lock().await;
        reg.promote_queued(&pending.run_id)?;
    }

    spawn_after_register(
        registry,
        pending.run_id,
        pending.plan_path,
        pending.repo_dir,
        pending.config_dir,
        verbose,
    )
    .await
}

/// Drain the next queued run for a repo after a run completes.
///
/// CRITICAL: Must be called AFTER releasing the registry lock.
/// Uses `tokio::spawn` to avoid deadlock — `spawn_run` re-acquires the lock.
pub(crate) async fn drain_if_available(
    registry: Arc<Mutex<RunRegistry>>,
    repo_dir: &Path,
    failed: bool,
) {
    // INV-19 / PERF-003: stage all mutations inside the mutex scope, then
    // release the guard before fsync'ing. The canonical multi-mutation
    // pattern uses `*_without_snapshot` helpers + a single trailing
    // `stage_queue_locked` so the queue.json fsync runs once outside the
    // lock, regardless of how many mutations the caller composed.
    //
    // The `registry_lock_held` debug event scopes the in-mutex window
    // for the PERF-003 benchmark methodology (see `plans/benchmark.md`).
    // Timed post-acquisition; explicit `Instant` is used because the
    // tracing span guard is `!Send` and would forbid awaiting on
    // `registry.lock()` while in scope.
    let (staged_batch, next, queue_snap) = {
        let mut reg = registry.lock().await;
        let lock_held_start = std::time::Instant::now();
        let staged_batch = if failed {
            reg.record_queue_failure_without_snapshot(repo_dir)
        } else {
            reg.record_queue_success_without_snapshot(repo_dir);
            Vec::new()
        };
        // Try the completing repo's queue first, then any other queue
        // with available capacity (handles runs queued by A6 global cap).
        let next = reg
            .drain_next_without_snapshot(repo_dir)
            .or_else(|| reg.drain_any_ready_without_snapshot());
        // One snapshot, captured after every mutation has landed in memory.
        let queue_snap = reg.stage_queue_locked();
        tracing::debug!(
            site = "drain_if_available",
            elapsed_us = lock_held_start.elapsed().as_micros() as u64,
            "registry_lock_held"
        );
        (staged_batch, next, queue_snap)
    }; // `reg` dropped — mutex released before fsync loop below

    for staged in &staged_batch {
        if let Err(e) = RunRegistry::flush_status(staged) {
            error!(run_id = %staged.entry.run_id, error = %e, "flush_status failed: circuit breaker drain");
        }
    }
    if let Err(e) = RunRegistry::flush_queue(&queue_snap) {
        error!(error = %e, "flush_queue failed after drain_if_available");
    }

    if let Some(pending) = next {
        let registry = Arc::clone(&registry);
        // Preserve values needed in the error branch BEFORE moving `pending`
        // into `spawn_queued_run`. The run_id is needed to mark the ghost
        // RunEntry Failed so it doesn't linger as a Queued row in `gw ps`.
        let pending_run_id = pending.run_id.clone();
        let repo_dir = pending.repo_dir.clone();
        let verbose = pending.verbose;

        // BACK-001: register the task in the process-wide drain JoinSet so
        // shutdown can await completion before flushing queue state.  Fall
        // back to bare tokio::spawn if init was not called (test paths).
        let task = async move {
            match spawn_queued_run(Arc::clone(&registry), pending, verbose).await {
                Ok(run_id) => {
                    info!(run_id = %run_id, "drained queued run — now running");
                }
                Err(e) => {
                    error!(error = %e, "failed to spawn drained run — recording failure");
                    // INV-19: stage everything under the lock, then release
                    // the guard before fsync'ing the circuit-breaker batch.
                    // Ensure the still-Queued RunEntry is cleared. Two cases:
                    //   1. Pre-flight failed → entry stays Queued, lock never
                    //      acquired. update_status(Failed) marks it terminal.
                    //   2. promote_queued or later step failed → entry may
                    //      still be Queued with lock held. update_status
                    //      releases the lock via its Failed branch.
                    // Without this, `drain_next` already popped the PendingRun
                    // so the entry would ghost as "queued" forever.
                    //
                    // PERF-003: same canonical pattern as the outer scope —
                    // `*_without_snapshot` for each mutation plus a single
                    // trailing `stage_queue_locked`, all flushed after lock
                    // release.
                    let (failed_staged, staged_batch, next, queue_snap) = {
                        let mut reg = registry.lock().await;
                        let lock_held_start = std::time::Instant::now();
                        let failed_staged = reg
                            .stage_status_locked(
                                &pending_run_id,
                                RunStatus::Failed,
                                None,
                                Some(format!("drain spawn failed: {e}")),
                            )
                            .map_err(|se| color_eyre::eyre::eyre!("{se}"));
                        let staged_batch = reg.record_queue_failure_without_snapshot(&repo_dir);
                        let next = reg.drain_next_without_snapshot(&repo_dir);
                        let queue_snap = reg.stage_queue_locked();
                        tracing::debug!(
                            site = "drain_if_available_retry",
                            elapsed_us = lock_held_start.elapsed().as_micros() as u64,
                            "registry_lock_held"
                        );
                        (failed_staged, staged_batch, next, queue_snap)
                    }; // `reg` dropped — mutex released before fsyncs

                    match failed_staged {
                        Ok(s) => {
                            if let Err(fe) = RunRegistry::flush_status(&s) {
                                error!(run_id = %pending_run_id, error = %fe, "flush_status failed: marking failed after drain spawn error");
                            }
                        }
                        Err(ue) => {
                            error!(run_id = %pending_run_id, error = %ue, "stage_status failed: marking failed after drain spawn error");
                        }
                    }
                    for staged in &staged_batch {
                        if let Err(fe) = RunRegistry::flush_status(staged) {
                            error!(run_id = %staged.entry.run_id, error = %fe, "flush_status failed: circuit breaker drain");
                        }
                    }
                    if let Err(fe) = RunRegistry::flush_queue(&queue_snap) {
                        error!(error = %fe, "flush_queue failed after drain spawn error recovery");
                    }
                    if let Some(retry) = next {
                        // BACK-004/FLAW-003: spawn retry in its own task so the
                        // 12s SPAWN_INIT_WAIT_SECS does not block this task
                        // (which would extend the shutdown race window).  The
                        // new task is also registered so shutdown awaits it
                        // before flushing queue state.
                        info!("retrying with next queued run after spawn failure");
                        let retry_verbose = retry.verbose;
                        let retry_task = async move {
                            if let Err(re) = spawn_queued_run(registry, retry, retry_verbose).await {
                                error!(error = %re, "retry drain spawn failed");
                            }
                        };
                        let handle = tokio::spawn(retry_task);
                        if let Some(dh) = DRAIN_HANDLES.get() {
                            dh.lock().await.push(handle);
                        }
                        // If DRAIN_HANDLES not initialized (test paths), handle
                        // is simply dropped (task still runs detached).
                    }
                }
            }
        };
        let handle = tokio::spawn(task);
        if let Some(dh) = DRAIN_HANDLES.get() {
            dh.lock().await.push(handle);
        }
    }
}

/// Shared pre-flight I/O checks used by both fresh and queued spawn paths.
///
/// Extracted so `spawn_run` and `spawn_queued_run` run an identical gate
/// sequence. Keep this set stable — reconciler, heartbeat, and circuit
/// breaker assume a run that reaches registry mutation has already passed
/// these checks.
async fn preflight_checks(plan_path: &Path, repo_dir: &Path) -> Result<()> {
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

    // Pre-flight disk space gate on BOTH the repo volume and GW_HOME.
    // Daemon-managed runs may target a repo on a different filesystem than
    // ~/.gw, so both must be checked. Bail out cleanly rather than spawn a
    // process that will fail mid-write.
    crate::cleanup::health::check_disk_space_at(repo_dir)
        .map_err(|e| eyre!("repo disk space check failed: {e}"))?;
    crate::cleanup::health::check_disk_space_at(&crate::daemon::state::gw_home())
        .map_err(|e| eyre!("GW_HOME disk space check failed: {e}"))?;

    // Pre-flight: network connectivity.
    // Uses spawn_blocking internally — safe for the tokio runtime.
    // On failure the caller (server.rs SubmitRun) enqueues via the existing queue path.
    if !crate::daemon::network::is_online_async().await {
        return Err(eyre!("network unavailable"));
    }

    Ok(())
}

/// Post-registration spawn: pre_phase_cleanup, tmux spawn, arc command
/// dispatch, and status flip to Running.
///
/// The caller must have already produced a RunEntry keyed by `run_id` in
/// the registry (via `register_run` or `promote_queued`) and acquired the
/// repo lock. Any error path here marks the entry Failed so the repo lock
/// is released by `update_status`'s terminal-state handler.
async fn spawn_after_register(
    registry: Arc<Mutex<RunRegistry>>,
    run_id: String,
    plan_path: PathBuf,
    repo_dir: PathBuf,
    config_dir: Option<PathBuf>,
    verbose: u8,
) -> Result<String> {
    let level_label = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    info!(run_id = %run_id, plan = %plan_path.display(), verbose = level_label, "run registered");

    // ── Pre-flight: match foreground sequence ───────────────────────
    // Parity with `engine::single_session::monitor::run_session_attempt`
    // (see plan GAP A3). Runs BEFORE tmux spawn so a dirty process tree
    // or stale signal files from a prior run do not corrupt the new
    // session's state.
    if let Err(e) = crate::cleanup::pre_phase_cleanup("daemon", "0") {
        // Propagate like the foreground path does — a failed pre-flight
        // indicates a process-tree state we cannot reason about. Mark the
        // registry entry Failed so consumers see accurate status (matches
        // the tmux-spawn-failure path below — addresses BACK-002).
        warn!(run_id = %run_id, error = %e, "pre_phase_cleanup failed");
        let reason = format!("pre_phase_cleanup failed: {e}");
        crate::daemon::events::append_event(&run_id, "spawn_failed", &reason);
        // BACK-007: stage under lock, release, then fsync outside (INV-19).
        let staged = {
            let mut reg = registry.lock().await;
            reg.stage_status_locked(&run_id, RunStatus::Failed, None, Some(reason))
                .map_err(|se| color_eyre::eyre::eyre!("{se}"))
        };
        match staged {
            Ok(s) => {
                if let Err(fe) = RunRegistry::flush_status(&s) {
                    tracing::error!(run_id = %run_id, error = %fe, "flush_status failed: marking failed after pre_phase_cleanup error");
                    // Silence spurious Drop BUG log: flush was attempted. Matches update_status convention.
                    s.mark_flushed();
                }
            }
            Err(ue) => {
                tracing::error!(run_id = %run_id, error = %ue, "stage_status failed: marking failed after pre_phase_cleanup error");
            }
        }
        return Err(e);
    }
    crate::commands::elden::clear_signals_from(&repo_dir);

    // ── Spawn tmux session ──────────────────────────────────────────
    // Always derive the tmux name from `run_id`. Previously the drain
    // path could thread a stale `pending.session_name` through — that
    // path is gone now that queued runs reuse their registered id.
    let tmux_session = format!("gw-{}", run_id);

    let config = SpawnConfig {
        session_id: tmux_session.clone(),
        working_dir: repo_dir.clone(),
        config_dir,
        claude_path: "claude".to_string(),
        mock: false,
    };

    let plan_str = plan_path.to_string_lossy().to_string();

    match spawn::spawn_claude_session(&config) {
        Ok(pid) => {
            info!(run_id = %run_id, pid = pid, tmux = %tmux_session, "tmux session spawned");

            // Write session_owner.json so a subsequent `gw run` (foreground)
            // can adopt this daemon-spawned session if the daemon crashes.
            // Parity with monitor.rs:73-77 (plan GAP A3).
            if let Err(e) = crate::monitor::session_owner::write_session_owner(
                &repo_dir,
                &tmux_session,
                &plan_str,
                pid,
            ) {
                warn!(run_id = %run_id, error = %e, "failed to write session owner (non-fatal)");
            }

            // Persist claude PID + set last_recovery_at so the first-attempt
            // uptime window (used at heartbeat.rs:697) is measured from the
            // current spawn, not from daemon boot. Plan GAP A5.
            let mut reg = registry.lock().await;
            if let Some(entry) = reg.get_mut(&run_id) {
                entry.claude_pid = Some(pid);
                // See heartbeat.rs:697 — `run_uptime = now - session_start`
                // where `session_start = last_recovery_at.unwrap_or(started_at)`.
                // Without this, a first-attempt crash produces a run_uptime
                // measured from daemon boot, inflating `record_healthy_runtime`.
                entry.last_recovery_at = Some(chrono::Utc::now());
            }
            drop(reg);

            // ── Spawn-init wait (C-5) ──────────────────────────────────
            // Match foreground monitor.rs:79-81: wait for Claude TUI to
            // finish initializing before dispatching /rune:arc.
            crate::daemon::events::append_event(
                &run_id,
                "spawn_wait_init",
                &format!("pid={} — waiting 12s for Claude TUI", pid),
            );
            tokio::time::sleep(std::time::Duration::from_secs(SPAWN_INIT_WAIT_SECS)).await;

            // After TUI init, `spawn_claude_session` returned the tmux pane (shell)
            // pid. Claude itself is a child of that shell — refresh `claude_pid`
            // with the real process so meta.json and any future signal/kill paths
            // reference Claude rather than the shell session leader.
            let session_for_pid = tmux_session.clone();
            if let Some(real_pid) = tokio::task::spawn_blocking(move || {
                spawn::get_claude_pid(&session_for_pid)
            })
            .await
            .ok()
            .flatten()
            {
                let mut reg = registry.lock().await;
                if let Some(entry) = reg.get_mut(&run_id) {
                    entry.claude_pid = Some(real_pid);
                }
                drop(reg);
                debug!(run_id = %run_id, shell_pid = pid, claude_pid = real_pid, "refreshed claude_pid after TUI init");
            } else {
                warn!(run_id = %run_id, shell_pid = pid, "could not resolve Claude PID after TUI init — keeping shell pid");
            }
        }
        Err(e) => {
            // Spawn failed — clean up registry entry
            warn!(run_id = %run_id, error = %e, "failed to spawn tmux session");
            let reason = format!("tmux spawn failed: {e}");
            crate::daemon::events::append_event(&run_id, "spawn_failed", &reason);
            let mut reg = registry.lock().await;
            if let Err(ue) = reg.update_status(
                &run_id,
                RunStatus::Failed,
                None,
                Some(reason),
            ) {
                tracing::error!(run_id = %run_id, error = %ue, "update_status failed: marking failed after tmux spawn error");
            }
            return Err(e);
        }
    }

    // ── Send arc command ────────────────────────────────────────────
    // Shared with the foreground path — see
    // `crate::engine::single_session::util::build_arc_command` for the
    // flag-aware variant used on foreground restarts.
    let arc_cmd = crate::engine::single_session::util::build_arc_command_plain(&plan_str);

    crate::daemon::events::append_event(&run_id, "spawn_dispatch", &format!("sending: {}", arc_cmd));

    if let Err(e) = spawn::send_keys_with_workaround(&tmux_session, &arc_cmd) {
        warn!(error = %e, "failed to send arc command — killing session");
        let reason = format!("failed to send arc command: {e}");
        // Persist pane capture before kill so post-mortem debugging works —
        // parity with monitor.rs:88 (plan GAP A3).
        crate::session::detect::save_crash_dump(&tmux_session, &repo_dir, &reason);
        crate::daemon::events::append_event(&run_id, "kill_session", &format!(
            "gw killed session '{}' — reason: {} (killed by: executor/send_keys_failed)",
            tmux_session, reason,
        ));
        let _ = spawn::kill_session(&tmux_session);
        // BACK-007: stage under lock, release, then fsync outside (INV-19).
        let staged = {
            let mut reg = registry.lock().await;
            reg.stage_status_locked(&run_id, RunStatus::Failed, None, Some(reason))
                .map_err(|se| color_eyre::eyre::eyre!("{se}"))
        };
        match staged {
            Ok(s) => {
                if let Err(fe) = RunRegistry::flush_status(&s) {
                    tracing::error!(run_id = %run_id, error = %fe, "flush_status failed: marking failed after send_keys error");
                    // Silence spurious Drop BUG log: flush was attempted. Matches update_status convention.
                    s.mark_flushed();
                }
            }
            Err(ue) => {
                tracing::error!(run_id = %run_id, error = %ue, "stage_status failed: marking failed after send_keys error");
            }
        }
        return Err(e);
    }

    // ── Update registry ─────────────────────────────────────────────
    // PERF-002: stage under the lock, release, then fsync (INV-19). Holding
    // the registry mutex across the flush would block all concurrent IPC
    // (ListRuns, StopRun, other SubmitRuns) for 1–20 ms on every spawn.
    let staged = {
        let mut reg = registry.lock().await;
        if let Some(entry) = reg.get_mut(&run_id) {
            entry.tmux_session = Some(tmux_session.clone());
        }
        reg.stage_status_locked(
            &run_id,
            RunStatus::Running,
            Some("starting".to_string()),
            None,
        )
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?
    }; // mutex released before fsync
    RunRegistry::flush_status(&staged)?;

    info!(
        run_id = %run_id,
        tmux = %tmux_session,
        "run started successfully"
    );

    // Log structured event for `gw logs`
    crate::daemon::events::log_run_started(&run_id, &plan_str);

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
    monitors: Arc<tokio::sync::Mutex<HashMap<String, MonitorHandle>>>,
    run_id: &str,
) -> Result<()> {
    // Step 0: Cancel the monitor BEFORE sending /exit to prevent race
    // where the monitor fires its kill gate between cancel and stop.
    {
        let mut mons = monitors.lock().await;
        if let Some(handle) = mons.remove(run_id) {
            handle.cancel.cancel();
            // Brief grace for monitor to exit its poll loop
            drop(mons);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let (tmux_session, repo_dir) = {
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

        (entry.tmux_session.clone(), entry.repo_dir.clone())
    };

    let tmux_session = match tmux_session {
        Some(s) => s,
        None => {
            // No tmux session — just mark as stopped
            crate::daemon::events::append_event(run_id, "stopped", "stopped by user (no tmux session to kill)");
            {
                let mut reg = registry.lock().await;
                reg.update_status(
                    run_id,
                    RunStatus::Stopped,
                    None,
                    Some("stopped (no tmux session)".to_string()),
                )?;
            }
            // Drain next queued run for this repo — parity with heartbeat.rs
            // completion paths. Without this, the queue stalls until the next
            // completion event (which may never arrive if no run is active).
            drain_if_available(
                Arc::clone(&registry),
                &repo_dir,
                false,
            )
            .await;
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
        crate::daemon::events::append_event(run_id, "kill_session", &format!(
            "gw force-killed session '{}' — reason: user requested stop, session did not exit within {}s grace (killed by: executor/stop_run)",
            tmux_session, GRACEFUL_STOP_TIMEOUT.as_secs(),
        ));
        if let Err(e) = spawn::kill_session(&tmux_session) {
            warn!(error = %e, "failed to force-kill tmux session");
        }
    } else {
        crate::daemon::events::append_event(run_id, "stopped", &format!(
            "stopped by user — session '{}' exited gracefully after /exit",
            tmux_session,
        ));
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
    crate::daemon::events::log_run_stopped(run_id);

    // Drain next queued run for this repo — parity with heartbeat.rs
    // completion paths (GAP-6). Without this, stopping the last Running run
    // leaves the queue stalled indefinitely because drain_if_available is
    // only invoked on completion events.
    drain_if_available(Arc::clone(&registry), &repo_dir, false).await;

    Ok(())
}

// ── Helper functions ────────────────────────────────────────────────

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
    use crate::engine::single_session::util::build_arc_command_plain;

    // Mirror tests pinning the escaping behavior of the shared
    // `build_arc_command_plain` helper from the daemon's call-site. Any
    // change to the escaping (e.g., heartbeat.rs:925 path) must also be
    // reflected here so the daemon spawn → send_keys contract stays tight.
    #[test]
    fn build_arc_command_simple() {
        let cmd = build_arc_command_plain("plans/feat.md");
        assert_eq!(cmd, "/rune:arc 'plans/feat.md'");
    }

    #[test]
    fn build_arc_command_escapes_quotes() {
        let cmd = build_arc_command_plain("plans/it's-a-plan.md");
        assert_eq!(cmd, "/rune:arc 'plans/it'\\''s-a-plan.md'");
    }

    #[test]
    fn check_git_clean_nonexistent_dir() {
        // Non-existent directory: Command::output() fails because the OS
        // cannot set the working directory, so .ok()? returns None.
        let result = check_git_clean(Path::new("/nonexistent/repo"));
        assert!(result.is_none());
    }
}
