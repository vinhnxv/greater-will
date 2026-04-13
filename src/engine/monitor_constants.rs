//! Timing constants shared between the foreground orchestrator
//! ([`crate::engine::single_session::monitor`]) and the daemon run monitor
//! ([`crate::daemon::run_monitor`]).
//!
//! Any divergence between paths must be declared here with an explicit
//! justification, never as a silent copy in each module. Before this module
//! existed, both files redeclared these constants, and `MIN_COMPLETION_AGE_SECS`
//! silently diverged (300 s foreground vs 60 s daemon — see DET-6).
//!
//! Unit: seconds.

/// Minimum session age before a vanished session is treated as
/// "likely completed" rather than "crashed". Foreground path.
///
/// Kept at 300 s (5 min) because the foreground orchestrator has no
/// additional disambiguation signals — a long grace window prevents
/// misclassifying fast-completing sessions as crashes.
pub const FOREGROUND_MIN_COMPLETION_AGE_SECS: u64 = 300;

/// Daemon counterpart to [`FOREGROUND_MIN_COMPLETION_AGE_SECS`].
///
/// Lower (60 s) because the daemon path has additional checkpoint-based
/// disambiguation (see DET-6 in `daemon/run_monitor.rs`): when a session
/// vanishes, the daemon also consults the arc-phase-loop state file and
/// the checkpoint to distinguish completion from crash, so the age gate
/// can fire sooner without false positives.
///
/// The divergence is intentional. The regression-pin test
/// [`tests/monitor_constants_pin`] asserts both values so any future
/// "align these" refactor surfaces as a deliberate decision.
pub const DAEMON_MIN_COMPLETION_AGE_SECS: u64 = 60;

/// Silent-pane threshold before the kill gate begins to arm. Both paths.
pub const KILL_GATE_MIN_SECS: u64 = 300;

/// Grace period after a completion signal before declaring the session
/// done. Both paths.
pub const COMPLETION_GRACE_SECS: u64 = 300;

/// Poll interval between monitor iterations. Both paths.
pub const POLL_INTERVAL_SECS: u64 = 5;

/// Interval between periodic `[gw] status` log lines. Both paths.
pub const STATUS_LOG_INTERVAL_SECS: u64 = 30;

/// How long `arc-phase-loop.local.md` may be missing before we treat the
/// session as crashed (vs. in between phases). Both paths.
pub const LOOP_STATE_GONE_GRACE_SECS: u64 = 300;

/// Activity-within-window that cancels a pending kill request. Both paths.
pub const KILL_GATE_RECOVERY_WINDOW_SECS: u64 = 30;

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression pin: the documented divergence between foreground and
    /// daemon must not be silently aligned. A change here forces an
    /// explicit review and commit message.
    #[test]
    fn min_completion_age_divergence_is_pinned() {
        assert_eq!(FOREGROUND_MIN_COMPLETION_AGE_SECS, 300);
        assert_eq!(DAEMON_MIN_COMPLETION_AGE_SECS, 60);
    }

    #[test]
    fn shared_constants_have_expected_values() {
        assert_eq!(KILL_GATE_MIN_SECS, 300);
        assert_eq!(COMPLETION_GRACE_SECS, 300);
        assert_eq!(POLL_INTERVAL_SECS, 5);
        assert_eq!(STATUS_LOG_INTERVAL_SECS, 30);
        assert_eq!(LOOP_STATE_GONE_GRACE_SECS, 300);
        assert_eq!(KILL_GATE_RECOVERY_WINDOW_SECS, 30);
    }
}
