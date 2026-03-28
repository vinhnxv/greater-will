//! Single-session pipeline executor.
//!
//! Runs the entire `/rune:arc` pipeline in a single Claude Code tmux session.
//! Greater-will acts as a watchdog: spawn, monitor, crash-recovery, cleanup.
//!
//! This is the default execution mode. Rune's own stop hook (`arc-phase-stop-hook.sh`)
//! drives phase iteration internally. Greater-will only intervenes on:
//! - Crash (process dies) → restart with `--resume`
//! - Stuck (no output for too long) → nudge, then kill + restart
//! - Timeout (total pipeline exceeds budget) → kill + report
//!
//! # Comparison with multi-group mode
//!
//! | Aspect          | Single-session (default) | Multi-group (`--multi-group`) |
//! |-----------------|--------------------------|-------------------------------|
//! | Sessions        | 1 tmux session           | 7 sessions (groups A-G)       |
//! | Phase control   | Rune manages internally  | gw drives per-group           |
//! | Context         | Rune auto-compacts       | Fresh context per group       |
//! | Crash recovery  | gw restarts + `--resume` | gw retries the group          |
//! | Complexity      | Low                      | High                          |

use crate::checkpoint::schema::Checkpoint;
use crate::cleanup::{self, startup_cleanup};
use crate::config::watchdog::WatchdogConfig;
use crate::engine::crash_loop::{CrashLoopDecision, CrashLoopDetector};
use crate::engine::retry::{ErrorClass, ErrorEvidence};
use crate::session::detect::{capture_pane, save_crash_dump};
use crate::session::spawn::{
    has_session, kill_session, send_keys_with_workaround, shell_escape,
    spawn_claude_session, SpawnConfig,
};
use color_eyre::eyre::{self};
use color_eyre::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

// Max crash retries is now in WatchdogConfig (default 5, env: GW_MAX_CRASH_RETRIES).

/// Poll interval for monitoring the session (seconds).
const POLL_INTERVAL_SECS: u64 = 5;

/// How often to print a status line during monitoring (seconds).
const STATUS_LOG_INTERVAL_SECS: u64 = 30;

/// Result of a single-session pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// Whether the pipeline completed successfully.
    pub success: bool,
    /// Total time taken.
    pub duration: Duration,
    /// Number of crash restarts that occurred.
    pub crash_restarts: u32,
    /// Final status message.
    pub message: String,
    /// Tmux session name used.
    pub session_name: String,
}

/// Configuration for single-session execution.
#[derive(Debug, Clone)]
pub struct SingleSessionConfig {
    /// Working directory.
    pub working_dir: PathBuf,
    /// Optional CLAUDE_CONFIG_DIR override.
    pub config_dir: Option<PathBuf>,
    /// Path to claude executable.
    pub claude_path: String,
    /// Total pipeline timeout.
    pub pipeline_timeout: Duration,
    /// Whether this is a resume (pass `--resume` to `/rune:arc`).
    pub resume: bool,
    /// Additional flags to pass to `/rune:arc`.
    pub arc_flags: Vec<String>,
    /// Watchdog tuning parameters (idle thresholds, scan intervals, etc.).
    pub watchdog: WatchdogConfig,
}

impl SingleSessionConfig {
    /// Create a default config for a working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        let watchdog = WatchdogConfig::from_env();
        Self {
            working_dir: working_dir.into(),
            config_dir: None,
            claude_path: "claude".to_string(),
            pipeline_timeout: watchdog.pipeline_timeout,
            resume: false,
            arc_flags: Vec::new(),
            watchdog,
        }
    }

    /// Set config directory.
    pub fn with_config_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config_dir = Some(dir.into());
        self
    }

    /// Mark as resume run.
    pub fn with_resume(mut self) -> Self {
        self.resume = true;
        self
    }
}

