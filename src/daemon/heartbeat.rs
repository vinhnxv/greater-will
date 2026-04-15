//! Heartbeat monitor for active daemon runs.
//!
//! Periodically checks tmux session health for each `Running` entry,
//! captures logs, updates phase information, and spawns per-run
//! [`DaemonRunMonitor`] tasks for full watchdog intelligence. When a
//! monitor exits, the outcome handler orchestrates crash recovery using
//! phase-aware restart commands and per-error-class backoff.

use crate::config::watchdog::WatchdogConfig;
use crate::daemon::protocol::RunStatus;
use crate::daemon::registry::{RunEntry, RunRegistry};
use crate::daemon::server::drain_if_available;
use crate::daemon::run_monitor::{DaemonRunMonitor, DaemonRunOutcome};
use crate::daemon::state::gw_home;
use crate::engine::crash_loop::{CrashLoopDecision, CrashLoopDetector};
use crate::engine::retry::ErrorClass;
use crate::engine::single_session::util::is_pipeline_complete;
use crate::session::spawn;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::daemon::network;
use crate::daemon::state::NetworkState;
use crate::monitor::loop_state::{read_arc_loop_state, LoopStateRead};

/// Interval between heartbeat checks.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Handle for a spawned per-run monitor task.
///
/// Stores the `JoinHandle` for the outcome-handler wrapper (which itself
/// awaits the inner `DaemonRunMonitor` task) and a `CancellationToken`
/// derived from the global cancel token via `child_token()`.
pub(crate) struct MonitorHandle {
    pub(crate) join: tokio::task::JoinHandle<()>,
    pub(crate) cancel: CancellationToken,
}

/// Per-run detection state for stage-transition logging.
///
/// Tracks what the daemon has already observed for each run so events
/// fire only on *transitions*, not every heartbeat tick. Pruned when the
/// run reaches a terminal state (see `prune_detection_state`).
#[derive(Debug, Default, Clone)]
// TODO(Patch-2): Wire event emission using detection_state fields in read_phase_via_loop_state.
// Once wired, remove this #[allow(dead_code)] — all fields will be actively read/written.
#[allow(dead_code)]
struct DetectionSnapshot {
    /// Whether an Active loop state has been seen at least once for this run.
    loop_state_seen_once: bool,
    /// Whether the `waiting_loop_state` event has been logged (fire-once).
    waiting_loop_state_logged: bool,
    /// Last observed loop state — used for `diff()`-based change detection.
    last_loop_state: Option<crate::monitor::loop_state::ArcLoopState>,
    /// Last resolved checkpoint path — for detecting path changes.
    last_checkpoint_path: Option<std::path::PathBuf>,
    /// Last effective phase name — for `phase_change` event dedup.
    last_effective_phase: Option<String>,
    /// Throttle for `checkpoint_missing` events (once per 60s per run).
    /// Uses `Instant` (monotonic) to avoid issues with system clock changes.
    last_checkpoint_missing_log: Option<std::time::Instant>,
}

/// Heartbeat monitor that watches over active runs.
///
/// Spawns a background tokio task that periodically:
/// 1. Spawns a [`DaemonRunMonitor`] per new Running entry
/// 2. Cancels monitors for stopped/failed/succeeded runs
/// 3. Captures pane output and updates phase for lightweight `gw ps` display
/// 4. Delegates crash recovery to [`handle_monitor_outcome`]
/// ## Lock ordering convention (BACK-011)
///
/// This module uses three `tokio::sync::Mutex`es: `registry`, `monitors`,
/// and `detection_state`. They must **never be held simultaneously** —
/// always acquire one, extract what you need, `drop` it, then acquire
/// the other. The canonical order when multiple are needed in sequence is:
///
/// 1. `registry` — snapshot data, then drop
/// 2. `monitors` — mutate monitor handles, then drop
/// 3. `detection_state` — mutate per-run detection snapshots, then drop
///
/// `executor.rs::stop_run` follows the same convention. Violating this
/// (holding multiple locks at once) creates an AB/BA deadlock risk with
/// concurrent stop_run calls.
pub struct HeartbeatMonitor {
    registry: Arc<Mutex<RunRegistry>>,
    /// Per-run monitor handles, keyed by run_id. Shared with `DaemonServer`
    /// so that stop/detach flows can cancel monitors before acting.
    monitors: Arc<tokio::sync::Mutex<HashMap<String, MonitorHandle>>>,
    /// Global cancellation token — child tokens are derived per-run.
    global_cancel: CancellationToken,
    /// Cached watchdog config — resolved once at construction to avoid
    /// re-parsing env vars on every heartbeat tick (BACK-002).
    watchdog: WatchdogConfig,
    /// Shared network state for internet recovery.
    /// Lock ordering: registry → network_state → monitors → detection_state.
    network_state: Arc<RwLock<NetworkState>>,
    /// Per-run detection state for stage-transition logging.
    /// Map key: run_id. Value: last observed snapshot.
    /// Lock ordering: acquire AFTER monitors (lowest priority lock).
    detection_state: Arc<Mutex<HashMap<String, DetectionSnapshot>>>,
}

