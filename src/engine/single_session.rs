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

use crate::cleanup::{self, startup_cleanup};
use crate::session::detect::{capture_pane, detect_error_pattern};
use crate::session::spawn::{
    has_session, kill_session, send_keys_with_workaround, spawn_claude_session,
    SpawnConfig,
};
use color_eyre::eyre::{self};
use color_eyre::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

// Used by verify_completion_via_checkpoint for parsing checkpoint JSON.
use serde_json;

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
}

/// Run one session attempt: spawn → dispatch → monitor → cleanup.
fn run_session_attempt(
    session_name: &str,
    command: &str,
    config: &SingleSessionConfig,
    pipeline_start: Instant,
) -> Result<SessionOutcome> {
    // Kill any existing session with the same name
    if has_session(session_name) {
        info!(session = %session_name, "Killing existing session before restart");
        kill_session(session_name)?;
        std::thread::sleep(Duration::from_secs(2));
    }

    // Pre-phase cleanup (only gw-owned processes)
    cleanup::pre_phase_cleanup("single", "0")?;

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
    let mut last_pane_content = String::new();

    /// Minimum session duration to consider a "real" run (not an instant crash).
    /// If the session dies within this window after dispatch, it's a crash.
    const MIN_SESSION_DURATION_SECS: u64 = 30;

    info!("Entering monitor loop (poll={}s, nudge={}s, kill={}s)",
        POLL_INTERVAL_SECS, IDLE_NUDGE_SECS, IDLE_KILL_SECS);
    println!("[gw] Monitoring session (poll every {}s)...", POLL_INTERVAL_SECS);

    loop {
        std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
        poll_count += 1;

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

            // Session ran for a meaningful time — verify via last pane output before declaring success
            if !crate::cleanup::process::is_pid_alive(pid) {
                if verify_pipeline_completed_via_pane(&last_pane_content, Some(&config.working_dir)) {
                    info!("Session ended, completion signal confirmed in pane output");
                    return Ok(SessionOutcome::Completed);
                }
                // No completion signal — treat as crash, not success
                warn!("Session ended without completion signal — treating as crash");
                return Ok(SessionOutcome::Crashed {
                    reason: format!(
                        "Session exited after {}s without completion signal",
                        session_age.as_secs()
                    ),
                });
            }

            // Session gone but process alive? Unusual — treat as crash.
            return Ok(SessionOutcome::Crashed {
                reason: "Tmux session disappeared but process still alive".to_string(),
            });
        }

        // Capture pane and check for activity
        let pane_content = match capture_pane(session_name) {
            Ok(content) => content,
            Err(_) => continue, // Transient error, retry next cycle
        };
        last_pane_content = pane_content.clone();

        // Compute output hash for idle detection
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&pane_content, &mut hasher);
        let current_hash = std::hash::Hasher::finish(&hasher);

        if current_hash != last_output_hash {
            last_output_hash = current_hash;
            last_activity = Instant::now();
            nudged = false; // Reset nudge on new activity
        }

        // Check for completion signals in pane output
        if is_pipeline_complete(&pane_content, Some(&config.working_dir)) {
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

        // Check for error patterns in pane output (billing, auth, rate limit, etc.)
        if let Some(error_type) = detect_error_pattern(&pane_content) {
            warn!(error_type = %error_type, "Error pattern detected in pane output");
            println!("[gw] Error detected: {} — killing session", error_type);
            kill_session(session_name)?;
            return Ok(SessionOutcome::Crashed {
                reason: format!("Error pattern detected: {}", error_type),
            });
        }

        // Check if Claude process is still alive (tmux session may exist but process dead)
        if !crate::cleanup::process::is_pid_alive(pid) {
            let session_age = dispatch_time.elapsed();
            warn!(pid = pid, age_secs = session_age.as_secs(), "Claude process died but tmux session still exists");
            println!(
                "[gw] Claude process (pid={}) died after {}m{}s",
                pid,
                session_age.as_secs() / 60,
                session_age.as_secs() % 60,
            );
            kill_session(session_name)?;

            if session_age < Duration::from_secs(MIN_SESSION_DURATION_SECS) {
                return Ok(SessionOutcome::Crashed {
                    reason: format!("Claude process died after only {}s", session_age.as_secs()),
                });
            }
            // Process ran for a while then died — verify via pane output
            if verify_pipeline_completed_via_pane(&pane_content, Some(&config.working_dir)) {
                info!("Process died but completion signal found in pane output");
                return Ok(SessionOutcome::Completed);
            }
            warn!("Process died without completion signal — treating as crash");
            return Ok(SessionOutcome::Crashed {
                reason: format!(
                    "Claude process died after {}m{}s without completion signal",
                    session_age.as_secs() / 60,
                    session_age.as_secs() % 60,
                ),
            });
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

/// Verify that the pipeline actually completed by checking pane output for
/// completion signals. Used when a session/process dies to distinguish
/// between a successful completion and a crash.
fn verify_pipeline_completed_via_pane(pane_content: &str, working_dir: Option<&Path>) -> bool {
    is_pipeline_complete(pane_content, working_dir)
}

/// Check pane output for signals that the arc pipeline has completed.
///
/// Looks for common arc completion patterns in the visible pane content,
/// then cross-verifies against the checkpoint file if a working directory
/// is provided. This prevents false positives from strings that happen
/// to appear in output without the pipeline actually finishing.
fn is_pipeline_complete(pane_content: &str, working_dir: Option<&Path>) -> bool {
    // Check last ~20 lines for completion signals
    let last_lines: Vec<&str> = pane_content.lines().rev().take(20).collect();
    let tail = last_lines.join("\n").to_lowercase();

    // Arc completion signals
    let string_match = tail.contains("arc completed")
        || tail.contains("all phases complete")
        || tail.contains("pipeline completed")
        || tail.contains("merge completed")
        || tail.contains("arc run finished")
        // Rune-specific completion markers
        || tail.contains("the tarnished rests")
        || tail.contains("arc result: success");

    if !string_match {
        return false;
    }

    // String matched — cross-verify via checkpoint if working_dir available
    if let Some(dir) = working_dir {
        match verify_completion_via_checkpoint(dir) {
            Some(true) => return true,
            Some(false) => {
                warn!("Completion string detected in pane but checkpoint shows incomplete — ignoring false signal");
                return false;
            }
            None => {
                // Checkpoint unreadable or not found, fall back to string-only detection
                info!("Completion string detected, checkpoint unavailable — accepting string signal");
                return true;
            }
        }
    }

    // No working_dir provided, accept string match
    true
}

/// Verify pipeline completion by reading the most recent checkpoint.json
/// under `.rune/arc/`. Returns `Some(true)` if all phases are completed/skipped,
/// `Some(false)` if checkpoint exists but shows incomplete, or `None` if
/// no checkpoint could be read.
fn verify_completion_via_checkpoint(working_dir: &Path) -> Option<bool> {
    let arc_dir = working_dir.join(".rune").join("arc");
    let entries = std::fs::read_dir(&arc_dir).ok()?;

    // Find the most recently modified checkpoint.json
    let mut checkpoints: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let cp = e.path().join("checkpoint.json");
            let mtime = cp.metadata().ok()?.modified().ok()?;
            Some((cp, mtime))
        })
        .collect();

    checkpoints.sort_by(|a, b| b.1.cmp(&a.1));

    let (cp_path, _) = checkpoints.first()?;
    let content = std::fs::read_to_string(cp_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let phases = json.get("phases")?.as_object()?;
    if phases.is_empty() {
        return None; // No phase data — can't verify
    }

    let all_done = phases.values().all(|v| {
        v.get("status")
            .and_then(|s| s.as_str())
            .map(|s| s == "completed" || s == "skipped")
            .unwrap_or(false)
    });

    Some(all_done)
}

/// Check if an arc flag is safe to pass to a shell command.
///
/// Valid flags must start with `--` and must not contain characters that
/// could enable command injection (newlines, semicolons, backticks, pipes,
/// dollar signs, ampersands).
fn is_valid_arc_flag(flag: &str) -> bool {
    flag.starts_with("--")
        && !flag.contains('\n')
        && !flag.contains('\r')
        && !flag.contains(';')
        && !flag.contains('`')
        && !flag.contains('|')
        && !flag.contains('$')
        && !flag.contains('&')
}

/// Append validated arc_flags to a command string.
/// Invalid flags are skipped with a warning log.
fn append_validated_flags(cmd: &mut String, flags: &[String]) {
    for flag in flags {
        if is_valid_arc_flag(flag) {
            cmd.push(' ');
            cmd.push_str(flag);
        } else {
            warn!(flag = %flag, "Skipping invalid arc flag (must start with '--' and contain no shell metacharacters)");
        }
    }
}

/// Build the `/rune:arc` command string for initial run.
fn build_arc_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {}", plan_path);

    if config.resume {
        cmd.push_str(" --resume");
    }

    append_validated_flags(&mut cmd, &config.arc_flags);

    cmd
}

/// Build the `/rune:arc --resume` command for crash recovery.
fn build_resume_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {} --resume", plan_path);

    append_validated_flags(&mut cmd, &config.arc_flags);

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
        // Without working_dir (None) falls back to string-only check
        assert!(is_pipeline_complete("some output\narc completed\n❯", None));
        assert!(is_pipeline_complete("The Tarnished rests after a long journey", None));
        assert!(!is_pipeline_complete("still working on phase 5...", None));
        assert!(!is_pipeline_complete("", None));
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
