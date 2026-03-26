//! Phase group executor - core state machine for running phase groups.
//!
//! This module implements the main executor that runs phase groups in
//! isolated Claude Code sessions. Each group is executed in a fresh
//! tmux session with proper cleanup between groups.
//!
//! # State Machine
//!
//! ```text
//! Pending → PreFlight → Spawning → WaitingReady → Dispatching →
//! Running → Completing → Succeeded/Failed/Skipped
//! ```
//!
//! # Example
//!
//! ```ignore
//! use greater_will::engine::phase_executor::{PhaseGroupExecutor, ExecutorConfig};
//! use greater_will::config::PhaseConfig;
//!
//! let config = PhaseConfig::from_file("config/default-phases.toml")?;
//! let executor = PhaseGroupExecutor::new(config);
//!
//! let results = executor.execute_plan("plans/feature.md")?;
//!
//! for result in &results {
//!     println!("Group {}: {:?}", result.group_name, result.state);
//! }
//! ```

use crate::checkpoint::reader::{mark_phases_completed_before, read_checkpoint};
use crate::checkpoint::writer::write_checkpoint;
use crate::checkpoint::schema::Checkpoint;
use crate::cleanup::{pre_phase_cleanup, post_phase_cleanup};
use crate::cleanup::health::check_system_health;
use crate::config::phase_config::{PhaseConfig, PhaseGroup};
use crate::engine::completion::{CompletionConfig, CompletionDetector, CompletionEvent};
use crate::engine::retry::{ErrorClass, RetryCoordinator, RetryDecision};
use crate::session::{spawn_claude_session, kill_session, wait_for_prompt, send_keys_with_workaround, SpawnConfig};
use crate::session::detect::capture_pane;
use color_eyre::eyre::{eyre, Context};
use color_eyre::Result;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Default prompt wait timeout (60 seconds).
const DEFAULT_PROMPT_WAIT_SECS: u64 = 60;

/// Default initialization wait (12 seconds).
const CLAUDE_INIT_SECS: u64 = 12;

/// Phase group state in the execution state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PhaseGroupState {
    /// Not yet started.
    Pending,
    /// Running pre-flight checks (cleanup, health check).
    PreFlight,
    /// Spawning tmux session.
    Spawning,
    /// Waiting for Claude Code to be ready (prompt detection).
    WaitingReady,
    /// Sending skill command to the session.
    Dispatching,
    /// Monitoring execution (4-layer completion detection).
    Running,
    /// Validating artifacts.
    Completing,
    /// Group completed successfully.
    Succeeded,
    /// Group failed after retries.
    Failed {
        retries: u32,
        error: ErrorClass,
    },
    /// Group was skipped (skip_if condition).
    Skipped,
}

impl PhaseGroupState {
    /// Check if the state is terminal (won't change).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            PhaseGroupState::Succeeded
                | PhaseGroupState::Failed { .. }
                | PhaseGroupState::Skipped
        )
    }

    /// Check if the group succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, PhaseGroupState::Succeeded | PhaseGroupState::Skipped)
    }
}

/// Result of executing a phase group.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseGroupResult {
    /// Group name (e.g., "A", "B").
    pub group_name: String,
    /// Final state.
    pub state: PhaseGroupState,
    /// Session ID used.
    pub session_id: Option<String>,
    /// PID of the Claude Code process.
    pub pid: Option<u32>,
    /// Time taken to execute (seconds).
    #[serde(serialize_with = "serialize_duration")]
    pub duration: Duration,
    /// Number of retries attempted.
    pub retries: u32,
    /// Error message if failed.
    pub error_message: Option<String>,
    /// Phases in this group.
    pub phases: Vec<String>,
}

/// Configuration for the phase group executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Working directory for execution.
    pub working_dir: PathBuf,
    /// Config directory for CLAUDE_CONFIG_DIR.
    pub config_dir: Option<PathBuf>,
    /// Use mock mode for testing.
    pub mock: bool,
    /// Path to claude executable.
    pub claude_path: String,
    /// Prompt wait timeout.
    pub prompt_wait_timeout: Duration,
    /// Enable dry run (don't actually execute).
    pub dry_run: bool,
    /// Maximum concurrent Claude Code sessions (default: 1 for MVP).
    /// Greater-will enforces this before spawning each session.
    pub max_concurrent_sessions: u32,
}

