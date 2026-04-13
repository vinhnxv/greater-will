#![allow(dead_code)]
//! Core types for the phase monitor evaluator.
//!
//! These types decouple the monitor's *evaluation logic* from the I/O and
//! async plumbing that surrounds it. The evaluator receives a [`TickInput`]
//! snapshot, updates [`TickHistory`] across ticks, and returns a `Vec<MonitorEvent>`
//! describing what happened.
//!
//! # Design Constraints
//!
//! - **Zero async**: no tokio, no thread::sleep — pure computation.
//! - **Borrow-friendly**: [`TickInput`] borrows pane text and checkpoint data
//!   so the caller keeps ownership.
//! - **Object-safe trait**: [`super::PhaseMonitor`] returns `Vec<MonitorEvent>`,
//!   not `Self`, so it can be used as `dyn PhaseMonitor`.

use std::time::Duration;

use crate::checkpoint::schema::Checkpoint;
use crate::engine::phase_profile::PhaseProfile;
use crate::engine::retry::ErrorClass;
use crate::monitor::loop_state::ArcLoopState;
use crate::monitor::phase_nav::PhaseNavigation;

// ──────────────────────────────────────────────────────────────────────
// MonitorConfig
// ──────────────────────────────────────────────────────────────────────

/// Configuration subset carried into the evaluator.
///
/// Mirrors the fields from [`crate::config::watchdog::WatchdogConfig`] that
/// detectors actually read. Constructed once at monitor start and passed to
/// [`super::DefaultPhaseMonitor::new`].
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Idle time (seconds) before sending a nudge keystroke.
    pub idle_nudge_secs: u64,
    /// Idle time (seconds) before requesting a kill.
    pub idle_kill_secs: u64,
    /// Staleness threshold for error-confidence scoring.
    pub error_stall_threshold_secs: u64,
    /// Confirmation period for medium-confidence errors (0.5–0.8).
    pub error_confirm_medium_secs: u64,
    /// Confirmation period for high-confidence errors (≥ 0.8).
    pub error_confirm_high_secs: u64,
    /// Total pipeline timeout.
    pub pipeline_timeout: Duration,
    /// Minimum session age (seconds) before a vanished process is treated
    /// as "likely completed" rather than "crashed".
    pub min_completion_age_secs: u64,
}

// ──────────────────────────────────────────────────────────────────────
// TickInput
// ──────────────────────────────────────────────────────────────────────

/// Snapshot of all data needed for one evaluation tick.
///
/// The adapter (foreground or daemon) collects these values from tmux,
/// the filesystem, and process tables, then hands the snapshot to
/// [`super::PhaseMonitor::evaluate_tick`].
///
/// Borrows are used for large or already-owned data (pane text,
/// checkpoint) to avoid unnecessary cloning on every 5-second tick.
#[derive(Debug)]
pub struct TickInput<'a> {
    // ── Timing ───────────────────────────────────────────────────────
    /// Seconds since the run/session was dispatched.
    pub elapsed_secs: u64,

    /// Seconds since the current phase started (if known).
    pub phase_elapsed_secs: Option<u64>,

    // ── Screen state ─────────────────────────────────────────────────
    /// Hash of the current tmux pane content.
    pub pane_hash: u64,

    /// Tail of the tmux pane output (last N lines).
    pub pane_tail: &'a str,

    // ── Process liveness ─────────────────────────────────────────────
    /// Whether the Claude Code process is still alive.
    pub process_alive: bool,

    /// PID of the Claude Code process, if known.
    pub claude_pid: Option<u32>,

    // ── Checkpoint data ──────────────────────────────────────────────
    /// Latest checkpoint snapshot (if one has been read successfully).
    pub checkpoint: Option<&'a Checkpoint>,

    /// Hash of the checkpoint file content (for change detection).
    pub checkpoint_hash: Option<u64>,

    /// Seconds since the checkpoint last changed.
    pub checkpoint_stale_secs: Option<u64>,

    // ── Phase navigation ─────────────────────────────────────────────
    /// Inferred phase position from checkpoint data.
    pub phase_nav: Option<&'a PhaseNavigation>,

    /// Current phase profile (timeout, idle thresholds, etc.).
    pub phase_profile: PhaseProfile,

    /// Effective phase timeout for the current phase (resolved from
    /// checkpoint phase_times or profile fallback).
    pub effective_phase_timeout_secs: u64,

    // ── Loop state ───────────────────────────────────────────────────
    /// Current arc loop state (parsed from `arc-phase-loop.local.md`).
    pub loop_state: Option<&'a ArcLoopState>,

    /// Whether the loop state file has ever been seen this session.
    pub loop_state_ever_seen: bool,

    /// Seconds since the loop state file disappeared (if it has).
    pub loop_state_gone_secs: Option<u64>,

    // ── Artifact activity ────────────────────────────────────────────
    /// Whether artifact files have been modified recently.
    pub artifacts_active: bool,

    /// Whether claude-swarm teammate processes are running.
    pub swarm_active: bool,
}

// ──────────────────────────────────────────────────────────────────────
// TickHistory
// ──────────────────────────────────────────────────────────────────────

/// Cross-tick state maintained by the adapter between evaluations.
///
/// The evaluator reads and mutates this on every tick. The adapter
/// creates it once at monitor start and passes `&mut` on each tick.
///
/// Fields are grouped by concern to match the eventual detector
/// decomposition (idle, phase, error, kill-gate, etc.).
#[derive(Debug)]
pub struct TickHistory {
    // ── Activity tracking ────────────────────────────────────────────
    /// Hash of the pane content on the previous tick.
    pub last_pane_hash: u64,

