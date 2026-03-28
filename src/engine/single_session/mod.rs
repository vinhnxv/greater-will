#![allow(dead_code)]
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
//!
//! # Module structure
//!
//! - [`orchestrator`] - Crash-recovery loop and batch execution
//! - [`monitor`] - Session monitoring loop (idle, phase tracking, error detection)
//! - [`util`] - Command builders, activity snapshots, checkpoint helpers

mod monitor;
mod orchestrator;
mod util;

use crate::config::watchdog::WatchdogConfig;
use std::path::PathBuf;
use std::time::Duration;

// Re-export the public API
pub use orchestrator::{run_single_session, run_single_session_batch};

/// Poll interval for monitoring the session (seconds).
pub(crate) const POLL_INTERVAL_SECS: u64 = 5;

/// How often to print a status line during monitoring (seconds).
pub(crate) const STATUS_LOG_INTERVAL_SECS: u64 = 30;

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

/// Outcome of a single session attempt.
#[derive(Debug)]
pub(crate) enum SessionOutcome {
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
        error_class: crate::engine::retry::ErrorClass,
        reason: String,
    },
}