impl HeartbeatMonitor {
    /// Create a new heartbeat monitor wrapping the shared registry.
    pub fn new(
        registry: Arc<Mutex<RunRegistry>>,
        global_cancel: CancellationToken,
        network_state: Arc<RwLock<NetworkState>>,
    ) -> Self {
        Self {
            registry,
            monitors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            global_cancel,
            watchdog: WatchdogConfig::from_env(),
            network_state,
            detection_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Clone the monitors `Arc` for sharing with `DaemonServer`.
    ///
    /// Must be called BEFORE `start()` consumes `self`.
    pub fn monitors(&self) -> Arc<tokio::sync::Mutex<HashMap<String, MonitorHandle>>> {
        Arc::clone(&self.monitors)
    }

    /// Cancel a specific run's monitor (used by stop/detach flows).
    ///
    /// Note: stop_run and detach currently access the monitors Arc directly.
    /// This method is provided for convenience when a `HeartbeatMonitor`
    /// reference is available.
    #[allow(dead_code)]
    pub async fn cancel_monitor(&self, run_id: &str) {
        let mut monitors = self.monitors.lock().await;
        if let Some(handle) = monitors.remove(run_id) {
            handle.cancel.cancel();
        }
    }

    /// Remove detection state entries for stopped runs and orphaned run_ids.
    ///
    /// Called after `monitors.retain()` on every heartbeat tick. Removes:
    /// 1. Entries whose run_id is in the `stopped` list (normal terminal path).
    /// 2. Entries whose run_id is not in `active_run_ids` (defensive — handles
    ///    external removal via `gw rm` or registry mutation outside heartbeat).
    async fn prune_detection_state(
        &self,
        stopped: &[String],
        active_run_ids: &std::collections::HashSet<&String>,
    ) {
        let mut det = self.detection_state.lock().await;
        for run_id in stopped {
            det.remove(run_id);
        }
        // Defensive: prune orphaned entries not in active registry
        det.retain(|id, _| active_run_ids.contains(id));
    }

    /// Start the heartbeat loop as a background tokio task.
    ///
    /// Returns a `JoinHandle` that can be used to await or abort the task.
    /// The task runs until the global cancellation token fires.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        let cancel = self.global_cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            // Default MissedTickBehavior is Burst: if a tick is missed (e.g., check_all_runs
            // took longer than HEARTBEAT_INTERVAL under load), the next tick fires immediately
            // with 0 delay, producing back-to-back ticks. Skip missed ticks instead to keep
            // the cadence steady under stress.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            info!("heartbeat monitor started (interval: {:?})", HEARTBEAT_INTERVAL);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("heartbeat monitor cancelled — exiting");
                        break;
                    }
                    _ = interval.tick() => {
                        self.check_all_runs().await;
                    }
                }
            }
        })
    }

    /// Check all Running entries in the registry.
    ///
    /// Spawns a [`DaemonRunMonitor`] for each new Running entry that doesn't
    /// already have a monitor, cancels monitors for stopped/finished runs,
    /// and prunes finished handles. Lightweight phase tracking via
    /// `handle_alive_session` continues for `gw ps` display.
    async fn check_all_runs(&self) {
        // BACK-007: Single list_runs(true) call, partitioned into running/stopped.
        // BACK-014: Pre-clone RunEntry snapshots in the initial lock scope to
        // eliminate per-entry re-locking when spawning monitors.
        let (running, stopped) = {
            let registry = self.registry.lock().await;
            let all_runs = registry.list_runs(true);

            let mut running: Vec<(String, Option<String>, Option<crate::daemon::registry::RunEntry>)> = Vec::new();
            let mut stopped: Vec<String> = Vec::new();

            for r in &all_runs {
                match r.status {
                    RunStatus::Running | RunStatus::Queued => {
                        let entry = registry.get(&r.run_id);
                        let tmux = entry.as_ref().and_then(|e| e.tmux_session.clone());
                        let snapshot = entry.cloned();
                        running.push((r.run_id.clone(), tmux, snapshot));
                    }
                    RunStatus::Stopped | RunStatus::Failed | RunStatus::Succeeded => {
                        stopped.push(r.run_id.clone());
                    }
                }
            }

            (running, stopped)
        }; // Registry lock released here

        let mut monitors = self.monitors.lock().await;

        // Cancel monitors for stopped/detached/completed runs
        for run_id in &stopped {
            if let Some(handle) = monitors.remove(run_id) {
                handle.cancel.cancel();
            }
        }

        // Spawn monitors for new Running entries
        for (run_id, tmux_session, entry_snapshot) in &running {
            if monitors.contains_key(run_id) {
                continue;
            }
            // Skip entries without a tmux session (Queued, not yet spawned)
            if tmux_session.is_none() {
                continue;
            }

            // Use pre-cloned snapshot (BACK-014: no second registry lock needed)
            let entry_snapshot = match entry_snapshot {
                Some(e) => e.clone(),
                None => continue,
            };

            let run_cancel = self.global_cancel.child_token();
            let watchdog = self.watchdog.clone();
            let monitor_registry = Arc::clone(&self.registry);
            let mut monitor = DaemonRunMonitor::new(&entry_snapshot, monitor_registry, watchdog);

            let cancel_clone = run_cancel.clone();
            let run_id_inner = run_id.clone();

            // Inner task: run the monitor, returning its outcome
            let monitor_join = tokio::spawn(async move {
                monitor.run(cancel_clone).await
            });

            // Outer task: await the inner, catch panics, dispatch outcome
            let outcome_registry = Arc::clone(&self.registry);
            let outcome_run_id = run_id.clone();
            let outcome_cancel = self.global_cancel.clone();
            let outcome_network_state = Arc::clone(&self.network_state);
            let join = tokio::spawn(async move {
                match monitor_join.await {
                    Ok(outcome) => {
                        handle_monitor_outcome(
                            &run_id_inner,
                            outcome,
                            outcome_registry,
                            outcome_cancel,
                            outcome_network_state,
                        )
                        .await;
                    }
                    Err(e) if e.is_panic() => {
                        error!(
                            run_id = %outcome_run_id,
                            "DaemonRunMonitor panicked: {:?}", e
                        );
                        // INV-19: stage under the lock, release, fsync outside.
                        let staged = {
                            let mut reg = outcome_registry.lock().await;
                            reg.stage_status_locked(
                                &outcome_run_id,
                                RunStatus::Failed,
                                None,
                                Some("Monitor panicked — marking failed".to_string()),
                            )
                            .map_err(|se| color_eyre::eyre::eyre!("{se}"))
                        }; // reg dropped — mutex released before fsync
                        match staged {
                            Ok(s) => {
                                if let Err(fe) = RunRegistry::flush_status(&s) {
                                    tracing::error!(run_id = %outcome_run_id, error = %fe, "flush_status failed: marking failed after monitor panic");
                                }
                            }
                            Err(se) => {
                                tracing::error!(run_id = %outcome_run_id, error = %se, "stage_status failed: marking failed after monitor panic");
                            }
                        }
                        append_event(
                            &outcome_run_id,
                            "monitor_panic",
                            &format!("DaemonRunMonitor panicked: {:?}", e),
                        );
                    }
                    Err(e) => {
                        debug!(
                            run_id = %outcome_run_id,
                            "Monitor task ended: {}", e
                        );
                    }
                }
            });

            monitors.insert(run_id.clone(), MonitorHandle { join, cancel: run_cancel });
        }

        // Prune finished handles (outcome already processed)
        monitors.retain(|_, h| !h.join.is_finished());

        // Collect active run_ids so we can prune orphaned detection state.
        let active_run_ids: std::collections::HashSet<&String> =
            running.iter().map(|(id, _, _)| id).collect();

        drop(monitors);

        // Prune detection state for stopped runs and orphaned entries whose
        // run_id has no corresponding registry entry (defensive — handles
        // external removal via `gw rm`).
        self.prune_detection_state(&stopped, &active_run_ids).await;

        // Lightweight phase tracking for `gw ps` — runs on every tick
        // independently of the per-run monitors.
        for (run_id, tmux_session, _) in &running {
            if let Some(session) = tmux_session {
                self.handle_alive_session(run_id, session).await;
            }
        }
    }

    /// Handle a healthy (alive) tmux session: capture logs and update phase.
    async fn handle_alive_session(&self, run_id: &str, tmux_session: &str) {
        // Capture pane output for log archival
        match spawn::capture_pane(tmux_session) {
            Ok(content) => {
                self.append_pane_log(run_id, &content);
                self.update_phase_from_pane(run_id, &content).await;
            }
            Err(e) => {
                debug!(run_id = %run_id, error = %e, "failed to capture pane");
            }
        }
    }

    // NOTE: handle_dead_session removed — DaemonRunMonitor handles dead
    // session detection and returns a DaemonRunOutcome. Recovery is
    // orchestrated by handle_monitor_outcome() below.

    /// Append captured pane content to the pane log (raw tmux capture).
    ///
    /// Rotates the log file to `pane.log.1` when it exceeds `PANE_LOG_ROTATE_BYTES`
    /// to bound disk usage — without rotation, a 12h run accumulates ~21 MB of
    /// pane captures, and `diagnose_session_death` would try to read all of it.
    fn append_pane_log(&self, run_id: &str, content: &str) {
        /// Rotate pane.log when it grows past this size (10 MiB).
        const PANE_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

        let log_dir = gw_home().join("runs").join(run_id).join("logs");
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            debug!(error = %e, "failed to create log directory");
            return;
        }

        let log_path = log_dir.join("pane.log");

        // Rotate if the current log has grown past the threshold. Best-effort:
        // metadata/rename failures degrade to "no rotation this tick" rather
        // than aborting the append.
        if let Ok(meta) = std::fs::metadata(&log_path) {
            if meta.len() > PANE_LOG_ROTATE_BYTES {
                let rotated = log_path.with_extension("log.1");
                if let Err(e) = std::fs::rename(&log_path, &rotated) {
                    debug!(error = %e, "failed to rotate pane.log");
                }
            }
        }

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
                debug!(error = %e, "failed to append to pane log");
            }
        }
    }

    /// Update the run's current phase from checkpoint.json (ground truth),
    /// falling back to pane content heuristics when no checkpoint is available.
    async fn update_phase_from_pane(&self, run_id: &str, pane_content: &str) {
        // Try loop-state → checkpoint first — this is the authoritative source
        let checkpoint_phase = self.read_phase_via_loop_state(run_id).await;
        // Capture whether the checkpoint independently confirms completion,
        // BEFORE we move `checkpoint_phase` into the phase-update path below.
        // Used later as a cross-check gate against pane-text false positives
        // (Claude writing documentation containing "arc completed", etc.).
        let cp_confirms_completion = checkpoint_phase.as_deref() == Some("complete");

        // Fall back to pane heuristics only when checkpoint is unavailable.
        // Track whether the phase came from checkpoint (which already emitted
        // the enriched phase_change event) or from heuristics (which needs a
        // separate event).
        let (phase, from_checkpoint) = if let Some(cp_phase) = checkpoint_phase {
            (Some(cp_phase), true)
        } else {
            (phase_from_pane_heuristic(pane_content), false)
        };

        if let Some(phase_name) = phase {
            let mut registry = self.registry.lock().await;
            let phase_changed = if let Some(entry) = registry.get_mut(run_id) {
                if entry.current_phase.as_deref() != Some(&phase_name) {
                    debug!(run_id = %run_id, phase = %phase_name, "phase updated");
                    entry.current_phase = Some(phase_name.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            };

            // Only persist and log when the phase actually changed
            if phase_changed {
                if let Err(e) = registry.update_status(
                    run_id,
                    RunStatus::Running,
                    Some(phase_name.clone()),
                    None,
                ) {
                    debug!(run_id = %run_id, error = %e, "failed to persist phase update");
                }

                drop(registry);
                // Only emit phase_change from here when the phase came from
                // pane heuristics. Checkpoint-sourced phases already emitted
                // the enriched phase_change event inside read_phase_via_loop_state.
                if !from_checkpoint {
                    append_event(run_id, "phase_change", &phase_name);
                }
            } else {
                drop(registry);
            }
        }

        // Check for pipeline completion. Checkpoint.json is the authoritative
        // source: if it reports phase == "complete", promote the run to
        // Succeeded regardless of pane text. If checkpoint disagrees (or is
        // unreadable) and only pane text matches, refuse to mark Succeeded —
        // `is_pipeline_complete` is a heuristic that false-positives when
        // Claude writes documentation containing "arc completed",
        // "pipeline completed", etc.
        //
        // Two-gate promotion:
        //   1. checkpoint says complete  → promote (pane agreement is a bonus)
        //   2. pane says complete alone  → suppressed as a false positive
        //   3. neither says complete     → no-op, keep running
        let pane_says_complete = is_pipeline_complete(pane_content);
        if !cp_confirms_completion {
            if pane_says_complete {
                // FINDING-002 note: checkpoint is the authoritative source.
                // Pane text is only trusted when checkpoint independently confirms.
                debug!(
                    run_id = %run_id,
                    "pane text matches completion but checkpoint disagrees — ignoring"
                );
            }
            return;
        }

        // cp_confirms_completion == true from here on.
        if pane_says_complete {
            info!(run_id = %run_id, "pipeline completed (pane + checkpoint confirmed)");
            append_event(run_id, "completion_detected", "pipeline completion detected via pane text pattern match (checkpoint-confirmed)");
        } else {
            info!(run_id = %run_id, "pipeline completed (checkpoint authoritative; no pane match)");
            append_event(run_id, "completion_detected", "pipeline completion detected via checkpoint.json (pane text did not match heuristic)");
        }
        {
            let mut registry = self.registry.lock().await;
            // Re-check status: stop_run (or another path) may have mutated the
            // entry between our earlier lock drop and this re-acquisition. If
            // the run is no longer Running, skip the Succeeded update to avoid
            // overwriting a Stopped/Failed terminal state.
            let still_running = registry
                .get(run_id)
                .map(|e| e.status == RunStatus::Running)
                .unwrap_or(false);
            if !still_running {
                debug!(
                    run_id = %run_id,
                    "status changed since alive-check began — skipping completion update"
                );
                return;
            }
            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Succeeded,
                Some("complete".to_string()),
                None,
            ) {
                tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking succeeded");
            }
            drop(registry);
            append_event(run_id, "completed", "pipeline finished successfully");
        }
    }

    /// Read the current phase from the authoritative checkpoint pointed to by
    /// `.rune/arc-phase-loop.local.md`. No directory scanning — matches
    /// `engine/single_session/util.rs::current_phase_from_checkpoint`.
    ///
    /// Returns:
    /// - `Some(phase_name)` when loop state is Active AND checkpoint is readable
    ///   AND `infer_phase_position().effective_phase()` yields a phase.
    /// - `None` when loop state is Missing, Inactive, or checkpoint path does not
    ///   exist yet (early arc bootstrap).
    ///
    /// Emits structured events via `append_event` on stage transitions:
    /// - `waiting_loop_state`: once per run when loop state is Missing
    /// - `loop_state_appeared`: once per run when Active first seen
    /// - `loop_state_changed`: on field-level diffs (anomalous diffs use `warn!`)
    /// - `checkpoint_missing`: throttled once per 60s when checkpoint not on disk
    /// - `checkpoint_read_failed`: every read error (message truncated to 200 chars)
    /// - `phase_change`: enriched with from/to/arc_id/iteration/checkpoint path
    async fn read_phase_via_loop_state(&self, run_id: &str) -> Option<String> {
        let repo_dir = {
            let registry = self.registry.lock().await;
            registry.get(run_id).map(|e| e.repo_dir.clone())?
        };

        let loop_state_read = read_arc_loop_state(&repo_dir);

        let state = match loop_state_read {
            LoopStateRead::Missing => {
                // Emit waiting_loop_state once per run
                let mut det = self.detection_state.lock().await;
                let snap = det.entry(run_id.to_string()).or_default();
                if !snap.waiting_loop_state_logged {
                    snap.waiting_loop_state_logged = true;
                    drop(det);
                    info!(run_id = %run_id, "loop state missing — waiting for arc to initialize");
                    append_event(run_id, "waiting_loop_state", "arc-phase-loop.local.md not found yet");
                }
                return None;
            }
            LoopStateRead::Inactive => {
                debug!(run_id = %run_id, "loop state inactive — no phase from checkpoint");
                return None;
            }
            LoopStateRead::Active(s) => s,
        };

        // Emit loop_state_appeared (once per run) and loop_state_changed (on diffs)
        {
            let mut det = self.detection_state.lock().await;
            let snap = det.entry(run_id.to_string()).or_default();

            if !snap.loop_state_seen_once {
                snap.loop_state_seen_once = true;
                let msg = format!(
                    "loop state active: iteration {}/{} plan={} branch={}",
                    state.iteration, state.max_iterations, state.plan_file, state.branch,
                );
                snap.last_loop_state = Some(state.clone());
                drop(det);
                info!(run_id = %run_id, "{}", msg);
                append_event(run_id, "loop_state_appeared", &msg);
            } else if let Some(ref prev) = snap.last_loop_state {
                let changes = prev.diff(&state);
                if !changes.is_empty() {
                    let has_anomaly = changes.iter().any(|c| c.anomalous);
                    let change_desc: Vec<String> = changes
                        .iter()
                        .map(|c| format!("{}={} → {}", c.field, c.old_value, c.new_value))
                        .collect();
                    let msg = change_desc.join(", ");
                    if has_anomaly {
                        warn!(run_id = %run_id, "loop state changed (anomalous): {}", msg);
                    } else {
                        info!(run_id = %run_id, "loop state changed: {}", msg);
                    }
                    snap.last_loop_state = Some(state.clone());
                    drop(det);
                    append_event(run_id, "loop_state_changed", &msg);
                } else {
                    drop(det);
                }
            } else {
                snap.last_loop_state = Some(state.clone());
                drop(det);
            }
        }

        let checkpoint_path = state.resolve_checkpoint_path(&repo_dir);
        if !checkpoint_path.exists() {
            // Throttle checkpoint_missing: once per 60s per run
            let mut det = self.detection_state.lock().await;
            let snap = det.entry(run_id.to_string()).or_default();
            let should_log = match snap.last_checkpoint_missing_log {
                None => true,
                Some(last) => last.elapsed() >= Duration::from_secs(60),
            };
            if should_log {
                snap.last_checkpoint_missing_log = Some(std::time::Instant::now());
                drop(det);
                info!(
                    run_id = %run_id,
                    path = %checkpoint_path.display(),
                    "checkpoint file not on disk yet"
                );
                append_event(
                    run_id,
                    "checkpoint_missing",
                    &format!("checkpoint not found: {}", checkpoint_path.display()),
                );
            }
            return None;
        }

        match crate::checkpoint::reader::read_checkpoint(&checkpoint_path) {
            Ok(cp) => {
                let position = cp.infer_phase_position();
                let phase = position.effective_phase()?.to_string();

                // Enriched phase_change event: emit only on transitions
                let mut det = self.detection_state.lock().await;
                let snap = det.entry(run_id.to_string()).or_default();
                let prev_phase = snap.last_effective_phase.clone();
                if prev_phase.as_deref() != Some(&phase) {
                    snap.last_effective_phase = Some(phase.clone());
                    snap.last_checkpoint_path = Some(checkpoint_path.clone());
                    drop(det);

                    let from = prev_phase.as_deref().unwrap_or("(none)");
                    let arc_id = state.arc_id().unwrap_or("unknown");
                    let msg = format!(
                        "phase: {} → {} [arc_id={} iteration={}/{} checkpoint={}]",
                        from, phase, arc_id,
                        state.iteration, state.max_iterations,
                        checkpoint_path.display(),
                    );
                    info!(run_id = %run_id, "{}", msg);
                    // phase_change event name preserved for backward compat with
                    // history.rs formatter — payload is enriched, not renamed.
                    append_event(run_id, "phase_change", &msg);
                }

                Some(phase)
            }
            Err(e) => {
                let err_msg = format!("{}", e);
                let truncated = if err_msg.len() > 200 { &err_msg[..200] } else { &err_msg };
                info!(run_id = %run_id, error = %truncated, "failed to read checkpoint");
                append_event(
                    run_id,
                    "checkpoint_read_failed",
                    &format!("error reading checkpoint: {}", truncated),
                );
                None
            }
        }
    }
}

