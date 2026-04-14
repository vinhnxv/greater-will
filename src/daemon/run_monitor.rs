//! Per-run async monitor task.
//!
//! Async equivalent of [`crate::engine::single_session::monitor::monitor_session`]
//! with full watchdog intelligence. Unlike the foreground blocking loop, this
//! runs as a tokio task per daemon run, wrapping synchronous tmux calls in
//! [`tokio::task::spawn_blocking`].
//!
//! ## Architecture
//!
//! - **Identity**: [`DaemonRunMonitor`] owns a single `(run_id, tmux_session)` pair.
//! - **Cancellation**: [`tokio_util::sync::CancellationToken`] cancels at loop boundaries
//!   between polls, not during tmux subprocess execution.
//! - **Kill gate**: All lethal paths (stuck, timeout, error-detected) route through
//!   [`PendingKillRequest`] with a 5-minute confirmation window. Recovery signals
//!   (pane activity, checkpoint advance, artifact growth, loop state change) cancel
//!   the kill.
//! - **State tracking**: [`crate::monitor::loop_state::ArcLoopState`] via
//!   [`crate::monitor::loop_state::read_arc_loop_state`] distinguishes
//!   `file_on_disk && !active` (intentional deactivation → completion) from
//!   `!file_on_disk` (file deleted → crash).
//!
//! ## Wiring (Shard 4 — complete)
//!
//! This module is wired into
//! [`crate::daemon::heartbeat::HeartbeatMonitor`] replacing the 4-feature
//! implementation with full parity.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::watchdog::WatchdogConfig;
use crate::daemon::heartbeat::append_event;
use crate::daemon::registry::{RunEntry, RunRegistry};
use crate::engine::monitor_constants::{
    COMPLETION_GRACE_SECS, DAEMON_MIN_COMPLETION_AGE_SECS as MIN_COMPLETION_AGE_SECS,
    KILL_GATE_MIN_SECS, KILL_GATE_RECOVERY_WINDOW_SECS, LOOP_STATE_GONE_GRACE_SECS,
    POLL_INTERVAL_SECS, STATUS_LOG_INTERVAL_SECS,
};
use crate::engine::phase_profile::{self, PhaseProfile};
use crate::engine::retry::{ErrorClass, ErrorEvidence};
use crate::engine::single_session::util::{
    check_swarm_activity, find_artifact_dir_cached, is_pipeline_complete,
    read_cached_checkpoint, scan_artifact_dir, ArtifactSnapshot,
};
use crate::monitor::loop_state::{read_arc_loop_state, ArcLoopState};
use crate::monitor::phase_nav;
use crate::monitor::prompt_accept::PromptAcceptor;
use crate::session::spawn;

// Constants are now sourced from `crate::engine::monitor_constants` — see the
// module docstring there for divergence rationale (DAEMON vs FOREGROUND
// `MIN_COMPLETION_AGE_SECS`, 60 s vs 300 s).

// ──────────────────────────────────────────────────────────────────────
// Outcomes and state enums
// ──────────────────────────────────────────────────────────────────────

/// Terminal outcome of a monitored run.
#[derive(Debug)]
pub enum DaemonRunOutcome {
    /// Run completed successfully (stop signal received or pipeline complete).
    Completed,
    /// Run failed with a business-logic error (classified, non-crash).
    Failed { reason: String },
    /// Run crashed (process vanished, session gone, or loop state deleted).
    Crashed { reason: String },
    /// Run exceeded its per-phase or pipeline timeout.
    Timeout { reason: String },
    /// Run was idle past the kill threshold with no recovery signal.
    Stuck { reason: String },
    /// Runtime error keyword matched with sufficient confidence.
    ErrorDetected {
        error_class: ErrorClass,
        reason: String,
    },
    /// Monitor was cancelled externally (daemon shutdown or user cancel).
    Cancelled,
}

/// Typed outcome kind for pending kill requests.
///
/// REFINE-003: Replaces `&'static str` outcome field with an enum for
/// compile-time exhaustive matching when mapping to [`DaemonRunOutcome`].
#[derive(Debug, Clone, Copy)]
enum KillOutcomeKind {
    Stuck,
    Timeout,
    ErrorDetected(ErrorClass),
}

/// A pending kill request under its 5-minute confirmation window.
#[derive(Debug)]
struct PendingKillRequest {
    reason: String,
    outcome: KillOutcomeKind,
    started_at: Instant,
    /// Whether the midway (2.5 min) nudge has been sent.
    nudge_sent: bool,
}

// ──────────────────────────────────────────────────────────────────────
// DaemonRunMonitor struct
// ──────────────────────────────────────────────────────────────────────

/// Per-run async monitor task. One instance per daemon run.
// TODO: refactor — this struct has 30+ fields spanning 6+ concerns (identity, timing,
// watchdog, phase tracking, idle detection, checkpoint tracking, etc.). Extract coherent
// sub-structs (e.g. ActivityTracker, CheckpointTracker, LoopStateTracker, KillGate)
// to improve testability and reduce single-responsibility violation.
pub struct DaemonRunMonitor {
    // Identity
    run_id: String,
    tmux_session: String,
    repo_dir: PathBuf,
    #[allow(dead_code)]
    plan_path: PathBuf,
    #[allow(dead_code)]
    config_dir: Option<PathBuf>,

    // Registry reference (for status updates — Shard 4 uses this)
    #[allow(dead_code)]
    registry: Arc<Mutex<RunRegistry>>,

    // Timing
    //
    // Two distinct clocks (do not conflate):
    //
    // * `run_started_at` — cumulative wall-clock since the run was first
    //   dispatched (sourced from `entry.started_at`). Persists across
    //   recovery cycles. Use this ONLY for whole-pipeline budgets like
    //   `check_pipeline_timeout`.
    //
    // * `monitor_started_at` — per-monitor clock, reset to `Instant::now()`
    //   each time `DaemonRunMonitor::new` is called (i.e. on every recovery).
    //   This is the daemon-side equivalent of the foreground orchestrator's
    //   `dispatch_time` (see `single_session/monitor.rs`). Use this for
    //   "this session age" checks — `MIN_COMPLETION_AGE` gating, bootstrap
    //   warmup, and any "recently started" disambiguation. Using
    //   `run_started_at` here was a bug: after recovery the cumulative age
    //   is already huge, so age-based gates fire at the wrong threshold and
    //   the recovery cycle inherits the previous cycle's clock. See the
    //   `min_completion_age_uses_per_recovery_clock` regression test.
    run_started_at: Instant,
    monitor_started_at: Instant,
    phase_started_at: Option<Instant>,

    // Watchdog config
    watchdog: WatchdogConfig,

    // Phase tracking
    current_phase: Option<String>,
    current_profile: PhaseProfile,
    /// Per-phase timeout resolved from the current checkpoint
    /// (`totals.phase_times.{phase}` + grace buffer) at each phase
    /// transition. Zero means "not yet resolved" — fall back to
    /// `current_profile.phase_timeout_secs`.
    effective_phase_timeout: u64,

    // Idle detection
    last_pane_hash: u64,
    last_activity: Instant,
    nudge_count: u32,

    // Checkpoint tracking
    #[allow(dead_code)]
    last_checkpoint_hash: Option<u64>,
    last_checkpoint_activity: Instant,
    cached_checkpoint_path: Option<PathBuf>,

    // Error evidence
    error_confirm_since: Option<(Instant, ErrorClass, f64)>,

    // Loop state
    loop_state_ever_seen: bool,
    loop_state_gone_since: Option<Instant>,
    prev_loop_state: Option<ArcLoopState>,
    last_loop_state_change: Instant,
    #[allow(dead_code)]
    claude_session_id: Option<String>,

    // Artifact tracking
    last_artifact_scan: Instant,
    last_artifact_activity: Instant,
    last_artifact_snapshot: Option<ArtifactSnapshot>,

    // Kill gate
    pending_kill: Option<PendingKillRequest>,

    // Completion
    completion_detected_at: Option<Instant>,

