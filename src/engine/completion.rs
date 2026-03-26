//! 4-layer completion detection for phase group monitoring.
//!
//! This module implements the detection system that monitors running
//! phase groups and determines when they have completed (or failed).
//!
//! # Detection Layers
//!
//! 1. **Checkpoint Polling (PRIMARY)**: Poll `checkpoint.json` every 3 seconds
//!    for status changes. When the last phase in a group shows "completed",
//!    the group is done.
//!
//! 2. **Phase Timeout (SAFETY NET)**: Each group has a configurable timeout.
//!    If exceeded, the session is killed and classified as `ErrorClass::Timeout`.
//!
//! 3. **Idle Detection + Nudge (ANTI-STALL)**: Monitor pane output hash.
//!    After 3 minutes of no changes, send "please continue" via send-keys.
//!    After 5 minutes, kill session and classify as `ErrorClass::Stuck`.
//!
//! 4. **Prompt Return Detection (SECONDARY)**: Detect `❯` prompt in last line.
//!    If prompt appears but checkpoint hasn't updated, wait 5s and recheck.
//!    Used as secondary signal, not primary (prompt can appear mid-execution).
//!
//! # Example
//!
//! ```ignore
//! use greater_will::engine::completion::{CompletionDetector, CompletionConfig};
//!
//! let config = CompletionConfig::default();
//! let mut detector = CompletionDetector::new(config, checkpoint_path, group_phases);
//!
//! loop {
//!     match detector.tick(&session)? {
//!         CompletionEvent::Completed => break,
//!         CompletionEvent::StillRunning => continue,
//!         CompletionEvent::Nudge => session.send_keys("please continue")?,
//!         CompletionEvent::Timeout => return Err(ErrorClass::Timeout),
//!         CompletionEvent::Stuck => return Err(ErrorClass::Stuck),
//!     }
//!     thread::sleep(Duration::from_secs(3));
//! }
//! ```

use crate::checkpoint::phase_order::phase_index;
use crate::checkpoint::reader::read_checkpoint;
use crate::checkpoint::schema::Checkpoint;
use crate::engine::retry::ErrorClass;
use color_eyre::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Default polling interval for checkpoint reads.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 3;

/// Default idle duration before nudging (3 minutes).
const DEFAULT_IDLE_NUDGE_SECS: u64 = 180;

/// Default idle duration before killing (5 minutes).
const DEFAULT_IDLE_KILL_SECS: u64 = 300;

/// Default grace period after prompt detection (5 seconds).
const DEFAULT_PROMPT_GRACE_SECS: u64 = 5;

/// Configuration for completion detection.
#[derive(Debug, Clone)]
pub struct CompletionConfig {
    /// Interval between checkpoint polls.
    pub poll_interval: Duration,
    /// Idle duration before nudging.
    pub idle_nudge_after: Duration,
    /// Idle duration before killing session.
    pub idle_kill_after: Duration,
    /// Grace period after prompt detection.
    pub prompt_grace_period: Duration,
    /// Phase timeout for this group.
    pub phase_timeout: Duration,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            idle_nudge_after: Duration::from_secs(DEFAULT_IDLE_NUDGE_SECS),
            idle_kill_after: Duration::from_secs(DEFAULT_IDLE_KILL_SECS),
            prompt_grace_period: Duration::from_secs(DEFAULT_PROMPT_GRACE_SECS),
            phase_timeout: Duration::from_secs(30 * 60), // 30 min default
        }
    }
}

impl CompletionConfig {
    /// Create config with custom timeouts for a specific group.
    pub fn for_group(timeout_min: u32, idle_nudge_sec: u64, idle_kill_sec: u64) -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            idle_nudge_after: Duration::from_secs(idle_nudge_sec),
            idle_kill_after: Duration::from_secs(idle_kill_sec),
            prompt_grace_period: Duration::from_secs(DEFAULT_PROMPT_GRACE_SECS),
            phase_timeout: Duration::from_secs(timeout_min as u64 * 60),
        }
    }
}

/// State tracked by the completion detector.
#[derive(Debug, Clone)]
pub struct CompletionState {
    /// When detection started.
    pub started_at: Instant,
    /// Last checkpoint read (for comparison).
    pub last_checkpoint: Option<Checkpoint>,
    /// Hash of last pane content (for idle detection).
    pub last_pane_hash: Option<u64>,
    /// When the pane content last changed.
    pub last_activity: Instant,
    /// Whether we've already nudged.
    pub nudged: bool,
    /// When prompt was detected (for grace period).
    pub prompt_detected_at: Option<Instant>,
    /// Phase sequence when detection started.
    pub starting_phase_sequence: Option<u32>,
}