// ── Uptime helpers ───────────────────────────────────────────────

/// Compute uptime for the current session cycle.
///
/// Uses `last_recovery_at` if set (from a previous error recovery),
/// otherwise falls back to `started_at` for the initial session.
/// This ensures the crash-loop detector's budget resets correctly
/// per recovery cycle, not across the lifetime of all crashes.
fn compute_run_uptime_secs(entry: &RunEntry) -> u64 {
    let session_start = entry.last_recovery_at.unwrap_or(entry.started_at);
    chrono::Utc::now()
        .signed_duration_since(session_start)
        .num_seconds()
        .max(0) as u64
}

// ── Structured event logging ───────────────────────────────────────

/// Append a structured event to the run's event log (events.jsonl).
///
/// Each line is a JSON object with timestamp, event type, and message.
/// This is the primary data source for `gw logs <id>`.
pub fn append_event(run_id: &str, event: &str, message: &str) {
    let log_dir = gw_home().join("runs").join(run_id).join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        debug!(error = %e, "failed to create log directory for events");
        return;
    }

    let log_path = log_dir.join("events.jsonl");
    let timestamp = chrono::Utc::now().to_rfc3339();
    let line = serde_json::json!({
        "ts": timestamp,
        "event": event,
        "msg": message,
    });

    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", line);
        }
        Err(e) => {
            debug!(error = %e, "failed to append event");
        }
    }
}

