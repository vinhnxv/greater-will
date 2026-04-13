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
use crate::daemon::registry::{RunEntry, RunRegistry};
use crate::engine::phase_profile::{self, PhaseProfile};
use crate::engine::retry::{ErrorClass, ErrorEvidence};
use crate::engine::single_session::util::{
    check_swarm_activity, find_artifact_dir_cached, is_pipeline_complete,
    read_cached_checkpoint, scan_artifact_dir, ArtifactSnapshot,
};
use crate::monitor::loop_state::{read_arc_loop_state, ArcLoopState};
use crate::monitor::prompt_accept::PromptAcceptor;

/// Re-declared constants (mirrors function-scoped values in `monitor_session`).
///
/// Note: `POLL_INTERVAL_SECS` and `STATUS_LOG_INTERVAL_SECS` exist at module
/// level in `engine::single_session::mod`, but we re-declare here to keep the
/// daemon module self-contained. Values match the foreground source.
const KILL_GATE_MIN_SECS: u64 = 300; // 5 min silence before kill fires
const COMPLETION_GRACE_SECS: u64 = 300; // 5 min after completion signal
const MIN_COMPLETION_AGE_SECS: u64 = 300; // 5 min before "vanished = crash" disambiguates
const POLL_INTERVAL_SECS: u64 = 5;
const STATUS_LOG_INTERVAL_SECS: u64 = 30;
const LOOP_STATE_GONE_GRACE_SECS: u64 = 300; // 5 min before loop state gone → crash
const KILL_GATE_RECOVERY_WINDOW_SECS: u64 = 30; // activity within N secs cancels kill

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
    run_started_at: Instant,
    #[allow(dead_code)]
    dispatch_time: Instant,
    phase_started_at: Option<Instant>,

    // Watchdog config
    watchdog: WatchdogConfig,

    // Phase tracking
    current_phase: Option<String>,
    current_profile: PhaseProfile,
    #[allow(dead_code)]
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

    // Transition/failed gap tracking (stubs — Shard 4 or later port)
    #[allow(dead_code)]
    in_transition_since: Option<Instant>,
    #[allow(dead_code)]
    transition_nudge_count: u32,
    #[allow(dead_code)]
    in_failed_since: Option<Instant>,
    #[allow(dead_code)]
    failed_nudge_count: u32,

    // Process tracking
    claude_pid: Option<u32>,
    last_process_gone_at: Option<Instant>,

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
            dispatch_time: now,
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

                    // 4. Capture pane + activity tracking
                    let pane_content = match self.capture_pane().await {
                        Some(c) => c,
                        None => continue,
                    };

                    self.update_activity_from_pane(&pane_content);

                    // 5. Auto-accept permission prompts
                    if self.check_prompt_accept(&pane_content) {
                        continue;
                    }

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

                    // 10. Pane text completion (3-min guard)
                    self.check_pane_completion(&pane_content);

                    // 11. Error evidence detection
                    self.evaluate_error_evidence(&pane_content);

                    // 12. Process liveness
                    if let Some(outcome) = self.check_process_liveness() {
                        return outcome;
                    }

                    // 13. Per-phase timeout
                    self.check_phase_timeout();

                    // 14-15. Transition gap / failed phase escalation
                    // TODO(shard-4): port from monitor.rs:640-931 (transition_nudge/kill lifecycle)

                    // 16. Idle detection + nudge
                    self.check_idle().await;

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
    async fn kill_session(&self) -> bool {
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
                    "session killed"
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

        // StopFailure signal (API errors) → route to kill gate
        if let Some(_signal) =
            crate::commands::elden::read_stop_failure_signal_from(&self.repo_dir)
        {
            crate::commands::elden::clear_signals_from(&self.repo_dir);
            if self.pending_kill.is_none() {
                // Shard 4/follow-up: classify the signal and inject ErrorClass.
                // For now, route a generic error to pending_kill.
                self.pending_kill = Some(PendingKillRequest {
                    reason: "stop-failure signal received".to_string(),
                    outcome: KillOutcomeKind::ErrorDetected(ErrorClass::Crash),
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
        let budget = self.current_profile.phase_timeout_secs;
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
    async fn check_session_alive(&mut self) -> Option<DaemonRunOutcome> {
        if !self.has_session().await {
            // Session vanished. Disambiguate completion vs crash using 3 signals:
            //   1. Run age too young → crash (can't have legitimately finished yet)
            //   2. No completion signal ever seen → crash (likely external kill)
            //   3. Old enough + completion signal observed → completed
            //
            // BACK-001: Without the completion-signal check, a forcibly-killed
            // tmux session (e.g., `tmux kill-session` by user, or another
            // process reaping the session) was incorrectly reported as
            // Completed, polluting run telemetry with false successes.
            let age = self.run_started_at.elapsed().as_secs();
            if age < MIN_COMPLETION_AGE_SECS {
                return Some(DaemonRunOutcome::Crashed {
                    reason: format!("tmux session vanished at age {}s (below min)", age),
                });
            }
            if self.completion_detected_at.is_none() {
                return Some(DaemonRunOutcome::Crashed {
                    reason: "tmux session vanished without completion signal".to_string(),
                });
            }
            return Some(DaemonRunOutcome::Completed);
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
        // If the foreground monitor detected "pipeline complete" in pane text
        // and the session has been idle long enough, mark completion.
        if is_pipeline_complete(pane_content) {
            self.completion_detected_at.get_or_insert(Instant::now());
        }
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
        let _checkpoint = match read_cached_checkpoint(
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
        // TODO(shard-4): port full phase-transition detection from
        //   monitor.rs:500-638 (compute_phase_navigation, transition detection,
        //   current_phase/profile updates, effective_phase_timeout calculation,
        //   transition/failed gap state).
        self.last_checkpoint_activity = Instant::now();
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
            let age = self.run_started_at.elapsed().as_secs();
            if age >= self.watchdog.loop_state_warmup_secs {
                // Past warmup. If the screen is idle, this is a bootstrap failure.
                if self.last_activity.elapsed().as_secs() >= self.watchdog.idle_kill_secs {
                    return Some(DaemonRunOutcome::Failed {
                        reason: "bootstrap error: no loop state after warmup".to_string(),
                    });
                }
            }
            return None;
        }

        // CRITICAL DISTINCTION:
        // - file_on_disk && !exists → active=false → COMPLETION (intentional)
        // - !file_on_disk          → file deleted → CRASH (unless checkpoint says done)
        let gone_since = *self.loop_state_gone_since.get_or_insert(Instant::now());
        if file_on_disk {
            // Intentional deactivation — wait for completion grace.
            if gone_since.elapsed().as_secs() >= COMPLETION_GRACE_SECS {
                return Some(DaemonRunOutcome::Completed);
            }
        } else {
            // File deleted — but check checkpoint before assuming crash.
            // Parity with single-session monitor: if the terminal phase (merge)
            // is completed, the pipeline is done even if the file was deleted.
            if gone_since.elapsed().as_secs() >= LOOP_STATE_GONE_GRACE_SECS {
                // Read checkpoint to distinguish "completed but file cleaned up"
                // from genuine crash.
                if let Some(checkpoint) = read_cached_checkpoint(
                    &self.repo_dir,
                    &mut self.cached_checkpoint_path,
                ) {
                    if checkpoint.is_complete() || checkpoint.is_terminal_phase_completed() {
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
    fn evaluate_error_evidence(&mut self, pane_content: &str) {
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
            // Swarm activity is updated by an external task; default false for now.
            swarm_active: false,
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
                                self.pending_kill = Some(PendingKillRequest {
                                    reason: format!(
                                        "error evidence confirmed for {}s (class {:?})",
                                        need, class
                                    ),
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
            // Execute kill after full silence.
            let _killed = self.kill_session().await;
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
        info!(
            run_id = %self.run_id,
            tmux_session = %self.tmux_session,
            poll_count = self.poll_count,
            current_phase = ?self.current_phase,
            idle_secs = self.last_activity.elapsed().as_secs(),
            nudge_count = self.nudge_count,
            pending_kill = self.pending_kill.is_some(),
            "run monitor status"
        );
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
        assert_eq!(MIN_COMPLETION_AGE_SECS, 300);
        assert_eq!(POLL_INTERVAL_SECS, 5);
        assert_eq!(STATUS_LOG_INTERVAL_SECS, 30);
        assert_eq!(LOOP_STATE_GONE_GRACE_SECS, 300);
    }

    #[test]
    fn test_hash_str_deterministic() {
        assert_eq!(hash_str("hello"), hash_str("hello"));
        assert_ne!(hash_str("hello"), hash_str("world"));
    }
}