impl Default for CompletionState {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            last_checkpoint: None,
            last_pane_hash: None,
            last_activity: Instant::now(),
            nudged: false,
            prompt_detected_at: None,
            starting_phase_sequence: None,
        }
    }
}

/// Events emitted by the completion detector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEvent {
    /// Group completed successfully.
    Completed,
    /// Group failed (checkpoint shows failed status).
    Failed { phase: String },
    /// Still running, no action needed.
    StillRunning,
    /// Session is idle, should nudge.
    Nudge,
    /// Session has been idle too long, should kill.
    Stuck,
    /// Phase timeout exceeded.
    Timeout,
    /// Prompt detected, checkpoint unchanged after grace period.
    PromptReturned,
    /// An error pattern was detected in the pane output.
    ErrorDetected { error_class: ErrorClass },
}

/// Completion detector for a single phase group.
///
/// Implements the 4-layer detection system:
/// 1. Checkpoint polling (primary)
/// 2. Phase timeout (safety net)
/// 3. Idle detection + nudge (anti-stall)
/// 4. Prompt return detection (secondary)
pub struct CompletionDetector {
    /// Configuration.
    config: CompletionConfig,
    /// Path to checkpoint.json.
    checkpoint_path: PathBuf,
    /// Phases in this group (in order).
    group_phases: Vec<String>,
    /// Current detection state.
    state: CompletionState,
}

impl CompletionDetector {
    /// Create a new completion detector.
    ///
    /// # Arguments
    ///
    /// * `config` - Detection configuration
    /// * `checkpoint_path` - Path to the checkpoint.json file
    /// * `group_phases` - List of phase names in this group
    pub fn new(
        config: CompletionConfig,
        checkpoint_path: PathBuf,
        group_phases: Vec<String>,
    ) -> Self {
        Self {
            config,
            checkpoint_path,
            group_phases,
            state: CompletionState::default(),
        }
    }

    /// Get the current detection state.
    pub fn state(&self) -> &CompletionState {
        &self.state
    }

    /// Get elapsed time since detection started.
    pub fn elapsed(&self) -> Duration {
        self.state.started_at.elapsed()
    }