/// Log the initial "run started" event. Called from executor after spawn.
pub fn log_run_started(run_id: &str, plan_path: &str) {
    append_event(run_id, "started", &format!("plan: {plan_path}"));
}

/// Log a run stopped event.
pub fn log_run_stopped(run_id: &str) {
    append_event(run_id, "stopped", "stopped by user");
}

// ── Monitor outcome handling ──────────────────────────────────────

/// Handle the terminal outcome of a [`DaemonRunMonitor`].
///
/// ## Recovery decision tree
///
/// ```text
/// Completed       → mark Succeeded, clear crash history
/// Failed          → mark Failed (business-logic error, no retry)
/// Cancelled       → no-op (caller handles registry update)
/// ErrorDetected   → if fatal (skips_plan): mark Failed
///                   if retryable: feed through CrashLoopDetector,
///                   backoff per error_class, then attempt_recovery
/// Crashed/Stuck/Timeout → feed through CrashLoopDetector with
///                   persistence, cooldown, then attempt_recovery
/// ```
///
/// All backoff sleeps are wrapped in `tokio::select!` with the cancel
/// token so that daemon shutdown is not blocked by long backoffs
/// (P1 concern STSM-005).
///
/// Both ErrorDetected and Crashed/Stuck/Timeout paths feed through
/// `CrashLoopDetector` as a unified exit budget (P1 concern: dual
/// exit-mechanism coordination).
async fn handle_monitor_outcome(
    run_id: &str,
    outcome: DaemonRunOutcome,
    registry: Arc<Mutex<RunRegistry>>,
    cancel: CancellationToken,
    network_state: Arc<RwLock<NetworkState>>,
) {
    match outcome {
        DaemonRunOutcome::Completed => {
            // QUAL-013: Single lock scope to prevent TOCTOU race where
            // stop_run could overwrite status between two acquisitions.
            let (repo_dir, tmux_session) = {
                let mut reg = registry.lock().await;
                let entry = reg.get(run_id);
                let dir = entry.map(|e| e.repo_dir.clone());
                let tmux = entry.and_then(|e| e.tmux_session.clone());
                if let Err(e) = reg.update_status(
                    run_id,
                    RunStatus::Succeeded,
                    Some("complete".to_string()),
                    None,
                ) {
                    tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking succeeded");
                }
                (dir, tmux)
            };
            if let Some(ref dir) = repo_dir {
                CrashLoopDetector::clear_history(dir);
            }
            append_event(run_id, "completed", "pipeline finished");

            // Reap the tmux session — natural completion paths
            // (`check_completion_grace`, `poll_loop_state`) can return
            // `Completed` while the session is still alive (e.g. arc reached
            // terminal phase but Claude pane is still rendering). Without
            // this kill, sessions linger forever as `gw-*` orphans even
            // after the run is `Succeeded`. Run in a blocking task so the
            // sync `tmux kill-session` call doesn't stall the async runtime.
            if let Some(session) = tmux_session {
                append_event(run_id, "kill_session", &format!(
                    "gw reaped session '{}' — reason: pipeline completed (killed by: heartbeat/handle_monitor_outcome)",
                    session,
                ));
                let session_for_kill = session.clone();
                let join = tokio::task::spawn_blocking(move || {
                    spawn::kill_session(&session_for_kill)
                })
                .await;
                match join {
                    Ok(Ok(())) => {
                        debug!(run_id = %run_id, tmux = %session, "reaped tmux session after completion");
                    }
                    Ok(Err(e)) => {
                        warn!(run_id = %run_id, tmux = %session, error = %e, "failed to reap tmux session after completion");
                    }
                    Err(join_err) => {
                        warn!(run_id = %run_id, tmux = %session, error = %join_err, "kill_session task panicked after completion");
                    }
                }
            }

            // Drain next queued run for this repo
            if let Some(ref dir) = repo_dir {
                drain_if_available(Arc::clone(&registry), dir, false).await;
            }
        }

        DaemonRunOutcome::Failed { reason } => {
            // INV-19: stage under the lock, release, fsync outside.
            let (staged, repo_dir) = {
                let mut reg = registry.lock().await;
                let staged = reg
                    .stage_status_locked(run_id, RunStatus::Failed, None, Some(reason.clone()))
                    .map_err(|se| color_eyre::eyre::eyre!("{se}"));
                // Extract repo_dir before dropping lock to avoid re-acquisition race
                let repo_dir = reg.get(run_id).map(|e| e.repo_dir.clone());
                (staged, repo_dir)
            }; // reg dropped — mutex released before fsync

            match staged {
                Ok(s) => {
                    if let Err(fe) = RunRegistry::flush_status(&s) {
                        tracing::error!(run_id = %run_id, error = %fe, "flush_status failed: marking failed");
                    }
                }
                Err(se) => {
                    tracing::error!(run_id = %run_id, error = %se, "stage_status failed: marking failed");
                }
            }
            append_event(run_id, "failed", &reason);
            // Drain next queued run for this repo (failure increments circuit breaker)
            if let Some(dir) = repo_dir {
                drain_if_available(Arc::clone(&registry), &dir, true).await;
            }
        }

        DaemonRunOutcome::Cancelled => {
            // No action — caller (stop_run, detach, shutdown) handles registry
            debug!(run_id = %run_id, "monitor cancelled — no-op");
        }

        DaemonRunOutcome::ErrorDetected { error_class, reason } => {
            // Fatal errors: no retry
            if error_class.skips_plan() {
                let mut reg = registry.lock().await;
                if let Err(e) = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!("Fatal {:?}: {}", error_class, reason)),
                ) {
                    tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking failed after fatal error");
                }
                drop(reg);
                append_event(run_id, "fatal_error", &reason);
                // Drain next queued run (fatal error counts as failure)
                let repo_dir_for_drain = {
                    let reg = registry.lock().await;
                    reg.get(run_id).map(|e| e.repo_dir.clone())
                };
                if let Some(dir) = repo_dir_for_drain {
                    drain_if_available(Arc::clone(&registry), &dir, true).await;
                }
                return;
            }

            // ── NetworkError: wait for connectivity instead of blind backoff ──
            //
            // Instead of retrying 3 times with 30s backoff (which wastes 5-6 min
            // spawning tmux sessions that immediately fail), wait for actual
            // connectivity restoration (up to 30 min, polling every 30s).
            if matches!(error_class, ErrorClass::NetworkError) {
                info!(run_id = %run_id, "network error — waiting for connectivity");

                // Update shared network state
                {
                    let mut state = network_state.write().await;
                    *state = NetworkState::WaitingForNetwork { since: chrono::Utc::now() };
                }

                append_event(run_id, "waiting_for_network", &format!(
                    "NetworkError: {} — waiting up to 30min for connectivity",
                    reason
                ));

                let restored = network::wait_for_connectivity(
                    network::CONNECTIVITY_POLL_INTERVAL,
                    network::CONNECTIVITY_MAX_WAIT,
                    cancel.clone(),
                ).await;

                // Restore network state
                {
                    let mut state = network_state.write().await;
                    *state = NetworkState::Online;
                }

                if restored {
                    info!(run_id = %run_id, "connectivity restored — attempting recovery");
                    let (repo_dir, plan_path, config_dir) = {
                        let reg = registry.lock().await;
                        match reg.get(run_id) {
                            Some(e) => (e.repo_dir.clone(), e.plan_path.clone(), e.config_dir.clone()),
                            None => return,
                        }
                    };
                    attempt_recovery(run_id, &repo_dir, &plan_path, config_dir, registry).await;
                } else {
                    // Cancelled or timed out after 30 min
                    let mut reg = registry.lock().await;
                    if let Err(e) = reg.update_status(
                        run_id,
                        RunStatus::Failed,
                        None,
                        Some("Network unavailable for 30 minutes — giving up".to_string()),
                    ) {
                        tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking failed after network timeout");
                    }
                    drop(reg);
                    append_event(run_id, "network_timeout", "30min connectivity timeout exceeded");
                }
                return;
            }

            // Feed ALL recovery paths through CrashLoopDetector (P1: unified budget)
            let (repo_dir, plan_path, config_dir, current_phase) = {
                let reg = registry.lock().await;
                match reg.get(run_id) {
                    Some(e) => (
                        e.repo_dir.clone(),
                        e.plan_path.clone(),
                        e.config_dir.clone(),
                        e.current_phase.clone(),
                    ),
                    None => return,
                }
            };

            let watchdog = WatchdogConfig::from_env();
            let mut detector = CrashLoopDetector::from_watchdog(&watchdog);
            detector.load_history(&repo_dir);

            // GAP 2: Check if session ran long enough to count as healthy.
            // Use last_recovery_at (per-session) to avoid inflating uptime across
            // multiple recovery cycles (FLAW-001 fix).
            let run_uptime = {
                let reg = registry.lock().await;
                reg.get(run_id).map(compute_run_uptime_secs).unwrap_or(0)
            };
            detector.record_healthy_runtime(run_uptime);

            // Phase-aware retry ceiling (DET-7): attribute this crash to
            // the run's current phase so per-phase budgets apply.
            match detector.record_restart_for_phase(current_phase.as_deref()) {
                CrashLoopDecision::StopCrashLoop => {
                    let mut reg = registry.lock().await;
                    if let Err(e) = reg.update_status(
                        run_id,
                        RunStatus::Failed,
                        None,
                        Some(format!("Crash loop ({:?}): {}", error_class, reason)),
                    ) {
                        tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking failed after crash loop");
                    }
                    drop(reg);
                    CrashLoopDetector::clear_history(&repo_dir);
                    append_event(run_id, "crash_loop", &format!("{:?}: {}", error_class, reason));
                    // GAP 6: Drain next queued run so the queue doesn't get stuck
                    drain_if_available(Arc::clone(&registry), &repo_dir, true).await;
                    return;
                }
                CrashLoopDecision::AllowRestart => {}
            }

            if let Err(e) = detector.persist(&repo_dir) {
                warn!(error = %e, "Failed to persist crash history");
            }

            // GAP 1: Per-error-class retry limit (parity with foreground orchestrator)
            let crash_count = {
                let reg = registry.lock().await;
                reg.get(run_id).map(|e| e.crash_restarts).unwrap_or(0)
            };
            if crash_count >= error_class.max_retries() {
                let mut reg = registry.lock().await;
                if let Err(e) = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!(
                        "Max retries ({}) for {:?}: {}",
                        error_class.max_retries(), error_class, reason
                    )),
                ) {
                    tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking failed after max retries");
                }
                drop(reg);
                append_event(run_id, "max_retries", &format!("{:?}: {}", error_class, reason));
                drain_if_available(Arc::clone(&registry), &repo_dir, true).await;
                return;
            }

            // Per-error-class exponential backoff (P1: cancellation-aware)
            let backoff = error_class.backoff_for_attempt(crash_count);
            append_event(run_id, "backoff", &format!(
                "{:?}: {}s before retry",
                error_class, backoff.as_secs()
            ));
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = cancel.cancelled() => {
                    debug!(run_id = %run_id, "backoff interrupted by shutdown");
                    return;
                }
            }

            attempt_recovery(run_id, &repo_dir, &plan_path, config_dir, registry).await;
        }

        outcome @ (DaemonRunOutcome::Crashed { .. }
        | DaemonRunOutcome::Stuck { .. }
        | DaemonRunOutcome::Timeout { .. }) => {
            // Extract reason, classify outcome kind for logging, and map to
            // implicit ErrorClass for per-class retry/backoff policy (GAP 1, 5).
            //
            // Note: Foreground treats Timeout as terminal (no retry).
            // Daemon retries because headless runs benefit from automatic recovery.
            // Per-class max_retries (3) still limits total attempts.
            let (implicit_class, outcome_kind, reason) = match outcome {
                DaemonRunOutcome::Crashed { reason } => (ErrorClass::Crash, "crashed", reason),
                DaemonRunOutcome::Stuck { reason } => (ErrorClass::Stuck, "stuck", reason),
                DaemonRunOutcome::Timeout { reason } => (ErrorClass::Timeout, "timeout", reason),
                _ => unreachable!(),
            };

            // Log the reason that triggered recovery (closes logging gap
            // where only "cooldown 60s" appeared without explaining WHY).
            append_event(run_id, outcome_kind, &reason);

            let (repo_dir, plan_path, config_dir, current_phase) = {
                let reg = registry.lock().await;
                match reg.get(run_id) {
                    Some(e) => (
                        e.repo_dir.clone(),
                        e.plan_path.clone(),
                        e.config_dir.clone(),
                        e.current_phase.clone(),
                    ),
                    None => return,
                }
            };

            // CrashLoopDetector with persistence (unified budget for all paths)
            let watchdog = WatchdogConfig::from_env();
            let mut detector = CrashLoopDetector::from_watchdog(&watchdog);
            detector.load_history(&repo_dir);

            // GAP 2: Check if session ran long enough to count as healthy.
            // If so, reset crash counters before recording this new crash.
            let run_uptime = {
                let reg = registry.lock().await;
                reg.get(run_id).map(compute_run_uptime_secs).unwrap_or(0)
            };
            detector.record_healthy_runtime(run_uptime);

            // Phase-aware retry ceiling (DET-7): attribute this crash
            // to the run's current phase so per-phase budgets apply.
            match detector.record_restart_for_phase(current_phase.as_deref()) {
                CrashLoopDecision::StopCrashLoop => {
                    let mut reg = registry.lock().await;
                    if let Err(e) = reg.update_status(
                        run_id,
                        RunStatus::Failed,
                        None,
                        Some(format!("Crash loop ({:?}): {}", implicit_class, reason)),
                    ) {
                        tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking failed after crash loop");
                    }
                    drop(reg);
                    CrashLoopDetector::clear_history(&repo_dir);
                    append_event(run_id, "crash_loop", &format!("{:?}: {}", implicit_class, reason));
                    // GAP 6: Drain next queued run so the queue doesn't get stuck
                    drain_if_available(Arc::clone(&registry), &repo_dir, true).await;
                    return;
                }
                CrashLoopDecision::AllowRestart => {}
            }

            if let Err(e) = detector.persist(&repo_dir) {
                warn!(error = %e, "Failed to persist crash history");
            }

            // GAP 1: Per-error-class retry limit (parity with foreground orchestrator)
            let crash_count = {
                let reg = registry.lock().await;
                reg.get(run_id).map(|e| e.crash_restarts).unwrap_or(0)
            };
            if crash_count >= implicit_class.max_retries() {
                let mut reg = registry.lock().await;
                if let Err(e) = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!(
                        "Max retries ({}) for {:?}: {}",
                        implicit_class.max_retries(), implicit_class, reason
                    )),
                ) {
                    tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking failed after max retries");
                }
                drop(reg);
                append_event(run_id, "max_retries", &format!("{:?}: {}", implicit_class, reason));
                drain_if_available(Arc::clone(&registry), &repo_dir, true).await;
                return;
            }

            // GAP 5: Per-class backoff instead of flat cooldown (parity with foreground)
            let backoff = implicit_class.backoff_for_attempt(crash_count);
            append_event(run_id, "backoff", &format!(
                "{:?}: {}s before retry",
                implicit_class, backoff.as_secs()
            ));
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = cancel.cancelled() => {
                    debug!(run_id = %run_id, "backoff interrupted by shutdown");
                    return;
                }
            }

            attempt_recovery(run_id, &repo_dir, &plan_path, config_dir, registry).await;
        }
    }
}

