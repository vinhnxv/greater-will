//! Crash-recovery orchestration loop.
//!
//! Contains the top-level `run_single_session` and `run_single_session_batch`
//! functions that manage the spawn → monitor → crash-restart lifecycle.

use super::monitor::{monitor_session, run_session_attempt};
use super::util::{build_arc_command, resolve_restart_command, RestartDecision};
use super::{PipelineResult, SessionOutcome, SingleSessionConfig};
use crate::cleanup::startup_cleanup;
use crate::engine::crash_loop::{CrashLoopDecision, CrashLoopDetector};
use color_eyre::eyre;
use color_eyre::Result;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

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
    use crate::session::spawn::kill_session;
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
    if let Some(existing_state) = crate::monitor::loop_state::read_arc_loop_state(&config.working_dir).active() {
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
    // Sanitize: keep only alphanumeric, hyphens, underscores; truncate to 40 chars
    let full_sanitized: String = plan_stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let sanitized: String = full_sanitized.chars().take(40).collect();
    if sanitized.len() < full_sanitized.len() {
        debug!(
            original_len = full_sanitized.len(),
            truncated_to = sanitized.len(),
            "Session name truncated from plan stem"
        );
    }
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
                SessionOutcome::Failed { phase, reason } => {
                    return Ok(PipelineResult {
                        success: false,
                        duration: start.elapsed(),
                        crash_restarts: crash_detector.total_restarts(),
                        message: format!("Phase '{}' failed in adopted session: {}", phase, reason),
                        session_name: session_name.clone(),
                    });
                }
                // Retryable errors — record restart and fall through to spawn a fresh session.
                //
                // RETRY SYSTEM INTERACTION: Two independent retry limiters operate here:
                //   1. CrashLoopDetector (sliding window) — safety net for rapid crashes.
                //      Triggers when N crashes occur within a time window (e.g., 5 in 900s).
                //   2. Per-error-class flat counter (below, line ~539) — policy limit.
                //      Triggers when total restarts >= max_retries() for the error class.
                // Either can terminate the session independently. The crash loop detector
                // fires first (checked via record_restart()), then the per-class check.
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
            SessionOutcome::Failed { phase, reason } => {
                // Phase failure (not a crash). Log and return — don't feed into
                // the crash loop detector since this is a Rune-reported failure,
                // not a process death.
                return Ok(PipelineResult {
                    success: false,
                    duration: start.elapsed(),
                    crash_restarts: crash_detector.total_restarts(),
                    message: format!("Phase '{}' failed: {}", phase, reason),
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

                // Per-error-class max retries still applies.
                // Uses >= so max_retries() is the actual maximum number of allowed retries
                // (e.g., max_retries() == 3 means at most 3 restarts, not 4).
                if crash_detector.total_restarts() >= error_class.max_retries() {
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
                if let Err(e) = crash_detector.persist(&config.working_dir) {
                    tracing::warn!(error = %e, "Failed to persist crash history (best-effort)");
                }

                std::thread::sleep(backoff);
            }
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
            if result.success { "\u{2713} Passed" } else { "\u{2717} Failed" },
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
    use std::path::PathBuf;

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