/// Run a full arc pipeline in a single tmux session.
///
/// This is the main entry point for single-session mode.
///
/// # Flow
///
/// ```text
/// spawn tmux → start claude → send /rune:arc {plan}
///     │
///     ├─ monitor loop (poll every 5s)
///     │   ├─ check: session alive?  → if dead, crash-restart
///     │   ├─ check: idle too long?  → nudge or kill
///     │   ├─ check: pipeline done?  → exit success
///     │   └─ check: total timeout?  → exit failure
///     │
///     └─ cleanup: kill session + process tree
/// ```
pub fn run_single_session(plan_path: &Path, config: &SingleSessionConfig) -> Result<PipelineResult> {
    let start = Instant::now();
    let plan_str = plan_path
        .to_str()
        .ok_or_else(|| eyre::eyre!("Plan path is not valid UTF-8"))?;

    // Validate plan exists
    if !plan_path.exists() {
        eyre::bail!("Plan file not found: {}", plan_path.display());
    }

    // Pre-flight: disk space check
    crate::cleanup::health::check_disk_space()?;

    // Startup cleanup
    startup_cleanup()?;

    // --- SESSION OWNER CHECK ---
    // Use gw's own PID (from session-owner.json) to detect orphaned sessions,
    // NOT Claude's owner_pid from loop state (which is always alive if Claude is running).
    use crate::monitor::session_owner::{self, OwnerCheck};
    let owner_check = session_owner::check_previous_owner(&config.working_dir);
    let adopt_session: Option<(String, u32)> = match &owner_check {
        OwnerCheck::NoPrevious => None,
        OwnerCheck::OrphanedDead { .. } => None, // stale file already cleaned up
        OwnerCheck::OrphanedStaleClaude { owner } => {
            // Tmux alive but Claude dead — kill the stale session
            println!(
                "[gw] Previous gw (pid {}) died. Session '{}' alive but Claude (pid {}) dead — killing stale session",
                owner.gw_pid, owner.session_name, owner.claude_pid,
            );
            let _ = kill_session(&owner.session_name);
            session_owner::remove_session_owner(&config.working_dir);
            None
        }
        OwnerCheck::OrphanedAlive { owner } => {
            // Previous gw dead, but tmux + Claude still running!
            // Verify the plan matches before adopting.
            let normalize = |p: &str| p.trim_start_matches("./").to_string();
            let owner_plan = normalize(&owner.plan_file);
            let our_plan = normalize(plan_str);

            if owner_plan == our_plan {
                println!(
                    "[gw] Previous gw (pid {}) died — adopting live session '{}' (Claude pid {})",
                    owner.gw_pid, owner.session_name, owner.claude_pid,
                );
                Some((owner.session_name.clone(), owner.claude_pid))
            } else {
                // Different plan — can't adopt. Kill and start fresh.
                warn!(
                    owner_plan = %owner.plan_file,
                    our_plan = %plan_str,
                    "Orphaned session is for a different plan — killing"
                );
                println!(
                    "[gw] Orphaned session '{}' is for '{}', not '{}' — killing",
                    owner.session_name, owner.plan_file, plan_str,
                );
                let _ = kill_session(&owner.session_name);
                session_owner::remove_session_owner(&config.working_dir);
                None
            }
        }
    };

    // Pre-flight: Check for existing arc-phase-loop.local.md.
    // Now that we use session-owner.json for gw-level orphan detection,
    // this check only handles the loop state file consistency.
    if let Some(existing_state) = crate::monitor::loop_state::read_arc_loop_state(&config.working_dir) {
        let existing_plan = existing_state.plan_file.clone();
        let our_plan = plan_str.to_string();

        let normalize = |p: &str| p.trim_start_matches("./").to_string();
        let existing_normalized = normalize(&existing_plan);
        let our_normalized = normalize(&our_plan);

        if existing_normalized == our_normalized {
            info!(
                plan = %existing_plan,
                iteration = existing_state.iteration,
                "Found existing loop state for same plan — will resume"
            );
            println!("[gw] Found existing arc session for this plan (iteration {})", existing_state.iteration);
        } else if adopt_session.is_none() {
            // Different plan and we're not adopting — check if it's truly orphaned.
            // Use session-owner.json's gw_pid (already checked above) as authoritative.
            // If we got here without adopting, the previous session is already killed.
            info!(
                existing_plan = %existing_plan,
                our_plan = %our_plan,
                "Cleaning up loop state from different plan (previous gw already handled)"
            );
            let loop_file = config.working_dir.join(".rune").join("arc-phase-loop.local.md");
            if let Err(e) = std::fs::remove_file(&loop_file) {
                warn!(error = %e, "Failed to remove orphaned loop state (non-fatal)");
            }
        }
        // If adopting + same plan: loop state is valid, keep it.
    }

    // Build session name from plan filename
    let plan_stem = plan_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plan".to_string());
    // Sanitize: keep only alphanumeric, hyphens, underscores; truncate
    let sanitized: String = plan_stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(40)
        .collect();
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let short_id = epoch % 1_000_000; // 6 digits, unique per second
    let session_name = format!("gw-{}-{}", sanitized, short_id);

    // Build the /rune:arc command
    let arc_command = build_arc_command(plan_str, config);

    println!("=== Greater-Will Single-Session Mode ===");
    println!("Plan: {}", plan_path.display());
    println!("Session: {}", session_name);
    if let Some(ref dir) = config.config_dir {
        println!("Config: {} (CLAUDE_CONFIG_DIR)", dir.display());
    }
    println!("Command: {}", arc_command);
    println!("Timeout: {}h", config.pipeline_timeout.as_secs() / 3600);
    println!("Tip: use -v for detailed monitoring logs, -vv for debug output");
    println!();

    // Crash-recovery loop — load history from previous gw if it crashed
    let mut crash_detector = CrashLoopDetector::from_watchdog(&config.watchdog);
    crash_detector.load_history(&config.working_dir);
    let mut is_first_run = true;

    // If we're adopting an orphaned session, use it on the first attempt.
    let mut pending_adopt = adopt_session;

    loop {
        // Check total timeout
        if start.elapsed() > config.pipeline_timeout {
            return Ok(PipelineResult {
                success: false,
                duration: start.elapsed(),
                crash_restarts: crash_detector.total_restarts(),
                message: "Pipeline timeout exceeded".to_string(),
                session_name: session_name.clone(),
            });
        }

        // ADOPT PATH: Reuse an existing orphaned session instead of spawning.
        // This only happens on the first iteration when a previous gw crashed
        // but tmux + Claude are still alive and running the same plan.
        if let Some((ref adopt_name, adopt_pid)) = pending_adopt.take() {
            info!(
                session = %adopt_name,
                claude_pid = adopt_pid,
                "Adopting orphaned session — skipping spawn and dispatch"
            );
            println!("[gw] Adopting session '{}' (pid {}) — entering monitor loop", adopt_name, adopt_pid);

            // Write new owner file with our gw pid
            if let Err(e) = session_owner::write_session_owner(
                &config.working_dir, adopt_name, plan_str, adopt_pid,
            ) {
                warn!(error = %e, "Failed to write session owner during adopt (non-fatal)");
            }

            // Go straight to monitoring — no spawn, no dispatch
            let attempt_start = Instant::now();
            let attempt_result = monitor_session(
                adopt_name,
                adopt_pid,
                config,
                start,
            )?;

            let attempt_duration = attempt_start.elapsed();
            if attempt_duration >= Duration::from_secs(config.watchdog.crash_stability_secs) {
                crash_detector.record_healthy_tick();
            }

            // Handle outcome same as normal attempt
            match attempt_result {
                SessionOutcome::Completed => {
                    session_owner::remove_session_owner(&config.working_dir);
                    CrashLoopDetector::clear_history(&config.working_dir);
                    return Ok(PipelineResult {
                        success: true,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: "Pipeline completed successfully (adopted session)".to_string(),
                        session_name: session_name.clone(),
                    });
                }
                // Fatal errors — no retry, return immediately
                SessionOutcome::ErrorDetected { error_class, ref reason } if error_class.skips_plan() => {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: format!("Fatal error in adopted session (no retry): {}", reason),
                        session_name: session_name.clone(),
                    });
                }
                SessionOutcome::Timeout => {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: "Pipeline timeout exceeded (adopted session)".to_string(),
                        session_name: session_name.clone(),
                    });
                }
                // Retryable errors — record restart and fall through to spawn a fresh session
                other => {
                    is_first_run = false;
                    match crash_detector.record_restart() {
                        CrashLoopDecision::StopCrashLoop => {
                            return Ok(PipelineResult {
                                success: false,
                                duration: start.elapsed(),
                                crash_restarts: crash_detector.total_restarts(),
                                message: format!(
                                    "Crash loop (adopted): {} crashes in {}s window. Last: {:?}",
                                    crash_detector.crashes_in_window(),
                                    config.watchdog.crash_window_secs,
                                    other,
                                ),
                                session_name: session_name.clone(),
                            });
                        }
                        CrashLoopDecision::AllowRestart => {}
                    }
                    warn!("Adopted session ended with {:?} — will restart with --resume", other);
                    println!("[gw] Waiting {}s before restart...", config.watchdog.restart_cooldown_secs);
                    std::thread::sleep(Duration::from_secs(config.watchdog.restart_cooldown_secs));
                    continue;
                }
            }
        }

        // Build command — phase-aware restart strategy:
        // 1. No checkpoint → fresh start (Rune never initialized)
        // 2. Checkpoint with progress → --resume (continue from last phase)
        // 3. Ship/merge phase with PR → --resume but check PR state
        let command = if is_first_run {
            arc_command.clone()
        } else {
            match resolve_restart_command(
                &config.working_dir,
                plan_str,
                config,
                &arc_command,
                crash_detector.total_restarts(),
            ) {
                RestartDecision::Fresh(cmd) => cmd,
                RestartDecision::Resume(cmd) => cmd,
                RestartDecision::AlreadyDone => {
                    info!("Checkpoint shows pipeline already complete — no restart needed");
                    println!("[gw] Pipeline already complete (PR merged or arc done) — skipping restart");
                    session_owner::remove_session_owner(&config.working_dir);
                    CrashLoopDetector::clear_history(&config.working_dir);
                    return Ok(PipelineResult {
                        success: true,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: "Pipeline already complete (detected on restart)".to_string(),
                        session_name,
                    });
                }
            }
        };

        // Run one session attempt
        let attempt_start = Instant::now();
        let attempt_result = run_session_attempt(
            &session_name,
            &command,
            config,
            start,
            plan_str,
        )?;

        // If the attempt ran long enough, record it as healthy
        let attempt_duration = attempt_start.elapsed();
        if attempt_duration >= Duration::from_secs(config.watchdog.crash_stability_secs) {
            crash_detector.record_healthy_tick();
        }

        match attempt_result {
            SessionOutcome::Completed => {
                session_owner::remove_session_owner(&config.working_dir);
                CrashLoopDetector::clear_history(&config.working_dir);
                return Ok(PipelineResult {
                    success: true,
                    duration: start.elapsed(),
                    crash_restarts: crash_detector.total_restarts(),
                    message: "Pipeline completed successfully".to_string(),
                    session_name,
                });
            }
            SessionOutcome::Crashed { reason } => {
                is_first_run = false;

                match crash_detector.record_restart() {
                    CrashLoopDecision::StopCrashLoop => {
                        CrashLoopDetector::clear_history(&config.working_dir);
                        return Ok(PipelineResult {
                            success: false,
                            duration: start.elapsed(),
                            crash_restarts: crash_detector.total_restarts(),
                            message: format!(
                                "Crash loop: {} crashes in {}s window. Last: {}",
                                crash_detector.crashes_in_window(),
                                config.watchdog.crash_window_secs,
                                reason
                            ),
                            session_name,
                        });
                    }
                    CrashLoopDecision::AllowRestart => {}
                }

                warn!(
                    restart = crash_detector.total_restarts(),
                    max = config.watchdog.max_crash_retries,
                    reason = %reason,
                    "Session crashed, will restart with --resume"
                );
                println!(
                    "[gw] Session crashed ({}/{}): {}. Restarting in {}s...",
                    crash_detector.total_restarts(), config.watchdog.max_crash_retries, reason,
                    config.watchdog.restart_cooldown_secs,
                );

                // Cooldown before restart — give system time to stabilize
                std::thread::sleep(Duration::from_secs(config.watchdog.restart_cooldown_secs));
            }
            SessionOutcome::Timeout => {
                return Ok(PipelineResult {
                    success: false,
                    duration: start.elapsed(),
                    crash_restarts: crash_detector.total_restarts(),
                    message: "Pipeline timeout exceeded".to_string(),
                    session_name,
                });
            }
            SessionOutcome::Stuck => {
                is_first_run = false;

                match crash_detector.record_restart() {
                    CrashLoopDecision::StopCrashLoop => {
                        return Ok(PipelineResult {
                            success: false,
                            duration: start.elapsed(),
                            crash_restarts: crash_detector.total_restarts(),
                            message: format!(
                                "Crash loop (stuck): {} crashes in {}s window",
                                crash_detector.crashes_in_window(),
                                config.watchdog.crash_window_secs,
                            ),
                            session_name,
                        });
                    }
                    CrashLoopDecision::AllowRestart => {}
                }

                warn!(restart = crash_detector.total_restarts(), "Session stuck, restarting");
                println!(
                    "[gw] Session stuck ({}/{}). Restarting in {}s...",
                    crash_detector.total_restarts(), config.watchdog.max_crash_retries,
                    config.watchdog.restart_cooldown_secs,
                );
                std::thread::sleep(Duration::from_secs(config.watchdog.restart_cooldown_secs));
            }
            SessionOutcome::ErrorDetected { error_class, reason } => {
                // Auth/billing errors are fatal — skip immediately, no retry
                if error_class.skips_plan() {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: format!("Fatal error (no retry): {}", reason),
                        session_name,
                    });
                }

                is_first_run = false;

                match crash_detector.record_restart() {
                    CrashLoopDecision::StopCrashLoop => {
                        return Ok(PipelineResult {
                            success: false,
                            duration: start.elapsed(),
                            crash_restarts: crash_detector.total_restarts(),
                            message: format!(
                                "Crash loop ({:?}): {} crashes in {}s window. Last: {}",
                                error_class,
                                crash_detector.crashes_in_window(),
                                config.watchdog.crash_window_secs,
                                reason
                            ),
                            session_name,
                        });
                    }
                    CrashLoopDecision::AllowRestart => {}
                }

                // Per-error-class max retries still applies
                if crash_detector.total_restarts() > error_class.max_retries() {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: format!(
                            "Pipeline failed after {} retries for {:?}. Last: {}",
                            crash_detector.total_restarts(), error_class, reason
                        ),
                        session_name,
                    });
                }

                let backoff = error_class.backoff_for_attempt(crash_detector.total_restarts() - 1);
                warn!(
                    restart = crash_detector.total_restarts(),
                    max = error_class.max_retries(),
                    backoff_secs = backoff.as_secs(),
                    error_class = ?error_class,
                    reason = %reason,
                    "Error detected, will restart with --resume after backoff"
                );
                println!(
                    "[gw] {:?} error ({}/{}): {}. Waiting {}s before restart...",
                    error_class,
                    crash_detector.total_restarts(),
                    error_class.max_retries(),
                    reason,
                    backoff.as_secs(),
                );

                // Persist crash history before sleeping — if gw is killed during
                // backoff, the next gw run will inherit the crash count.
                crash_detector.persist(&config.working_dir);

                std::thread::sleep(backoff);
            }
        }
    }
}