/// Phase-aware recovery using `resolve_restart_command`.
///
/// Determines the correct restart strategy (Fresh, Resume, or AlreadyDone)
/// by reading the checkpoint, then delegates to `spawn_recovery` with the
/// resolved command.
async fn attempt_recovery(
    run_id: &str,
    repo_dir: &Path,
    plan_path: &Path,
    config_dir: Option<PathBuf>,
    registry: Arc<Mutex<RunRegistry>>,
) {
    use crate::engine::single_session::util::resolve_restart_command;
    use crate::engine::single_session::util::RestartDecision;
    use crate::engine::single_session::SingleSessionConfig;

    // Terminal-status gate: if the run has been moved to Stopped/Failed/Succeeded
    // by stop_run/detach/completion between the monitor outcome firing and this
    // recovery call, DO NOT respawn. This closes the auto-resume race where
    // `stop_run` cancels the per-run monitor token but the outer outcome task
    // holds the global token (heartbeat.rs:206) and sails past the cooldown into
    // recovery. Without this gate, `gw stop` would be overridden and the run
    // would respawn repeatedly — exactly the "can't stop" symptom reported.
    {
        let reg = registry.lock().await;
        if let Some(entry) = reg.get(run_id) {
            if matches!(
                entry.status,
                RunStatus::Stopped | RunStatus::Failed | RunStatus::Succeeded
            ) {
                info!(
                    run_id = %run_id,
                    status = ?entry.status,
                    "Recovery skipped: run has terminal status (stopped/failed/succeeded)"
                );
                append_event(
                    run_id,
                    "recovery_skipped",
                    &format!("terminal status {:?} — not respawning", entry.status),
                );
                return;
            }
        } else {
            // Run gone from registry entirely (e.g., pruned) — nothing to recover.
            debug!(run_id = %run_id, "Recovery skipped: run not in registry");
            return;
        }
    }

    let config = SingleSessionConfig::new(repo_dir);
    let plan_str = plan_path.to_string_lossy();
    // Shell-escape single quotes to prevent command injection (SEC-001).
    // Matches the escaping in executor.rs::build_arc_command.
    let escaped = plan_str.replace('\'', "'\\''");
    let arc_command = format!("/rune:arc '{}'", escaped);

    let crash_count = {
        let reg = registry.lock().await;
        reg.get(run_id).map(|e| e.crash_restarts).unwrap_or(0)
    };

    let restart_cmd = resolve_restart_command(
        repo_dir, &plan_str, &config, &arc_command, crash_count,
    );

    match restart_cmd {
        RestartDecision::AlreadyDone => {
            info!(run_id = %run_id, "Pipeline already complete — skipping recovery");
            let mut reg = registry.lock().await;
            if let Err(e) = reg.update_status(
                run_id,
                RunStatus::Succeeded,
                Some("complete".to_string()),
                None,
            ) {
                tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking succeeded during recovery skip");
            }
            drop(reg);
            append_event(run_id, "already_done", "pipeline complete on recovery check");
        }
        RestartDecision::Fresh(cmd) | RestartDecision::Resume(cmd) => {
            // Disk space check before spawning (BACK-004: check repo volume,
            // not cwd — matches spawn_recovery_with_command pattern)
            if let Err(e) = crate::cleanup::health::check_disk_space_at(repo_dir) {
                let mut reg = registry.lock().await;
                if let Err(e2) = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!("Recovery aborted (repo disk): {}", e)),
                ) {
                    tracing::error!(run_id = %run_id, error = %e2, "update_status failed: marking failed after repo disk check");
                }
                drop(reg);
                return;
            }
            if let Err(e) = crate::cleanup::health::check_disk_space_at(&gw_home()) {
                let mut reg = registry.lock().await;
                if let Err(e2) = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!("Recovery aborted (GW_HOME disk): {}", e)),
                ) {
                    tracing::error!(run_id = %run_id, error = %e2, "update_status failed: marking failed after GW_HOME disk check");
                }
                drop(reg);
                return;
            }

            spawn_recovery_with_command(registry, run_id, repo_dir, &cmd, config_dir).await;
        }
    }
}