    /// Perform a single detection tick.
    ///
    /// This should be called every `config.poll_interval`.
    ///
    /// # Arguments
    ///
    /// * `pane_content` - Current content of the tmux pane (for idle detection)
    ///
    /// # Returns
    ///
    /// A `CompletionEvent` indicating what action to take.
    pub fn tick(&mut self, pane_content: &str) -> Result<CompletionEvent> {
        // Layer 2: Check for phase timeout
        if self.elapsed() > self.config.phase_timeout {
            tracing::warn!(
                elapsed_secs = self.elapsed().as_secs(),
                timeout_secs = self.config.phase_timeout.as_secs(),
                "Phase timeout exceeded"
            );
            return Ok(CompletionEvent::Timeout);
        }

        // Layer 0 (highest priority after timeout): Check for error patterns.
        // Detects rate limits, auth errors, overload — must fire before idle/prompt
        // checks, otherwise the session wastes minutes appearing "stuck" when
        // the real cause is an API error.
        if let Some(error_class) = ErrorClass::from_pane_output(pane_content) {
            tracing::warn!(error_class = ?error_class, "Error pattern detected in pane output");
            return Ok(CompletionEvent::ErrorDetected { error_class });
        }

        // Layer 1: Read checkpoint
        let checkpoint_result = read_checkpoint(&self.checkpoint_path);

        match checkpoint_result {
            Ok(checkpoint) => {
                // Initialize starting phase sequence if not set
                if self.state.starting_phase_sequence.is_none() {
                    self.state.starting_phase_sequence = checkpoint.phase_sequence;
                }

                // Check if group is complete
                if self.is_group_complete(&checkpoint) {
                    tracing::info!("Group completed successfully");
                    return Ok(CompletionEvent::Completed);
                }

                // Check for failed phase
                if let Some(failed_phase) = self.find_failed_phase(&checkpoint) {
                    tracing::warn!(phase = %failed_phase, "Phase failed");
                    return Ok(CompletionEvent::Failed {
                        phase: failed_phase,
                    });
                }

                // Detect checkpoint changes and log them
                if let Some(ref prev) = self.state.last_checkpoint {
                    for phase_name in &self.group_phases {
                        let prev_status = prev.phases.get(phase_name).map(|s| s.status.as_str());
                        let curr_status = checkpoint.phases.get(phase_name).map(|s| s.status.as_str());
                        if prev_status != curr_status {
                            tracing::info!(
                                phase = %phase_name,
                                from = ?prev_status,
                                to = ?curr_status,
                                "Phase status changed: {} -> {}",
                                prev_status.unwrap_or("none"),
                                curr_status.unwrap_or("none"),
                            );
                        }
                    }
                }

                // Update state
                self.state.last_checkpoint = Some(checkpoint);
            }
            Err(e) => {
                // Checkpoint might not exist yet (early in phase)
                tracing::debug!(
                    error = %e,
                    "Could not read checkpoint (may be early in phase)"
                );
            }
        }

        // Layer 4: Check for prompt return
        let prompt_returned = self.detect_prompt(pane_content);
        if prompt_returned {
            if let Some(detected_at) = self.state.prompt_detected_at {
                // Prompt was already detected, check grace period
                if detected_at.elapsed() > self.config.prompt_grace_period {
                    // Re-read checkpoint to see if it updated
                    if let Ok(checkpoint) = read_checkpoint(&self.checkpoint_path) {
                        if self.is_group_complete(&checkpoint) {
                            return Ok(CompletionEvent::Completed);
                        }
                    }
                    // Checkpoint unchanged after grace period
                    tracing::info!("Prompt detected but checkpoint unchanged after grace period");
                    return Ok(CompletionEvent::PromptReturned);
                }
            } else {
                // First time detecting prompt
                self.state.prompt_detected_at = Some(Instant::now());
                tracing::debug!("Prompt detected, starting grace period");
            }
        }

        // Layer 3: Check for idle
        let pane_hash = compute_pane_hash(pane_content);

        if let Some(last_hash) = self.state.last_pane_hash {
            if pane_hash != last_hash {
                // Pane content changed, update activity time
                self.state.last_activity = Instant::now();
                self.state.nudged = false;
                self.state.prompt_detected_at = None; // Reset prompt detection
            } else {
                // Content unchanged - check idle thresholds
                let idle_duration = self.state.last_activity.elapsed();

                if idle_duration > self.config.idle_kill_after {
                    tracing::warn!(
                        idle_secs = idle_duration.as_secs(),
                        kill_after_secs = self.config.idle_kill_after.as_secs(),
                        "Session stuck (idle too long)"
                    );
                    return Ok(CompletionEvent::Stuck);
                }

                if idle_duration > self.config.idle_nudge_after && !self.state.nudged {
                    tracing::info!(
                        idle_secs = idle_duration.as_secs(),
                        "Session idle, should nudge"
                    );
                    return Ok(CompletionEvent::Nudge);
                }
            }
        }

        self.state.last_pane_hash = Some(pane_hash);

        Ok(CompletionEvent::StillRunning)
    }

    /// Mark that a nudge was sent.
    pub fn mark_nudged(&mut self) {
        self.state.nudged = true;
    }

    /// Check if all phases in the group are complete (or skipped).
    fn is_group_complete(&self, checkpoint: &Checkpoint) -> bool {
        self.group_phases.iter().all(|phase| {
            checkpoint
                .phases
                .get(phase)
                .map(|s| s.status == "completed" || s.status == "skipped")
                .unwrap_or(false)
        })
    }