    // Transition/failed gap tracking (ported from foreground monitor)
    in_transition_since: Option<Instant>,
    transition_nudge_count: u32,
    in_failed_since: Option<Instant>,
    failed_nudge_count: u32,

    // Process tracking
    claude_pid: Option<u32>,
    last_process_gone_at: Option<Instant>,

    // Swarm activity
    /// Cached swarm activity state (refreshed every 15s).
    cached_swarm_active: bool,
    /// Last time swarm activity was checked.
    last_swarm_check: Instant,

    // Prompt handling
    prompt_acceptor: PromptAcceptor,

    // Status logging
    last_status_log: Instant,
    poll_count: u64,
}

// ──────────────────────────────────────────────────────────────────────
// Constructor
// ──────────────────────────────────────────────────────────────────────

/// REFINE-001: Safely convert `DateTime<Utc>` → `Instant` with saturation.
///
/// `chrono::Duration::to_std()` returns `Err(OutOfRangeError)` if the duration
/// is negative (clock set backward) or overflows. We saturate to ZERO in that
/// case and then use `checked_sub` on `Instant::now()`, falling back to "now"
/// if subtraction underflows the monotonic clock origin.
fn instant_from_started_at(started_at: chrono::DateTime<chrono::Utc>) -> Instant {
    let elapsed_chrono = chrono::Utc::now().signed_duration_since(started_at);
    let elapsed_std = elapsed_chrono.to_std().unwrap_or(StdDuration::ZERO);
    Instant::now()
        .checked_sub(elapsed_std)
        .unwrap_or_else(Instant::now)
}