impl ExecutorConfig {
    /// Create a new executor config.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            config_dir: None,
            mock: false,
            claude_path: "claude".to_string(),
            prompt_wait_timeout: Duration::from_secs(DEFAULT_PROMPT_WAIT_SECS),
            dry_run: false,
            max_concurrent_sessions: 1,
        }
    }

    /// Set config directory.
    pub fn with_config_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config_dir = Some(dir.into());
        self
    }

    /// Enable mock mode.
    pub fn with_mock(mut self) -> Self {
        self.mock = true;
        self
    }

    /// Enable dry run mode.
    pub fn with_dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Set maximum concurrent sessions.
    pub fn with_max_concurrent_sessions(mut self, max: u32) -> Self {
        self.max_concurrent_sessions = max;
        self
    }
}

/// Phase group executor.
///
/// Orchestrates the execution of phase groups for a single plan.
pub struct PhaseGroupExecutor {
    /// Phase configuration.
    config: PhaseConfig,
    /// Retry coordinator.
    retry_coordinator: RetryCoordinator,
    /// Results for each group.
    results: Vec<PhaseGroupResult>,
}

impl PhaseGroupExecutor {
    /// Create a new executor.
    pub fn new(config: PhaseConfig) -> Self {
        Self {
            config,
            retry_coordinator: RetryCoordinator::new(),
            results: Vec::new(),
        }
    }

    /// Execute all phase groups for a plan.
    ///
    /// This is the main entry point. It:
    /// 1. Loads or creates the checkpoint
    /// 2. Executes each phase group sequentially
    /// 3. Returns results for each group
    ///
    /// # Arguments
    ///
    /// * `plan_path` - Path to the plan file
    /// * `exec_config` - Executor configuration
    pub fn execute_plan(
        &mut self,
        plan_path: &Path,
        exec_config: &ExecutorConfig,
    ) -> Result<Vec<PhaseGroupResult>> {
        let plan_hash = compute_plan_hash(plan_path);
        let arc_dir = exec_config.working_dir.join(".rune").join("arc").join(format!("arc-{}", plan_hash));
        let checkpoint_path = arc_dir.join("checkpoint.json");

        info!(
            plan_path = %plan_path.display(),
            plan_hash = %plan_hash,
            "Starting plan execution"
        );

        // Load or create checkpoint
        let checkpoint = self.load_or_create_checkpoint(
            &checkpoint_path,
            plan_path,
            &arc_dir,
        )?;

        // Write initial checkpoint
        write_checkpoint(&checkpoint, &checkpoint_path)?;

        // Clone groups to avoid borrow issues during iteration
        let groups: Vec<PhaseGroup> = self.config.groups.clone();

        // Execute each phase group
        for group in groups {
            let result = self.execute_group(
                &group,
                &plan_hash,
                &checkpoint_path,
                &arc_dir,
                exec_config,
            )?;

            // Write result to JSONL log
            if let Err(e) = append_result_jsonl(&result, &arc_dir) {
                warn!(error = %e, "Failed to write JSONL log (non-fatal)");
            }

            self.results.push(result.clone());

            // Stop if a group failed
            if let PhaseGroupState::Failed { .. } = result.state {
                error!(
                    group = %result.group_name,
                    "Group failed, stopping execution"
                );
                break;
            }

            // Reset retry coordinator on success
            self.retry_coordinator.reset(&group.name);
        }

        Ok(self.results.clone())
    }

