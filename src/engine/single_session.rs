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

use crate::checkpoint::reader::find_checkpoint_for_plan;
use crate::checkpoint::schema::Checkpoint;
use crate::cleanup::{self, startup_cleanup};
use crate::engine::retry::{ErrorClass, ErrorEvidence};
use crate::session::detect::capture_pane;
use crate::session::spawn::{
    has_session, kill_session, send_keys_with_workaround, spawn_claude_session,
    SpawnConfig,
};
use color_eyre::eyre::{self};
use color_eyre::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Maximum number of crash-recovery restarts before giving up.
const MAX_CRASH_RETRIES: u32 = 3;

/// Poll interval for monitoring the session (seconds).
const POLL_INTERVAL_SECS: u64 = 5;

/// How often to print a status line during monitoring (seconds).
const STATUS_LOG_INTERVAL_SECS: u64 = 30;

/// Idle threshold before sending a nudge (seconds).
const IDLE_NUDGE_SECS: u64 = 300; // 5 min

/// Idle threshold before killing a stuck session (seconds).
const IDLE_KILL_SECS: u64 = 600; // 10 min

/// Default total pipeline timeout (6 hours).
const DEFAULT_PIPELINE_TIMEOUT_SECS: u64 = 6 * 3600;

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
}

impl SingleSessionConfig {
    /// Create a default config for a working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            config_dir: None,
            claude_path: "claude".to_string(),
            pipeline_timeout: Duration::from_secs(DEFAULT_PIPELINE_TIMEOUT_SECS),
            resume: false,
            arc_flags: Vec::new(),
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

    // Startup cleanup
    startup_cleanup()?;

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
    let session_name = format!("gw-{}", sanitized);

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

    // Crash-recovery loop
    let mut crash_restarts: u32 = 0;
    let mut is_first_run = true;