/// Spawn a recovery session using a pre-resolved restart command.
///
/// Similar to the original `spawn_recovery` but accepts the command
/// string from `resolve_restart_command` instead of building it internally.
async fn spawn_recovery_with_command(
    registry: Arc<Mutex<RunRegistry>>,
    run_id: &str,
    repo_dir: &Path,
    cmd: &str,
    config_dir: Option<PathBuf>,
) {
    let tmux_session = format!("gw-{}", run_id);

    // Kill existing dead session for clean recreation.
    // QUAL-011: Wrap blocking std::process::Command calls in spawn_blocking
    // to avoid stalling the tokio async thread pool (matches DaemonRunMonitor pattern).
    let session_check = tmux_session.clone();
    let session_exists = tokio::task::spawn_blocking(move || spawn::has_session(&session_check))
        .await
        .unwrap_or(false);
    if session_exists {
        let session_check = tmux_session.clone();
        let claude_alive = tokio::task::spawn_blocking(move || is_claude_alive_in_session(&session_check))
            .await
            .unwrap_or(false);
        if claude_alive {
            info!(run_id = %run_id, "ABORT recovery: session and claude are alive");
            let mut reg = registry.lock().await;
            if let Some(entry) = reg.get_mut(run_id) {
                entry.current_phase = Some("running".to_string());
            }
            if let Err(e) = reg.update_status(run_id, RunStatus::Running, Some("running".to_string()), None) {
                tracing::error!(run_id = %run_id, error = %e, "update_status failed: marking running");
            }
            return;
        }
        append_event(run_id, "kill_session", &format!(
            "gw killed session '{}' for clean recovery", tmux_session,
        ));
        let _ = spawn::kill_session(&tmux_session);
    }

    // Pre-flight disk space checks
    if let Err(e) = crate::cleanup::health::check_disk_space_at(repo_dir) {
        warn!(run_id = %run_id, error = %e, "aborting recovery: repo disk too low");
        append_event(run_id, "recovery_failed", &format!("disk full (repo): {e}"));
        let mut reg = registry.lock().await;
        if let Err(e2) = reg.update_status(run_id, RunStatus::Failed, None, Some(format!("crash recovery aborted: {e}"))) {
            tracing::error!(run_id = %run_id, error = %e2, "update_status failed: marking failed after repo disk check");
        }
        return;
    }
    if let Err(e) = crate::cleanup::health::check_disk_space_at(&gw_home()) {
        warn!(run_id = %run_id, error = %e, "aborting recovery: GW_HOME disk too low");
        append_event(run_id, "recovery_failed", &format!("disk full (GW_HOME): {e}"));
        let mut reg = registry.lock().await;
        if let Err(e2) = reg.update_status(run_id, RunStatus::Failed, None, Some(format!("crash recovery aborted: {e}"))) {
            tracing::error!(run_id = %run_id, error = %e2, "update_status failed: marking failed after GW_HOME disk check");
        }
        return;
    }

    info!(run_id = %run_id, tmux = %tmux_session, "starting crash recovery");

    let spawn_config = spawn::SpawnConfig {
        session_id: tmux_session.clone(),
        working_dir: repo_dir.to_path_buf(),
        config_dir,
        claude_path: "claude".to_string(),
        mock: false,
    };

    match spawn::spawn_claude_session(&spawn_config) {
        Ok(pid) => {
            // FLAW-005: Wait for Claude TUI to finish initializing before sending
            // the resume command — without this, send_keys arrives before stdin
            // is ready and the command is silently dropped, leaving the session
            // alive but never resuming. Matches executor::spawn_after_register.
            append_event(
                run_id,
                "recovery_wait_init",
                &format!("pid={} — waiting {}s for Claude TUI", pid, crate::daemon::executor::SPAWN_INIT_WAIT_SECS),
            );
            tokio::time::sleep(Duration::from_secs(crate::daemon::executor::SPAWN_INIT_WAIT_SECS)).await;

            // FLAW-009: `spawn_claude_session` returned the tmux pane (shell)
            // pid; resolve the real Claude process pid now that the TUI is up.
            let session_for_pid = tmux_session.clone();
            let real_pid = tokio::task::spawn_blocking(move || spawn::get_claude_pid(&session_for_pid))
                .await
                .ok()
                .flatten();
            if real_pid.is_none() {
                warn!(run_id = %run_id, shell_pid = pid, "could not resolve Claude PID after TUI init — keeping shell pid");
            }

            if let Err(e) = spawn::send_keys_with_workaround(&tmux_session, cmd) {
                warn!(error = %e, "failed to send resume command");
                let mut reg = registry.lock().await;
                if let Err(e2) = reg.update_status(run_id, RunStatus::Failed, None, Some(format!("crash recovery failed: {e}"))) {
                    tracing::error!(run_id = %run_id, error = %e2, "update_status failed: marking failed after crash recovery");
                }
                return;
            }

            let mut reg = registry.lock().await;
            if let Some(entry) = reg.get_mut(run_id) {
                entry.tmux_session = Some(tmux_session.clone());
                entry.current_phase = Some("resuming".to_string());
                entry.crash_restarts = entry.crash_restarts.saturating_add(1);
                entry.claude_pid = Some(real_pid.unwrap_or(pid));
                entry.last_recovery_at = Some(chrono::Utc::now());
            }
            if let Err(e) = reg.update_status(run_id, RunStatus::Running, Some("resuming".to_string()), None) {
                warn!(run_id = %run_id, error = %e, "failed to persist recovery state");
            }
            drop(reg);

            info!(run_id = %run_id, tmux = %tmux_session, "crash recovery started");
            append_event(run_id, "recovery_started", &format!("new session: {tmux_session}"));
            crate::daemon::drain::clear_snapshot(run_id);
        }
        Err(e) => {
            warn!(run_id = %run_id, error = %e, "failed to spawn recovery session");
            let mut reg = registry.lock().await;
            if let Err(e2) = reg.update_status(run_id, RunStatus::Failed, None, Some(format!("crash recovery spawn failed: {e}"))) {
                tracing::error!(run_id = %run_id, error = %e2, "update_status failed: marking failed after recovery spawn failure");
            }
            drop(reg);
            append_event(run_id, "recovery_failed", &e.to_string());
        }
    }
}