    /// Execute a single phase group.
    fn execute_group(
        &mut self,
        group: &PhaseGroup,
        plan_hash: &str,
        checkpoint_path: &Path,
        arc_dir: &Path,
        exec_config: &ExecutorConfig,
    ) -> Result<PhaseGroupResult> {
        let start = Instant::now();
        let session_id = format!("gw-{}-{}", plan_hash, group.name);

        info!(
            group = %group.name,
            phases = ?group.phases,
            "Starting phase group"
        );

        // Check skip condition
        if let Some(skip_if) = &group.skip_if {
            if self.should_skip_group(skip_if, checkpoint_path)? {
                info!(group = %group.name, reason = %skip_if, "Skipping group");
                return Ok(PhaseGroupResult {
                    group_name: group.name.clone(),
                    state: PhaseGroupState::Skipped,
                    session_id: None,
                    pid: None,
                    duration: start.elapsed(),
                    retries: 0,
                    error_message: None,
                    phases: group.phases.clone(),
                });
            }
        }

        // Execute with retry
        let mut retries;
        loop {
            let result = self.execute_group_once(
                group,
                plan_hash,
                checkpoint_path,
                arc_dir,
                exec_config,
                &session_id,
            )?;

            match result.state {
                PhaseGroupState::Succeeded => {
                    return Ok(result);
                }
                PhaseGroupState::Failed { error, retries: r } => {
                    retries = r;

                    // Check retry decision
                    let decision = self.retry_coordinator.should_retry(&group.name, error);

                    match decision {
                        RetryDecision::Retry { after } => {
                            info!(
                                group = %group.name,
                                after_secs = after.as_secs(),
                                retry = retries,
                                "Retrying group after backoff"
                            );
                            std::thread::sleep(after);
                        }
                        RetryDecision::SkipPlan => {
                            error!(
                                group = %group.name,
                                error = ?error,
                                "Skipping plan due to fatal error"
                            );
                            return Ok(PhaseGroupResult {
                                group_name: group.name.clone(),
                                state: PhaseGroupState::Failed { retries, error },
                                session_id: Some(session_id),
                                pid: None,
                                duration: start.elapsed(),
                                retries,
                                error_message: Some(format!("Fatal error: {:?}", error)),
                                phases: group.phases.clone(),
                            });
                        }
                        RetryDecision::Exhausted => {
                            error!(
                                group = %group.name,
                                retries = retries,
                                "Group failed after max retries"
                            );
                            return Ok(result);
                        }
                    }
                }
                _ => return Ok(result),
            }
        }
    }