/// Outcome of a single session attempt.
#[derive(Debug)]
enum SessionOutcome {
    /// Pipeline completed (all phases done or session exited cleanly).
    Completed,
    /// Session crashed (process died unexpectedly).
    Crashed { reason: String },
    /// Total pipeline timeout exceeded.
    Timeout,
    /// Session stuck (no output for too long).
    Stuck,
    /// A classified error was detected in pane output (rate limit, overload, auth).
    ErrorDetected {
        error_class: ErrorClass,
        reason: String,
    },
}

/// Run one session attempt: spawn → dispatch → monitor → cleanup.
fn run_session_attempt(
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
/// This is the core monitor loop extracted from `run_session_attempt`.
/// Both the normal spawn path and the adopt-orphaned-session path
/// converge here.
fn monitor_session(
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
    const LOOP_STATE_GONE_GRACE_SECS: u64 = 15;

    // Content change tracking: detect field-level changes in the loop state file.
    // Tracks iteration progression, anomalous field mutations, and content stall.
    let mut prev_loop_state: Option<crate::monitor::loop_state::ArcLoopState> = None;
    let mut last_loop_state_change = Instant::now();

    // Error confirmation state: when confidence is in the "watch" zone (0.5-0.8),
    // we start a confirmation timer. If the timer expires without new activity,
    // we kill. Any heartbeat/artifact/checkpoint change cancels the pending kill.
    //
    // | Confidence | Confirmation Period | Rationale |
    // |------------|-------------------|-----------|
    // | < 0.5      | never act         | Below threshold |
    // | 0.5 - 0.8  | 15 min            | Medium confidence — could be transient |
    // | >= 0.8     | 5 min             | High confidence — strong evidence |
    let mut error_confirm_since: Option<(Instant, ErrorClass, f64)> = None;
    let wd = &config.watchdog;

    const MIN_SESSION_DURATION_SECS: u64 = 30;
    let error_confirm_medium_secs = wd.error_confirm_medium_secs;
    let error_confirm_high_secs = wd.error_confirm_high_secs;

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
                let elapsed = dispatch_time.elapsed();
                println!(
                    "[gw] Session ended cleanly (signal: {}, {}m{}s elapsed)",
                    event,
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60,
                );
                // Give a moment for final writes
                std::thread::sleep(Duration::from_secs(3));
                let _ = kill_session(session_name);
                return Ok(SessionOutcome::Completed);
            }
            // Signal says not complete — let other checks determine what happened
            // (could be a stop mid-pipeline, will be caught by session-alive check)
        }

        // SIGNAL CHECK: StopFailure — API error reported by Claude Code itself.
        // This is far more reliable than pane text matching because Claude Code
        // explicitly tells us the turn failed via the StopFailure hook.
        if let Some(_signal) = crate::commands::elden::read_stop_failure_signal() {
            info!("StopFailure signal detected — API error reported by Claude Code");
            // Clear the signal so we don't re-detect on restart
            crate::commands::elden::clear_signals();
            // Classify from pane output — StopFailure confirms a real error,
            // so we can trust keyword matching without stall evidence.
            // Fall back to ApiOverload if no specific pattern matches.
            let pane_for_classify = capture_pane(session_name).unwrap_or_default();
            let error_class = ErrorClass::from_pane_output(&pane_for_classify, true)
                .unwrap_or(ErrorClass::ApiOverload);
            let reason = format!("StopFailure hook — classified as {:?}", error_class);
            println!("[gw] {} — killing session", reason);
            save_crash_dump(session_name, &config.working_dir, &reason);
            kill_session(session_name)?;
            return Ok(SessionOutcome::ErrorDetected {
                error_class,
                reason,
            });
        }

        // SIGNAL CHECK: Permission pending — reset idle timer.
        // When Claude is waiting for user permission, pane output stops changing.
        // Without this check, the idle detector would kill the session.
        if crate::commands::elden::is_permission_pending() {
            debug!("Permission request pending — resetting idle timer");
            last_activity = Instant::now();
        }

        // Check total pipeline timeout
        if pipeline_start.elapsed() > config.pipeline_timeout {
            warn!("Pipeline timeout exceeded, killing session");
            save_crash_dump(session_name, &config.working_dir, "Pipeline timeout exceeded");
            kill_session(session_name)?;
            return Ok(SessionOutcome::Timeout);
        }

        // Check if session is still alive
        if !has_session(session_name) {
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
                        "Session exited after only {}s — Claude Code likely failed to start",
                        session_age.as_secs()
                    ),
                });
            }

            // Session ran for a meaningful time — verify via checkpoint
            if !crate::cleanup::process::is_pid_alive(pid) {
                if let Some(checkpoint) = read_cached_checkpoint(
                    &config.working_dir, &mut cached_checkpoint_path,
                ) {
                    if checkpoint.is_complete() {
                        info!("Session ended, checkpoint confirms completion");
                        return Ok(SessionOutcome::Completed);
                    }
                    let current = checkpoint.current_phase().unwrap_or("unknown");
                    warn!(current_phase = current, "Session ended with incomplete checkpoint");
                    return Ok(SessionOutcome::Crashed {
                        reason: format!(
                            "Session ended during phase '{}' after {}s",
                            current, session_age.as_secs(),
                        ),
                    });
                }
                // No checkpoint found. Use session age to disambiguate:
                // Long-running sessions (>5 min) likely completed and Rune cleaned up.
                // Short sessions crashed before creating a checkpoint.
                const MIN_COMPLETION_AGE_SECS: u64 = 300;
                if session_age.as_secs() >= MIN_COMPLETION_AGE_SECS {
                    warn!(
                        age_secs = session_age.as_secs(),
                        "Session ended, no checkpoint, ran {}m — no positive completion signal, treating as crash",
                        session_age.as_secs() / 60,
                    );
                    return Ok(SessionOutcome::Crashed {
                        reason: format!(
                            "Session ran {}m but produced no checkpoint — cannot confirm completion",
                            session_age.as_secs() / 60,
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
                        "Session ended after {}s with no checkpoint (too short for completion)",
                        session_age.as_secs(),
                    ),
                });
            }

            // Session gone but process alive? Unusual — treat as crash.
            return Ok(SessionOutcome::Crashed {
                reason: "Tmux session disappeared but process still alive".to_string(),
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

        // PRIMARY: Check checkpoint.json for arc completion.
        // This is the most reliable signal — Rune writes structured state here.
        // Only poll every CHECKPOINT_POLL_INTERVAL_SECS to avoid excessive file I/O.
        if last_checkpoint_poll.elapsed() >= Duration::from_secs(wd.checkpoint_poll_interval_secs) {
            last_checkpoint_poll = Instant::now();
            if let Some(checkpoint) = read_cached_checkpoint(
                &config.working_dir, &mut cached_checkpoint_path,
            ) {
                if checkpoint.is_complete() {
                    let elapsed = dispatch_time.elapsed();
                    info!("Pipeline completion detected via checkpoint.json");
                    println!(
                        "[gw] Pipeline complete (checkpoint confirmed)! ({}m{}s elapsed)",
                        elapsed.as_secs() / 60,
                        elapsed.as_secs() % 60,
                    );
                    // Give it a moment to finish writing artifacts
                    std::thread::sleep(Duration::from_secs(5));
                    kill_session(session_name)?;
                    return Ok(SessionOutcome::Completed);
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

                if last_checkpoint_hash.map_or(true, |h| h != cp_hash) {
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
        let current_loop_state = crate::monitor::loop_state::read_arc_loop_state(&config.working_dir);
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
                                "[gw] ⚠ Loop state anomaly: {} changed '{}' → '{}'",
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
                                    "[gw] Arc iteration: {} → {} (max {})",
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
                        if checkpoint.is_complete() {
                            info!("Loop state gone + checkpoint complete → success");
                            let _ = kill_session(session_name);
                            return Ok(SessionOutcome::Completed);
                        }
                        let current = checkpoint.current_phase().unwrap_or("unknown");
                        warn!(current_phase = current, "Loop state gone but checkpoint incomplete");
                        let reason = format!(
                            "arc-phase-loop.local.md deleted during phase '{}' after {}s",
                            current, session_age.as_secs(),
                        );
                        save_crash_dump(session_name, &config.working_dir, &reason);
                        let _ = kill_session(session_name);
                        return Ok(SessionOutcome::Crashed { reason });
                    }
                    // No checkpoint found. Disambiguate using session age:
                    // - Long-running (>5 min): Rune ran and cleaned up after itself → Completed
                    // - Short-running (<5 min): Rune failed early before creating checkpoint → Crashed
                    const MIN_COMPLETION_AGE_SECS: u64 = 300; // 5 min
                    if session_age.as_secs() >= MIN_COMPLETION_AGE_SECS {
                        info!(
                            age_secs = session_age.as_secs(),
                            "Loop state gone, no checkpoint, but session ran {}m — assuming Rune cleaned up after completion",
                            session_age.as_secs() / 60,
                        );
                        let _ = kill_session(session_name);
                        return Ok(SessionOutcome::Completed);
                    }
                    warn!(
                        age_secs = session_age.as_secs(),
                        "Loop state gone, no checkpoint, session only ran {}s — treating as crash",
                        session_age.as_secs(),
                    );
                    let reason = format!(
                        "arc-phase-loop.local.md gone after only {}s with no checkpoint",
                        session_age.as_secs(),
                    );
                    save_crash_dump(session_name, &config.working_dir, &reason);
                    let _ = kill_session(session_name);
                    return Ok(SessionOutcome::Crashed { reason });
                }
            } else if session_age > Duration::from_secs(wd.loop_state_warmup_secs) {
                // Past the warmup deadline, but only act if Claude Code is
                // truly stuck. If the screen is still changing (Claude is
                // loading MCP servers, processing skills, etc.), keep waiting
                // — the session is alive and making progress.
                let screen_idle_secs = last_activity.elapsed().as_secs();
                let screen_active = screen_idle_secs < wd.loop_state_warmup_secs;

                if screen_active {
                    // Claude Code is still producing output — likely still
                    // initializing (loading plugins, MCP servers, etc.).
                    // Don't kill it; just log periodically.
                    if session_age.as_secs() % 60 < (wd.checkpoint_poll_interval_secs + 1) {
                        info!(
                            warmup_secs = wd.loop_state_warmup_secs,
                            elapsed_secs = session_age.as_secs(),
                            screen_idle_secs,
                            "arc-phase-loop.local.md not yet created, but screen is active — waiting"
                        );
                        println!(
                            "[gw] Warmup exceeded {}s but Claude Code still active (idle {}s) — waiting",
                            wd.loop_state_warmup_secs, screen_idle_secs,
                        );
                    }
                } else {
                    // Screen is stale AND past warmup → Rune genuinely failed
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
                    if last_artifact_snapshot.map_or(true, |prev| prev != snapshot) {
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
        if is_pipeline_complete(&pane_content) {
            let elapsed = dispatch_time.elapsed();
            info!("Pipeline completion detected in pane output");
            println!(
                "[gw] Pipeline complete! ({}m{}s elapsed)",
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60,
            );
            // Give it a moment to finish writing
            std::thread::sleep(Duration::from_secs(5));
            kill_session(session_name)?;
            return Ok(SessionOutcome::Completed);
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
        let swarm_active = swarm.map_or(false, |s| s.active_count > 0);

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
                    if error_class.skips_plan() {
                        warn!(
                            prev_class = ?prev_class,
                            new_class = ?error_class,
                            "Error class escalated to fatal — acting immediately"
                        );
                        let reason = format!(
                            "Fatal error detected during confirmation: {:?} (was {:?})",
                            error_class, prev_class,
                        );
                        save_crash_dump(session_name, &config.working_dir, &reason);
                        kill_session(session_name)?;
                        return Ok(SessionOutcome::ErrorDetected {
                            reason,
                            error_class,
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
                if elapsed_confirm >= confirmation_secs {
                    // Confirmation period expired — act now
                    warn!(
                        error_class = ?error_class,
                        confidence = conf,
                        screen_stall_secs = screen_stall_secs,
                        confirmation_secs = elapsed_confirm,
                        "Error confirmed after {}s observation — killing session",
                        elapsed_confirm,
                    );
                    let reason = format!(
                        "Error pattern confirmed: {:?} (confidence={:.1}, observed={}s, stall={}s)",
                        error_class, conf, elapsed_confirm, screen_stall_secs,
                    );
                    println!(
                        "[gw] Error confirmed: {:?} (confidence={:.1}, observed for {}s) — killing session",
                        error_class, conf, elapsed_confirm,
                    );
                    save_crash_dump(session_name, &config.working_dir, &reason);
                    kill_session(session_name)?;
                    return Ok(SessionOutcome::ErrorDetected {
                        reason,
                        error_class,
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
                    reason: format!("Claude process died after only {}s", session_age.as_secs()),
                });
            }

            // Process ran for a while then died — verify via checkpoint before
            // assuming success. If checkpoint says incomplete, it's a crash.
            if let Some(checkpoint) = read_cached_checkpoint(
                &config.working_dir, &mut cached_checkpoint_path,
            ) {
                if checkpoint.is_complete() {
                    info!("Process died but checkpoint confirms completion");
                    return Ok(SessionOutcome::Completed);
                }
                // Checkpoint exists but not complete — this is a crash mid-pipeline
                let current = checkpoint.current_phase().unwrap_or("unknown");
                warn!(current_phase = current, "Process died with incomplete checkpoint");
                return Ok(SessionOutcome::Crashed {
                    reason: format!(
                        "Claude process died during phase '{}' after {}s",
                        current, session_age.as_secs(),
                    ),
                });
            }

            // No checkpoint found. Use session age to disambiguate:
            // Long-running sessions (>5 min) likely completed and Rune cleaned up.
            // Short sessions crashed before creating a checkpoint.
            const MIN_COMPLETION_AGE_SECS: u64 = 300;
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
                    "Claude process died after {}s with no checkpoint (too short for completion)",
                    session_age.as_secs(),
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
                save_crash_dump(session_name, &config.working_dir, &format!(
                    "Phase '{}' timeout: {}s > {}s budget", phase_name, phase_elapsed, effective_phase_timeout,
                ));
                kill_session(session_name)?;
                return Ok(SessionOutcome::Stuck);
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
                save_crash_dump(session_name, &config.working_dir, &format!(
                    "Transition gap timeout: {}s between phases (kill after {}s)",
                    gap_secs, phase_nav::TRANSITION_KILL_SECS,
                ));
                kill_session(session_name)?;
                return Ok(SessionOutcome::Stuck);
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
                save_crash_dump(session_name, &config.working_dir, &format!(
                    "Failed phase '{}' not recovered: {}s (kill after {}s)",
                    phase_name, gap_secs, phase_nav::FAILED_KILL_SECS,
                ));
                kill_session(session_name)?;
                return Ok(SessionOutcome::Stuck);
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

        if idle_duration > Duration::from_secs(effective_kill_secs) {
            warn!(
                idle_secs = idle_duration.as_secs(),
                phase = current_phase_name.as_deref().unwrap_or("unknown"),
                category = ?current_profile.category,
                limit = effective_kill_secs,
                "Session stuck (idle too long for {:?} phase), killing",
                current_profile.category,
            );
            println!(
                "[gw] Session stuck (idle {}s > {}s {:?} limit), killing...",
                idle_duration.as_secs(),
                effective_kill_secs,
                current_profile.category,
            );
            save_crash_dump(session_name, &config.working_dir, &format!(
                "Session idle {}s > {}s {:?} limit",
                idle_duration.as_secs(), effective_kill_secs, current_profile.category,
            ));
            kill_session(session_name)?;
            return Ok(SessionOutcome::Stuck);
        }

        // Escalating nudge strategy (intervals scale with phase profile):
        //   Nudge 1 at effective_nudge_secs: "please continue"
        //   Nudge 2 at 2x: "are you stuck? please continue working"
        //   Nudge 3 at 3x: "/compact" to recover context
        // After all nudges, effective_kill_secs still applies as hard kill.
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
    }
}

/// Check pane output for signals that the arc pipeline has completed.
///
/// Looks for common arc completion patterns in the visible pane content.
fn is_pipeline_complete(pane_content: &str) -> bool {
    // Check last ~20 lines for completion signals
    let last_lines: Vec<&str> = pane_content.lines().rev().take(20).collect();
    let tail = last_lines.join("\n").to_lowercase();

    // Arc completion signals
    tail.contains("arc completed")
        || tail.contains("all phases complete")
        || tail.contains("pipeline completed")
        || tail.contains("merge completed")
        || tail.contains("arc run finished")
        // Rune-specific completion markers
        || tail.contains("the tarnished rests")
        || tail.contains("arc result: success")
}

/// Read checkpoint with three-strategy resolution.
///
/// Resolution order:
/// 1. Return from cache if path still exists
/// 2. Read `checkpoint_path` from `.rune/arc-phase-loop.local.md` (authoritative when active)
/// 3. Fall back to plan-based discovery (when loop state is absent or inactive)
///
/// Returns `None` if no checkpoint can be resolved yet (early arc init).
/// Once resolved, the path is cached until invalidated.
/// Only reads from arc-phase-loop.local.md — no directory scanning.
fn read_cached_checkpoint(
    working_dir: &Path,
    cached_path: &mut Option<PathBuf>,
) -> Option<Checkpoint> {
    /// Try reading a checkpoint and optionally cache the path on success.
    fn try_read_and_cache(
        cp_path: &Path,
        cached_path: &mut Option<PathBuf>,
        cache_on_success: bool,
        label: &str,
    ) -> Option<Checkpoint> {
        match crate::checkpoint::reader::read_checkpoint(cp_path) {
            Ok(cp) => {
                if cache_on_success {
                    *cached_path = Some(cp_path.to_path_buf());
                }
                Some(cp)
            }
            Err(e) => {
                debug!(error = %e, "{label}");
                None
            }
        }
    }

    // If we have a cached path, try reading directly — O(1)
    if let Some(cp_path) = cached_path.as_ref() {
        if cp_path.exists() {
            return match crate::checkpoint::reader::read_checkpoint(cp_path) {
                Ok(cp) => Some(cp),
                Err(e) => {
                    debug!(error = %e, "Could not read cached checkpoint (transient)");
                    None
                }
            };
        }
        // Cached path no longer exists — invalidate and re-discover
        debug!("Cached checkpoint path disappeared, re-discovering");
        *cached_path = None;
    }

    // Only source: arc-phase-loop.local.md — no directory scanning.
    // All checkpoint reads go through arc-phase-loop.local.md — no directory scanning.
    if let Some(loop_state) = crate::monitor::loop_state::read_arc_loop_state(working_dir) {
        let cp_path = loop_state.resolve_checkpoint_path(working_dir);
        if cp_path.exists() {
            info!(
                checkpoint = %cp_path.display(),
                arc_id = ?loop_state.arc_id(),
                iteration = loop_state.iteration,
                "Resolved checkpoint from arc-phase-loop.local.md"
            );
            return try_read_and_cache(&cp_path, cached_path, true, "Could not read checkpoint (transient)");
        }
        debug!(path = %cp_path.display(), "Loop state checkpoint_path doesn't exist yet");
    }

    None
}

/// Find artifact dir using the cached checkpoint path (avoids re-scanning).
fn find_artifact_dir_cached(cached_cp_path: &Option<PathBuf>, working_dir: &Path) -> Option<PathBuf> {
    let cp_path = cached_cp_path.as_ref()?;
    // checkpoint.json is at .rune/arc/arc-{id}/checkpoint.json
    // artifact dir is at   tmp/arc/arc-{id}/
    let arc_id = cp_path.parent()?.file_name()?;
    let artifact_dir = working_dir.join("tmp").join("arc").join(arc_id);
    if artifact_dir.is_dir() {
        Some(artifact_dir)
    } else {
        None
    }
}

/// Build the `/rune:arc` command string for initial run.
fn build_arc_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {}", shell_escape(plan_path));

    if config.resume {
        cmd.push_str(" --resume");
    }

    for flag in &config.arc_flags {
        cmd.push(' ');
        cmd.push_str(&shell_escape(flag));
    }

    cmd
}

/// Build the `/rune:arc --resume` command for crash recovery.
fn build_resume_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {} --resume", shell_escape(plan_path));

    for flag in &config.arc_flags {
        cmd.push(' ');
        cmd.push_str(&shell_escape(flag));
    }

    cmd
}

/// Decision for how to restart after a crash.
enum RestartDecision {
    /// Run the original command (no --resume).
    Fresh(String),
    /// Run with --resume.
    Resume(String),
    /// Pipeline already complete — don't restart at all.
    AlreadyDone,
}

/// Determine the correct restart command based on checkpoint state.
///
/// Phase-aware logic:
/// - No checkpoint or no progress → fresh start
/// - Mid-arc with progress → --resume
/// - Ship phase with PR already merged → AlreadyDone
/// - Ship phase with PR open → --resume
fn resolve_restart_command(
    working_dir: &Path,
    plan_str: &str,
    config: &SingleSessionConfig,
    arc_command: &str,
    restart_count: u32,
) -> RestartDecision {
    use crate::engine::phase_profile::{self, RecoveryStrategy};

    // Read checkpoint path from arc-phase-loop.local.md — no directory scanning.
    let cp_path = match crate::monitor::loop_state::read_arc_loop_state(working_dir) {
        Some(loop_state) => {
            let path = loop_state.resolve_checkpoint_path(working_dir);
            if path.exists() {
                path
            } else {
                info!(restart = restart_count, "Loop state exists but checkpoint not written yet — fresh start");
                println!("[gw] Checkpoint not written yet — running fresh (not --resume)");
                return RestartDecision::Fresh(arc_command.to_string());
            }
        }
        None => {
            info!(restart = restart_count, "No loop state for plan '{}' — fresh start", plan_str);
            println!("[gw] No active arc state found — running fresh (not --resume)");
            return RestartDecision::Fresh(arc_command.to_string());
        }
    };

    let checkpoint = match crate::checkpoint::reader::read_checkpoint(&cp_path) {
        Ok(cp) => {
            let has_progress = cp.phases.values().any(|p| {
                p.status == "completed" || p.status == "in_progress" || p.status == "skipped"
            });
            if !has_progress {
                info!(restart = restart_count, "Checkpoint for plan '{}' has no progress — fresh start", plan_str);
                println!("[gw] Checkpoint exists but no progress — running fresh (not --resume)");
                return RestartDecision::Fresh(arc_command.to_string());
            }
            cp
        }
        Err(e) => {
            warn!(error = %e, "Failed to read checkpoint — fresh start");
            println!("[gw] Failed to read checkpoint — running fresh (not --resume)");
            return RestartDecision::Fresh(arc_command.to_string());
        }
    };

    // Check if already complete
    if checkpoint.is_complete() {
        return RestartDecision::AlreadyDone;
    }

    // Determine current phase and its recovery strategy
    let current_phase = checkpoint.current_phase().unwrap_or("unknown");
    let profile = phase_profile::profile_for_phase(current_phase)
        .unwrap_or_else(phase_profile::default_profile);

    let completed = checkpoint.count_by_status("completed");
    let skipped = checkpoint.count_by_status("skipped");
    let total = checkpoint.phases.len();

    match profile.recovery {
        RecoveryStrategy::FreshStart => {
            info!(
                restart = restart_count,
                phase = current_phase,
                "Recovery strategy: fresh start for {:?} phase",
                profile.category,
            );
            println!("[gw] Phase '{}' recovery: fresh start", current_phase);
            RestartDecision::Fresh(arc_command.to_string())
        }
        RecoveryStrategy::Resume => {
            info!(
                restart = restart_count,
                phase = current_phase,
                progress = format!("{}/{}", completed + skipped, total),
                "Recovery strategy: --resume from {:?} phase",
                profile.category,
            );
            println!(
                "[gw] Phase '{}' ({}/{} done) — resuming with --resume",
                current_phase, completed + skipped, total,
            );
            RestartDecision::Resume(build_resume_command(plan_str, config))
        }
        RecoveryStrategy::ResumeCheckPR => {
            // Ship/merge phase — check if PR already exists and is merged
            if let Some(pr_url) = &checkpoint.pr_url {
                info!(
                    restart = restart_count,
                    phase = current_phase,
                    pr_url = %pr_url,
                    "Ship phase with existing PR — resuming to finish merge"
                );
                println!("[gw] Phase '{}' with PR {} — resuming", current_phase, pr_url);
            } else {
                info!(
                    restart = restart_count,
                    phase = current_phase,
                    "Ship phase without PR — resuming to create PR"
                );
            }
            RestartDecision::Resume(build_resume_command(plan_str, config))
        }
    }
}


/// Run multiple plans sequentially in single-session mode.
///
/// Each plan gets its own session. If a plan fails, execution stops.
pub fn run_single_session_batch(
    plans: &[String],
    config: &SingleSessionConfig,
) -> Result<Vec<PipelineResult>> {
    let mut results = Vec::new();

    for (i, plan) in plans.iter().enumerate() {
        let plan_path = Path::new(plan);
        println!(
            "--- Plan {}/{}: {} ---",
            i + 1,
            plans.len(),
            plan_path.display()
        );

        let result = run_single_session(plan_path, config)?;

        println!(
            "  {} ({:.1}s, {} restarts)",
            if result.success { "✓ Passed" } else { "✗ Failed" },
            result.duration.as_secs_f64(),
            result.crash_restarts,
        );

        let failed = !result.success;
        results.push(result);

        if failed {
            error!(plan = %plan, "Plan failed, stopping batch");
            break;
        }

        println!();
    }

    Ok(results)
}

/// Swarm activity snapshot — number of active Claude teammates.
///
/// Claude Code spawns `tmux -L claude-swarm-{lead_pid}` for agent teams.
/// Each pane runs a teammate. If teammates are running, the system is working
/// even if the main screen is stalled (team lead waiting for agents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SwarmSnapshot {
    /// Number of tmux panes in the swarm server.
    pane_count: u32,
    /// Number of panes with a live Claude child process.
    active_count: u32,
}

/// Check swarm activity for a Claude Code process.
///
/// Looks for `claude-swarm-{pid}` tmux server and counts active panes.
/// Returns `None` if no swarm server exists (not running agent teams).
///
/// Lightweight: one `tmux list-panes` call + one `ps` per pane.
fn check_swarm_activity(claude_pid: u32) -> Option<SwarmSnapshot> {
    use std::process::Command;

    let socket_name = format!("claude-swarm-{}", claude_pid);

    // Check if the swarm server exists by listing panes
    let output = Command::new("tmux")
        .args(["-L", &socket_name, "list-panes", "-a", "-F", "#{pane_pid}"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None; // No swarm server for this PID
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pane_pids: Vec<u32> = stdout
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();

    if pane_pids.is_empty() {
        return None;
    }

    // Count panes with active Claude child processes
    // Each pane runs a shell → claude child. Check if claude child is alive.
    let mut active_count = 0u32;
    for &shell_pid in &pane_pids {
        // Check if shell has a claude child via pgrep
        let child_check = Command::new("pgrep")
            .args(["-P", &shell_pid.to_string(), "-x", "claude"])
            .output();
        if let Ok(out) = child_check {
            if out.status.success() {
                active_count += 1;
            }
        }
    }

    Some(SwarmSnapshot {
        pane_count: pane_pids.len() as u32,
        active_count,
    })
}

/// Lightweight snapshot of an artifact directory — file count and total size.
///
/// Used as an activity signal: if file_count or total_bytes changes between
/// poll cycles, agents are actively writing output (review files, TOME,
/// worker reports, etc.) even if the screen is stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArtifactSnapshot {
    file_count: u64,
    total_bytes: u64,
}

/// Scan a directory recursively for file count and total size.
///
/// Lightweight: only calls `metadata()` per file, never reads content.
/// Returns `None` if the directory doesn't exist (normal for early phases).
fn scan_artifact_dir(dir: &Path) -> Option<ArtifactSnapshot> {
    if !dir.is_dir() {
        return None;
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    // Use a stack-based walk to avoid recursion depth issues
    let mut dirs_to_visit = vec![dir.to_path_buf()];
    while let Some(current) = dirs_to_visit.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                dirs_to_visit.push(entry.path());
            } else if ft.is_file() {
                file_count += 1;
                if let Ok(meta) = entry.metadata() {
                    total_bytes += meta.len();
                }
            }
        }
    }

    Some(ArtifactSnapshot {
        file_count,
        total_bytes,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_arc_command() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_arc_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test.md'");
    }

    #[test]
    fn test_build_arc_command_with_resume() {
        let config = SingleSessionConfig::new("/tmp").with_resume();
        let cmd = build_arc_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test.md' --resume");
    }

    #[test]
    fn test_build_resume_command() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_resume_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test.md' --resume");
    }

    #[test]
    fn test_build_arc_command_escapes_special_chars() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_arc_command("plans/test; rm -rf /.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test; rm -rf /.md'");
    }

    #[test]
    fn test_is_pipeline_complete() {
        assert!(is_pipeline_complete("some output\narc completed\n❯"));
        assert!(is_pipeline_complete("The Tarnished rests after a long journey"));
        assert!(!is_pipeline_complete("still working on phase 5..."));
        assert!(!is_pipeline_complete(""));
    }

    #[test]
    fn test_single_session_config_defaults() {
        let config = SingleSessionConfig::new("/project");
        assert_eq!(config.working_dir, PathBuf::from("/project"));
        assert_eq!(config.claude_path, "claude");
        assert!(!config.resume);
        assert!(config.arc_flags.is_empty());
    }

    #[test]
    fn test_pipeline_result() {
        let result = PipelineResult {
            success: true,
            duration: Duration::from_secs(120),
            crash_restarts: 0,
            message: "ok".to_string(),
            session_name: "gw-test".to_string(),
        };
        assert!(result.success);
        assert_eq!(result.crash_restarts, 0);
    }
}