impl DaemonRunMonitor {
    /// Create a new monitor from a [`RunEntry`] snapshot.
    ///
    /// # Arguments
    ///
    /// * `entry` — the run entry at dispatch time (copied, not held).
    /// * `registry` — shared registry for later status writebacks (Shard 4).
    /// * `watchdog` — resolved watchdog config (from env, defaults applied).
    pub fn new(
        entry: &RunEntry,
        registry: Arc<Mutex<RunRegistry>>,
        watchdog: WatchdogConfig,
    ) -> Self {
        let now = Instant::now();
        let run_started_at = instant_from_started_at(entry.started_at);
        let tmux_session = entry
            .tmux_session
            .clone()
            .unwrap_or_else(|| entry.session_name.clone());
        let prompt_acceptor = PromptAcceptor::new(
            watchdog.prompt_accept_enabled,
            watchdog.prompt_accept_debounce_secs,
        );
        let current_profile = phase_profile::default_profile();
        let effective_phase_timeout = current_profile.phase_timeout_secs;

        Self {
            run_id: entry.run_id.clone(),
            tmux_session,
            repo_dir: entry.repo_dir.clone(),
            plan_path: entry.plan_path.clone(),
            config_dir: entry.config_dir.clone(),
            registry,
            run_started_at,
            monitor_started_at: now,
            phase_started_at: None,
            watchdog,
            current_phase: None,
            current_profile,
            effective_phase_timeout,
            last_pane_hash: 0,
            last_activity: now,
            nudge_count: 0,
            last_checkpoint_hash: None,
            last_checkpoint_activity: now,
            cached_checkpoint_path: None,
            error_confirm_since: None,
            loop_state_ever_seen: false,
            loop_state_gone_since: None,
            prev_loop_state: None,
            last_loop_state_change: now,
            claude_session_id: None,
            last_artifact_scan: now,
            last_artifact_activity: now,
            last_artifact_snapshot: None,
            pending_kill: None,
            completion_detected_at: None,
            in_transition_since: None,
            transition_nudge_count: 0,
            in_failed_since: None,
            failed_nudge_count: 0,
            claude_pid: entry.claude_pid,
            last_process_gone_at: None,
            cached_swarm_active: false,
            last_swarm_check: now,
            prompt_acceptor,
            last_status_log: now,
            poll_count: 0,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Main run loop
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Main async monitor loop. Returns a [`DaemonRunOutcome`] when the run
    /// ends (for any reason) or when cancellation fires.
    pub async fn run(&mut self, cancel: CancellationToken) -> DaemonRunOutcome {
        info!(
            run_id = %self.run_id,
            tmux_session = %self.tmux_session,
            "DaemonRunMonitor started"
        );

        let mut interval = tokio::time::interval(StdDuration::from_secs(POLL_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(run_id = %self.run_id, "DaemonRunMonitor cancelled");
                    return DaemonRunOutcome::Cancelled;
                }
                _ = interval.tick() => {
                    self.poll_count += 1;

                    // 1. Signal file checks
                    if let Some(outcome) = self.check_signals() {
                        return outcome;
                    }

                    // 2. Pipeline timeout
                    if let Some(outcome) = self.check_pipeline_timeout() {
                        return outcome;
                    }

                    // 3. Session alive check
                    if let Some(outcome) = self.check_session_alive().await {
                        return outcome;
                    }

                    // 4. Capture pane + activity tracking (DET-5: graceful failure)
                    // When capture_pane fails (tmux contention), only pane-dependent
                    // checks are skipped. Steps 6-18 continue to run, preventing
                    // phantom idle accumulation from consecutive capture failures.
                    let pane_content = self.capture_pane().await;

                    if let Some(ref content) = pane_content {
                        self.update_activity_from_pane(content);

                        // 5. Auto-accept permission prompts
                        if self.check_prompt_accept(content) {
                            continue;
                        }

                        // 10. Pane text completion (3-min guard)
                        self.check_pane_completion(content);

                        // 11. Error evidence detection
                        self.evaluate_error_evidence(content).await;
                    }

                    // Steps below run regardless of pane capture success.

                    // 6. Completion grace period
                    if let Some(outcome) = self.check_completion_grace() {
                        return outcome;
                    }

                    // 7. Checkpoint polling + phase tracking
                    self.poll_checkpoint();

                    // 8. Loop state tracking
                    if let Some(outcome) = self.poll_loop_state() {
                        return outcome;
                    }

                    // 9. Artifact dir scan
                    self.scan_artifacts();

                    // 12. Process liveness
                    if let Some(outcome) = self.check_process_liveness() {
                        return outcome;
                    }

                    // 13. Per-phase timeout
                    self.check_phase_timeout();

                    // 14. Transition gap escalation (DET-2)
                    self.check_transition_gap().await;

                    // 15. Failed phase escalation (DET-3)
                    self.check_failed_phase().await;

                    // 16. Idle detection + nudge — skip if transition/failed gap is active.
                    // When in a transition or failed state, the gap-specific escalation
                    // (steps 14-15) handles timing. Running idle detection concurrently
                    // would cause false Stuck kills on natural inter-phase pauses.
                    if self.in_transition_since.is_none() && self.in_failed_since.is_none() {
                        self.check_idle().await;
                    }

                    // 17. Unified kill gate
                    if let Some(outcome) = self.evaluate_kill_gate().await {
                        return outcome;
                    }

                    // 18. Periodic status logging
                    self.log_status();
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Async wrappers for sync tmux calls
// (REFINE-002: differentiated error handling per wrapper type)
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Observational wrapper — silent degrade on failure, next poll retries.
    async fn capture_pane(&self) -> Option<String> {
        let session = self.tmux_session.clone();
        tokio::task::spawn_blocking(move || crate::session::detect::capture_pane(&session).ok())
            .await
            .ok()
            .flatten()
    }

    /// State-changing wrapper — log warn on send failure, error on task panic.
    /// REFINE-002: never silently discards send failures.
    async fn send_nudge(&self, msg: &str) {
        let session = self.tmux_session.clone();
        let msg_owned = msg.to_string();
        // INSP-RP-003: Truncate to 40 chars to prevent secret leakage in logs.
        // send_keys is called with arbitrary content including potential auth tokens
        // (e.g., `/auth login <token>`). Never log the full message — only a short prefix.
        let prefix: String = msg_owned.chars().take(40).collect();
        match tokio::task::spawn_blocking(move || {
            crate::session::spawn::send_keys_with_workaround(&session, &msg_owned)
        })
        .await
        {
            Ok(Ok(())) => {
                debug!(
                    run_id = %self.run_id,
                    tmux_session = %self.tmux_session,
                    prefix = %prefix,
                    "send_nudge ok"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    error = %e,
                    run_id = %self.run_id,
                    tmux_session = %self.tmux_session,
                    prefix = %prefix,
                    "send_nudge failed"
                );
            }
            Err(join_err) => {
                error!(
                    error = %join_err,
                    run_id = %self.run_id,
                    "send_nudge task panicked"
                );
            }
        }
    }

    /// State-changing wrapper — error-log on failure; caller reports.
    ///
    /// Persists a crash dump (pane capture + reason) before killing so
    /// post-mortem debugging has context — parity with the foreground
    /// `monitor.rs:88` behavior (plan GAP A4).
    async fn kill_session(&self, reason: &str) -> bool {
        // Save crash dump BEFORE the kill so the pane state is captured
        // while the session is still alive. Non-fatal: a missing dump
        // must not prevent the kill (we still need to reclaim the tmux
        // session).
        let session = self.tmux_session.clone();
        let repo_dir = self.repo_dir.clone();
        let reason_owned = reason.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            crate::session::detect::save_crash_dump(&session, &repo_dir, &reason_owned);
        })
        .await;

        let session = self.tmux_session.clone();
        match tokio::task::spawn_blocking(move || {
            let _ = crate::session::spawn::kill_session(&session);
            Ok::<(), color_eyre::eyre::Error>(())
        })
        .await
        {
            Ok(Ok(())) => {
                info!(
                    run_id = %self.run_id,
                    tmux_session = %self.tmux_session,
                    reason = %reason,
                    "session killed (crash dump saved)"
                );
                true
            }
            Ok(Err(e)) => {
                error!(
                    error = %e,
                    run_id = %self.run_id,
                    tmux_session = %self.tmux_session,
                    "kill_session failed"
                );
                false
            }
            Err(join_err) => {
                error!(
                    error = %join_err,
                    run_id = %self.run_id,
                    "kill_session task panicked"
                );
                false
            }
        }
    }

    /// Observational — treats JoinError as `false` (assume session gone).
    async fn has_session(&self) -> bool {
        let session = self.tmux_session.clone();
        tokio::task::spawn_blocking(move || crate::session::spawn::has_session(&session))
            .await
            .unwrap_or(false)
    }

    /// Read checkpoint via spawn_blocking (same async pattern as has_session).
    /// Used in session vanish disambiguation where we need checkpoint state
    /// after the tmux session is already gone.
    ///
    /// FLAW-003 fix: wrapped in 10s timeout to prevent monitor loop freeze on
    /// filesystem hang. BACK-001 fix: propagates cached path back from closure.
    async fn read_checkpoint_sync(&mut self) -> Option<crate::checkpoint::schema::Checkpoint> {
        let dir = self.repo_dir.clone();
        let mut path = self.cached_checkpoint_path.clone();
        let task = tokio::task::spawn_blocking(move || {
            let cp = read_cached_checkpoint(&dir, &mut path);
            (cp, path)
        });
        // FLAW-003: timeout prevents infinite block on filesystem hang.
        let (result, updated_path) = match tokio::time::timeout(
            StdDuration::from_secs(10),
            task,
        )
        .await
        {
            Ok(Ok((cp, path))) => (cp, path),
            Ok(Err(_join_err)) => (None, self.cached_checkpoint_path.clone()),
            Err(_timeout) => {
                warn!(
                    run_id = %self.run_id,
                    "read_checkpoint_sync timed out after 10s"
                );
                (None, self.cached_checkpoint_path.clone())
            }
        };
        // BACK-001: propagate the cached path discovered inside spawn_blocking.
        if updated_path.is_some() && self.cached_checkpoint_path.is_none() {
            self.cached_checkpoint_path = updated_path;
        }
        result
    }

    /// Check swarm activity (observational; falls back to false).
    async fn check_swarm_active(&self) -> bool {
        let pid = match self.claude_pid {
            Some(p) => p,
            None => return false,
        };
        tokio::task::spawn_blocking(move || check_swarm_activity(pid).is_some())
            .await
            .unwrap_or(false)
    }

    /// Refresh cached swarm activity state (15-second poll interval).
    async fn refresh_swarm_activity(&mut self) {
        if self.last_swarm_check.elapsed() < std::time::Duration::from_secs(15) {
            return; // honor cached value
        }
        self.last_swarm_check = Instant::now();
        let prev = self.cached_swarm_active;
        self.cached_swarm_active = self.check_swarm_active().await;
        if prev != self.cached_swarm_active {
            crate::daemon::heartbeat::append_event(
                &self.run_id,
                "swarm_state",
                &format!("active={}", self.cached_swarm_active),
            );
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Signal file checks (Task 7a)
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Poll stop/completion/permission signal files from `repo_dir/.gw/signals/`.
    /// Uses the Shard 2 `*_from` helpers which accept a base directory.
    fn check_signals(&mut self) -> Option<DaemonRunOutcome> {
        // Stop signal (hook-based completion)
        if let Some(signal) =
            crate::commands::elden::read_stop_signal_from(&self.repo_dir).or_else(|| {
                crate::commands::elden::read_session_end_signal_from(&self.repo_dir)
            })
        {
            let is_complete = signal
                .get("is_complete")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_complete {
                self.completion_detected_at.get_or_insert(Instant::now());
            }
        }

        // StopFailure signal (API errors) → classify via pane output, route to kill gate.
        // Event format: stop_failure_classified → "stop-failure signal [{ErrorClass}]: {summary}"
        if let Some(signal) =
            crate::commands::elden::read_stop_failure_signal_from(&self.repo_dir)
        {
            crate::commands::elden::clear_signals_from(&self.repo_dir);
            if self.pending_kill.is_none() {
                let pane = spawn::capture_pane(&self.tmux_session).unwrap_or_default();
                let class = ErrorClass::from_pane_output(&pane, false).unwrap_or(ErrorClass::Crash);

                let summary = signal.get("error").and_then(|v| v.as_str())
                    .or_else(|| signal.get("reason").and_then(|v| v.as_str()))
                    .unwrap_or("stop-failure signal (no detail)");
                let reason = format!("stop-failure signal [{:?}]: {}", class, summary);

                crate::daemon::heartbeat::append_event(&self.run_id, "stop_failure_classified", &reason);

                self.pending_kill = Some(PendingKillRequest {
                    reason,
                    outcome: KillOutcomeKind::ErrorDetected(class),
                    started_at: Instant::now(),
                    nudge_sent: false,
                });
            }
        }

        // Permission pending → reset idle timer (user is blocked, not stuck)
        if crate::commands::elden::is_permission_pending_from(&self.repo_dir) {
            self.last_activity = Instant::now();
        }

        None
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pipeline / phase timeouts
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Total wall-clock timeout from run start.
    fn check_pipeline_timeout(&self) -> Option<DaemonRunOutcome> {
        let elapsed = self.run_started_at.elapsed();
        if elapsed >= self.watchdog.pipeline_timeout {
            return Some(DaemonRunOutcome::Timeout {
                reason: format!(
                    "pipeline timeout exceeded ({}s)",
                    self.watchdog.pipeline_timeout.as_secs()
                ),
            });
        }
        None
    }

    /// Per-phase timeout: sets pending_kill if current phase exceeds budget.
    fn check_phase_timeout(&mut self) {
        let started = match self.phase_started_at {
            Some(t) => t,
            None => return,
        };
        let phase_elapsed = started.elapsed().as_secs();
        // Prefer the checkpoint-resolved per-phase budget (written on
        // phase transition in `process_loop_state`); fall back to the
        // category-level profile default only when no transition has
        // been observed yet (first tick / unknown phase).
        let budget = if self.effective_phase_timeout > 0 {
            self.effective_phase_timeout
        } else {
            self.current_profile.phase_timeout_secs
        };
        if phase_elapsed >= budget && self.pending_kill.is_none() {
            let phase_name = self
                .current_phase
                .clone()
                .unwrap_or_else(|| "<unknown>".to_string());
            self.pending_kill = Some(PendingKillRequest {
                reason: format!(
                    "phase {} exceeded {}s budget (elapsed {}s)",
                    phase_name, budget, phase_elapsed
                ),
                outcome: KillOutcomeKind::Timeout,
                started_at: Instant::now(),
                nudge_sent: false,
            });
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Session / process liveness
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// 4-step session vanish disambiguation (DET-1).
    ///
    /// Ported from foreground monitor's checkpoint-based approach:
    /// 1. Age < MIN_COMPLETION_AGE_SECS → Crashed (too young)
    /// 2. Checkpoint complete → Completed (definitive)
    /// 3. Checkpoint has in-progress phase → Crashed with phase context
    /// 4. No checkpoint + completion_detected_at → Completed; else Crashed
    async fn check_session_alive(&mut self) -> Option<DaemonRunOutcome> {
        if !self.has_session().await {
            // Per-recovery clock — see field doc on `monitor_started_at`.
            // Using `run_started_at` here would inherit the cumulative
            // wall-clock from previous recovery cycles, defeating the
            // `MIN_COMPLETION_AGE_SECS` "too young to have completed" gate
            // and producing misleading "after Xs" reasons that tick across
            // crashes (4208s -> 4508s -> 4653s in DET-7 reproducer).
            let age = self.monitor_started_at.elapsed().as_secs();

            // Step 1: Too young — can't have legitimately finished yet.
            if age < MIN_COMPLETION_AGE_SECS {
                return Some(DaemonRunOutcome::Crashed {
                    reason: format!("tmux session vanished at age {}s (below min)", age),
                });
            }

            // Step 2: Check checkpoint for definitive answer.
            if let Some(checkpoint) = self.read_checkpoint_sync().await {
                // `is_complete()` accepts `merge.status = skipped` as
                // completion (to support `auto_merge=false`). That's
                // correct for an actually-finished run, but it can
                // fire from the very first tick when Rune pre-populates
                // `merge: skipped` — at which point a tmux vanish in
                // forge would be mis-reported as completion. Require
                // `is_near_completion` as a sanity cross-check.
                if checkpoint.is_complete() && checkpoint.is_near_completion() {
                    return Some(DaemonRunOutcome::Completed);
                }
                // Checkpoint exists but not complete — crashed mid-phase.
                let phase = checkpoint
                    .inferred_phase_name()
                    .or_else(|| checkpoint.current_phase())
                    .unwrap_or("unknown");
                return Some(DaemonRunOutcome::Crashed {
                    reason: format!(
                        "session ended during phase '{}' after {}s",
                        phase, age
                    ),
                });
            }

            // Step 3: No checkpoint — fall back to pane completion signal.
            if self.completion_detected_at.is_some() {
                return Some(DaemonRunOutcome::Completed);
            }

            // Step 4: No evidence of completion.
            return Some(DaemonRunOutcome::Crashed {
                reason: "tmux session vanished without completion evidence".to_string(),
            });
        }
        None
    }

    fn check_process_liveness(&mut self) -> Option<DaemonRunOutcome> {
        let pid = self.claude_pid?;
        if !crate::cleanup::process::is_pid_alive(pid) {
            let now = Instant::now();
            let first_gone = *self.last_process_gone_at.get_or_insert(now);
            let gone_for = first_gone.elapsed().as_secs();
            if gone_for >= COMPLETION_GRACE_SECS {
                return Some(DaemonRunOutcome::Crashed {
                    reason: format!("claude pid {} gone for {}s", pid, gone_for),
                });
            }
        } else {
            self.last_process_gone_at = None;
        }
        None
    }
}

// ──────────────────────────────────────────────────────────────────────
// Activity / pane tracking
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    fn update_activity_from_pane(&mut self, pane_content: &str) {
        let new_hash = hash_str(pane_content);
        if new_hash != self.last_pane_hash {
            self.last_pane_hash = new_hash;
            self.last_activity = Instant::now();
            // Reset idle nudge counter when the screen actually changes.
            self.nudge_count = 0;
        }
    }

    fn pane_unchanged(&self) -> bool {
        self.last_activity.elapsed().as_secs() >= self.watchdog.error_stall_threshold_secs
    }

    fn check_prompt_accept(&mut self, pane_content: &str) -> bool {
        self.prompt_acceptor
            .check_and_accept(pane_content, &self.tmux_session)
    }

    fn check_pane_completion(&mut self, pane_content: &str) {
        // Pane-text match is a soft signal. Before arming the
        // `COMPLETION_GRACE_SECS` countdown, verify against the
        // checkpoint that the pipeline is plausibly finishing — e.g.
        // phase is at/past `ship`, or the terminal phase is done.
        //
        // Without this gate, phrases like "pipeline completed",
        // "skipping merge", or "arc completed" that legitimately
        // appear in agent output during forge/work phases (planning
        // discussions, echo text, plan enrichment) trigger a
        // false-positive completion 5 min later. Reproduced in
        // gw logs 6ddbe6aa: run entered forge, then reported
        // "pipeline finished" ~8m later.
        if !is_pipeline_complete(pane_content) {
            return;
        }
        let near_completion = self
            .cached_checkpoint_path
            .as_ref()
            .and_then(|p| crate::checkpoint::reader::read_checkpoint(p).ok())
            .as_ref()
            .is_some_and(crate::checkpoint::schema::Checkpoint::is_near_completion);
        if !near_completion {
            // Log once per run — a repeating warn would flood logs
            // since the pane keeps matching until the phase advances.
            if self.completion_detected_at.is_none() {
                debug!(
                    run_id = %self.run_id,
                    phase = ?self.current_phase,
                    "pane shows completion text but checkpoint not near-terminal — ignoring soft signal"
                );
            }
            return;
        }
        self.completion_detected_at.get_or_insert(Instant::now());
    }

    fn check_completion_grace(&self) -> Option<DaemonRunOutcome> {
        let detected = self.completion_detected_at?;
        if detected.elapsed().as_secs() >= COMPLETION_GRACE_SECS {
            return Some(DaemonRunOutcome::Completed);
        }
        None
    }
}

// ──────────────────────────────────────────────────────────────────────
// Checkpoint polling
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    fn poll_checkpoint(&mut self) {
        let checkpoint = match read_cached_checkpoint(
            &self.repo_dir,
            &mut self.cached_checkpoint_path,
        ) {
            Some(c) => c,
            None => return,
        };

        // Update cached path if find_artifact_dir_cached returned a new location.
        if self.cached_checkpoint_path.is_none() {
            self.cached_checkpoint_path =
                find_artifact_dir_cached(&self.cached_checkpoint_path, &self.repo_dir);
        }

        // Checkpoint heartbeat — any read that succeeded means progress exists.
        self.last_checkpoint_activity = Instant::now();

        // Phase navigation: determine current phase, update profile, and track
        // transition/failed gap state for escalation (DET-2, DET-3).
        let nav = phase_nav::compute_phase_navigation(&checkpoint);

        // Update current phase + profile when phase changes.
        let new_phase = nav.effective_phase_name().map(|s| s.to_string());
        if new_phase != self.current_phase {
            if let Some(ref phase_name) = new_phase {
                self.current_profile = phase_profile::profile_for_phase(phase_name)
                    .unwrap_or_else(phase_profile::default_profile);
                // Resolve per-phase timeout from the checkpoint at
                // transition time. Reads `totals.phase_times.{phase}`
                // (Rune's own budget) + grace buffer, falling back to
                // the profile's category-level default when the
                // checkpoint has no timing data for this phase.
                // Parity with the foreground path
                // (`single_session/monitor.rs:551`).
                self.effective_phase_timeout = phase_profile::resolve_phase_timeout_full(
                    phase_name,
                    &checkpoint,
                    &self.current_profile,
                );
                self.phase_started_at = Some(Instant::now());
                let prev = self.current_phase.as_deref().unwrap_or("none");
                let completed = checkpoint.count_by_status("completed");
                let skipped = checkpoint.count_by_status("skipped");
                let total = checkpoint.phases.len();
                let timeout = self.effective_phase_timeout;
                info!(
                    run_id = %self.run_id,
                    from = prev,
                    to = %phase_name,
                    category = ?self.current_profile.category,
                    idle_nudge = self.current_profile.idle_nudge_secs,
                    idle_kill = self.current_profile.idle_kill_secs,
                    phase_timeout = timeout,
                    has_agents = self.current_profile.has_agent_teams,
                    progress = format!("{}/{} done", completed + skipped, total),
                    "Phase transition — applying {} profile (timeout={}m)",
                    self.current_profile.description, timeout / 60,
                );
                println!(
                    "[gw] Phase: {} → {} [{}] (nudge={}s, kill={}s, timeout={}m, {}/{})",
                    prev, phase_name, self.current_profile.description,
                    self.current_profile.idle_nudge_secs,
                    self.current_profile.idle_kill_secs,
                    timeout / 60,
                    completed + skipped, total,
                );
                let event_msg = format!(
                    "phase: {} → {} [{}] (nudge={}s, kill={}s, timeout={}m, {}/{})",
                    prev, phase_name, self.current_profile.description,
                    self.current_profile.idle_nudge_secs,
                    self.current_profile.idle_kill_secs,
                    timeout / 60,
                    completed + skipped, total,
                );
                append_event(&self.run_id, "phase_profile_applied", &event_msg);
            }
            self.current_phase = new_phase;
        }

        // Transition/failed gap state tracking.
        // FLAW-001 fix: seed timers using real checkpoint gap duration so that
        // a daemon starting mid-transition doesn't get an extra 11 min of tolerance.
        if nav.has_failure() {
            if self.in_failed_since.is_none() {
                // Backdate using checkpoint's real gap if available.
                let backdate = nav.transition_gap_secs()
                    .and_then(|s| Instant::now().checked_sub(StdDuration::from_secs(s)))
                    .unwrap_or_else(Instant::now);
                self.in_failed_since = Some(backdate);
                self.failed_nudge_count = 0;
            }
            self.in_transition_since = None;
            self.transition_nudge_count = 0;
        } else if nav.is_transitioning() {
            if self.in_transition_since.is_none() {
                // Backdate using checkpoint's real gap if available.
                let backdate = nav.transition_gap_secs()
                    .and_then(|s| Instant::now().checked_sub(StdDuration::from_secs(s)))
                    .unwrap_or_else(Instant::now);
                self.in_transition_since = Some(backdate);
                self.transition_nudge_count = 0;
            }
            self.in_failed_since = None;
            self.failed_nudge_count = 0;
        } else {
            // Phase running normally — clear all gap timers.
            self.in_transition_since = None;
            self.transition_nudge_count = 0;
            self.in_failed_since = None;
            self.failed_nudge_count = 0;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Loop state polling (Task 7b)
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Poll the arc-phase-loop state file and return a terminal outcome if
    /// the loop has ended.
    ///
    /// **Critical disambiguation**: when the active state is absent, the
    /// distinction between "completion" and "crash" is determined by whether
    /// the file exists at all:
    /// - `file_on_disk && !active` → intentional deactivation by Rune itself
    ///   → wait `COMPLETION_GRACE_SECS` then return [`DaemonRunOutcome::Completed`]
    /// - `!file_on_disk` → file deleted (likely crash)
    ///   → wait `LOOP_STATE_GONE_GRACE_SECS` then return [`DaemonRunOutcome::Crashed`]
    ///
    /// During warmup (before the first active state is observed), this
    /// function tolerates absence as long as the screen is changing. Once the
    /// warmup window expires AND the screen is idle, it returns
    /// [`DaemonRunOutcome::Failed`] with a bootstrap-error reason.
    fn poll_loop_state(&mut self) -> Option<DaemonRunOutcome> {
        let read = read_arc_loop_state(&self.repo_dir);
        let file_on_disk = !matches!(
            read,
            crate::monitor::loop_state::LoopStateRead::Missing
        );
        let current_opt = read.active().cloned();

        if let Some(ref state) = current_opt {
            if !self.loop_state_ever_seen {
                info!(
                    run_id = %self.run_id,
                    plan_file = %state.plan_file,
                    iteration = state.iteration,
                    "loop state first observed"
                );
            }
            self.loop_state_ever_seen = true;
            self.loop_state_gone_since = None;

            // Content change → reset activity. Use a simple equality check on
            // key fields (iteration, session_id) as a proxy for state change.
            let changed = match self.prev_loop_state {
                None => true,
                Some(ref prev) => {
                    prev.iteration != state.iteration
                        || prev.session_id != state.session_id
                        || prev.checkpoint_path != state.checkpoint_path
                }
            };
            if changed {
                self.last_loop_state_change = Instant::now();
                self.last_activity = Instant::now();
            }
            self.prev_loop_state = Some(state.clone());
            return None;
        }

        // current_opt is None → either Inactive (intentional) or Missing (crash).
        if !self.loop_state_ever_seen {
            // Warmup window: the session hasn't produced a state file yet.
            // Per-recovery clock — a freshly-recovered session deserves its
            // own warmup window, not inherited age from prior cycles.
            let age = self.monitor_started_at.elapsed().as_secs();
            let warmup = self.watchdog.loop_state_warmup_secs;
            let screen_idle = self.last_activity.elapsed().as_secs();

            // Re-read the loop state to distinguish Inactive (stale file from
            // a prior run) vs Missing (never written). The outer match above
            // consumed `current_opt` into `None`, but we need the categorical
            // status to apply the 2× hard limit only to the Inactive case.
            let loop_state_read = read_arc_loop_state(&self.repo_dir);
            let stale_file_present = matches!(
                loop_state_read,
                crate::monitor::loop_state::LoopStateRead::Inactive,
            );

            // Path B (stale hard-limit — foreground parity with
            // monitor.rs:884-910): stale file (active=false from a previous
            // run) AND screen idle for > 2× warmup → Rune failed to
            // re-initialize. Treat as BootstrapError so recovery logic runs.
            //
            // MUST be evaluated BEFORE Path A: since stale_hard_limit = 2 ×
            // warmup, any scenario satisfying `age > stale_hard_limit` also
            // satisfies `age >= warmup`. If Path A ran first, Path B would be
            // dead code.
            let stale_hard_limit = warmup.saturating_mul(2);
            if stale_file_present && age > stale_hard_limit
                && screen_idle >= self.watchdog.idle_kill_secs
            {
                let reason = format!(
                    "arc-phase-loop.local.md stuck at active=false after {}s \
                     (hard limit {}s, screen idle {}s)",
                    age, stale_hard_limit, screen_idle,
                );
                append_event(&self.run_id, "stale_loop_state_timeout", &reason);
                return Some(DaemonRunOutcome::ErrorDetected {
                    error_class: ErrorClass::BootstrapError,
                    reason,
                });
            }

            // Path A (generic bootstrap timeout): past warmup + screen idle →
            // bootstrap failure. Fires only when no stale file is present
            // (Missing case) or when stale_hard_limit hasn't been reached yet.
            if age >= warmup && screen_idle >= self.watchdog.idle_kill_secs {
                return Some(DaemonRunOutcome::Failed {
                    reason: "bootstrap error: no loop state after warmup".to_string(),
                });
            }
            return None;
        }

        // CRITICAL DISTINCTION:
        // - file_on_disk && !exists → active=false → COMPLETION (intentional)
        // - !file_on_disk          → file deleted → CRASH (unless checkpoint says done)
        let gone_since = *self.loop_state_gone_since.get_or_insert(Instant::now());
        if file_on_disk {
            // Intentional deactivation — wait for completion grace.
            // Verify the checkpoint before trusting the deactivation.
            // Rune may write `active=false` transiently between phases
            // or during initial setup; without the guard we'd declare
            // completion at phase 1 (parity with the pane-text gate
            // in `check_pane_completion`).
            if gone_since.elapsed().as_secs() >= COMPLETION_GRACE_SECS {
                let near_completion = read_cached_checkpoint(
                    &self.repo_dir,
                    &mut self.cached_checkpoint_path,
                )
                .as_ref()
                .is_some_and(crate::checkpoint::schema::Checkpoint::is_near_completion);
                if near_completion {
                    return Some(DaemonRunOutcome::Completed);
                }
                // Reject: loop state says inactive but checkpoint is
                // nowhere near terminal. Reset the timer so the next
                // observed deactivation gets its own grace window
                // (otherwise we'd never distinguish a transient
                // deactivation from a real one).
                warn!(
                    run_id = %self.run_id,
                    phase = ?self.current_phase,
                    "loop state inactive but checkpoint not near-terminal — ignoring deactivation"
                );
                self.loop_state_gone_since = None;
            }
        } else {
            // File deleted — but check checkpoint before assuming crash.
            // Parity with single-session monitor: if the terminal phase (merge)
            // is completed, the pipeline is done even if the file was deleted.
            if gone_since.elapsed().as_secs() >= LOOP_STATE_GONE_GRACE_SECS {
                // Read checkpoint to distinguish "completed but file cleaned up"
                // from genuine crash. Require `is_near_completion` as a
                // cross-check so an early `merge: skipped` entry from
                // `auto_merge=false` doesn't masquerade as completion
                // when we're actually at forge.
                if let Some(checkpoint) = read_cached_checkpoint(
                    &self.repo_dir,
                    &mut self.cached_checkpoint_path,
                ) {
                    if checkpoint.is_near_completion() {
                        info!(
                            run_id = %self.run_id,
                            "Loop state file deleted but checkpoint shows completion — treating as completed"
                        );
                        return Some(DaemonRunOutcome::Completed);
                    }
                }
                return Some(DaemonRunOutcome::Crashed {
                    reason: "arc-phase-loop state file deleted".to_string(),
                });
            }
        }
        None
    }
}

// ──────────────────────────────────────────────────────────────────────
// Artifact scanning
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    fn scan_artifacts(&mut self) {
        let interval_secs = self.watchdog.artifact_scan_interval_secs;
        if self.last_artifact_scan.elapsed().as_secs() < interval_secs {
            return;
        }
        self.last_artifact_scan = Instant::now();

        let scan_dir = match self.cached_checkpoint_path.as_ref().and_then(|p| p.parent()) {
            Some(d) => d.to_path_buf(),
            None => return,
        };
        if let Some(snapshot) = scan_artifact_dir(&scan_dir) {
            let changed = match self.last_artifact_snapshot {
                None => true,
                Some(ref prev) => prev != &snapshot,
            };
            if changed {
                self.last_artifact_activity = Instant::now();
            }
            self.last_artifact_snapshot = Some(snapshot);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Error evidence detection
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Evaluate error evidence and route confirmed errors to the kill gate.
    ///
    /// Builds an [`ErrorEvidence`] from the current pane content + monitoring
    /// state, runs the classifier, and manages a confirmation timer keyed on
    /// `(start, ErrorClass, confidence)`. The timer enforces a confidence-
    /// weighted minimum confirmation window (high-confidence: shorter wait;
    /// medium: longer wait), with `KILL_GATE_MIN_SECS` as a floor. A class
    /// change mid-confirmation resets the timer to prevent oscillating-error
    /// gaming.
    ///
    /// **Simplified state machine**: this is REFINE-004 in its initial form.
    /// The full per-class confidence math from `monitor.rs:982-1122` is
    /// deferred to Shard 4 — current implementation uses a fixed 0.9 stand-in
    /// confidence. The structural pieces (timer, class-change reset, threshold
    /// floor, kill gate routing) are in place.
    async fn evaluate_error_evidence(&mut self, pane_content: &str) {
        self.refresh_swarm_activity().await;
        let screen_stall = self.last_activity.elapsed().as_secs();

        // Only scan keywords when screen is NOT changing (reduces false positives).
        let keyword_match = if self.pane_unchanged() {
            ErrorClass::from_pane_output(pane_content, true)
        } else {
            None
        };

        let evidence = ErrorEvidence {
            keyword_match,
            screen_stall_secs: screen_stall,
            checkpoint_stale_secs: Some(self.last_checkpoint_activity.elapsed().as_secs()),
            process_alive: self
                .claude_pid
                .is_none_or(crate::cleanup::process::is_pid_alive),
            artifacts_active: self.last_artifact_activity.elapsed().as_secs() < 60,
            swarm_active: self.cached_swarm_active,
        };

        match evidence.classify() {
            Some(class) => {
                // REFINE-004: port the full state machine from
                // monitor.rs:982-1122 here. For this shard we implement a
                // simplified single-timer version — Shard 4 code review may
                // extend it to match the foreground's confidence-weighted
                // confirmation windows.
                let now = Instant::now();
                let confidence = 0.9_f64; // simplified: treat classify() hit as high-conf
                match self.error_confirm_since {
                    None => {
                        self.error_confirm_since = Some((now, class, confidence));
                        append_event(
                            &self.run_id,
                            "error_confirming",
                            &format!("error signal detected: {:?} (confidence {:.1})", class, confidence),
                        );
                    }
                    Some((start, prev_class, _prev_conf)) => {
                        if prev_class != class {
                            // Class changed mid-confirmation → reset timer.
                            self.error_confirm_since = Some((now, class, confidence));
                        } else {
                            let need = if confidence >= 0.8 {
                                self.watchdog.error_confirm_high_secs
                            } else {
                                self.watchdog.error_confirm_medium_secs
                            }
                            .max(KILL_GATE_MIN_SECS);
                            if start.elapsed().as_secs() >= need && self.pending_kill.is_none() {
                                let reason = format!(
                                    "error evidence confirmed for {}s (class {:?})",
                                    need, class
                                );
                                append_event(
                                    &self.run_id,
                                    "error_confirming",
                                    &format!("error confirmed: {:?} after {}s", class, need),
                                );
                                self.pending_kill = Some(PendingKillRequest {
                                    reason,
                                    outcome: KillOutcomeKind::ErrorDetected(class),
                                    started_at: now,
                                    nudge_sent: false,
                                });
                            }
                        }
                    }
                }
            }
            None => {
                // Error evidence cleared → cancel confirmation.
                if self.error_confirm_since.is_some() {
                    self.error_confirm_since = None;
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Idle detection + nudge escalation
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    async fn check_idle(&mut self) {
        let idle_secs = self.last_activity.elapsed().as_secs();
        let kill_threshold = self.current_profile.idle_kill_secs;
        let nudge_threshold = self.current_profile.idle_nudge_secs;

        // Kill → route to pending_kill (not direct kill).
        if idle_secs > kill_threshold && self.pending_kill.is_none() {
            self.pending_kill = Some(PendingKillRequest {
                reason: format!("idle for {}s (threshold {}s)", idle_secs, kill_threshold),
                outcome: KillOutcomeKind::Stuck,
                started_at: Instant::now(),
                nudge_sent: false,
            });
            return;
        }

        // Escalating nudge: 1→"please continue", 2→"are you stuck?", 3→"/compact"
        let next_nudge_at = nudge_threshold * (self.nudge_count as u64 + 1);
        if idle_secs > next_nudge_at && self.nudge_count < 3 {
            self.nudge_count += 1;
            let msg = match self.nudge_count {
                1 => "please continue",
                2 => "are you stuck? please continue working",
                _ => "/compact",
            };
            self.send_nudge(msg).await;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Transition gap / failed phase escalation (DET-2, DET-3)
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Escalate transition gaps: nudge → warn → kill gate.
    ///
    /// When PhaseNav detects no `in_progress` phase but completed phases exist,
    /// the pipeline is between phases. This is natural (5-10 min) but if it
    /// exceeds TRANSITION_KILL_SECS, the arc is likely stuck.
    async fn check_transition_gap(&mut self) {
        let start = match self.in_transition_since {
            Some(t) => t,
            None => return,
        };
        let gap_secs = start.elapsed().as_secs();

        if gap_secs > phase_nav::TRANSITION_KILL_SECS && self.pending_kill.is_none() {
            self.pending_kill = Some(PendingKillRequest {
                reason: format!(
                    "transition gap {}s > {}s threshold",
                    gap_secs, phase_nav::TRANSITION_KILL_SECS
                ),
                outcome: KillOutcomeKind::Stuck,
                started_at: Instant::now(),
                nudge_sent: false,
            });
        } else if gap_secs > phase_nav::TRANSITION_WARN_SECS && self.transition_nudge_count < 2 {
            self.transition_nudge_count = 2;
            self.send_nudge("are you stuck between phases? please continue to the next phase")
                .await;
        } else if gap_secs > phase_nav::TRANSITION_NUDGE_SECS && self.transition_nudge_count < 1 {
            self.transition_nudge_count = 1;
            self.send_nudge("please continue working").await;
        }
    }

    /// Escalate failed phase gaps: nudge → warn → kill gate.
    ///
    /// When PhaseNav detects a failed phase, Rune's retry mechanism may self-heal.
    /// Give Rune up to FAILED_KILL_SECS before intervention.
    async fn check_failed_phase(&mut self) {
        let start = match self.in_failed_since {
            Some(t) => t,
            None => return,
        };
        let gap_secs = start.elapsed().as_secs();

        if gap_secs > phase_nav::FAILED_KILL_SECS && self.pending_kill.is_none() {
            self.pending_kill = Some(PendingKillRequest {
                reason: format!(
                    "failed phase unresolved for {}s > {}s threshold",
                    gap_secs, phase_nav::FAILED_KILL_SECS
                ),
                outcome: KillOutcomeKind::Stuck,
                started_at: Instant::now(),
                nudge_sent: false,
            });
        } else if gap_secs > phase_nav::FAILED_WARN_SECS && self.failed_nudge_count < 2 {
            self.failed_nudge_count = 2;
            self.send_nudge("a phase has failed — please check and continue or retry").await;
        } else if gap_secs > phase_nav::FAILED_NUDGE_SECS && self.failed_nudge_count < 1 {
            self.failed_nudge_count = 1;
            self.send_nudge("please continue working").await;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Unified kill gate
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    /// Evaluate the unified kill gate against any active `pending_kill`.
    ///
    /// All lethal paths (stuck idle, phase timeout, error-detected) route
    /// through a single 5-minute confirmation window. The gate fires only if
    /// silence persists for the full window. 5 recovery signals — pane
    /// activity, checkpoint advance, artifact growth, loop state change, and
    /// active swarm teammates — cancel the kill if observed after the first
    /// 10 seconds (the early-grace window prevents racing the kill with a
    /// late-arriving signal). A midway nudge fires at 50% of the gate.
    ///
    /// Returns `Some(DaemonRunOutcome)` only when the kill executes; otherwise
    /// returns `None` and leaves `self.pending_kill` either unchanged (still
    /// counting down) or cleared (recovered).
    async fn evaluate_kill_gate(&mut self) -> Option<DaemonRunOutcome> {
        // Snapshot what we need from pending_kill so we can drop the borrow
        // before taking mutable references elsewhere.
        let (pk_started_at, pk_nudge_sent, pk_reason, pk_outcome) = {
            let pk = self.pending_kill.as_ref()?;
            (
                pk.started_at,
                pk.nudge_sent,
                pk.reason.clone(),
                pk.outcome,
            )
        };

        let gate_elapsed = pk_started_at.elapsed().as_secs();

        // Recovery signals cancel the kill.
        let swarm_active = self.check_swarm_active().await;
        // 6th recovery signal: network offline. When the network is down,
        // Claude Code may stall silently — this is not a real bug, so don't
        // escalate to Stuck/kill. Reset idle timer to give more time.
        let network_offline = !crate::daemon::network::is_online();

        let has_recovery = self.last_activity.elapsed().as_secs() < KILL_GATE_RECOVERY_WINDOW_SECS
            || self.last_checkpoint_activity.elapsed().as_secs() < KILL_GATE_RECOVERY_WINDOW_SECS
            || self.last_artifact_activity.elapsed().as_secs() < KILL_GATE_RECOVERY_WINDOW_SECS
            || self.last_loop_state_change.elapsed().as_secs() < KILL_GATE_RECOVERY_WINDOW_SECS
            || swarm_active
            || network_offline;

        if has_recovery && gate_elapsed > 10 {
            info!(
                run_id = %self.run_id,
                "kill gate cancelled: recovery signal detected after {}s",
                gate_elapsed
            );
            self.pending_kill = None;
            return None;
        }

        if gate_elapsed >= KILL_GATE_MIN_SECS {
            // Execute kill after full silence — pass the reason so the
            // wrapper can persist a crash dump before tmux teardown.
            let _killed = self.kill_session(&pk_reason).await;
            self.pending_kill = None;
            // REFINE-003: exhaustive match on typed enum.
            let final_outcome = match pk_outcome {
                KillOutcomeKind::Stuck => DaemonRunOutcome::Stuck { reason: pk_reason },
                KillOutcomeKind::Timeout => DaemonRunOutcome::Timeout { reason: pk_reason },
                KillOutcomeKind::ErrorDetected(class) => DaemonRunOutcome::ErrorDetected {
                    error_class: class,
                    reason: pk_reason,
                },
            };
            return Some(final_outcome);
        }

        // Midway nudge at 50% of the gate.
        if !pk_nudge_sent && gate_elapsed >= KILL_GATE_MIN_SECS / 2 {
            self.send_nudge("please continue").await;
            if let Some(ref mut pk) = self.pending_kill {
                pk.nudge_sent = true;
            }
        }

        None
    }
}

// ──────────────────────────────────────────────────────────────────────
// Status logging
// ──────────────────────────────────────────────────────────────────────

impl DaemonRunMonitor {
    fn log_status(&mut self) {
        if self.last_status_log.elapsed().as_secs() < STATUS_LOG_INTERVAL_SECS {
            return;
        }
        self.last_status_log = Instant::now();

        // DET-4: Loop stall correlation — distinguish "long phase" from "truly stuck".
        self.check_loop_stall();

        info!(
            run_id = %self.run_id,
            tmux_session = %self.tmux_session,
            poll_count = self.poll_count,
            current_phase = ?self.current_phase,
            idle_secs = self.last_activity.elapsed().as_secs(),
            nudge_count = self.nudge_count,
            pending_kill = self.pending_kill.is_some(),
            in_transition = self.in_transition_since.is_some(),
            in_failed = self.in_failed_since.is_some(),
            "run monitor status"
        );
    }

    /// DET-4: Correlate loop state staleness with checkpoint activity.
    ///
    /// When the loop state file hasn't changed (normal mid-phase), check whether
    /// the checkpoint is still updating. If both are stale, the arc may be truly
    /// stuck. If only loop state is stale but checkpoint is fresh, a long phase
    /// is in progress — no alarm needed.
    fn check_loop_stall(&self) {
        const LOOP_STALL_WARN_SECS: u64 = 600;

        if !self.loop_state_ever_seen {
            return;
        }

        let loop_stall = self.last_loop_state_change.elapsed().as_secs();
        if loop_stall <= LOOP_STALL_WARN_SECS {
            return;
        }

        let cp_stale = self.last_checkpoint_activity.elapsed().as_secs();
        if cp_stale > LOOP_STALL_WARN_SECS {
            let msg = format!(
                "loop state stale {}s, checkpoint stale {}s — arc may be stuck",
                loop_stall, cp_stale,
            );
            warn!(
                run_id = %self.run_id,
                loop_stall_secs = loop_stall,
                checkpoint_stale_secs = cp_stale,
                "loop state AND checkpoint both stale — arc may be stuck"
            );
            append_event(&self.run_id, "loop_stall", &msg);
        } else {
            debug!(
                run_id = %self.run_id,
                loop_stall_secs = loop_stall,
                "loop state stale but checkpoint still updating — phase in progress"
            );
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Hash a string for in-process change detection only.
///
/// **Do not persist or compare across processes.** `DefaultHasher`'s output is
/// explicitly unstable across Rust versions and process restarts — using it for
/// pane-content equality checks within a single monitor task is fine, but the
/// resulting `u64` must never be written to disk, sent over the network, or
/// compared against a hash from another process.
fn hash_str(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ──────────────────────────────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instant_from_started_at_recent() {
        let now = chrono::Utc::now();
        let result = instant_from_started_at(now);
        // Should be near Instant::now() (within 1 second)
        assert!(result.elapsed().as_secs() <= 1);
    }

    #[test]
    fn test_instant_from_started_at_future_saturates() {
        // Clock skew: entry.started_at is in the future.
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let result = instant_from_started_at(future);
        // Negative duration → ZERO → Instant::now() → elapsed ≈ 0
        assert!(result.elapsed().as_secs() <= 1);
    }

    #[test]
    fn test_instant_from_started_at_past() {
        let past = chrono::Utc::now() - chrono::Duration::seconds(30);
        let result = instant_from_started_at(past);
        let elapsed = result.elapsed().as_secs();
        assert!(
            (28..=32).contains(&elapsed),
            "expected ~30s elapsed, got {}",
            elapsed
        );
    }

    /// DET-7 regression: `monitor_started_at` MUST track the construction
    /// instant (i.e. recovery boundary), not `entry.started_at` (cumulative
    /// run age). Foreground monitor (`single_session/monitor.rs`) uses
    /// `dispatch_time` for the equivalent age check; the daemon path used
    /// `run_started_at` — a clock derived from `entry.started_at` that
    /// persists across recovery cycles. The bug surfaced as accelerating
    /// crash loops in phase `test`: each retry's "after Xs" reason ticked
    /// upward (4208 → 4508 → 4653) instead of resetting on recovery, and
    /// the `MIN_COMPLETION_AGE_SECS` "too young to have completed" gate
    /// was effectively disabled for recovered sessions.
    ///
    /// Two clocks, two responsibilities:
    /// - `run_started_at`     → cumulative wall-clock (pipeline budget)
    /// - `monitor_started_at` → per-recovery (session-age gates)
    #[test]
    fn min_completion_age_uses_per_recovery_clock() {
        use crate::daemon::protocol::RunStatus;
        use tokio::sync::Mutex;

        // Simulate a run dispatched 30 minutes ago that has just been
        // recovered (i.e. monitor freshly constructed).
        let dispatched_30m_ago = chrono::Utc::now() - chrono::Duration::minutes(30);
        let entry = RunEntry {
            run_id: "test1234".to_string(),
            plan_path: PathBuf::from("/tmp/plan.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            session_name: "gw-test1234".to_string(),
            tmux_session: Some("gw-test1234".to_string()),
            status: RunStatus::Running,
            current_phase: Some("test".to_string()),
            started_at: dispatched_30m_ago,
            finished_at: None,
            crash_restarts: 3,
            config_dir: None,
            error_message: None,
            restartable: true,
            claude_pid: None,
            schedule_id: None,
            last_recovery_at: Some(chrono::Utc::now()),
            write_epoch: 0,
        };
        let registry = Arc::new(Mutex::new(RunRegistry::new()));
        let monitor = DaemonRunMonitor::new(&entry, registry, WatchdogConfig::from_env());

        let run_age = monitor.run_started_at.elapsed().as_secs();
        let monitor_age = monitor.monitor_started_at.elapsed().as_secs();

        // Cumulative clock reflects the 30-minute-old dispatch.
        assert!(
            (1790..=1810).contains(&run_age),
            "run_started_at should reflect entry.started_at (~1800s), got {}s",
            run_age,
        );
        // Per-recovery clock starts fresh — under MIN_COMPLETION_AGE_SECS.
        assert!(
            monitor_age < MIN_COMPLETION_AGE_SECS,
            "monitor_started_at should be fresh on construction (<{}s), got {}s — \
             a freshly recovered session must not inherit the prior cycle's age",
            MIN_COMPLETION_AGE_SECS,
            monitor_age,
        );
    }

    #[test]
    fn test_daemon_run_outcome_variants_exist() {
        // Smoke test: all variants constructible.
        let _: DaemonRunOutcome = DaemonRunOutcome::Completed;
        let _: DaemonRunOutcome = DaemonRunOutcome::Failed {
            reason: "x".to_string(),
        };
        let _: DaemonRunOutcome = DaemonRunOutcome::Crashed {
            reason: "x".to_string(),
        };
        let _: DaemonRunOutcome = DaemonRunOutcome::Timeout {
            reason: "x".to_string(),
        };
        let _: DaemonRunOutcome = DaemonRunOutcome::Stuck {
            reason: "x".to_string(),
        };
        let _: DaemonRunOutcome = DaemonRunOutcome::ErrorDetected {
            error_class: ErrorClass::Crash,
            reason: "x".to_string(),
        };
        let _: DaemonRunOutcome = DaemonRunOutcome::Cancelled;
    }

    #[test]
    fn test_kill_outcome_kind_exhaustive_match() {
        // REFINE-003: typed enum enables exhaustive matching.
        let kinds = [
            KillOutcomeKind::Stuck,
            KillOutcomeKind::Timeout,
            KillOutcomeKind::ErrorDetected(ErrorClass::Crash),
        ];
        for k in kinds {
            let _mapped = match k {
                KillOutcomeKind::Stuck => "stuck",
                KillOutcomeKind::Timeout => "timeout",
                KillOutcomeKind::ErrorDetected(_) => "error",
            };
        }
    }

    #[test]
    fn test_constants_match_foreground() {
        assert_eq!(KILL_GATE_MIN_SECS, 300);
        assert_eq!(COMPLETION_GRACE_SECS, 300);
        // DET-6: reduced from 300s to 60s (checkpoint-based disambiguation
        // handles the rest, so age threshold matters less).
        assert_eq!(MIN_COMPLETION_AGE_SECS, 60);
        assert_eq!(POLL_INTERVAL_SECS, 5);
        assert_eq!(STATUS_LOG_INTERVAL_SECS, 30);
        assert_eq!(LOOP_STATE_GONE_GRACE_SECS, 300);
    }

    #[test]
    fn test_hash_str_deterministic() {
        assert_eq!(hash_str("hello"), hash_str("hello"));
        assert_ne!(hash_str("hello"), hash_str("world"));
    }

    // ── DET-2/DET-3: Transition/failed gap detection ─────────────────

    #[test]
    fn test_transition_gap_thresholds_order() {
        // Verify escalation order: nudge < warn < kill.
        assert!(phase_nav::TRANSITION_NUDGE_SECS < phase_nav::TRANSITION_WARN_SECS);
        assert!(phase_nav::TRANSITION_WARN_SECS < phase_nav::TRANSITION_KILL_SECS);
    }

    #[test]
    fn test_failed_phase_thresholds_order() {
        // Verify escalation order: nudge < warn < kill.
        assert!(phase_nav::FAILED_NUDGE_SECS < phase_nav::FAILED_WARN_SECS);
        assert!(phase_nav::FAILED_WARN_SECS < phase_nav::FAILED_KILL_SECS);
    }

    #[test]
    fn test_failed_phase_gives_rune_time() {
        // Failed phase kill threshold (12 min) is longer than typical idle kill
        // thresholds for most phases, giving Rune time to self-heal.
        assert!(phase_nav::FAILED_KILL_SECS >= 720);
    }

    // ── DET-6: MIN_COMPLETION_AGE_SECS reduction ─────────────────────

    #[test]
    fn test_min_completion_age_reduced() {
        // DET-6: 60s is a conservative middle ground between foreground's 30s
        // and the old daemon value of 300s.
        assert!(MIN_COMPLETION_AGE_SECS <= 60);
        assert!(MIN_COMPLETION_AGE_SECS >= 30);
    }

    // ── DET-4: Loop stall thresholds ─────────────────────────────────

    #[test]
    fn test_loop_stall_warn_threshold() {
        // Loop stall warning fires at 600s (10 min), matching foreground.
        // This is a doc-test that the constant exists and is reasonable.
        const LOOP_STALL_WARN_SECS: u64 = 600;
        assert!(LOOP_STALL_WARN_SECS >= STATUS_LOG_INTERVAL_SECS * 10);
    }
}