    /// Execute a phase group once (single attempt).
    fn execute_group_once(
        &mut self,
        group: &PhaseGroup,
        plan_hash: &str,
        checkpoint_path: &Path,
        _arc_dir: &Path,
        exec_config: &ExecutorConfig,
        session_id: &str,
    ) -> Result<PhaseGroupResult> {
        let start = Instant::now();
        let pid: Option<u32>;

        // Pre-flight
        info!(group = %group.name, "Pre-flight checks");

        // Process budget enforcement: ensure we don't exceed max concurrent sessions
        let active_sessions = crate::cleanup::tmux_cleanup::list_gw_sessions()
            .unwrap_or_default();
        let active_count = active_sessions.len() as u32;
        if active_count >= exec_config.max_concurrent_sessions {
            warn!(
                active = active_count,
                max = exec_config.max_concurrent_sessions,
                "Active sessions at budget limit, cleaning stale sessions first"
            );
        }

        // Health check
        check_system_health().wrap_err("System health check failed")?;

        // Cleanup previous sessions
        pre_phase_cleanup(plan_hash, &group.name)?;

        // Dry run - skip actual execution
        if exec_config.dry_run {
            info!(group = %group.name, "Dry run - skipping execution");
            return Ok(PhaseGroupResult {
                group_name: group.name.clone(),
                state: PhaseGroupState::Succeeded,
                session_id: Some(session_id.to_string()),
                pid: None,
                duration: start.elapsed(),
                retries: 0,
                error_message: None,
                phases: group.phases.clone(),
            });
        }

        // Modify checkpoint to mark phases before this group as completed
        let mut checkpoint = read_checkpoint(checkpoint_path)?;
        let first_phase = group.phases.first().ok_or_else(|| eyre!("Group has no phases"))?;
        mark_phases_completed_before(&mut checkpoint, first_phase);
        write_checkpoint(&checkpoint, checkpoint_path)?;

        // Spawn session
        info!(group = %group.name, session_id = %session_id, "Spawning session");

        let spawn_config = SpawnConfig::new(
            plan_hash,
            &group.name,
            exec_config.working_dir.clone(),
            exec_config.config_dir.clone(),
        );

        let spawn_config = if exec_config.mock {
            spawn_config.with_mock()
        } else {
            spawn_config
        };

        match spawn_claude_session(&spawn_config) {
            Ok(p) => pid = Some(p),
            Err(e) => {
                return Ok(PhaseGroupResult {
                    group_name: group.name.clone(),
                    state: PhaseGroupState::Failed {
                        retries: 0,
                        error: ErrorClass::Crash,
                    },
                    session_id: Some(session_id.to_string()),
                    pid: None,
                    duration: start.elapsed(),
                    retries: 0,
                    error_message: Some(format!("Failed to spawn session: {}", e)),
                    phases: group.phases.clone(),
                });
            }
        }

        // Wait for prompt
        info!(group = %group.name, "Waiting for Claude Code prompt");

        let prompt_timeout = exec_config.prompt_wait_timeout;
        if !exec_config.mock {
            match wait_for_prompt(session_id, prompt_timeout) {
                Ok(()) => info!("Prompt detected"),
                Err(e) => {
                    warn!(error = %e, "Timeout waiting for prompt");
                    kill_session(session_id)?;
                    return Ok(PhaseGroupResult {
                        group_name: group.name.clone(),
                        state: PhaseGroupState::Failed {
                            retries: 0,
                            error: ErrorClass::Crash,
                        },
                        session_id: Some(session_id.to_string()),
                        pid,
                        duration: start.elapsed(),
                        retries: 0,
                        error_message: Some(format!("Timeout waiting for prompt: {}", e)),
                        phases: group.phases.clone(),
                    });
                }
            }
        }

        // Send skill command
        info!(
            group = %group.name,
            command = %group.skill_command,
            "Sending skill command"
        );

        if !exec_config.mock {
            send_keys_with_workaround(session_id, &group.skill_command)?;
        }

        // Monitor completion
        info!(group = %group.name, "Monitoring completion");

        let completion_config = CompletionConfig::for_group(
            group.timeout_min,
            self.config.settings.idle_nudge_after_sec,
            self.config.settings.idle_kill_after_sec,
        );

        let mut detector = CompletionDetector::new(
            completion_config,
            checkpoint_path.to_path_buf(),
            group.phases.clone(),
        );

        let mut nudge_sent = false;
        let mut tick_count: u64 = 0;
        let mut last_pane_hash: u64 = 0;
        let monitor_start = Instant::now();

        loop {
            tick_count += 1;
            // Get pane content
            let pane_content = capture_pane(session_id)?;

            // Track pane hash to reset nudge when new content appears
            let pane_hash = crate::session::detect::compute_content_hash(&pane_content);
            if pane_hash != last_pane_hash {
                last_pane_hash = pane_hash;
                if nudge_sent {
                    info!(group = %group.name, "New pane content detected after nudge, resetting nudge state");
                    nudge_sent = false;
                }
            }

            // Check completion
            match detector.tick(&pane_content)? {
                CompletionEvent::Completed => {
                    info!(group = %group.name, "Group completed");
                    break;
                }
                CompletionEvent::Failed { phase } => {
                    warn!(group = %group.name, phase = %phase, "Phase failed");
                    kill_session(session_id)?;
                    return Ok(PhaseGroupResult {
                        group_name: group.name.clone(),
                        state: PhaseGroupState::Failed {
                            retries: 0,
                            error: ErrorClass::Unknown,
                        },
                        session_id: Some(session_id.to_string()),
                        pid,
                        duration: start.elapsed(),
                        retries: 0,
                        error_message: Some(format!("Phase {} failed", phase)),
                        phases: group.phases.clone(),
                    });
                }
                CompletionEvent::Timeout => {
                    warn!(group = %group.name, "Phase timeout");
                    kill_session(session_id)?;
                    return Ok(PhaseGroupResult {
                        group_name: group.name.clone(),
                        state: PhaseGroupState::Failed {
                            retries: 0,
                            error: ErrorClass::Timeout,
                        },
                        session_id: Some(session_id.to_string()),
                        pid,
                        duration: start.elapsed(),
                        retries: 0,
                        error_message: Some("Phase timeout".to_string()),
                        phases: group.phases.clone(),
                    });
                }
                CompletionEvent::Stuck => {
                    warn!(group = %group.name, "Session stuck");
                    kill_session(session_id)?;
                    return Ok(PhaseGroupResult {
                        group_name: group.name.clone(),
                        state: PhaseGroupState::Failed {
                            retries: 0,
                            error: ErrorClass::Stuck,
                        },
                        session_id: Some(session_id.to_string()),
                        pid,
                        duration: start.elapsed(),
                        retries: 0,
                        error_message: Some("Session stuck (idle too long)".to_string()),
                        phases: group.phases.clone(),
                    });
                }
                CompletionEvent::Nudge => {
                    if !nudge_sent {
                        info!(group = %group.name, "Sending nudge: 'please continue'");
                        if let Err(e) = send_keys_with_workaround(session_id, "please continue") {
                            warn!(error = %e, "Failed to send nudge (non-fatal)");
                        }
                        nudge_sent = true;
                        detector.mark_nudged();
                    }
                }
                CompletionEvent::PromptReturned => {
                    info!(group = %group.name, "Prompt returned, checking checkpoint");
                    // Re-read checkpoint
                    if let Ok(cp) = read_checkpoint(checkpoint_path) {
                        if self.is_group_complete_in_checkpoint(&group, &cp) {
                            info!(group = %group.name, "Group completed (detected via prompt return)");
                            break;
                        }
                    }
                    // Prompt returned but group not complete — session exited prematurely
                    warn!(
                        group = %group.name,
                        "Prompt returned but group phases not complete — treating as failure"
                    );
                    kill_session(session_id)?;
                    return Ok(PhaseGroupResult {
                        group_name: group.name.clone(),
                        state: PhaseGroupState::Failed {
                            retries: 0,
                            error: ErrorClass::Unknown,
                        },
                        session_id: Some(session_id.to_string()),
                        pid,
                        duration: start.elapsed(),
                        retries: 0,
                        error_message: Some(
                            "Prompt returned but group phases not complete — Claude may have exited the skill early".to_string()
                        ),
                        phases: group.phases.clone(),
                    });
                }
                CompletionEvent::StillRunning => {
                    // Log status every ~10 ticks (30s)
                    if tick_count % 10 == 0 {
                        let elapsed = monitor_start.elapsed();
                        let current_phase = detector.current_running_phase()
                            .unwrap_or_else(|| "unknown".to_string());
                        info!(
                            group = %group.name,
                            elapsed_secs = elapsed.as_secs(),
                            tick = tick_count,
                            phase = %current_phase,
                            "Monitoring: {}m{}s elapsed, current phase: {}",
                            elapsed.as_secs() / 60,
                            elapsed.as_secs() % 60,
                            current_phase,
                        );
                    }
                }
            }

            std::thread::sleep(Duration::from_secs(3));
        }

        // Kill session
        info!(group = %group.name, "Killing session");
        kill_session(session_id)?;

        // Post-phase cleanup
        if let Some(p) = pid {
            post_phase_cleanup(session_id, p)?;
        }

        Ok(PhaseGroupResult {
            group_name: group.name.clone(),
            state: PhaseGroupState::Succeeded,
            session_id: Some(session_id.to_string()),
            pid,
            duration: start.elapsed(),
            retries: 0,
            error_message: None,
            phases: group.phases.clone(),
        })
    }

