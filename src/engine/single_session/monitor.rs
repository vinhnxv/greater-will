//! Session monitoring loop.
//!
//! Contains the core monitor loop that watches a running Claude Code session:
//! - Signal checks (stop, failure, permission)
//! - Checkpoint-based phase tracking
//! - Loop state tracking (arc-phase-loop.local.md)
//! - Error detection with confirmation periods
//! - Idle detection with escalating nudges
//! - Transition/failed gap escalation

use super::util::{
    check_swarm_activity, find_artifact_dir_cached, is_pipeline_complete, read_cached_checkpoint,
    scan_artifact_dir, ArtifactSnapshot,
};
use super::{SingleSessionConfig, SessionOutcome, POLL_INTERVAL_SECS, STATUS_LOG_INTERVAL_SECS};
use crate::cleanup;
use crate::engine::monitor_constants::{
    COMPLETION_GRACE_SECS, FOREGROUND_MIN_COMPLETION_AGE_SECS as MIN_COMPLETION_AGE_SECS,
    KILL_GATE_MIN_SECS, LOOP_STATE_GONE_GRACE_SECS,
};
use crate::engine::retry::{ErrorClass, ErrorEvidence};
use crate::session::detect::{capture_pane, save_crash_dump};
use crate::session::spawn::{
    has_session, kill_session, probe_session, send_keys_with_workaround, SessionProbe,
    SpawnConfig, spawn_claude_session,
};
use color_eyre::Result;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Diagnostic suffix for crash reason strings — exposes the three clocks
/// that matter for understanding *why* a crash was declared:
///
/// * `dispatch_age` — session wall-clock since `dispatch_time` (monotonic
///   across the whole foreground attempt; not reset on phase change).
/// * `phase_age`    — time in the current phase (`0` when unknown).
/// * `silence`      — seconds since the pane last changed.
///
/// Without these, a log like `"after 1761s"` is ambiguous — readers
/// cannot tell whether `1761s` was the phase duration, silence gap, or
/// cumulative session lifetime.
fn crash_clock_suffix(
    dispatch_age: Duration,
    phase_started_at: Option<Instant>,
    last_activity: Instant,
) -> String {
    let phase_age = phase_started_at
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    let silence = last_activity.elapsed().as_secs();
    format!(
        "dispatch_age={}s phase_age={}s silence={}s",
        dispatch_age.as_secs(),
        phase_age,
        silence
    )
}

/// Run one session attempt: spawn → dispatch → monitor → cleanup.
pub(crate) fn run_session_attempt(
    session_name: &str,
    command: &str,
    config: &SingleSessionConfig,
    pipeline_start: Instant,
    plan_str: &str,
) -> Result<SessionOutcome> {
    // Kill any existing session with the same name
    if has_session(session_name) {
        info!(session = %session_name, "Killing existing session before restart");
        kill_session(session_name)?;
        std::thread::sleep(Duration::from_secs(2));
    }

    // Pre-phase cleanup (only gw-owned processes)
    cleanup::pre_phase_cleanup("single", "0")?;

    // Clear signal files from previous session
    #[allow(deprecated)]
    crate::commands::elden::clear_signals();

    // Spawn session
    info!(session = %session_name, "Spawning Claude Code session");
    let spawn_config = SpawnConfig {
        session_id: session_name.to_string(),
        working_dir: config.working_dir.clone(),
        config_dir: config.config_dir.clone(),
        claude_path: config.claude_path.clone(),
        mock: false,
    };

    let pid = match spawn_claude_session(&spawn_config) {
        Ok(p) => p,
        Err(e) => {
            return Ok(SessionOutcome::Crashed {
                reason: format!("Failed to spawn session: {}", e),
            });
        }
    };

    // Write session owner file so next gw run can adopt if we crash
    if let Err(e) = crate::monitor::session_owner::write_session_owner(
        &config.working_dir, session_name, plan_str, pid,
    ) {
        warn!(error = %e, "Failed to write session owner (non-fatal)");
    }

    info!(session = %session_name, pid = pid, "Session spawned, waiting for Claude Code init");
    println!("[gw] Session spawned (pid={}), waiting 12s for Claude Code init...", pid);
    std::thread::sleep(Duration::from_secs(12));

    // Dispatch the /rune:arc command
    info!(command = %command, "Dispatching arc command");
    println!("[gw] Dispatching: {}", command);
    if let Err(e) = send_keys_with_workaround(session_name, command) {
        let reason = format!("Failed to send command: {}", e);
        save_crash_dump(session_name, &config.working_dir, &reason);
        kill_session(session_name)?;
        return Ok(SessionOutcome::Crashed { reason });
    }

    // Delegate to the shared monitor loop
    monitor_session(session_name, pid, config, pipeline_start)
}

