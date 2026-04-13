#![allow(dead_code)]
//! Core monitor evaluator — pure-logic phase monitoring.
//!
//! This module separates the monitor's *decision logic* from I/O concerns
//! (tmux reads, process checks, file watches). The [`PhaseMonitor`] trait
//! takes a [`TickInput`] snapshot and returns [`MonitorEvent`]s describing
//! what happened, without performing any side effects.
//!
//! # Architecture
//!
//! ```text
//! Adapter (foreground / daemon)
//!   │
//!   │  collects tmux, checkpoint, process data
//!   ▼
//! TickInput  ──► PhaseMonitor::evaluate_tick() ──► Vec<MonitorEvent>
//!                       │                              │
//!                       ▼                              ▼
//!                  TickHistory (mutated)         Adapter dispatches
//!                                               events (kill, nudge, log)
//! ```
//!
//! # Example
//!
//! ```rust
//! use greater_will::engine::monitor_core::{
//!     DefaultPhaseMonitor, MonitorConfig, MonitorEvent,
//!     PhaseMonitor, TickHistory, TickInput,
//! };
//! use greater_will::engine::phase_profile;
//! use std::time::Duration;
//!
//! let config = MonitorConfig {
//!     idle_nudge_secs: 180,
//!     idle_kill_secs: 1800,
//!     error_stall_threshold_secs: 60,
//!     error_confirm_medium_secs: 300,
//!     error_confirm_high_secs: 300,
//!     pipeline_timeout: Duration::from_secs(7200),
//!     min_completion_age_secs: 300,
//! };
//!
//! let monitor = DefaultPhaseMonitor::new(config);
//! let mut history = TickHistory::new();
//!
//! let input = TickInput {
//!     elapsed_secs: 10,
//!     phase_elapsed_secs: None,
//!     pane_hash: 12345,
//!     pane_tail: "Claude is thinking...",
//!     process_alive: true,
//!     claude_pid: Some(1234),
//!     checkpoint: None,
//!     checkpoint_hash: None,
//!     checkpoint_stale_secs: None,
//!     phase_nav: None,
//!     phase_profile: phase_profile::default_profile(),
//!     effective_phase_timeout_secs: 3600,
//!     loop_state: None,
//!     loop_state_ever_seen: false,
//!     loop_state_gone_secs: None,
//!     artifacts_active: false,
//!     swarm_active: false,
//! };
//!
//! let events = monitor.evaluate_tick(&input, &mut history);
//! assert!(events.is_empty()); // stub returns no events
//! ```

pub mod detectors;
pub mod types;

pub use types::*;

/// Trait for evaluating monitor ticks.
///
/// Object-safe so it can be used as `dyn PhaseMonitor` in tests and
/// alternative implementations. The evaluator receives a borrowed
/// [`TickInput`] snapshot, mutates [`TickHistory`] for cross-tick state,
/// and returns zero or more [`MonitorEvent`]s.
///
/// # Object Safety
///
/// This trait is object-safe: `evaluate_tick` takes `&self` and returns
/// a concrete `Vec<MonitorEvent>`, with no `Self` in return position.
pub trait PhaseMonitor {
    /// Evaluate a single monitoring tick.
    ///
    /// Called once per poll interval (~5 s). The implementation should:
    /// 1. Compare `input` against `history` to detect changes.
    /// 2. Update `history` with the new state.
    /// 3. Return events describing any actions the adapter should take.
    fn evaluate_tick(&self, input: &TickInput, history: &mut TickHistory) -> Vec<MonitorEvent>;
}

/// Default implementation of [`PhaseMonitor`].
///
/// Holds a [`MonitorConfig`] and will eventually delegate to individual
/// detector functions in the [`detectors`] submodule. Currently returns
/// an empty event list (stub).
pub struct DefaultPhaseMonitor {
    config: MonitorConfig,
}

impl DefaultPhaseMonitor {
    /// Create a new monitor with the given configuration.
    pub fn new(config: MonitorConfig) -> Self {
        Self { config }
    }

    /// Borrow the monitor configuration.
    pub fn config(&self) -> &MonitorConfig {
        &self.config
    }
}

impl PhaseMonitor for DefaultPhaseMonitor {
    fn evaluate_tick(&self, _input: &TickInput, _history: &mut TickHistory) -> Vec<MonitorEvent> {
        // Stub — will call detectors once Task 2 populates them.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::phase_profile;
    use std::time::Duration;

    fn test_config() -> MonitorConfig {
        MonitorConfig {
            idle_nudge_secs: 180,
            idle_kill_secs: 1800,
            error_stall_threshold_secs: 60,
            error_confirm_medium_secs: 300,
            error_confirm_high_secs: 300,
            pipeline_timeout: Duration::from_secs(7200),
            min_completion_age_secs: 300,
        }
    }

    #[test]
    fn stub_returns_no_events() {
        let monitor = DefaultPhaseMonitor::new(test_config());
        let mut history = TickHistory::new();
        let input = TickInput {
            elapsed_secs: 10,
            phase_elapsed_secs: None,
            pane_hash: 12345,
            pane_tail: "",
            process_alive: true,
            claude_pid: None,
            checkpoint: None,
            checkpoint_hash: None,
            checkpoint_stale_secs: None,
            phase_nav: None,
            phase_profile: phase_profile::default_profile(),
            effective_phase_timeout_secs: 3600,
            loop_state: None,
            loop_state_ever_seen: false,
            loop_state_gone_secs: None,
            artifacts_active: false,
            swarm_active: false,
        };
        let events = monitor.evaluate_tick(&input, &mut history);
        assert!(events.is_empty());
    }

    #[test]
    fn trait_is_object_safe() {
        let monitor = DefaultPhaseMonitor::new(test_config());
        // Proves the trait can be used as a trait object.
        let _dyn_ref: &dyn PhaseMonitor = &monitor;
    }

    #[test]
    fn tick_history_default() {
        let h = TickHistory::default();
        assert_eq!(h.nudge_count, 0);
        assert_eq!(h.poll_count, 0);
        assert!(h.pending_kill.is_none());
    }
}