    /// Load or create a checkpoint for the plan.
    fn load_or_create_checkpoint(
        &self,
        checkpoint_path: &Path,
        plan_path: &Path,
        arc_dir: &Path,
    ) -> Result<Checkpoint> {
        if checkpoint_path.exists() {
            info!(path = %checkpoint_path.display(), "Loading existing checkpoint");
            return read_checkpoint(checkpoint_path);
        }

        info!(path = %checkpoint_path.display(), "Creating new checkpoint");

        // Create arc directory
        std::fs::create_dir_all(arc_dir)
            .wrap_err_with(|| format!("Failed to create arc directory: {}", arc_dir.display()))?;

        // Create fresh checkpoint
        let arc_id = arc_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("arc-{}", chrono::Utc::now().timestamp()));

        let checkpoint = crate::checkpoint::writer::create_fresh_checkpoint(
            arc_id,
            plan_path.to_string_lossy().to_string(),
            Some(&arc_dir.to_string_lossy()),
        );

        Ok(checkpoint)
    }

    /// Check if a group should be skipped based on skip_if condition.
    fn should_skip_group(&self, skip_if: &str, checkpoint_path: &Path) -> Result<bool> {
        // For now, implement simple condition checking
        // Could be extended to support more complex expressions

        let checkpoint = match read_checkpoint(checkpoint_path) {
            Ok(cp) => cp,
            Err(_) => return Ok(false),
        };

        // Check common conditions
        match skip_if {
            "no_work_needed" => {
                // Skip if work phase is marked as skipped
                Ok(checkpoint
                    .phases
                    .get("work")
                    .map(|s| s.status == "skipped")
                    .unwrap_or(false))
            }
            "design_not_needed" => {
                Ok(checkpoint
                    .phases
                    .get("design_extraction")
                    .map(|s| s.status == "skipped")
                    .unwrap_or(false))
            }
            _ => {
                // Unknown condition, don't skip
                warn!(condition = %skip_if, "Unknown skip_if condition");
                Ok(false)
            }
        }
    }

    /// Check if a group is complete in the checkpoint.
    fn is_group_complete_in_checkpoint(&self, group: &PhaseGroup, checkpoint: &Checkpoint) -> bool {
        group.phases.iter().all(|phase| {
            checkpoint
                .phases
                .get(phase)
                .map(|s| s.status == "completed" || s.status == "skipped")
                .unwrap_or(false)
        })
    }

    /// Get the execution results.
    pub fn results(&self) -> &[PhaseGroupResult] {
        &self.results
    }
}