    /// Seconds since last detected screen activity.
    pub idle_secs: u64,

    /// Number of nudge keystrokes sent so far.
    pub nudge_count: u32,

    /// Total ticks elapsed.
    pub poll_count: u64,

    // ── Phase tracking ───────────────────────────────────────────────
    /// Name of the phase that was active on the previous tick.
    pub prev_phase_name: Option<String>,

    /// Previous loop state snapshot (for diff detection).
    pub prev_loop_state: Option<ArcLoopState>,

    /// Seconds since the last loop-state content change.
    pub loop_state_unchanged_secs: u64,

    // ── Transition / failed gap tracking ─────────────────────────────
    /// Seconds spent in the current transition gap (between phases).
    pub transition_gap_secs: Option<u64>,

    /// Number of transition-gap nudges sent.
    pub transition_nudge_count: u32,

    /// Seconds spent with a failed phase unresolved.
    pub failed_gap_secs: Option<u64>,

    /// Number of failed-phase nudges sent.
    pub failed_nudge_count: u32,

    // ── Error evidence ───────────────────────────────────────────────
    /// Active error confirmation timer: (seconds active, class, confidence).
    pub error_confirm: Option<(u64, ErrorClass, f64)>,

    // ── Kill gate ────────────────────────────────────────────────────
    /// Pending kill request waiting for confirmation period to expire.
    pub pending_kill: Option<PendingKill>,

    // ── Completion ───────────────────────────────────────────────────
    /// Seconds since a completion signal was first detected (grace period).
    pub completion_detected_secs: Option<u64>,

    // ── Checkpoint tracking ──────────────────────────────────────────
    /// Hash of the last-seen checkpoint (for change detection).
    pub last_checkpoint_hash: Option<u64>,
}

/// A pending kill request tracked inside [`TickHistory`].
#[derive(Debug, Clone)]
pub struct PendingKill {
    /// Human-readable reason for the kill.
    pub reason: String,
    /// Error classification, if the kill is error-driven.
    pub error_class: Option<ErrorClass>,
    /// Outcome label for logging ("stuck", "crashed", "timeout", etc.).
    pub outcome: &'static str,
    /// Seconds the kill request has been pending.
    pub pending_secs: u64,
    /// Whether a midway nudge has been sent during the gate period.
    pub nudge_sent: bool,
}

impl TickHistory {
    /// Create a fresh history with all counters at zero.
    pub fn new() -> Self {
        Self {
            last_pane_hash: 0,
            idle_secs: 0,
            nudge_count: 0,
            poll_count: 0,
            prev_phase_name: None,
            prev_loop_state: None,
            loop_state_unchanged_secs: 0,
            transition_gap_secs: None,
            transition_nudge_count: 0,
            failed_gap_secs: None,
            failed_nudge_count: 0,
            error_confirm: None,
            pending_kill: None,
            completion_detected_secs: None,
            last_checkpoint_hash: None,
        }
    }
}

impl Default for TickHistory {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// MonitorEvent
// ──────────────────────────────────────────────────────────────────────

/// Events emitted by [`super::PhaseMonitor::evaluate_tick`].
///
/// Each variant represents an action the adapter should take or a
/// state transition it should record. The evaluator never performs I/O
/// itself — it only *describes* what should happen.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    /// The monitored session has transitioned to a new phase.
    PhaseTransition {
        /// Previous phase name (if any).
        from: Option<String>,
        /// New phase name.
        to: String,
    },

    /// The session appears idle — send a nudge keystroke.
    Nudge {
        /// How many seconds the screen has been idle.
        idle_secs: u64,
        /// Which nudge attempt this is (1-based).
        nudge_number: u32,
    },

    /// The session is stuck in a transition gap between phases.
    TransitionStuck {
        /// Seconds spent in the gap.
        gap_secs: u64,
        /// Nudge attempt number within this gap.
        nudge_number: u32,
    },

    /// A failed phase has not been resolved by Rune's own retry system.
    FailedPhaseStuck {
        /// Seconds since the failure was detected.
        gap_secs: u64,
        /// Nudge attempt number for this failure.
        nudge_number: u32,
    },

    /// Error evidence has reached the confidence threshold.
    ErrorDetected {
        /// Classified error type.
        error_class: ErrorClass,
        /// Combined confidence score (0.0–1.0).
        confidence: f64,
    },

    /// The session appears crashed (process gone, not a clean completion).
    Crashed {
        /// Human-readable description of the crash.
        reason: String,
    },

    /// The kill gate has confirmed — request session termination.
    Kill {
        /// Why the kill was requested.
        reason: String,
        /// Error classification for retry decisions.
        error_class: Option<ErrorClass>,
        /// Outcome label ("stuck", "crashed", "timeout", "error").
        outcome: &'static str,
    },

    /// A pending kill was cancelled because recovery signals appeared.
    KillCancelled {
        /// What recovery signal was detected.
        recovery_signal: String,
    },

    /// The session appears to have completed successfully.
    Completed {
        /// Seconds since the completion signal was first seen.
        grace_elapsed_secs: u64,
    },

    /// The arc loop state file disappeared after being seen.
    LoopStateGone {
        /// Seconds since the file was last seen.
        gone_secs: u64,
    },

    /// Periodic status log event (emitted every STATUS_LOG_INTERVAL_SECS).
    StatusLog {
        /// Total ticks elapsed.
        poll_count: u64,
        /// Total elapsed seconds.
        elapsed_secs: u64,
        /// Current phase name, if known.
        phase: Option<String>,
    },
}