// BACK-003: Legacy spawn_recovery removed — superseded by
// spawn_recovery_with_command with phase-aware resolve_restart_command.
// SEC-002: Also eliminates the shell injection vector in the legacy code.
// See git history for the original implementation if needed.

/// Fallback: extract a coarse phase name from pane content heuristics.
///
/// Used only when checkpoint.json is not available (e.g., session just started
/// and Rune hasn't written a checkpoint yet).
fn phase_from_pane_heuristic(pane_content: &str) -> Option<String> {
    // Primary: match the arc stop-hook's system message format:
    //   "Arc phase loop — executing phase: {PHASE_NAME} (iteration N)"
    // Scan bottom-up so the most recent phase wins.
    for line in pane_content.lines().rev().take(30) {
        if let Some(rest) = line.to_lowercase().strip_prefix("arc phase loop") {
            // Extract phase name after "executing phase: "
            if let Some(phase_start) = rest.find("executing phase:") {
                let after = &rest[phase_start + "executing phase:".len()..];
                let phase = after.split_whitespace().next()?;
                return Some(phase.to_string());
            }
        }
    }

    // Fallback: match "phase_N" markers (these are unambiguous)
    let last_lines: Vec<&str> = pane_content.lines().rev().take(20).collect();
    let tail = last_lines.join("\n").to_lowercase();

    if tail.contains("phase_1") {
        Some("plan".to_string())
    } else if tail.contains("phase_2") {
        Some("work".to_string())
    } else if tail.contains("phase_3") {
        Some("review".to_string())
    } else if tail.contains("phase_4") {
        Some("test".to_string())
    } else if tail.contains("phase_5") {
        Some("ship".to_string())
    } else {
        None
    }
}


// BACK-006: Legacy diagnose_session_death and diagnose_tmux_server_death
// removed (~180 lines of dead code). DaemonRunMonitor performs its own
// diagnosis. See git history for the original implementation if needed.