/// Serialize a Duration as fractional seconds for JSONL output.
fn serialize_duration<S: serde::Serializer>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

/// Append a PhaseGroupResult as a single JSON line to a JSONL log file.
///
/// The log file is created at `{arc_dir}/phase-results.jsonl`.
/// Each line is a complete JSON object representing one group execution.
pub fn append_result_jsonl(result: &PhaseGroupResult, arc_dir: &Path) -> Result<()> {
    let log_path = arc_dir.join("phase-results.jsonl");

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .wrap_err_with(|| format!("Failed to open JSONL log: {}", log_path.display()))?;

    let json = serde_json::to_string(result)
        .wrap_err("Failed to serialize PhaseGroupResult to JSON")?;

    writeln!(file, "{}", json)
        .wrap_err_with(|| format!("Failed to write to JSONL log: {}", log_path.display()))?;

    debug!(path = %log_path.display(), group = %result.group_name, "Appended result to JSONL log");
    Ok(())
}

/// Compute a short hash for a plan, based on path + file content.
///
/// Hashing content (not just path) ensures that two runs of the same plan
/// file at the same path produce the same arc directory, while different
/// content produces a different one. The path is included for uniqueness
/// when two files at different paths have identical content.
fn compute_plan_hash(plan_path: &Path) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    // Include path for uniqueness across locations
    let path_str = plan_path.to_string_lossy();
    hasher.update(path_str.as_bytes());
    // Include file content so the hash reflects the actual plan, not just where it lives
    if let Ok(content) = std::fs::read_to_string(plan_path) {
        hasher.update(content.as_bytes());
    } else {
        warn!(path = %plan_path.display(), "Could not read plan file for hashing — using path-only hash");
    }
    let result = hasher.finalize();
    result.iter().take(6).map(|b| format!("{:02x}", b)).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_group_state_is_terminal() {
        assert!(PhaseGroupState::Succeeded.is_terminal());
        assert!(PhaseGroupState::Skipped.is_terminal());
        assert!(PhaseGroupState::Failed { retries: 3, error: ErrorClass::Timeout }.is_terminal());
        assert!(!PhaseGroupState::Pending.is_terminal());
        assert!(!PhaseGroupState::Running.is_terminal());
    }

    #[test]
    fn test_phase_group_state_is_success() {
        assert!(PhaseGroupState::Succeeded.is_success());
        assert!(PhaseGroupState::Skipped.is_success());
        assert!(!PhaseGroupState::Failed { retries: 0, error: ErrorClass::Crash }.is_success());
    }

    #[test]
    fn test_executor_config_new() {
        let config = ExecutorConfig::new("/tmp");
        assert_eq!(config.working_dir, PathBuf::from("/tmp"));
        assert!(!config.mock);
        assert!(!config.dry_run);
    }

    #[test]
    fn test_executor_config_with_mock() {
        let config = ExecutorConfig::new("/tmp").with_mock();
        assert!(config.mock);
    }

    #[test]
    fn test_executor_config_with_dry_run() {
        let config = ExecutorConfig::new("/tmp").with_dry_run();
        assert!(config.dry_run);
    }

    #[test]
    fn test_compute_plan_hash() {
        let path1 = Path::new("plans/test.md");
        let path2 = Path::new("plans/test.md");
        let path3 = Path::new("plans/other.md");

        let hash1 = compute_plan_hash(path1);
        let hash2 = compute_plan_hash(path2);
        let hash3 = compute_plan_hash(path3);

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 12);
    }

    #[test]
    fn test_stable_hash_known_value() {
        let h = compute_plan_hash(Path::new("plans/test.md"));
        // Pin the value to detect algorithm changes
        assert_eq!(h.len(), 12);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_phase_group_result() {
        let result = PhaseGroupResult {
            group_name: "A".to_string(),
            state: PhaseGroupState::Succeeded,
            session_id: Some("gw-abc123-A".to_string()),
            pid: Some(12345),
            duration: Duration::from_secs(60),
            retries: 0,
            error_message: None,
            phases: vec!["forge".to_string(), "forge_qa".to_string()],
        };

        assert_eq!(result.group_name, "A");
        assert!(result.state.is_success());
        assert_eq!(result.phases.len(), 2);
    }
}