    /// Find the first failed phase in the group.
    fn find_failed_phase(&self, checkpoint: &Checkpoint) -> Option<String> {
        self.group_phases
            .iter()
            .find(|phase| {
                checkpoint
                    .phases
                    .get(*phase)
                    .map(|s| s.status == "failed")
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// Detect if the prompt symbol is in the last line.
    fn detect_prompt(&self, pane_content: &str) -> bool {
        let last_line = pane_content
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.to_string());

        match last_line {
            Some(line) => line.contains('❯'),
            None => false,
        }
    }

    /// Get the current running phase from checkpoint.
    ///
    /// Returns the first phase with "in_progress" status,
    /// or the first "pending" phase after all completed phases.
    pub fn current_running_phase(&self) -> Option<String> {
        let checkpoint = self.state.last_checkpoint.as_ref()?;

        for phase_name in &self.group_phases {
            if let Some(status) = checkpoint.phases.get(phase_name) {
                match status.status.as_str() {
                    "in_progress" => return Some(phase_name.clone()),
                    "pending" => return Some(phase_name.clone()),
                    "completed" | "skipped" | "failed" => continue,
                    _ => continue,
                }
            } else {
                // Phase not in map = hasn't been reached yet
                return Some(phase_name.clone());
            }
        }

        None
    }

    /// Check if phase sequence has advanced beyond this group.
    pub fn has_advanced_past_group(&self) -> bool {
        let checkpoint = match self.state.last_checkpoint.as_ref() {
            Some(cp) => cp,
            None => return false,
        };

        let phase_seq = match checkpoint.phase_sequence {
            Some(seq) => seq as usize,
            None => return false,
        };

        // Get the last phase in our group
        let last_phase = match self.group_phases.last() {
            Some(p) => p,
            None => return false,
        };

        // Check if phase_sequence is past our last phase
        if let Some(last_idx) = phase_index(last_phase) {
            return phase_seq > last_idx;
        }

        false
    }
}

/// Compute a hash of pane content for idle detection.
///
/// Uses a simple DefaultHasher for speed.
pub fn compute_pane_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Capture pane content from tmux session.
///
/// Returns the visible text in the current pane.
pub fn capture_pane_content(session_id: &str) -> Result<String> {
    let output = std::process::Command::new("tmux")
        .args(["capture-pane", "-t", session_id, "-p"])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Capture the last line from the tmux pane.
///
/// Skips empty lines.
pub fn capture_last_line(session_id: &str) -> Result<Option<String>> {
    let content = capture_pane_content(session_id)?;

    let last_line = content
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.to_string());

    Ok(last_line)
}

/// Check if a prompt (❯) is present in the last line.
pub fn has_prompt_in_last_line(session_id: &str) -> Result<bool> {
    let last_line = capture_last_line(session_id)?;

    match last_line {
        Some(line) => Ok(line.contains('❯')),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_config_default() {
        let config = CompletionConfig::default();
        assert_eq!(config.poll_interval, Duration::from_secs(3));
        assert_eq!(config.idle_nudge_after, Duration::from_secs(180));
        assert_eq!(config.idle_kill_after, Duration::from_secs(300));
    }

    #[test]
    fn test_completion_config_for_group() {
        let config = CompletionConfig::for_group(45, 120, 240);
        assert_eq!(config.phase_timeout, Duration::from_secs(45 * 60));
        assert_eq!(config.idle_nudge_after, Duration::from_secs(120));
        assert_eq!(config.idle_kill_after, Duration::from_secs(240));
    }

    #[test]
    fn test_compute_pane_hash() {
        let content1 = "Hello world";
        let content2 = "Hello world";
        let content3 = "Different content";

        let hash1 = compute_pane_hash(content1);
        let hash2 = compute_pane_hash(content2);
        let hash3 = compute_pane_hash(content3);

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_detect_prompt_present() {
        let content = "Some output\nMore output\n❯ ";
        let detector = CompletionDetector::new(
            CompletionConfig::default(),
            PathBuf::from("/nonexistent"),
            vec!["forge".to_string()],
        );

        assert!(detector.detect_prompt(content));
    }

    #[test]
    fn test_detect_prompt_absent() {
        let content = "Some output\nMore output\nStill running...";
        let detector = CompletionDetector::new(
            CompletionConfig::default(),
            PathBuf::from("/nonexistent"),
            vec!["forge".to_string()],
        );

        assert!(!detector.detect_prompt(content));
    }

    #[test]
    fn test_detect_prompt_with_empty_lines() {
        let content = "Output\n\n\n   \n❯ prompt";
        let detector = CompletionDetector::new(
            CompletionConfig::default(),
            PathBuf::from("/nonexistent"),
            vec!["forge".to_string()],
        );

        assert!(detector.detect_prompt(content));
    }

    #[test]
    fn test_completion_state_default() {
        let state = CompletionState::default();
        assert!(state.last_checkpoint.is_none());
        assert!(state.last_pane_hash.is_none());
        assert!(!state.nudged);
        assert!(state.prompt_detected_at.is_none());
    }
}