/// Check if a claude process is running inside a tmux session.
///
/// Uses `tmux list-panes` to get the pane PID, then checks if it has
/// a claude child process via `pgrep -P`.
fn is_claude_alive_in_session(tmux_session: &str) -> bool {
    use std::process::Command;

    // Get pane PID
    let output = Command::new("tmux")
        .args(["list-panes", "-t", tmux_session, "-F", "#{pane_pid}"])
        .output();

    let pane_pid = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
        }
        _ => None,
    };

    let Some(pid) = pane_pid else {
        return false;
    };

    // Check if pane's shell has a claude child process
    Command::new("pgrep")
        .args(["-P", &pid.to_string(), "-f", "claude"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::RunRegistry;

    /// Helper: create a registry with one Running entry and a fake tmux session.
    fn setup_running_entry(
        tmp: &tempfile::TempDir,
    ) -> (Arc<Mutex<RunRegistry>>, String) {
        let mut reg = RunRegistry::new();
        let repo_dir = tmp.path().to_path_buf();
        let plan_path = repo_dir.join("plan.md");
        std::fs::write(&plan_path, "# test plan").unwrap();

        let run_id = reg
            .register_run(plan_path, repo_dir, Some("test-session".into()), None)
            .unwrap();

        if let Some(entry) = reg.get_mut(&run_id) {
            entry.tmux_session = Some(format!("gw-{}", run_id));
            entry.status = RunStatus::Running;
        }

        (Arc::new(Mutex::new(reg)), run_id)
    }

    #[tokio::test]
    async fn heartbeat_monitor_new_creates_empty_monitors() {
        let reg = Arc::new(Mutex::new(RunRegistry::new()));
        let cancel = CancellationToken::new();
        let hb = HeartbeatMonitor::new(reg, cancel, Arc::new(RwLock::new(NetworkState::default())));
        let monitors = hb.monitors();
        let map = monitors.lock().await;
        assert!(map.is_empty(), "new HeartbeatMonitor should have no monitors");
    }

    #[tokio::test]
    async fn monitors_arc_is_shared() {
        let reg = Arc::new(Mutex::new(RunRegistry::new()));
        let cancel = CancellationToken::new();
        let hb = HeartbeatMonitor::new(reg, cancel, Arc::new(RwLock::new(NetworkState::default())));
        let monitors1 = hb.monitors();
        let monitors2 = hb.monitors();
        assert!(Arc::ptr_eq(&monitors1, &monitors2));
    }

    #[tokio::test]
    async fn monitor_cancelled_on_stop() {
        let monitors: Arc<tokio::sync::Mutex<HashMap<String, MonitorHandle>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
        });

        let run_id = "test-run-123".to_string();
        {
            let mut map = monitors.lock().await;
            map.insert(
                run_id.clone(),
                MonitorHandle {
                    join,
                    cancel: cancel_token.clone(),
                },
            );
        }

        // Cancel the monitor (simulating stop_run behavior)
        {
            let mut map = monitors.lock().await;
            if let Some(handle) = map.remove(&run_id) {
                handle.cancel.cancel();
            }
        }

        assert!(cancel_token.is_cancelled());
        let map = monitors.lock().await;
        assert!(map.is_empty(), "monitor should be removed after cancel");
    }

    #[tokio::test]
    async fn global_cancel_cascades_to_children() {
        let global_cancel = CancellationToken::new();
        let monitors: Arc<tokio::sync::Mutex<HashMap<String, MonitorHandle>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let mut child_tokens = Vec::new();
        for i in 0..3 {
            let child = global_cancel.child_token();
            let child_clone = child.clone();
            child_tokens.push(child.clone());

            let join = tokio::spawn(async move {
                child_clone.cancelled().await;
            });

            let mut map = monitors.lock().await;
            map.insert(
                format!("run-{}", i),
                MonitorHandle { join, cancel: child },
            );
        }

        global_cancel.cancel();
        tokio::task::yield_now().await;

        for (i, token) in child_tokens.iter().enumerate() {
            assert!(
                token.is_cancelled(),
                "child token {} should be cancelled when global fires",
                i
            );
        }
    }

    #[tokio::test]
    async fn panic_marks_run_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        std::fs::create_dir_all(tmp.path().join("runs")).unwrap();

        let (registry, run_id) = setup_running_entry(&tmp);

        // Simulate what the outcome handler does on panic
        {
            let mut reg = registry.lock().await;
            let _ = reg.update_status(
                &run_id,
                RunStatus::Failed,
                None,
                Some("Monitor panicked — marking failed".to_string()),
            );
        }

        let reg = registry.lock().await;
        let entry = reg.get(&run_id).expect("entry should exist");
        assert_eq!(entry.status, RunStatus::Failed);
        assert!(
            entry.error_message.as_deref().unwrap_or("").contains("panicked"),
            "error message should mention panic"
        );

        unsafe { std::env::remove_var("GW_HOME") };
    }

    #[tokio::test]
    async fn fatal_error_no_retry() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        std::fs::create_dir_all(tmp.path().join("runs")).unwrap();

        let (registry, run_id) = setup_running_entry(&tmp);

        // Fatal error → immediate failure, no retry
        {
            let mut reg = registry.lock().await;
            let _ = reg.update_status(
                &run_id,
                RunStatus::Failed,
                None,
                Some("Fatal AuthError: invalid API key".to_string()),
            );
        }

        let reg = registry.lock().await;
        let entry = reg.get(&run_id).expect("entry should exist");
        assert_eq!(entry.status, RunStatus::Failed);
        assert_eq!(
            entry.crash_restarts, 0,
            "fatal errors should not increment crash_restarts"
        );

        unsafe { std::env::remove_var("GW_HOME") };
    }

    #[tokio::test]
    async fn completed_marks_succeeded() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        std::fs::create_dir_all(tmp.path().join("runs")).unwrap();

        let (registry, run_id) = setup_running_entry(&tmp);

        // Completed outcome
        {
            let mut reg = registry.lock().await;
            let _ = reg.update_status(
                &run_id,
                RunStatus::Succeeded,
                Some("complete".to_string()),
                None,
            );
        }

        let reg = registry.lock().await;
        let entry = reg.get(&run_id).expect("entry should exist");
        assert_eq!(entry.status, RunStatus::Succeeded);

        unsafe { std::env::remove_var("GW_HOME") };
    }

    #[tokio::test]
    async fn cancelled_no_registry_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (registry, run_id) = setup_running_entry(&tmp);

        // Cancelled outcome: no registry change (caller handles it)
        let reg = registry.lock().await;
        let entry = reg.get(&run_id).expect("entry should exist");
        assert_eq!(entry.status, RunStatus::Running);
    }

    #[tokio::test]
    #[ignore] // Requires tmux
    async fn monitor_spawned_for_running_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        std::fs::create_dir_all(tmp.path().join("runs")).unwrap();

        let (registry, _run_id) = setup_running_entry(&tmp);

        let cancel = CancellationToken::new();
        let hb = HeartbeatMonitor::new(Arc::clone(&registry), cancel.clone(), Arc::new(RwLock::new(NetworkState::default())));
        let monitors = hb.monitors();

        let _handle = hb.start();
        tokio::time::sleep(std::time::Duration::from_secs(12)).await;

        let map = monitors.lock().await;
        assert!(!map.is_empty(), "monitor should be spawned for running entry");

        cancel.cancel();
        unsafe { std::env::remove_var("GW_HOME") };
    }

    // ── phase_from_pane_heuristic tests ──

    #[test]
    fn heuristic_matches_arc_system_message() {
        let pane = "some startup output\n\
                     Arc phase loop — executing phase: forge (iteration 1)\n\
                     Loading plan...";
        assert_eq!(phase_from_pane_heuristic(pane), Some("forge".to_string()));
    }

    #[test]
    fn heuristic_picks_most_recent_phase() {
        let pane = "Arc phase loop — executing phase: forge (iteration 1)\n\
                     ...forge output...\n\
                     Arc phase loop — executing phase: plan_review (iteration 2)\n\
                     reviewing...";
        assert_eq!(phase_from_pane_heuristic(pane), Some("plan_review".to_string()));
    }

    #[test]
    fn heuristic_no_false_positive_on_work_keyword() {
        // "work" appears in casual text but there's no arc system message
        let pane = "Starting work on plan...\n\
                     Working directory: /tmp/foo\n\
                     Network request completed";
        assert_eq!(phase_from_pane_heuristic(pane), None);
    }

    #[test]
    fn heuristic_falls_back_to_phase_n_markers() {
        let pane = "some output\nphase_3 starting\nmore output";
        assert_eq!(phase_from_pane_heuristic(pane), Some("review".to_string()));
    }

    #[test]
    fn heuristic_returns_none_on_empty() {
        assert_eq!(phase_from_pane_heuristic(""), None);
    }

}