/// Monitor an active session (shared between spawn and adopt paths).
///
/// This is the core monitor loop. Both the normal spawn path and the
/// adopt-orphaned-session path converge here.
pub(crate) fn monitor_session(
    session_name: &str,
    pid: u32,
    config: &SingleSessionConfig,
    pipeline_start: Instant,
) -> Result<SessionOutcome> {

    // Monitor loop
    let dispatch_time = Instant::now();
    let mut last_output_hash: u64 = 0;
    let mut last_activity = Instant::now();
    let mut nudge_count: u32 = 0;
    let mut last_status_log = Instant::now();
    let mut poll_count: u64 = 0;
    let mut last_checkpoint_poll = Instant::now();
    let mut last_checkpoint_activity = Instant::now();
    // Session-probe grace counter. `probe_session` reports `Unreachable`
    // when the tmux server is hung (vs `Absent` when the session truly
    // does not exist). Short bursts of unreachability are transient;
    // only persistent unreachability escalates to the crash path.
    let mut consecutive_unreachable: u32 = 0;
    const UNREACHABLE_GRACE_TICKS: u32 = 3;
    let mut last_checkpoint_hash: Option<u64> = None;
    // Cache the resolved checkpoint path from loop state for O(1) reads each poll.
    let mut cached_checkpoint_path: Option<PathBuf> = None;
    // Track whether we've archived stale checkpoints for this session.
    // We delay archiving until the SECOND successful poll to ensure the
    let mut last_process_gone_at: Option<Instant> = None;
    let mut last_artifact_scan = Instant::now();
    let mut last_artifact_snapshot: Option<ArtifactSnapshot> = None;
    let mut prompt_acceptor = crate::monitor::prompt_accept::PromptAcceptor::new(
        config.watchdog.prompt_accept_enabled,
        config.watchdog.prompt_accept_debounce_secs,
    );
    let mut last_artifact_activity = Instant::now();

    // Loop state warmup tracking: detect when arc-phase-loop.local.md never appears
    // or disappears after being created (Rune arc finished or crashed).
    let mut loop_state_ever_seen = false;
    // When the file disappears, we wait for a grace period before acting,
    // to avoid false positives from Rune briefly rewriting the file.
    let mut loop_state_gone_since: Option<Instant> = None;
    // LOOP_STATE_GONE_GRACE_SECS is imported from crate::engine::monitor_constants.

    // Content change tracking: detect field-level changes in the loop state file.
    // Tracks iteration progression, anomalous field mutations, and content stall.
    let mut prev_loop_state: Option<crate::monitor::loop_state::ArcLoopState> = None;
    let mut last_loop_state_change = Instant::now();

    // ─── UNIFIED KILL GATE ─────────────────────────────────────────────
    //
    // RULE: GW must NEVER kill a session instantly. ALL kill paths go through
    // a single pending-kill gate with a minimum 5-minute confirmation period.
    //
    // During the confirmation period, GW watches for recovery signals:
    //   - Screen activity (pane output changing)
    //   - Checkpoint changes (phases progressing)
    //   - Artifact changes (files being written)
    //   - Loop state changes (iteration advancing)
    //
    // If ANY recovery signal is detected → cancel the pending kill.
    // Only after 5 minutes of COMPLETE silence → execute the kill.
    //
    // Note on Stop/StopFailure signals: these may fire from TEAMMATE agents
    // (not the team lead), so a StopFailure does NOT mean the main session
    // is broken. Rune and Claude Code have their own recovery mechanisms.
    // GW must give them time to self-heal.

    // KILL_GATE_MIN_SECS is imported from crate::engine::monitor_constants.
    // Rune and Claude Code have self-recovery mechanisms — GW must wait at
    // least this long and verify no recovery signals before killing.

    /// A pending kill request. Set by detection logic, executed by the kill gate.
    struct PendingKillRequest {
        reason: String,
        error_class: Option<ErrorClass>,
        outcome: &'static str, // "stuck", "error", "timeout", "crashed"
        started_at: Instant,
        nudge_sent: bool,
    }

    let mut pending_kill: Option<PendingKillRequest> = None;
    let wd = &config.watchdog;

    const MIN_SESSION_DURATION_SECS: u64 = 30;

    // Error evidence confirmation timer (feeds into kill gate).
    // When error evidence reaches confidence threshold, this timer starts.
    // After the confirmation period, the kill request is routed to the kill gate.
    let mut error_confirm_since: Option<(Instant, ErrorClass, f64)> = None;
    let error_confirm_medium_secs = wd.error_confirm_medium_secs.max(KILL_GATE_MIN_SECS);
    let error_confirm_high_secs = wd.error_confirm_high_secs.max(KILL_GATE_MIN_SECS);

    // Track Claude Code's session_id from arc-phase-loop.local.md.
    // Used to filter out stale signals from previous sessions.
    let mut claude_session_id: Option<String> = None;

    // Phase-aware monitoring: track current phase and apply per-phase thresholds.
    // When the phase is unknown (early in pipeline), use watchdog defaults.
    use crate::engine::phase_profile::{self, PhaseProfile};
    use crate::monitor::phase_nav;
    let mut current_phase_name: Option<String> = None;
    let mut current_profile: PhaseProfile = phase_profile::default_profile();
    let mut phase_started_at: Option<Instant> = None;
    // Effective timeout for current phase — resolved from checkpoint.totals.phase_times
    // or falls back to profile default. Reset on every phase transition.
    let mut effective_phase_timeout: u64 = current_profile.phase_timeout_secs;
    // Transition/failed gap tracking: detect when arc is between phases or has a
    // failed phase, and apply escalating nudge → warn → kill timeouts.
    // gw is an observer — always give Rune time to self-heal before intervening.
    let mut in_transition_since: Option<Instant> = None;
    let mut transition_nudge_count: u32 = 0;
    // Failed phase tracking: separate timer because Rune has its own retry system.
    let mut in_failed_since: Option<Instant> = None;
    let mut failed_nudge_count: u32 = 0;

    // Completion grace period: when arc completes, don't kill tmux immediately.
    // Give Claude Code time to finalize writes (commits, echoes, etc.).
    // Uses a hard elapsed timeout from completion detection — NOT idle-based.
    // Idle-based detection is unreliable because Claude Code's TUI continuously
    // redraws (cursor, status bar, auto-compaction), causing the pane hash to
    // change every poll cycle and the idle timer to reset indefinitely.
    let mut completion_detected_at: Option<Instant> = None;
    // COMPLETION_GRACE_SECS is imported from crate::engine::monitor_constants.

    info!("Entering monitor loop (poll={}s, default nudge={}s, default kill={}s)",
        POLL_INTERVAL_SECS, wd.idle_nudge_secs, wd.idle_kill_secs);
    println!("[gw] Monitoring session (poll every {}s)...", POLL_INTERVAL_SECS);

    loop {
        std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
        poll_count += 1;

        // SIGNAL CHECK: Stop/SessionEnd signal from gw elden hook.
        // This is the most reliable completion signal — written by Claude Code's
        // own hook system, not inferred from text or process state.
        if let Some(signal) = crate::commands::elden::read_stop_signal()
            .or_else(crate::commands::elden::read_session_end_signal)
        {
            let is_complete = signal.get("is_complete").and_then(|v| v.as_bool()).unwrap_or(false);
            let event = signal.get("event").and_then(|v| v.as_str()).unwrap_or("unknown");
            info!(event = event, is_complete = is_complete, "Stop signal detected from hook");

            if is_complete {
                if completion_detected_at.is_none() {
                    let elapsed = dispatch_time.elapsed();
                    info!(
                        event = event,
                        elapsed_secs = elapsed.as_secs(),
                        "Arc completed (hook signal) — entering grace period ({}s to kill)",
                        COMPLETION_GRACE_SECS,
                    );
                    println!(
                        "[gw] Arc completed (signal: {}, {}m{}s elapsed) — grace period active (kill in {}m)",
                        event,
                        elapsed.as_secs() / 60,
                        elapsed.as_secs() % 60,
                        COMPLETION_GRACE_SECS / 60,
                    );
                    completion_detected_at = Some(Instant::now());
                }
                // Don't return — let the grace period handler (above) manage the kill
            }
            // Signal says not complete — let other checks determine what happened
            // (could be a stop mid-pipeline, will be caught by session-alive check)
        }

        // SIGNAL CHECK: StopFailure — API error reported by Claude Code itself.
        //
        // StopFailure means ONE API turn failed — NOT that the session is broken.
        // Claude Code and Rune have self-recovery mechanisms. The signal may also
        // come from a TEAMMATE agent, not the team lead.
        //
        // Action: log the signal, route through unified kill gate (5 min wait).
        // The kill gate will cancel if any recovery signal appears.
        if let Some(signal) = crate::commands::elden::read_stop_failure_signal() {
            #[allow(deprecated)]
            crate::commands::elden::clear_signals();

            let signal_session = signal.get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            // Filter stale signals from previous sessions
            let is_current_session = match &claude_session_id {
                Some(current_id) => signal_session == current_id
                    || signal_session == "unknown"
                    || current_id == "unknown",
                None => true,
            };

            if !is_current_session {
                info!(
                    signal_session = signal_session,
                    current_session = claude_session_id.as_deref().unwrap_or("none"),
                    "StopFailure signal from different session — ignoring"
                );
            } else if pending_kill.is_none() {
                let pane_for_classify = capture_pane(session_name).unwrap_or_default();
                let error_class = ErrorClass::from_pane_output(&pane_for_classify, true)
                    .unwrap_or(ErrorClass::ApiOverload);

                let reason = format!(
                    "StopFailure signal: {:?} (session={}, may be teammate — waiting {}m for recovery)",
                    error_class, signal_session, KILL_GATE_MIN_SECS / 60,
                );
                warn!(
                    error_class = ?error_class,
                    signal_session = signal_session,
                    "StopFailure detected — routing to kill gate ({}s wait)",
                    KILL_GATE_MIN_SECS,
                );
                println!(
                    "[gw] StopFailure ({:?}) — waiting {}m for Rune/Claude self-recovery before acting",
                    error_class, KILL_GATE_MIN_SECS / 60,
                );
                pending_kill = Some(PendingKillRequest {
                    reason,
                    error_class: Some(error_class),
                    outcome: "error",
                    started_at: Instant::now(),
                    nudge_sent: false,
                });
            }
        }

        // SIGNAL CHECK: Permission pending — reset idle timer.
        // When Claude is waiting for user permission, pane output stops changing.
        // Without this check, the idle detector would kill the session.
        if crate::commands::elden::is_permission_pending() {
            debug!("Permission request pending — resetting idle timer");
            last_activity = Instant::now();
        }

        // Check total pipeline timeout — route through kill gate
        if pipeline_start.elapsed() > config.pipeline_timeout && pending_kill.is_none() {
            warn!(
                "Pipeline timeout exceeded — routing to kill gate ({}s wait)",
                KILL_GATE_MIN_SECS,
            );
            println!(
                "[gw] Pipeline timeout exceeded — waiting {}m for recovery before killing",
                KILL_GATE_MIN_SECS / 60,
            );
            pending_kill = Some(PendingKillRequest {
                reason: "Pipeline timeout exceeded".to_string(),
                error_class: None,
                outcome: "timeout",
                started_at: Instant::now(),
                nudge_sent: false,
            });
        }

        // Check if session is still alive. Use `probe_session` instead
        // of the `has_session` boolean so a momentarily hung tmux
        // server doesn't false-positive as a session crash. Only
        // `Absent`, or `Unreachable` that persists past the grace
        // window, drives the vanish path below.
        let probe = probe_session(session_name);
        let session_vanished = match probe {
            SessionProbe::Present => {
                consecutive_unreachable = 0;
                false
            }
            SessionProbe::Absent => {
                consecutive_unreachable = 0;
                true
            }
            SessionProbe::Unreachable(ref reason) => {
                consecutive_unreachable += 1;
                if consecutive_unreachable >= UNREACHABLE_GRACE_TICKS {
                    warn!(
                        session = %session_name,
                        consecutive = consecutive_unreachable,
                        last_error = %reason,
                        "tmux unreachable for {} consecutive probes — escalating to crash path",
                        consecutive_unreachable
                    );
                    true
                } else {
                    debug!(
                        session = %session_name,
                        consecutive = consecutive_unreachable,
                        grace = UNREACHABLE_GRACE_TICKS,
                        last_error = %reason,
                        "tmux unreachable — skipping tick (within grace window)"
                    );
                    continue;
                }
            }
        };
        if session_vanished {
            let session_age = dispatch_time.elapsed();
            info!(
                age_secs = session_age.as_secs(),
                "Tmux session ended"
            );

            // If session died too quickly, it's a crash (Claude failed to start,
            // auth error, or immediate exit). Not a successful completion.
            if session_age < Duration::from_secs(MIN_SESSION_DURATION_SECS) {
                return Ok(SessionOutcome::Crashed {
                    reason: format!(
                        "Session exited after only {}s — Claude Code likely failed to start ({})",
                        session_age.as_secs(),
                        crash_clock_suffix(session_age, phase_started_at, last_activity)
                    ),
                });
            }

            // Session ran for a meaningful time — verify via checkpoint
            if !crate::cleanup::process::is_pid_alive(pid) {
                if let Some(checkpoint) = read_cached_checkpoint(
                    &config.working_dir, &mut cached_checkpoint_path,
                ) {
                    // Require `is_near_completion` alongside `is_complete`
                    // so an `auto_merge=false` run that pre-populates
                    // `merge.status = skipped` doesn't look complete
                    // from phase 1.
                    if checkpoint.is_complete() && checkpoint.is_near_completion() {
                        info!("Session ended, checkpoint confirms completion");
                        return Ok(SessionOutcome::Completed);
                    }
                    // Use inferred phase (scans actual statuses) instead of
                    // current_phase() which relies on often-stale phase_sequence.
                    let current = checkpoint.inferred_phase_name()
                        .unwrap_or_else(|| checkpoint.current_phase().unwrap_or("unknown"));
                    warn!(current_phase = current, "Session ended with incomplete checkpoint");
                    return Ok(SessionOutcome::Crashed {
                        reason: format!(
                            "Session ended during phase '{}' ({})",
                            current,
                            crash_clock_suffix(session_age, phase_started_at, last_activity)
                        ),
                    });
                }
                // No checkpoint found. Use session age to disambiguate:
                // Long-running sessions (>5 min) likely completed and Rune cleaned up.
                // Short sessions crashed before creating a checkpoint.
                if session_age.as_secs() >= MIN_COMPLETION_AGE_SECS {
                    warn!(
                        age_secs = session_age.as_secs(),
                        "Session ended, no checkpoint, ran {}m — no positive completion signal, treating as crash",
                        session_age.as_secs() / 60,
                    );
                    return Ok(SessionOutcome::Crashed {
                        reason: format!(
                            "Session ran {}m but produced no checkpoint — cannot confirm completion ({})",
                            session_age.as_secs() / 60,
                            crash_clock_suffix(session_age, phase_started_at, last_activity)
                        ),
                    });
                }
                warn!(
                    age_secs = session_age.as_secs(),
                    "Session ended after only {}s with no checkpoint — treating as crash",
                    session_age.as_secs(),
                );
                return Ok(SessionOutcome::Crashed {
                    reason: format!(
                        "Session ended after {}s with no checkpoint (too short for completion; {})",
                        session_age.as_secs(),
                        crash_clock_suffix(session_age, phase_started_at, last_activity)
                    ),
                });
            }

            // Session gone but process alive? Unusual — treat as crash.
            return Ok(SessionOutcome::Crashed {
                reason: format!(
                    "Tmux session disappeared but process still alive ({})",
                    crash_clock_suffix(session_age, phase_started_at, last_activity)
                ),
            });
        }

        // Process is alive — reset crash grace tracker if it was set
        // (process came back after disappearing briefly, e.g., self-update)
        if last_process_gone_at.is_some() {
            info!("Claude process recovered after temporary disappearance");
            last_process_gone_at = None;
        }

        // Capture pane and check for activity
        let pane_content = match capture_pane(session_name) {
            Ok(content) => content,
            Err(_) => continue, // Transient error, retry next cycle
        };

        // Compute output hash for idle detection
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&pane_content, &mut hasher);
        let current_hash = std::hash::Hasher::finish(&hasher);

        // Auto-accept permission prompts (y/n dialogs)
        if prompt_acceptor.check_and_accept(&pane_content, session_name) {
            last_activity = Instant::now();
            last_output_hash = current_hash; // Prevent idle detection from also acting
            continue; // Skip rest of loop — we just interacted
        }

        if current_hash != last_output_hash {
            last_output_hash = current_hash;
            last_activity = Instant::now();
            nudge_count = 0; // Reset nudge on new activity
        }

        // COMPLETION GRACE PERIOD: if arc completed, wait a fixed duration before
        // killing tmux. This gives Claude Code time to finalize writes (commits,
        // echoes, etc.). Uses hard elapsed timeout — NOT idle-based, because
        // Claude Code's TUI redraws cause pane hash to change every poll cycle,
        // making idle detection unreliable.
        if let Some(completed_at) = completion_detected_at {
            let grace_elapsed = completed_at.elapsed().as_secs();

            if grace_elapsed >= COMPLETION_GRACE_SECS {
                info!(
                    grace_elapsed_secs = grace_elapsed,
                    "Completion grace period expired after {}m, killing tmux",
                    grace_elapsed / 60,
                );
                println!(
                    "[gw] Arc completed {}m ago — killing tmux session",
                    grace_elapsed / 60,
                );
                let _ = kill_session(session_name);
                return Ok(SessionOutcome::Completed);
            }

            // Check if session died on its own during grace
            if !has_session(session_name) {
                info!(
                    grace_elapsed_secs = grace_elapsed,
                    "Session exited on its own during completion grace period",
                );
                return Ok(SessionOutcome::Completed);
            }

            // Still in grace period — skip all other monitoring checks
            if last_status_log.elapsed() >= Duration::from_secs(STATUS_LOG_INTERVAL_SECS) {
                last_status_log = Instant::now();
                let remaining = COMPLETION_GRACE_SECS.saturating_sub(grace_elapsed);
                println!(
                    "[gw] Arc completed — grace period active ({}s elapsed, kill in {}s)",
                    grace_elapsed, remaining,
                );
            }
            continue;
        }

        // PRIMARY: Check checkpoint.json for arc completion.
        // This is the most reliable signal — Rune writes structured state here.
        // Only poll every CHECKPOINT_POLL_INTERVAL_SECS to avoid excessive file I/O.
        if last_checkpoint_poll.elapsed() >= Duration::from_secs(wd.checkpoint_poll_interval_secs) {
            last_checkpoint_poll = Instant::now();
            if let Some(checkpoint) = read_cached_checkpoint(
                &config.working_dir, &mut cached_checkpoint_path,
            ) {
                // Cross-check `is_complete` with `is_near_completion`:
                // `is_complete` accepts a skipped terminal phase as
                // done, which fires prematurely when Rune pre-populates
                // `merge: skipped` for `auto_merge=false` runs.
                if checkpoint.is_complete() && checkpoint.is_near_completion() {
                    if completion_detected_at.is_none() {
                        let elapsed = dispatch_time.elapsed();
                        info!(
                            elapsed_secs = elapsed.as_secs(),
                            "Arc completed (checkpoint) — entering grace period ({}s to kill)",
                            COMPLETION_GRACE_SECS,
                        );
                        println!(
                            "[gw] Pipeline complete (checkpoint confirmed, {}m{}s elapsed) — grace period active (kill in {}m)",
                            elapsed.as_secs() / 60,
                            elapsed.as_secs() % 60,
                            COMPLETION_GRACE_SECS / 60,
                        );
                        completion_detected_at = Some(Instant::now());
                    }
                    // Don't return — grace period handler manages the kill
                }

                // PHASE TRACKING: infer phase from phases map (not phase_sequence).
                // phase_sequence is often stale/not updated by Rune.
                let nav = phase_nav::compute_phase_navigation(&checkpoint);
                let inferred_phase = nav.effective_phase_name().map(|s| s.to_string());

                // Track checkpoint heartbeat — hash inferred phase + completed count
                let mut cp_hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&inferred_phase, &mut cp_hasher);
                std::hash::Hash::hash(&checkpoint.count_by_status("completed"), &mut cp_hasher);
                std::hash::Hash::hash(&nav.is_transitioning(), &mut cp_hasher);
                let cp_hash = std::hash::Hasher::finish(&cp_hasher);

                if last_checkpoint_hash != Some(cp_hash) {
                    last_checkpoint_activity = Instant::now();
                }
                last_checkpoint_hash = Some(cp_hash);

                // Detect phase transitions using inferred phase
                if inferred_phase != current_phase_name {
                    let prev = current_phase_name.as_deref().unwrap_or("none");
                    let curr = inferred_phase.as_deref().unwrap_or("unknown");

                    if let Some(profile) = inferred_phase.as_deref().and_then(phase_profile::profile_for_phase) {
                        let completed = checkpoint.count_by_status("completed");
                        let skipped = checkpoint.count_by_status("skipped");
                        let total = checkpoint.phases.len();

                        // Resolve timeout from checkpoint data (phase_times + reactions)
                        let timeout = phase_profile::resolve_phase_timeout_full(
                            curr, &checkpoint, &profile,
                        );
                        effective_phase_timeout = timeout;

                        info!(
                            from = prev,
                            to = curr,
                            category = ?profile.category,
                            idle_nudge = profile.idle_nudge_secs,
                            idle_kill = profile.idle_kill_secs,
                            phase_timeout = timeout,
                            has_agents = profile.has_agent_teams,
                            transitioning = nav.is_transitioning(),
                            progress = format!("{}/{} done", completed + skipped, total),
                            "Phase transition (inferred) — applying {} profile (timeout={}m)",
                            profile.description, timeout / 60,
                        );
                        println!(
                            "[gw] Phase: {} → {} [{}] (nudge={}s, kill={}s, timeout={}m, {}/{})",
                            prev, curr, profile.description,
                            profile.idle_nudge_secs, profile.idle_kill_secs,
                            timeout / 60,
                            completed + skipped, total,
                        );
                        current_profile = profile;
                        phase_started_at = Some(Instant::now());
                        in_transition_since = None; // clear transition state on real phase change
                    }

                    current_phase_name = inferred_phase;
                }

                // FAILED PHASE DETECTION: start failed timer.
                // Don't start transition timer — arc has its own retry/halt reactions.
                // gw observes and only intervenes after giving Rune time to self-heal.
                if nav.has_failure() {
                    if in_failed_since.is_none() {
                        if let crate::checkpoint::schema::PhasePosition::Failed {
                            failed_phase, next_pending
                        } = &nav.position {
                            info!(
                                failed = failed_phase,
                                next = next_pending.unwrap_or("none"),
                                "Phase failure detected — observing for Rune self-healing",
                            );
                        }
                        in_failed_since = Some(Instant::now());
                        failed_nudge_count = 0;
                    }
                    in_transition_since = None;
                    transition_nudge_count = 0;
                }
                // TRANSITION GAP DETECTION: if no phase is in_progress but
                // completed phases exist, we're between phases.
                // Claude Code often needs 5-10 min to process between phases.
                else if nav.is_transitioning() {
                    if in_transition_since.is_none() {
                        let gap = nav.transition_gap_secs().unwrap_or(0);
                        info!(
                            last_completed = nav.prev.as_ref().map(|p| p.name).unwrap_or("?"),
                            next_pending = nav.next.unwrap_or("?"),
                            gap_secs = gap,
                            "Transition gap detected — between phases (normal: up to 10 min)",
                        );
                        in_transition_since = Some(Instant::now());
                        transition_nudge_count = 0;
                    }
                    // Clear failed state if we moved past it
                    in_failed_since = None;
                    failed_nudge_count = 0;
                } else {
                    // Phase is running — clear all gap timers
                    if in_transition_since.is_some() {
                        debug!("Transition gap ended — phase now in_progress");
                    }
                    if in_failed_since.is_some() {
                        info!("Failed phase resolved — Rune self-healed");
                    }
                    in_transition_since = None;
                    transition_nudge_count = 0;
                    in_failed_since = None;
                    failed_nudge_count = 0;
                }

            }
        }

        // LOOP STATE TRACKING: Monitor arc-phase-loop.local.md for existence,
        // content changes, and stall detection. This file is Rune's heartbeat.
        let loop_state_read = crate::monitor::loop_state::read_arc_loop_state(&config.working_dir);
        let loop_state_file_on_disk = loop_state_read.file_exists();
        let current_loop_state = loop_state_read.active().cloned();
        let loop_state_exists = current_loop_state.is_some();

        if let Some(ref current) = current_loop_state {
            if !loop_state_ever_seen {
                // First time seeing the file — log initial state
                info!(
                    iteration = current.iteration,
                    max_iterations = current.max_iterations,
                    plan = %current.plan_file,
                    branch = %current.branch,
                    "Loop state appeared — arc initialized"
                );
                println!(
                    "[gw] Arc initialized: iteration {}/{}, plan={}",
                    current.iteration, current.max_iterations, current.plan_file,
                );
            }

            loop_state_ever_seen = true;
            loop_state_gone_since = None; // Reset grace timer — file is back

            // Track Claude's session_id for signal filtering
            if claude_session_id.as_deref() != Some(&current.session_id) {
                if claude_session_id.is_some() {
                    info!(
                        old = claude_session_id.as_deref().unwrap_or("none"),
                        new = %current.session_id,
                        "Claude session_id changed"
                    );
                }
                claude_session_id = Some(current.session_id.clone());
            }

            // Content change detection: compare all fields with previous snapshot
            if let Some(ref prev) = prev_loop_state {
                if prev != current {
                    let changes = prev.diff(current);
                    last_loop_state_change = Instant::now();

                    for change in &changes {
                        if change.anomalous {
                            warn!(
                                field = change.field,
                                old = %change.old_value,
                                new = %change.new_value,
                                "Anomalous loop state change detected"
                            );
                            println!(
                                "[gw] \u{26a0} Loop state anomaly: {} changed '{}' \u{2192} '{}'",
                                change.field, change.old_value, change.new_value,
                            );
                        } else {
                            info!(
                                field = change.field,
                                old = %change.old_value,
                                new = %change.new_value,
                                "Loop state field changed"
                            );
                            if change.field == "iteration" {
                                println!(
                                    "[gw] Arc iteration: {} \u{2192} {} (max {})",
                                    change.old_value, change.new_value, current.max_iterations,
                                );
                            }
                        }
                    }

                    // Any content change is activity — reset idle-adjacent timers
                    last_activity = Instant::now();
                }
            }

            // Update snapshot for next poll
            prev_loop_state = Some(current.clone());
        } else if loop_state_ever_seen {
            // File gone — clear previous state (will be set again if file returns)
            prev_loop_state = None;
        }

        if !loop_state_exists {
            let session_age = dispatch_time.elapsed();

            if loop_state_ever_seen {
                // File existed before but is now gone. Use grace period to avoid
                // false positives from Rune briefly rewriting the file.
                let gone_since = loop_state_gone_since.get_or_insert_with(|| {
                    debug!("arc-phase-loop.local.md disappeared — starting grace timer");
                    Instant::now()
                });

                if gone_since.elapsed() < Duration::from_secs(LOOP_STATE_GONE_GRACE_SECS) {
                    // Still within grace period — wait for next poll
                    debug!(
                        gone_secs = gone_since.elapsed().as_secs(),
                        grace_secs = LOOP_STATE_GONE_GRACE_SECS,
                        "Loop state gone, waiting for grace period"
                    );
                } else {
                    // Grace period expired — file is truly gone.
                    info!(
                        gone_secs = gone_since.elapsed().as_secs(),
                        "arc-phase-loop.local.md confirmed gone — arc has ended or was deleted"
                    );
                    if let Some(checkpoint) = read_cached_checkpoint(
                        &config.working_dir, &mut cached_checkpoint_path,
                    ) {
                        if checkpoint.is_complete() && checkpoint.is_near_completion() {
                            if completion_detected_at.is_none() {
                                info!(
                                    "Loop state gone + checkpoint complete — entering grace period ({}s to kill)",
                                    COMPLETION_GRACE_SECS,
                                );
                                println!(
                                    "[gw] Arc completed (loop state gone, checkpoint confirmed) — grace period active (kill in {}m)",
                                    COMPLETION_GRACE_SECS / 60,
                                );
                                completion_detected_at = Some(Instant::now());
                            }
                            // Don't return — grace period handler manages the kill
                        } else {
                            let current = checkpoint.inferred_phase_name()
                                .unwrap_or_else(|| checkpoint.current_phase().unwrap_or("unknown"));

                            // Distinguish between file truly deleted (crash) vs
                            // file still on disk with active=false (graceful shutdown).
                            // When arc completes, it sets active=false but the checkpoint
                            // may not have its "complete" flag set yet (race condition).
                            // If the file is still on disk, this is an intentional
                            // deactivation — enter grace period instead of killing.
                            if loop_state_file_on_disk {
                                info!(
                                    current_phase = current,
                                    "Loop state deactivated (active=false) during phase '{}' — treating as completion (file still on disk)",
                                    current,
                                );
                                if completion_detected_at.is_none() {
                                    println!(
                                        "[gw] Arc deactivated during '{}' (active=false, file on disk) — grace period active (kill in {}m)",
                                        current, COMPLETION_GRACE_SECS / 60,
                                    );
                                    completion_detected_at = Some(Instant::now());
                                }
                                // Don't return — grace period handler manages the kill
                            } else {
                                warn!(current_phase = current, "Loop state gone but checkpoint incomplete");
                                let reason = format!(
                                    "arc-phase-loop.local.md deleted during phase '{}' ({})",
                                    current,
                                    crash_clock_suffix(session_age, phase_started_at, last_activity)
                                );
                                save_crash_dump(session_name, &config.working_dir, &reason);
                                let _ = kill_session(session_name);
                                return Ok(SessionOutcome::Crashed { reason });
                            }
                        }
                    }
                    // No checkpoint found. Disambiguate using session age:
                    // - Long-running (>5 min): Rune ran and cleaned up after itself → Completed
                    // - Short-running (<5 min): Rune failed early before creating checkpoint → Crashed
                    if session_age.as_secs() >= MIN_COMPLETION_AGE_SECS {
                        if completion_detected_at.is_none() {
                            info!(
                                age_secs = session_age.as_secs(),
                                "Loop state gone, no checkpoint, session ran {}m — entering grace period ({}s to kill)",
                                session_age.as_secs() / 60,
                                COMPLETION_GRACE_SECS,
                            );
                            println!(
                                "[gw] Arc likely completed (loop state gone, ran {}m) — grace period active (kill in {}m)",
                                session_age.as_secs() / 60,
                                COMPLETION_GRACE_SECS / 60,
                            );
                            completion_detected_at = Some(Instant::now());
                        }
                        // Don't return — grace period handler manages the kill
                    } else {
                        warn!(
                            age_secs = session_age.as_secs(),
                            "Loop state gone, no checkpoint, session only ran {}s — treating as crash",
                            session_age.as_secs(),
                        );
                        let reason = format!(
                            "arc-phase-loop.local.md gone after only {}s with no checkpoint ({})",
                            session_age.as_secs(),
                            crash_clock_suffix(session_age, phase_started_at, last_activity)
                        );
                        save_crash_dump(session_name, &config.working_dir, &reason);
                        let _ = kill_session(session_name);
                        return Ok(SessionOutcome::Crashed { reason });
                    }
                }
            } else if session_age > Duration::from_secs(wd.loop_state_warmup_secs) {
                // Past the warmup deadline, but only act if Claude Code is
                // truly stuck. If the screen is still changing (Claude is
                // loading MCP servers, processing skills, etc.), keep waiting
                // — the session is alive and making progress.
                let screen_idle_secs = last_activity.elapsed().as_secs();
                let screen_active = screen_idle_secs < wd.loop_state_warmup_secs;

                // If the file exists on disk with active=false, it's a stale
                // leftover from a previous arc run. The new arc hasn't flipped
                // it to active yet. This is NOT the same as "file never appeared"
                // — give extra warmup time for the new session to claim it.
                let stale_file_present = loop_state_file_on_disk && !loop_state_exists;

                if screen_active || stale_file_present {
                    // Claude Code is still producing output or a stale file is
                    // present (new arc hasn't activated yet). Keep waiting.
                    if session_age.as_secs() % 60 < (wd.checkpoint_poll_interval_secs + 1) {
                        if stale_file_present && !screen_active {
                            info!(
                                warmup_secs = wd.loop_state_warmup_secs,
                                elapsed_secs = session_age.as_secs(),
                                screen_idle_secs,
                                "arc-phase-loop.local.md exists with active=false (stale from previous run) — waiting for new arc to activate"
                            );
                            println!(
                                "[gw] Stale arc-phase-loop.local.md (active=false) from previous run — waiting for activation ({}s elapsed, idle {}s)",
                                session_age.as_secs(), screen_idle_secs,
                            );
                        } else {
                            info!(
                                warmup_secs = wd.loop_state_warmup_secs,
                                elapsed_secs = session_age.as_secs(),
                                screen_idle_secs,
                                "arc-phase-loop.local.md not yet active, but screen is active — waiting"
                            );
                            println!(
                                "[gw] Warmup exceeded {}s but Claude Code still active (idle {}s) — waiting",
                                wd.loop_state_warmup_secs, screen_idle_secs,
                            );
                        }
                    }

                    // Safety valve: if stale file persists AND screen is idle
                    // for 2x warmup, give up — something is genuinely wrong.
                    let stale_hard_limit = wd.loop_state_warmup_secs * 2;
                    if stale_file_present && !screen_active
                        && session_age.as_secs() > stale_hard_limit
                    {
                        warn!(
                            elapsed_secs = session_age.as_secs(),
                            screen_idle_secs,
                            hard_limit_secs = stale_hard_limit,
                            "Stale arc-phase-loop.local.md never activated and screen is idle — giving up"
                        );
                        let reason = format!(
                            "arc-phase-loop.local.md stuck at active=false after {}s (hard limit {}s, screen idle {}s) — Rune failed to initialize",
                            session_age.as_secs(), stale_hard_limit, screen_idle_secs,
                        );
                        println!(
                            "[gw] Stale loop state never activated after {}s — restarting session",
                            session_age.as_secs(),
                        );
                        save_crash_dump(session_name, &config.working_dir, &reason);
                        kill_session(session_name)?;
                        return Ok(SessionOutcome::ErrorDetected {
                            error_class: ErrorClass::BootstrapError,
                            reason,
                        });
                    }
                } else {
                    // Screen is stale AND no file on disk → Rune genuinely failed
                    warn!(
                        warmup_secs = wd.loop_state_warmup_secs,
                        elapsed_secs = session_age.as_secs(),
                        screen_idle_secs,
                        "arc-phase-loop.local.md never appeared and screen is idle — arc failed to initialize"
                    );
                    let reason = format!(
                        "arc-phase-loop.local.md never appeared after {}s (warmup {}s, screen idle {}s) — Rune failed to initialize",
                        session_age.as_secs(), wd.loop_state_warmup_secs, screen_idle_secs,
                    );
                    println!(
                        "[gw] arc-phase-loop.local.md not found after {}s (screen idle {}s) — restarting session",
                        session_age.as_secs(), screen_idle_secs,
                    );
                    save_crash_dump(session_name, &config.working_dir, &reason);
                    kill_session(session_name)?;
                    // Classify as BootstrapError — Rune failed to initialize arc.
                    // This is likely a deterministic failure (plugin not loaded, bad
                    // command, config issue) that won't resolve by retrying.
                    return Ok(SessionOutcome::ErrorDetected {
                        error_class: ErrorClass::BootstrapError,
                        reason,
                    });
                }
            }
        }

        // ARTIFACT DIR SCAN: Check if agents are writing output files.
        // This catches activity that neither screen nor checkpoint reflect —
        // e.g., review agents writing TOME fragments, workers creating files.
        if last_artifact_scan.elapsed() >= Duration::from_secs(wd.artifact_scan_interval_secs) {
            last_artifact_scan = Instant::now();
            if let Some(artifact_dir) = find_artifact_dir_cached(&cached_checkpoint_path, &config.working_dir) {
                if let Some(snapshot) = scan_artifact_dir(&artifact_dir) {
                    if last_artifact_snapshot != Some(snapshot) {
                        debug!(
                            files = snapshot.file_count,
                            bytes = snapshot.total_bytes,
                            dir = %artifact_dir.display(),
                            "Artifact dir activity detected"
                        );
                        last_artifact_activity = Instant::now();
                        last_artifact_snapshot = Some(snapshot);
                    }
                }
            }
        }

        // SECONDARY: Check for completion signals in pane output (text matching).
        // Fallback for when checkpoint hasn't been written yet or is stale.
        // Guard: ignore pane text completion for the first 3 minutes — no arc can
        // complete that fast, and Claude Code's startup rendering often contains
        // completion-like strings (Rune routing tables, system-reminders, etc.)
        // that trigger false positives.
        const PANE_COMPLETION_MIN_ELAPSED_SECS: u64 = 180;
        if dispatch_time.elapsed().as_secs() >= PANE_COMPLETION_MIN_ELAPSED_SECS
            && is_pipeline_complete(&pane_content)
        {
            // Verify against the checkpoint before trusting pane text.
            // Forge/work-era agents regularly discuss "merge skipped",
            // "pipeline completed", etc. as plan content — the pane
            // heuristic alone mistook that for real completion (see
            // daemon gw logs 6ddbe6aa reproducer). Require
            // `is_near_completion` so the signal only fires from
            // ship/merge onwards (or when the terminal phase is done).
            let near_completion = read_cached_checkpoint(
                &config.working_dir, &mut cached_checkpoint_path,
            )
            .as_ref()
            .is_some_and(crate::checkpoint::schema::Checkpoint::is_near_completion);
            if near_completion && completion_detected_at.is_none() {
                let elapsed = dispatch_time.elapsed();
                info!(
                    elapsed_secs = elapsed.as_secs(),
                    "Arc completed (pane text + checkpoint verified) — entering grace period ({}s to kill)",
                    COMPLETION_GRACE_SECS,
                );
                println!(
                    "[gw] Pipeline complete! ({}m{}s elapsed) — grace period active (kill in {}m)",
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60,
                    COMPLETION_GRACE_SECS / 60,
                );
                completion_detected_at = Some(Instant::now());
            } else if !near_completion && completion_detected_at.is_none() {
                debug!(
                    "Pane shows completion text but checkpoint not near-terminal — ignoring soft signal"
                );
            }
            // Don't return — grace period handler manages the kill
        }

        // Evidence-based error detection.
        // Error keywords are the LOWEST confidence signal — Claude Code often
        // discusses auth, billing, HTTP codes in normal output (code, plans, docs).
        // Only act when multiple signals confirm an error:
        //   - Screen stalled (pane unchanged)
        //   - Error keyword matched in tail
        //   - Checkpoint not updating (heartbeat stale)
        //   - Process may be dead
        let screen_stall_secs = last_activity.elapsed().as_secs();
        let keyword_match = if current_hash == last_output_hash {
            // Only scan for keywords when screen is not changing
            ErrorClass::from_pane_output(&pane_content, true)
        } else {
            None
        };

        // Check if swarm teammates are running (lightweight: one tmux call)
        let swarm = check_swarm_activity(pid);
        let swarm_active = swarm.as_ref().is_some_and(|s| s.has_working_teammates());

        let evidence = ErrorEvidence {
            keyword_match,
            screen_stall_secs,
            checkpoint_stale_secs: Some(last_checkpoint_activity.elapsed().as_secs()),
            process_alive: crate::cleanup::process::is_pid_alive(pid),
            artifacts_active: last_artifact_activity.elapsed().as_secs() < wd.error_stall_threshold_secs,
            swarm_active,
        };

        if let Some(error_class) = evidence.classify() {
            let conf = evidence.confidence();

            // Confirmation period: don't kill immediately. Start a timer and
            // wait for confirmation_secs. If activity resumes, cancel the kill.
            let confirmation_secs = if conf >= 0.8 {
                error_confirm_high_secs  // 5 min for high confidence
            } else {
                error_confirm_medium_secs // 15 min for medium confidence
            };

            if let Some((started, prev_class, _prev_conf)) = &error_confirm_since {
                // If the error class changed, reset the confirmation timer.
                // A new error type needs its own observation window.
                // Exception: if the new class is fatal (skips_plan), act immediately.
                if *prev_class != error_class {
                    if error_class.skips_plan() && pending_kill.is_none() {
                        // Fatal errors still go through the kill gate — no instant kills
                        warn!(
                            prev_class = ?prev_class,
                            new_class = ?error_class,
                            "Error class escalated to fatal — routing to kill gate"
                        );
                        let reason = format!(
                            "Fatal error detected during confirmation: {:?} (was {:?})",
                            error_class, prev_class,
                        );
                        pending_kill = Some(PendingKillRequest {
                            reason,
                            error_class: Some(error_class),
                            outcome: "error",
                            started_at: Instant::now(),
                            nudge_sent: false,
                        });
                    }
                    info!(
                        prev_class = ?prev_class,
                        new_class = ?error_class,
                        "Error class changed — resetting confirmation timer"
                    );
                    error_confirm_since = Some((Instant::now(), error_class, conf));
                    continue;
                }
                // Already in confirmation period — check if time to act
                let elapsed_confirm = started.elapsed().as_secs();
                if elapsed_confirm >= confirmation_secs && pending_kill.is_none() {
                    // Confirmation period expired — route through kill gate
                    let reason = format!(
                        "Error pattern confirmed: {:?} (confidence={:.1}, observed={}s, stall={}s)",
                        error_class, conf, elapsed_confirm, screen_stall_secs,
                    );
                    warn!(
                        error_class = ?error_class,
                        confidence = conf,
                        confirmation_secs = elapsed_confirm,
                        "Error confirmed after {}s — routing to kill gate",
                        elapsed_confirm,
                    );
                    println!(
                        "[gw] Error confirmed: {:?} ({:.1}, {}s) — waiting {}m more for recovery",
                        error_class, conf, elapsed_confirm, KILL_GATE_MIN_SECS / 60,
                    );
                    pending_kill = Some(PendingKillRequest {
                        reason,
                        error_class: Some(error_class),
                        outcome: "error",
                        started_at: Instant::now(),
                        nudge_sent: false,
                    });
                } else {
                    // Still in confirmation period — log and continue
                    debug!(
                        error_class = ?prev_class,
                        confidence = conf,
                        remaining_secs = confirmation_secs - elapsed_confirm,
                        "Error confirmation in progress — {}s remaining",
                        confirmation_secs - elapsed_confirm,
                    );
                }
            } else {
                // Start confirmation period
                warn!(
                    error_class = ?error_class,
                    confidence = conf,
                    confirmation_secs = confirmation_secs,
                    "Error detected — starting {}s confirmation period before killing",
                    confirmation_secs,
                );
                println!(
                    "[gw] Error signal: {:?} (confidence={:.1}) — watching for {}m before killing",
                    error_class, conf, confirmation_secs / 60,
                );
                error_confirm_since = Some((Instant::now(), error_class, conf));
            }
        } else {
            // No error detected — cancel any pending confirmation
            if error_confirm_since.is_some() {
                info!("Error signals cleared — cancelling pending confirmation");
                println!("[gw] Error signals cleared — system recovered, cancelling pending kill");
                error_confirm_since = None;
            }

            if evidence.keyword_match.is_some() {
                // Keyword matched but confidence too low — log and continue monitoring
                debug!(
                    keyword = ?evidence.keyword_match,
                    confidence = evidence.confidence(),
                    screen_stall_secs = screen_stall_secs,
                    "Error keyword found but confidence too low — Claude likely still active"
                );
            }
        }

        // Check if Claude process is still alive (tmux session may exist but process dead).
        // Grace period: Claude Code self-updates can take 45-90s. Wait CRASH_GRACE_SECS
        // before declaring a crash to avoid false positives during updates.
        // Ported from torrent's D7 crash grace period.
        if !crate::cleanup::process::is_pid_alive(pid) {
            // Track when we first noticed the process gone
            if last_process_gone_at.is_none() {
                last_process_gone_at = Some(Instant::now());
                info!(pid = pid, "Claude process not found — starting crash grace period ({}s)", crate::engine::retry::CRASH_GRACE_SECS);
                println!(
                    "[gw] Claude process (pid={}) not found — waiting {}s grace period (self-update?)",
                    pid, crate::engine::retry::CRASH_GRACE_SECS,
                );
                continue; // Don't kill yet — wait for grace period
            }
            let gone_duration = last_process_gone_at.unwrap().elapsed();
            if gone_duration.as_secs() < crate::engine::retry::CRASH_GRACE_SECS {
                // Within grace period — process may be restarting (self-update)
                debug!(
                    gone_secs = gone_duration.as_secs(),
                    grace_secs = crate::engine::retry::CRASH_GRACE_SECS,
                    "Process still gone — within grace period"
                );
                continue;
            }
            // Grace period expired — process is actually dead
            let session_age = dispatch_time.elapsed();
            warn!(pid = pid, age_secs = session_age.as_secs(), gone_secs = gone_duration.as_secs(), "Claude process died (grace period expired)");
            println!(
                "[gw] Claude process (pid={}) confirmed dead after {}s grace — killing session",
                pid,
                gone_duration.as_secs(),
            );
            save_crash_dump(session_name, &config.working_dir, &format!(
                "Claude process (pid={}) died after {}s grace", pid, gone_duration.as_secs(),
            ));
            kill_session(session_name)?;

            if session_age < Duration::from_secs(MIN_SESSION_DURATION_SECS) {
                return Ok(SessionOutcome::Crashed {
                    reason: format!(
                        "Claude process died after only {}s ({})",
                        session_age.as_secs(),
                        crash_clock_suffix(session_age, phase_started_at, last_activity)
                    ),
                });
            }

            // Process ran for a while then died — verify via checkpoint before
            // assuming success. If checkpoint says incomplete, it's a crash.
            if let Some(checkpoint) = read_cached_checkpoint(
                &config.working_dir, &mut cached_checkpoint_path,
            ) {
                if checkpoint.is_complete() && checkpoint.is_near_completion() {
                    info!("Process died but checkpoint confirms completion");
                    return Ok(SessionOutcome::Completed);
                }
                // Checkpoint exists but not complete — this is a crash mid-pipeline
                let current = checkpoint.inferred_phase_name()
                    .unwrap_or_else(|| checkpoint.current_phase().unwrap_or("unknown"));
                warn!(current_phase = current, "Process died with incomplete checkpoint");
                return Ok(SessionOutcome::Crashed {
                    reason: format!(
                        "Claude process died during phase '{}' ({})",
                        current,
                        crash_clock_suffix(session_age, phase_started_at, last_activity)
                    ),
                });
            }

            // No checkpoint found. Use session age to disambiguate:
            // Long-running sessions (>5 min) likely completed and Rune cleaned up.
            // Short sessions crashed before creating a checkpoint.
            if session_age.as_secs() >= MIN_COMPLETION_AGE_SECS {
                info!(
                    age_secs = session_age.as_secs(),
                    "Process died, no checkpoint, but ran {}m — assuming completion",
                    session_age.as_secs() / 60,
                );
                return Ok(SessionOutcome::Completed);
            }
            warn!(
                age_secs = session_age.as_secs(),
                "Process died after only {}s with no checkpoint — treating as crash",
                session_age.as_secs(),
            );
            return Ok(SessionOutcome::Crashed {
                reason: format!(
                    "Claude process died after {}s with no checkpoint (too short for completion; {})",
                    session_age.as_secs(),
                    crash_clock_suffix(session_age, phase_started_at, last_activity)
                ),
            });
        }

        // Periodic status logging
        if last_status_log.elapsed() >= Duration::from_secs(STATUS_LOG_INTERVAL_SECS) {
            let elapsed = dispatch_time.elapsed();
            let idle_secs = last_activity.elapsed().as_secs();
            let remaining = config.pipeline_timeout.saturating_sub(pipeline_start.elapsed());
            let loop_stall_secs = last_loop_state_change.elapsed().as_secs();

            // Include loop state info if available
            let loop_info = prev_loop_state.as_ref().map(|s| {
                format!("iter={}/{}, stale={}s", s.iteration, s.max_iterations, loop_stall_secs)
            }).unwrap_or_else(|| "no loop state".to_string());

            let phase_name = current_phase_name.as_deref().unwrap_or("unknown");
            let phase_elapsed = phase_started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let transition_gap = in_transition_since.map(|t| t.elapsed().as_secs());
            info!(
                elapsed_secs = elapsed.as_secs(),
                idle_secs = idle_secs,
                remaining_secs = remaining.as_secs(),
                loop_stall_secs = loop_stall_secs,
                poll_count = poll_count,
                nudge_count = nudge_count,
                phase = phase_name,
                phase_category = ?current_profile.category,
                phase_elapsed_secs = phase_elapsed,
                transition_gap_secs = transition_gap,
                "Monitor: {}m{}s, idle {}s, phase={} [{}] ({}s), {}m left, {}{}",
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60,
                idle_secs,
                phase_name,
                current_profile.description,
                phase_elapsed,
                remaining.as_secs() / 60,
                loop_info,
                transition_gap.map_or(String::new(), |g| format!(", transition_gap={}s", g)),
            );

            // Log stall warning if loop state hasn't changed for a long time
            // (10 min threshold — convergence loops typically complete in 5-8 min)
            const LOOP_STALL_WARN_SECS: u64 = 600;
            if loop_state_ever_seen && loop_stall_secs > LOOP_STALL_WARN_SECS {
                let cp_stale = last_checkpoint_activity.elapsed().as_secs();
                if cp_stale > LOOP_STALL_WARN_SECS {
                    warn!(
                        loop_stall_secs = loop_stall_secs,
                        checkpoint_stale_secs = cp_stale,
                        "Loop state AND checkpoint both stale — arc may be stuck"
                    );
                    println!(
                        "[gw] Loop state unchanged for {}m, checkpoint stale for {}m — possible stall",
                        loop_stall_secs / 60, cp_stale / 60,
                    );
                } else {
                    debug!(
                        loop_stall_secs = loop_stall_secs,
                        "Loop state stale but checkpoint still updating — phase in progress"
                    );
                }
            }

            last_status_log = Instant::now();
        }

        // PER-PHASE TIMEOUT: kill if current phase exceeds its budget.
        // Timeout is resolved from checkpoint.totals.phase_times + 5 min grace,
        // falling back to profile defaults when checkpoint data is unavailable.
        // The timer resets on every phase transition (phase_started_at updated above).
        if let Some(phase_start) = phase_started_at {
            let phase_elapsed = phase_start.elapsed().as_secs();
            if phase_elapsed > effective_phase_timeout {
                let phase_name = current_phase_name.as_deref().unwrap_or("unknown");
                warn!(
                    phase = phase_name,
                    elapsed_secs = phase_elapsed,
                    timeout_secs = effective_phase_timeout,
                    category = ?current_profile.category,
                    "Phase timeout exceeded for {:?} phase '{}' ({}s > {}s budget)",
                    current_profile.category, phase_name,
                    phase_elapsed, effective_phase_timeout,
                );
                println!(
                    "[gw] Phase '{}' timeout: {}m > {}m budget — killing session",
                    phase_name,
                    phase_elapsed / 60,
                    effective_phase_timeout / 60,
                );
                if pending_kill.is_none() {
                    let reason = format!(
                        "Phase '{}' timeout: {}s > {}s budget",
                        phase_name, phase_elapsed, effective_phase_timeout,
                    );
                    println!(
                        "[gw] Phase '{}' over budget — waiting {}m for recovery before killing",
                        phase_name, KILL_GATE_MIN_SECS / 60,
                    );
                    pending_kill = Some(PendingKillRequest {
                        reason,
                        error_class: None,
                        outcome: "stuck",
                        started_at: Instant::now(),
                        nudge_sent: false,
                    });
                }
            }
        }

        // TRANSITION GAP ESCALATION: nudge → warn → kill.
        // Claude Code needs time between phases (5-10 min is normal).
        // gw observes and escalates gradually, giving Rune time to proceed.
        if let Some(transition_start) = in_transition_since {
            let gap_secs = transition_start.elapsed().as_secs();
            let phase_name = current_phase_name.as_deref().unwrap_or("unknown");

            if gap_secs > phase_nav::TRANSITION_KILL_SECS {
                // KILL: hard timeout — Rune had enough time, likely stuck
                warn!(
                    phase = phase_name,
                    gap_secs = gap_secs,
                    "Transition gap kill — stuck between phases for {}s",
                    gap_secs,
                );
                println!(
                    "[gw] Transition gap: {}m between phases — killing session",
                    gap_secs / 60,
                );
                if pending_kill.is_none() {
                    let reason = format!(
                        "Transition gap timeout: {}s between phases",
                        gap_secs,
                    );
                    pending_kill = Some(PendingKillRequest {
                        reason,
                        error_class: None,
                        outcome: "stuck",
                        started_at: Instant::now(),
                        nudge_sent: false,
                    });
                }
            } else if gap_secs > phase_nav::TRANSITION_WARN_SECS && transition_nudge_count < 2 {
                // WARN: stronger nudge
                transition_nudge_count = 2;
                info!(gap_secs = gap_secs, "Transition gap warn nudge");
                println!(
                    "[gw] Transition gap: {}m between phases — sending warn nudge (kill at {}m)",
                    gap_secs / 60, phase_nav::TRANSITION_KILL_SECS / 60,
                );
                let _ = send_keys_with_workaround(
                    session_name,
                    "are you stuck between phases? please continue to the next phase",
                );
            } else if gap_secs > phase_nav::TRANSITION_NUDGE_SECS && transition_nudge_count < 1 {
                // NUDGE: gentle reminder
                transition_nudge_count = 1;
                info!(gap_secs = gap_secs, "Transition gap gentle nudge");
                println!(
                    "[gw] Transition gap: {}m between phases — sending gentle nudge",
                    gap_secs / 60,
                );
                let _ = send_keys_with_workaround(
                    session_name,
                    "please continue working",
                );
            }
        }

        // FAILED PHASE ESCALATION: nudge → warn → kill.
        // Rune has its own retry/halt reaction system. gw gives it time to self-heal.
        if let Some(failed_start) = in_failed_since {
            let gap_secs = failed_start.elapsed().as_secs();
            let phase_name = current_phase_name.as_deref().unwrap_or("unknown");

            if gap_secs > phase_nav::FAILED_KILL_SECS {
                warn!(
                    phase = phase_name,
                    gap_secs = gap_secs,
                    "Failed phase kill — Rune did not recover in time",
                );
                println!(
                    "[gw] Failed phase '{}': no recovery after {}m — killing session",
                    phase_name, gap_secs / 60,
                );
                if pending_kill.is_none() {
                    let reason = format!(
                        "Failed phase '{}' not recovered: {}s",
                        phase_name, gap_secs,
                    );
                    pending_kill = Some(PendingKillRequest {
                        reason,
                        error_class: None,
                        outcome: "stuck",
                        started_at: Instant::now(),
                        nudge_sent: false,
                    });
                }
            } else if gap_secs > phase_nav::FAILED_WARN_SECS && failed_nudge_count < 2 {
                failed_nudge_count = 2;
                info!(gap_secs = gap_secs, "Failed phase warn nudge");
                println!(
                    "[gw] Failed phase '{}': {}m without recovery — sending warn nudge",
                    phase_name, gap_secs / 60,
                );
                let _ = send_keys_with_workaround(
                    session_name,
                    "a phase has failed — please check and continue or retry",
                );
            } else if gap_secs > phase_nav::FAILED_NUDGE_SECS && failed_nudge_count < 1 {
                failed_nudge_count = 1;
                info!(gap_secs = gap_secs, "Failed phase gentle nudge");
                println!(
                    "[gw] Failed phase '{}': {}m without recovery — sending gentle nudge",
                    phase_name, gap_secs / 60,
                );
                let _ = send_keys_with_workaround(
                    session_name,
                    "please continue working",
                );
            }
        }

        // Idle detection — uses phase-aware thresholds from current_profile.
        // Work phases get longer idle tolerance (swarm workers cause screen silence).
        // Planning phases get shorter tolerance (should be fast).
        let idle_duration = last_activity.elapsed();
        let effective_kill_secs = current_profile.idle_kill_secs;
        let effective_nudge_secs = current_profile.idle_nudge_secs;

        if idle_duration > Duration::from_secs(effective_kill_secs) && pending_kill.is_none() {
            let reason = format!(
                "Session idle {}s > {}s {:?} limit",
                idle_duration.as_secs(), effective_kill_secs, current_profile.category,
            );
            warn!(
                idle_secs = idle_duration.as_secs(),
                phase = current_phase_name.as_deref().unwrap_or("unknown"),
                category = ?current_profile.category,
                limit = effective_kill_secs,
                "Session idle — requesting kill via gate (waiting {}s for recovery)",
                KILL_GATE_MIN_SECS,
            );
            println!(
                "[gw] Session idle {}s > {}s {:?} limit — kill requested ({}m confirmation)",
                idle_duration.as_secs(), effective_kill_secs,
                current_profile.category, KILL_GATE_MIN_SECS / 60,
            );
            pending_kill = Some(PendingKillRequest {
                reason,
                error_class: None,
                outcome: "stuck",
                started_at: Instant::now(),
                nudge_sent: false,
            });
        }

        // Escalating nudge strategy (intervals scale with phase profile):
        //   Nudge 1 at effective_nudge_secs: "please continue"
        //   Nudge 2 at 2x: "are you stuck? please continue working"
        //   Nudge 3 at 3x: "/compact" to recover context
        let nudge_interval = effective_nudge_secs;
        let next_nudge_at = nudge_interval * (nudge_count as u64 + 1);
        if idle_duration.as_secs() > next_nudge_at && nudge_count < 3 {
            nudge_count += 1;
            let nudge_msg = match nudge_count {
                1 => "please continue",
                2 => "are you stuck? please continue working",
                _ => "/compact",
            };
            info!(
                idle_secs = idle_duration.as_secs(),
                nudge = nudge_count,
                message = nudge_msg,
                "Session idle, sending escalating nudge"
            );
            println!(
                "[gw] Session idle for {}s, sending nudge {}/3: {}",
                idle_duration.as_secs(), nudge_count, nudge_msg,
            );
            let _ = send_keys_with_workaround(session_name, nudge_msg);
        }

        // ─── UNIFIED KILL GATE ───────────────────────────────────────────
        //
        // ALL kill requests pass through this single gate. No other code
        // path may call kill_session() directly inside the monitor loop.
        //
        // Recovery signals that cancel a pending kill:
        //   - Screen changed (pane output different from last poll)
        //   - Checkpoint updated (phase status changed)
        //   - Artifact directory changed (new files written)
        //   - Loop state changed (iteration advanced)
        //   - Swarm teammates active (Claude is waiting for agent results)
        if let Some(ref pk) = pending_kill {
            let gate_elapsed = pk.started_at.elapsed().as_secs();
            let screen_idle = last_activity.elapsed().as_secs();
            let checkpoint_idle = last_checkpoint_activity.elapsed().as_secs();
            let artifact_idle = last_artifact_activity.elapsed().as_secs();
            let loop_state_idle = last_loop_state_change.elapsed().as_secs();

            // Check if swarm teammates are running — this is the strongest
            // recovery signal. When Claude spawns agent teams, the main pane
            // goes idle (waiting for results) but teammates are actively working
            // in separate tmux sessions (claude-swarm-{pid}).
            let swarm_running = check_swarm_activity(pid)
                .is_some_and(|s| s.has_working_teammates());

            // Recovery = ANY signal of life within the last 30 seconds,
            // OR swarm teammates are actively running
            const RECOVERY_WINDOW_SECS: u64 = 30;
            let has_recovery = screen_idle < RECOVERY_WINDOW_SECS
                || checkpoint_idle < RECOVERY_WINDOW_SECS
                || artifact_idle < RECOVERY_WINDOW_SECS
                || loop_state_idle < RECOVERY_WINDOW_SECS
                || swarm_running;

            if has_recovery && gate_elapsed > 10 {
                // Recovery detected — cancel the pending kill
                info!(
                    reason = %pk.reason,
                    gate_elapsed_secs = gate_elapsed,
                    screen_idle = screen_idle,
                    checkpoint_idle = checkpoint_idle,
                    artifact_idle = artifact_idle,
                    swarm_running = swarm_running,
                    "Kill cancelled — recovery signal detected (Rune/Claude self-healed)"
                );
                if swarm_running {
                    println!(
                        "[gw] Kill cancelled — agent teammates still active (screen idle but swarm working)",
                    );
                } else {
                    println!(
                        "[gw] Kill cancelled — recovery detected after {}s (screen={}s, checkpoint={}s, artifact={}s)",
                        gate_elapsed, screen_idle, checkpoint_idle, artifact_idle,
                    );
                }
                pending_kill = None;
            } else if gate_elapsed >= KILL_GATE_MIN_SECS {
                // 5 minutes of silence — execute the kill
                let reason = pk.reason.clone();
                let error_class = pk.error_class.clone();
                let outcome = pk.outcome;

                warn!(
                    reason = %reason,
                    gate_elapsed_secs = gate_elapsed,
                    screen_idle = screen_idle,
                    checkpoint_idle = checkpoint_idle,
                    "Kill gate: {}s elapsed with no recovery — executing kill",
                    gate_elapsed,
                );
                println!(
                    "[gw] Kill confirmed after {}m silence: {}",
                    gate_elapsed / 60, reason,
                );

                save_crash_dump(session_name, &config.working_dir, &reason);
                kill_session(session_name)?;

                return Ok(match outcome {
                    "error" => SessionOutcome::ErrorDetected {
                        error_class: error_class.unwrap_or(ErrorClass::ApiOverload),
                        reason,
                    },
                    "timeout" => SessionOutcome::Timeout,
                    _ => SessionOutcome::Stuck,
                });
            } else {
                // Still waiting — send a nudge at the halfway point
                if !pk.nudge_sent && gate_elapsed >= KILL_GATE_MIN_SECS / 2 {
                    info!(
                        gate_elapsed_secs = gate_elapsed,
                        "Kill gate halfway — sending nudge before kill"
                    );
                    println!(
                        "[gw] Kill pending ({}s/{}s) — sending nudge to check recovery",
                        gate_elapsed, KILL_GATE_MIN_SECS,
                    );
                    let _ = send_keys_with_workaround(session_name, "please continue");
                    if let Some(ref mut pk) = pending_kill {
                        pk.nudge_sent = true;
                    }
                }

                // Periodic status during wait
                if gate_elapsed % 60 < (POLL_INTERVAL_SECS + 1) && gate_elapsed > 0 {
                    debug!(
                        remaining_secs = KILL_GATE_MIN_SECS - gate_elapsed,
                        screen_idle = screen_idle,
                        "Kill gate: waiting for recovery ({}s remaining)",
                        KILL_GATE_MIN_SECS - gate_elapsed,
                    );
                }
            }
        }
    }
}