    loop {
        // Check total timeout
        if start.elapsed() > config.pipeline_timeout {
            return Ok(PipelineResult {
                success: false,
                duration: start.elapsed(),
                crash_restarts,
                message: "Pipeline timeout exceeded".to_string(),
                session_name: session_name.clone(),
            });
        }

        // Build command — use --resume after first crash
        let command = if is_first_run {
            arc_command.clone()
        } else {
            info!(restart = crash_restarts, "Restarting with --resume after crash");
            build_resume_command(plan_str, config)
        };

        // Run one session attempt
        let attempt_result = run_session_attempt(
            &session_name,
            &command,
            config,
            start,
            plan_path,
        )?;

        match attempt_result {
            SessionOutcome::Completed => {
                return Ok(PipelineResult {
                    success: true,
                    duration: start.elapsed(),
                    crash_restarts,
                    message: "Pipeline completed successfully".to_string(),
                    session_name,
                });
            }
            SessionOutcome::Crashed { reason } => {
                crash_restarts += 1;
                is_first_run = false;

                if crash_restarts > MAX_CRASH_RETRIES {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts,
                        message: format!(
                            "Pipeline failed after {} crash restarts. Last: {}",
                            crash_restarts, reason
                        ),
                        session_name,
                    });
                }

                warn!(
                    restart = crash_restarts,
                    max = MAX_CRASH_RETRIES,
                    reason = %reason,
                    "Session crashed, will restart with --resume"
                );
                println!(
                    "[gw] Session crashed ({}/{}): {}. Restarting in 5s...",
                    crash_restarts, MAX_CRASH_RETRIES, reason,
                );

                // Brief cooldown before restart
                std::thread::sleep(Duration::from_secs(5));
            }
            SessionOutcome::Timeout => {
                return Ok(PipelineResult {
                    success: false,
                    duration: start.elapsed(),
                    crash_restarts,
                    message: "Pipeline timeout exceeded".to_string(),
                    session_name,
                });
            }
            SessionOutcome::Stuck => {
                crash_restarts += 1;
                is_first_run = false;

                if crash_restarts > MAX_CRASH_RETRIES {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts,
                        message: "Pipeline stuck and max restarts exceeded".to_string(),
                        session_name,
                    });
                }

                warn!(restart = crash_restarts, "Session stuck, restarting");
                println!(
                    "[gw] Session stuck ({}/{}). Restarting in 5s...",
                    crash_restarts, MAX_CRASH_RETRIES,
                );
                std::thread::sleep(Duration::from_secs(5));
            }
            SessionOutcome::ErrorDetected { error_class, reason } => {
                // Auth/billing errors are fatal — skip immediately, no retry
                if error_class.skips_plan() {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts,
                        message: format!("Fatal error (no retry): {}", reason),
                        session_name,
                    });
                }

                crash_restarts += 1;
                is_first_run = false;

                if crash_restarts > error_class.max_retries() {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts,
                        message: format!(
                            "Pipeline failed after {} retries for {:?}. Last: {}",
                            crash_restarts, error_class, reason
                        ),
                        session_name,
                    });
                }

                let backoff = error_class.backoff_for_attempt(crash_restarts - 1);
                warn!(
                    restart = crash_restarts,
                    max = error_class.max_retries(),
                    backoff_secs = backoff.as_secs(),
                    error_class = ?error_class,
                    reason = %reason,
                    "Error detected, will restart with --resume after backoff"
                );
                println!(
                    "[gw] {:?} error ({}/{}): {}. Waiting {}s before restart...",
                    error_class,
                    crash_restarts,
                    error_class.max_retries(),
                    reason,
                    backoff.as_secs(),
                );

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
    plan_path: &Path,
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

    info!(session = %session_name, pid = pid, "Session spawned, waiting for Claude Code init");
    println!("[gw] Session spawned (pid={}), waiting 12s for Claude Code init...", pid);
    std::thread::sleep(Duration::from_secs(12));

    // Dispatch the /rune:arc command
    info!(command = %command, "Dispatching arc command");
    println!("[gw] Dispatching: {}", command);
    if let Err(e) = send_keys_with_workaround(session_name, command) {
        kill_session(session_name)?;
        return Ok(SessionOutcome::Crashed {
            reason: format!("Failed to send command: {}", e),
        });
    }

    // Monitor loop
    let dispatch_time = Instant::now();
    let mut last_output_hash: u64 = 0;
    let mut last_activity = Instant::now();
    let mut nudged = false;
    let mut last_status_log = Instant::now();
    let mut poll_count: u64 = 0;
    let mut last_checkpoint_poll = Instant::now();
    let mut last_checkpoint_activity = Instant::now();
    let mut last_checkpoint_hash: Option<u64> = None;
    let mut last_process_gone_at: Option<Instant> = None;

    /// Minimum session duration to consider a "real" run (not an instant crash).
    /// If the session dies within this window after dispatch, it's a crash.
    const MIN_SESSION_DURATION_SECS: u64 = 30;

    /// How often to check the checkpoint file (seconds).
    /// Less frequent than pane polling since file I/O is heavier.
    const CHECKPOINT_POLL_INTERVAL_SECS: u64 = 10;

    info!("Entering monitor loop (poll={}s, nudge={}s, kill={}s)",
        POLL_INTERVAL_SECS, IDLE_NUDGE_SECS, IDLE_KILL_SECS);
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
            println!("[gw] API error detected via StopFailure hook — killing session");
            // Clear the signal so we don't re-detect on restart
            crate::commands::elden::clear_signals();
            kill_session(session_name)?;
            // Classify as ApiOverload — the most common StopFailure cause.
            // The outer loop will apply exponential backoff (15→30→60→120 min).
            return Ok(SessionOutcome::ErrorDetected {
                error_class: ErrorClass::ApiOverload,
                reason: "API error reported via StopFailure hook".to_string(),
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
                if let Some(checkpoint) = try_read_plan_checkpoint(plan_path, &config.working_dir) {
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
                // No checkpoint — fall back to old heuristic
                info!("Session ended, process dead, no checkpoint — assuming completion");
                return Ok(SessionOutcome::Completed);
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

        if current_hash != last_output_hash {
            last_output_hash = current_hash;
            last_activity = Instant::now();
            nudged = false; // Reset nudge on new activity
        }

        // PRIMARY: Check checkpoint.json for arc completion.
        // This is the most reliable signal — Rune writes structured state here.
        // Only poll every CHECKPOINT_POLL_INTERVAL_SECS to avoid excessive file I/O.
        if last_checkpoint_poll.elapsed() >= Duration::from_secs(CHECKPOINT_POLL_INTERVAL_SECS) {
            last_checkpoint_poll = Instant::now();
            if let Some(checkpoint) = try_read_plan_checkpoint(plan_path, &config.working_dir) {
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

                // Track checkpoint heartbeat — hash the current phase to detect changes
                let mut cp_hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&checkpoint.current_phase().unwrap_or("none"), &mut cp_hasher);
                std::hash::Hash::hash(&checkpoint.count_by_status("completed"), &mut cp_hasher);
                let cp_hash = std::hash::Hasher::finish(&cp_hasher);

                if last_checkpoint_hash.map_or(true, |h| h != cp_hash) {
                    last_checkpoint_activity = Instant::now();
                }
                last_checkpoint_hash = Some(cp_hash);
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

        let evidence = ErrorEvidence {
            keyword_match,
            screen_stall_secs,
            checkpoint_stale_secs: Some(last_checkpoint_activity.elapsed().as_secs()),
            process_alive: crate::cleanup::process::is_pid_alive(pid),
        };

        if let Some(error_class) = evidence.classify() {
            warn!(
                error_class = ?error_class,
                confidence = evidence.confidence(),
                screen_stall_secs = screen_stall_secs,
                checkpoint_stale_secs = last_checkpoint_activity.elapsed().as_secs(),
                keyword = ?evidence.keyword_match,
                process_alive = evidence.process_alive,
                "Error evidence threshold reached — multiple signals confirm error"
            );
            println!(
                "[gw] Error detected: {:?} (confidence={:.1}, stall={}s) — killing session",
                error_class,
                evidence.confidence(),
                screen_stall_secs,
            );
            kill_session(session_name)?;
            return Ok(SessionOutcome::ErrorDetected {
                reason: format!(
                    "Error pattern detected: {:?} (confidence={:.1}, stall={}s, checkpoint_stale={}s)",
                    error_class,
                    evidence.confidence(),
                    screen_stall_secs,
                    last_checkpoint_activity.elapsed().as_secs(),
                ),
                error_class,
            });
        } else if evidence.keyword_match.is_some() {
            // Keyword matched but confidence too low — log and continue monitoring
            debug!(
                keyword = ?evidence.keyword_match,
                confidence = evidence.confidence(),
                screen_stall_secs = screen_stall_secs,
                "Error keyword found but confidence too low — Claude likely still active"
            );
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
            kill_session(session_name)?;

            if session_age < Duration::from_secs(MIN_SESSION_DURATION_SECS) {
                return Ok(SessionOutcome::Crashed {
                    reason: format!("Claude process died after only {}s", session_age.as_secs()),
                });
            }

            // Process ran for a while then died — verify via checkpoint before
            // assuming success. If checkpoint says incomplete, it's a crash.
            if let Some(checkpoint) = try_read_plan_checkpoint(plan_path, &config.working_dir) {
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

            // No checkpoint found — fall back to assuming completion
            // (old behavior, for cases where checkpoint wasn't created yet)
            info!("Process died, no checkpoint found — assuming completion");
            return Ok(SessionOutcome::Completed);
        }

        // Periodic status logging
        if last_status_log.elapsed() >= Duration::from_secs(STATUS_LOG_INTERVAL_SECS) {
            let elapsed = dispatch_time.elapsed();
            let idle_secs = last_activity.elapsed().as_secs();
            let remaining = config.pipeline_timeout.saturating_sub(pipeline_start.elapsed());
            info!(
                elapsed_secs = elapsed.as_secs(),
                idle_secs = idle_secs,
                remaining_secs = remaining.as_secs(),
                poll_count = poll_count,
                nudged = nudged,
                "Monitor status: running for {}m{}s, idle {}s, {}m remaining",
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60,
                idle_secs,
                remaining.as_secs() / 60,
            );
            last_status_log = Instant::now();
        }

        // Idle detection
        let idle_duration = last_activity.elapsed();

        if idle_duration > Duration::from_secs(IDLE_KILL_SECS) {
            warn!(
                idle_secs = idle_duration.as_secs(),
                "Session stuck (idle too long), killing"
            );
            println!(
                "[gw] Session stuck (idle {}s > {}s limit), killing...",
                idle_duration.as_secs(),
                IDLE_KILL_SECS,
            );
            kill_session(session_name)?;
            return Ok(SessionOutcome::Stuck);
        }

        if idle_duration > Duration::from_secs(IDLE_NUDGE_SECS) && !nudged {
            info!(
                idle_secs = idle_duration.as_secs(),
                "Session idle, sending nudge"
            );
            println!(
                "[gw] Session idle for {}s, sending nudge...",
                idle_duration.as_secs(),
            );
            let _ = send_keys_with_workaround(session_name, "please continue");
            nudged = true;
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

/// Try to read the checkpoint for a given plan.
///
/// Returns `None` if no checkpoint exists yet or if it can't be read.
/// This is a non-fatal operation — the monitor loop should continue
/// even if the checkpoint is temporarily unreadable.
fn try_read_plan_checkpoint(plan_path: &Path, working_dir: &Path) -> Option<Checkpoint> {
    match find_checkpoint_for_plan(plan_path, working_dir) {
        Ok(Some(cp_path)) => {
            match crate::checkpoint::reader::read_checkpoint(&cp_path) {
                Ok(cp) => Some(cp),
                Err(e) => {
                    debug!(error = %e, "Could not read checkpoint (transient)");
                    None
                }
            }
        }
        Ok(None) => None,
        Err(e) => {
            debug!(error = %e, "Could not find checkpoint for plan");
            None
        }
    }
}

/// Build the `/rune:arc` command string for initial run.
fn build_arc_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {}", plan_path);

    if config.resume {
        cmd.push_str(" --resume");
    }

    for flag in &config.arc_flags {
        cmd.push(' ');
        cmd.push_str(flag);
    }

    cmd
}

/// Build the `/rune:arc --resume` command for crash recovery.
fn build_resume_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {} --resume", plan_path);

    for flag in &config.arc_flags {
        cmd.push(' ');
        cmd.push_str(flag);
    }

    cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_arc_command() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_arc_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc plans/test.md");
    }

    #[test]
    fn test_build_arc_command_with_resume() {
        let config = SingleSessionConfig::new("/tmp").with_resume();
        let cmd = build_arc_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc plans/test.md --resume");
    }

    #[test]
    fn test_build_resume_command() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_resume_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc plans/test.md --resume");
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
