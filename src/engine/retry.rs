//! Retry logic, error classification, and evidence-based error detection.
//!
//! This module implements the retry system that handles failures during
//! phase group execution. Each error type has a specific backoff curve
//! and maximum retry count.
//!
//! # Evidence-Based Error Detection
//!
//! Error classification uses [`ErrorEvidence`] to combine multiple signals
//! (keyword match, screen stall, checkpoint staleness, process liveness)
//! into a confidence score. No single signal triggers a kill decision.
//! See [`ErrorEvidence::confidence`] for the scoring model.
//!
//! # Error Classes
//!
//! | ErrorClass | Backoff | Max Retries |
//! |------------|---------|-------------|
//! | Crash | 30s flat | 3 |
//! | Stuck | 30s flat | 2 |
//! | Timeout | 30s flat | 3 |
//! | ApiOverload | 15→30→60→120 min | 4 |
//! | AuthError | 0 (skip plan) | 0 |
//! | Unknown | 60s flat | 1 |
//!
//! # Example
//!
//! ```ignore
//! use greater_will::engine::retry::{RetryCoordinator, ErrorClass};
//!
//! let mut coordinator = RetryCoordinator::new();
//!
//! match coordinator.should_retry(ErrorClass::Timeout) {
//!     RetryDecision::Retry { after } => {
//!         println!("Retry in {:?}", after);
//!     }
//!     RetryDecision::SkipPlan => {
//!         println!("Skipping plan due to fatal error");
//!     }
//!     RetryDecision::Exhausted => {
//!         println!("Max retries exceeded");
//!     }
//! }
//! ```

use serde::Serialize;
use std::time::Duration;

/// Minimum stall duration before error keywords are considered meaningful.
/// Below this threshold, keyword matches are ignored because Claude Code
/// is likely still actively working.
///
/// Set to 60s because Claude routinely "thinks" for 30-45s without screen
/// updates. A 30s threshold caused false-positive kills during normal
/// thinking pauses.
pub const ERROR_STALL_THRESHOLD_SECS: u64 = 60;

/// Grace period before declaring a process crash (D7).
///
/// Claude Code self-updates, Rune plugin recovery, and teammate shutdown
/// can take several minutes. Using 5-minute grace period to avoid false
/// positive crash detection. Minimum 5 minutes per GW kill gate rule.
pub const CRASH_GRACE_SECS: u64 = 300;

/// Evidence collected for error diagnosis.
///
/// Each signal contributes a confidence weight. Only when combined confidence
/// exceeds the action threshold does `gw` take action (kill/retry/skip).
///
/// # Confidence Model
///
/// | Signal | Weight | Rationale |
/// |--------|--------|-----------|
/// | Keyword match | +0.2 | Lowest — Claude often writes about errors in code/plans |
/// | Screen stall (>60s) | +0.3 | Medium — could be "thinking" or real stall |
/// | Checkpoint stale (>60s) | +0.3 | Medium — no heartbeat from Claude Code |
/// | Process dead | +0.5 | High — strong evidence of real failure |
/// | Checkpoint fresh (<60s) | -0.2 | Negative — system is making progress |
/// | Artifacts active (<60s) | -0.2 | Negative — agents writing output files |
/// | Swarm active            | -0.3 | Strongest negative — teammates are running |
///
/// Action thresholds:
/// - `>= 0.5` → Classify error and act (kill/retry/skip)
/// - `< 0.5` → Continue monitoring (StillRunning)
#[derive(Debug, Clone)]
pub struct ErrorEvidence {
    /// Error keyword matched in tail of pane output.
    pub keyword_match: Option<ErrorClass>,
    /// Screen has not changed for this duration.
    pub screen_stall_secs: u64,
    /// Checkpoint has not updated for this duration (None if no checkpoint exists).
    pub checkpoint_stale_secs: Option<u64>,
    /// Whether the Claude process is still alive.
    pub process_alive: bool,
    /// Whether artifact directory has changed recently (files being written).
    /// When true, agents are actively producing output — strong negative signal.
    pub artifacts_active: bool,
    /// Whether claude-swarm teammates are running (agent team execution).
    /// When true, the main process may appear idle but teammates are working.
    /// Strongest negative signal — active teammates mean the system is healthy.
    pub swarm_active: bool,
}

impl ErrorEvidence {
    /// Compute combined confidence score (0.0 - 1.0).
    ///
    /// Positive signals increase confidence:
    /// - Keyword match: +0.2 (lowest — Claude often writes about errors)
    /// - Screen stall (>60s): +0.3 (medium)
    /// - Checkpoint stale (>60s): +0.3 (medium)
    /// - Process dead: +0.5 (high)
    ///
    /// Negative signals decrease confidence:
    /// - Checkpoint fresh (still updating): -0.2 (system is making progress,
    ///   e.g., Claude waiting for teammates while checkpoint advances)
    /// - Artifacts active (files changing): -0.2 (agents writing output,
    ///   e.g., review agents writing TOME fragments)
    pub fn confidence(&self) -> f64 {
        let mut score: f64 = 0.0;

        if self.keyword_match.is_some() {
            score += 0.2;
        }

        if self.screen_stall_secs >= ERROR_STALL_THRESHOLD_SECS {
            score += 0.3;
        }

        if let Some(stale_secs) = self.checkpoint_stale_secs {
            if stale_secs >= ERROR_STALL_THRESHOLD_SECS {
                score += 0.3;
            } else {
                // Checkpoint is fresh — system is making progress.
                // This is a negative signal: even if screen is stalled
                // (e.g., Claude waiting for teammate agents), checkpoint
                // updates prove work is happening.
                score -= 0.2;
            }
        }

        // Artifact directory is actively changing — agents writing output.
        // Similar to checkpoint freshness: even if screen is stalled,
        // file I/O proves work is happening.
        if self.artifacts_active {
            score -= 0.2;
        }

        // Swarm teammates are running — strongest negative signal.
        // The main process appears idle because it's waiting for agent results,
        // but the actual work is happening in claude-swarm-{pid} panes.
        if self.swarm_active {
            score -= 0.3;
        }

        if !self.process_alive {
            // Process death overrides checkpoint freshness — if the process
            // is dead, checkpoint can't update anymore regardless.
            score = (score + 0.5).max(0.5);
        }

        // Clamp to [0.0, 1.0]
        score.clamp(0.0, 1.0)
    }

    /// Whether confidence is high enough to act on the error.
    ///
    /// Requires at least one actual error indicator (keyword match or process
    /// death) before acting. Screen stall + checkpoint staleness alone is NOT
    /// an error — it's handled by the idle detection system (nudge at 180s,
    /// kill at 300s). Without this gate, normal Claude "thinking" pauses
    /// (30-60s of no output) combined with checkpoint staleness would falsely
    /// trigger kills, especially during agent team execution when the team
    /// lead waits for teammates.
    pub fn should_act(&self) -> bool {
        // No error indicator at all → defer to idle detection, not error detection
        if self.keyword_match.is_none() && self.process_alive {
            return false;
        }
        self.confidence() >= 0.5
    }

    /// Get the error classification if confidence is sufficient.
    ///
    /// Returns the keyword-matched error class only when combined evidence
    /// supports it. If no keyword matched but confidence is high (e.g.,
    /// process dead + stall), returns `ErrorClass::Crash` — a dead process
    /// with no specific error pattern is a crash by definition.
    ///
    /// Aligned with torrent's philosophy: unclassified = healthy when process
    /// is alive. Only process death without a keyword produces a non-keyword
    /// classification (Crash).
    pub fn classify(&self) -> Option<ErrorClass> {
        if !self.should_act() {
            return None;
        }
        // If keyword matched, use that classification.
        // If no keyword but we got here, process must be dead (should_act gate
        // ensures keyword_match.is_some() || !process_alive). Dead process
        // without specific error = Crash.
        Some(self.keyword_match.unwrap_or(ErrorClass::Crash))
    }
}

/// Classification of errors that can occur during phase execution.
///
/// Each variant maps to specific retry behavior defined by `RetryStrategy`.
///
/// Ported from torrent's `DiagnosticState` for comprehensive coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ErrorClass {
    /// Claude Code process crashed or died unexpectedly.
    /// → Retry with 30s backoff, max 3 retries.
    Crash,

    /// Session was idle too long (stuck).
    /// → Kill session, retry with 30s backoff, max 2 retries.
    Stuck,

    /// Phase timeout exceeded.
    /// → Kill session, retry with 30s backoff, max 3 retries.
    Timeout,

    /// API is overloaded (429, 529, 500-class errors, bad gateway, service unavailable).
    /// → Exponential backoff: 15→30→60→120 min, max 4 retries.
    ApiOverload,

    /// Authentication or billing error.
    /// → Skip plan entirely (no retry).
    AuthError,

    /// Network/connection error (ECONNREFUSED, ECONNRESET, DNS failure, etc.).
    /// → Retry with escalating backoff, max 3 retries.
    NetworkError,

    /// Permission blocked — Claude waiting for user approval.
    /// → Retry session (permissions may auto-resolve on restart).
    PermissionBlocked,

    /// Request too large (413) — context won't fit.
    /// → Skip plan entirely (no retry, won't get smaller).
    RequestTooLarge,

    /// Bootstrap error — plan not found, plugin missing, etc.
    /// → Skip plan entirely (no retry, need user intervention).
    BootstrapError,
}

impl ErrorClass {
    /// Get the default backoff duration for this error class.
    ///
    /// For `ApiOverload`, this returns the first backoff (15 min).
    /// Use `backoff_for_attempt` for the full exponential curve.
    pub fn default_backoff(&self) -> Duration {
        match self {
            ErrorClass::Crash => Duration::from_secs(30),
            ErrorClass::Stuck => Duration::from_secs(30),
            ErrorClass::Timeout => Duration::from_secs(30),
            ErrorClass::ApiOverload => Duration::from_secs(15 * 60), // 15 min
            ErrorClass::AuthError => Duration::ZERO,
            ErrorClass::NetworkError => Duration::from_secs(30),
            ErrorClass::PermissionBlocked => Duration::from_secs(30),
            ErrorClass::RequestTooLarge => Duration::ZERO,
            ErrorClass::BootstrapError => Duration::ZERO,
        }
    }

    /// Get the maximum number of retries for this error class.
    pub fn max_retries(&self) -> u32 {
        match self {
            ErrorClass::Crash => 3,
            ErrorClass::Stuck => 2,
            ErrorClass::Timeout => 3,
            ErrorClass::ApiOverload => 4,
            ErrorClass::AuthError => 0,
            ErrorClass::NetworkError => 3,
            ErrorClass::PermissionBlocked => 2,
            ErrorClass::RequestTooLarge => 0,
            ErrorClass::BootstrapError => 0, // terminal — skip plan
        }
    }

    /// Get the backoff duration for a specific attempt number.
    ///
    /// For `ApiOverload`, this implements exponential backoff:
    /// - Attempt 0: 15 min
    /// - Attempt 1: 30 min
    /// - Attempt 2: 60 min
    /// - Attempt 3+: 120 min
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        match self {
            ErrorClass::ApiOverload => {
                let minutes = [15, 30, 60, 120];
                let idx = (attempt as usize).min(minutes.len() - 1);
                Duration::from_secs(minutes[idx] * 60)
            }
            _ => self.default_backoff(),
        }
    }

    /// Check if this error class should skip the entire plan.
    pub fn skips_plan(&self) -> bool {
        matches!(self, ErrorClass::AuthError | ErrorClass::RequestTooLarge | ErrorClass::BootstrapError)
    }

    /// Classify error patterns found in pane output.
    ///
    /// This is a **low-confidence signal** — keyword matches alone do NOT
    /// indicate a real error. Claude Code often discusses auth, billing,
    /// HTTP status codes in normal output (code, plans, docs). Callers
    /// MUST gate this behind stall detection (screen unchanged + checkpoint
    /// stale) before taking action.
    ///
    /// # Arguments
    ///
    /// * `output` - Pane text to scan (last N lines captured from tmux)
    /// * `runtime` - If true, skip bootstrap-only patterns (plan_not_found,
    ///   plugin_missing) that would false-positive during active arc execution.
    ///   Ported from torrent's `bootstrap_only` pattern flag.
    ///
    /// # Detection Strategy (ported from torrent)
    ///
    /// 1. Simple patterns — checked in priority order (billing > auth > permission > network > overload > rate-limit > request-too-large)
    /// 2. Anchored patterns — HTTP status codes require co-occurrence with
    ///    anchor strings (e.g., "500" + "api_error") to avoid false positives
    ///    like "Processing 500 items" or "ticket #429".
    /// 3. Error-context gate — requires at least one error indicator line
    ///    (starts with "error:", "fatal:", "failed", or contains "❌").
    pub fn from_pane_output(output: &str, runtime: bool) -> Option<Self> {
        // Only check the tail — errors that matter are recent.
        // Scanning full pane causes false positives when Claude's output
        // contains error keywords in code, docs, or plan enrichments.
        let tail: String = output
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();

        // Require error-adjacent context: the keyword must appear near
        // an indicator that it's an actual error, not just mentioned in output.
        // Note: `tail` is already lowercased above, so these checks are case-insensitive.
        let has_error_context = tail.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("error:") || trimmed.starts_with("error ")
            || trimmed.starts_with("fatal:") || trimmed.starts_with("fatal ")
            || trimmed.starts_with("exception:") || trimmed.starts_with("exception ")
            || trimmed.starts_with("failed")
            || trimmed.contains("❌")
        });

        if !has_error_context {
            return None;
        }

        // --- Priority 1: Billing (terminal — stops entire batch) ---
        if tail.contains("billing")
            || tail.contains("payment required")
            || tail.contains("payment_required")
            || tail.contains("insufficient funds")
            || tail.contains("subscription expired")
        {
            return Some(ErrorClass::AuthError);
        }

        // --- Priority 2: Auth errors ---
        if tail.contains("authentication_error")
            || tail.contains("invalid_api_key")
            || tail.contains("invalid api key")
            || tail.contains("api key expired")
            || tail.contains("unauthorized")
            || tail.contains("token expired")
            || tail.contains("not authenticated")
        {
            return Some(ErrorClass::AuthError);
        }

        // --- Priority 3: Permission blocked ---
        if tail.contains("permission denied")
            || tail.contains("permission_error")
            || tail.contains("not permitted")
            || tail.contains("access denied")
        {
            return Some(ErrorClass::PermissionBlocked);
        }

        // --- Priority 4: Bootstrap-only patterns (skip during runtime) ---
        // These patterns should never trigger mid-arc because the plan/plugin
        // has already been loaded. Normal tool output (Read errors, file probes)
        // can contain these phrases and would cause false SkipPlan actions.
        if !runtime {
            if tail.contains("plan not found")
                || tail.contains("plan file not found")
                || tail.contains("plan does not exist")
            {
                return Some(ErrorClass::BootstrapError);
            }
            if tail.contains("plugin not found")
                || tail.contains("plugin not installed")
                || tail.contains("skill not found")
            {
                return Some(ErrorClass::BootstrapError); // terminal — missing plugin/skill
            }
        }

        // --- Priority 5: Network/connection errors ---
        if tail.contains("connection_error")
            || tail.contains("network error")
            || tail.contains("connection refused")
            || tail.contains("connection reset")
            || tail.contains("connection timed out")
            || tail.contains("dns resolution failed")
            || tail.contains("econnrefused")
            || tail.contains("econnreset")
            || tail.contains("etimedout")
        {
            return Some(ErrorClass::NetworkError);
        }

        // --- Priority 6: API overload / server errors ---
        if tail.contains("overloaded_error")
            || tail.contains("overloaded")
            || tail.contains("api is overloaded")
            || tail.contains("server_error")
        {
            return Some(ErrorClass::ApiOverload);
        }

        // --- Priority 7: Rate limit ---
        if tail.contains("rate_limit_error")
            || tail.contains("rate_limit")
            || tail.contains("rate limit")
            || tail.contains("rate-limit")
            || tail.contains("too many requests")
        {
            return Some(ErrorClass::ApiOverload);
        }

        // --- Priority 8: Request too large ---
        if tail.contains("request too large")
            || tail.contains("request_too_large")
            || tail.contains("payload too large")
            || tail.contains("content too long")
        {
            return Some(ErrorClass::RequestTooLarge);
        }

        // --- Priority 9: Service unavailable ---
        if tail.contains("bad gateway")
            || tail.contains("bad_gateway")
            || tail.contains("service unavailable")
            || tail.contains("service_unavailable")
        {
            return Some(ErrorClass::ApiOverload);
        }

        // --- Anchored patterns: HTTP status codes requiring co-occurrence ---
        // Prevents false positives like "Processing 500 items" or "ticket #429".
        // Ported from torrent's AnchoredPattern system.
        struct AnchoredCode {
            code: &'static str,
            anchors: &'static [&'static str],
            class: ErrorClass,
        }

        let anchored_codes = [
            AnchoredCode {
                code: "500",
                anchors: &["api_error", "internal server error", "internal_server_error", "server error", "status code", "http error"],
                class: ErrorClass::ApiOverload,
            },
            AnchoredCode {
                code: "429",
                anchors: &["rate_limit", "rate limit", "too many requests", "status code", "http error"],
                class: ErrorClass::ApiOverload,
            },
            AnchoredCode {
                code: "529",
                anchors: &["overloaded", "overloaded_error", "status code", "http error"],
                class: ErrorClass::ApiOverload,
            },
            AnchoredCode {
                code: "502",
                anchors: &["bad gateway", "bad_gateway", "status code", "http error"],
                class: ErrorClass::ApiOverload,
            },
            AnchoredCode {
                code: "503",
                anchors: &["service unavailable", "service_unavailable", "status code", "http error"],
                class: ErrorClass::ApiOverload,
            },
            AnchoredCode {
                code: "413",
                anchors: &["request too large", "payload too large", "content too long", "status code", "http error"],
                class: ErrorClass::RequestTooLarge,
            },
        ];

        for anchored in &anchored_codes {
            if tail.contains(anchored.code) {
                for anchor in anchored.anchors {
                    if tail.contains(anchor) {
                        return Some(anchored.class);
                    }
                }
            }
        }

        None
    }
}

/// Strategy for retrying failed operations.
///
/// This enum is returned by `RetryCoordinator::should_retry` to indicate
/// what action should be taken after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    /// Should retry after the specified duration.
    Retry { after: Duration },

    /// Should skip the entire plan (fatal error).
    SkipPlan,

    /// Max retries exceeded, should fail the phase.
    Exhausted,
}

/// Backoff strategy for retry operations.
///
/// Defines how backoff duration changes with each attempt.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // Public API reserved for future retry configuration
pub enum BackoffStrategy {
    /// Fixed duration, same for every attempt.
    Fixed(Duration),

    /// Exponential backoff with a maximum duration.
    Exponential {
        initial: Duration,
        multiplier: f64,
        max: Duration,
    },

    /// Custom durations for each attempt.
    Custom(Vec<Duration>),
}

#[allow(dead_code)]
impl BackoffStrategy {
    /// Get the backoff duration for a specific attempt.
    pub fn duration_for_attempt(&self, attempt: u32) -> Duration {
        match self {
            BackoffStrategy::Fixed(d) => *d,

            BackoffStrategy::Exponential {
                initial,
                multiplier,
                max,
            } => {
                let mut duration = initial.as_secs_f64();
                for _ in 0..attempt {
                    duration *= multiplier;
                }
                Duration::from_secs(duration as u64).min(*max)
            }

            BackoffStrategy::Custom(durations) => {
                if durations.is_empty() {
                    return Duration::from_secs(30);
                }
                let idx = (attempt as usize).min(durations.len() - 1);
                durations[idx]
            }
        }
    }
}

/// Per-phase retry state tracker.
#[derive(Debug, Clone)]
pub struct RetryState {
    /// Error class for this retry sequence.
    pub error_class: ErrorClass,
    /// Number of attempts so far.
    pub attempts: u32,
    /// Time of last failure.
    pub last_failure: Option<std::time::Instant>,
    /// Whether rapid failure detection triggered.
    pub is_rapid_failure: bool,
}

impl RetryState {
    /// Create a new retry state for the given error class.
    pub fn new(error_class: ErrorClass) -> Self {
        Self {
            error_class,
            attempts: 0,
            last_failure: None,
            is_rapid_failure: false,
        }
    }

    /// Record a failure and increment attempt count.
    pub fn record_failure(&mut self) {
        let now = std::time::Instant::now();

        self.attempts += 1;
        self.last_failure = Some(now);

        // Check for rapid failure (3+ attempts within 30 seconds).
        // Checked AFTER increment so `attempts` reflects the current failure.
        if let Some(last) = self.last_failure {
            if now.duration_since(last) < Duration::from_secs(30) && self.attempts >= 3 {
                self.is_rapid_failure = true;
            }
        }
    }

    /// Check if this retry sequence has exceeded max retries.
    pub fn is_exhausted(&self) -> bool {
        self.attempts >= self.error_class.max_retries()
    }

    /// Get the backoff duration before next retry.
    pub fn next_backoff(&self) -> Duration {
        if self.is_rapid_failure {
            // Escalate backoff for rapid failures
            Duration::from_secs(180)
        } else {
            self.error_class.backoff_for_attempt(self.attempts)
        }
    }
}

/// Coordinator for managing retry decisions across phase groups.
///
/// Tracks retry counts per phase group and provides decisions on
/// whether to retry, skip, or fail.
#[derive(Debug, Clone)]
pub struct RetryCoordinator {
    /// Retry state per phase group (by group name).
    states: std::collections::HashMap<String, RetryState>,
    /// Global rapid failure count.
    global_failure_count: u32,
}

impl Default for RetryCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)] // Public API — query methods reserved for future dashboards
impl RetryCoordinator {
    /// Create a new retry coordinator.
    pub fn new() -> Self {
        Self {
            states: std::collections::HashMap::new(),
            global_failure_count: 0,
        }
    }

    /// Check if we should retry a failed phase group.
    ///
    /// # Arguments
    ///
    /// * `group_name` - Name of the phase group
    /// * `error_class` - Classification of the error
    ///
    /// # Returns
    ///
    /// A `RetryDecision` indicating what action to take.
    pub fn should_retry(&mut self, group_name: &str, error_class: ErrorClass) -> RetryDecision {
        // Auth errors always skip the plan
        if error_class.skips_plan() {
            return RetryDecision::SkipPlan;
        }

        // Get or create retry state
        let state = self
            .states
            .entry(group_name.to_string())
            .or_insert_with(|| RetryState::new(error_class));

        // Update error class if it changed
        state.error_class = error_class;

        // Check if exhausted
        if state.is_exhausted() {
            return RetryDecision::Exhausted;
        }

        // Record the failure
        state.record_failure();
        self.global_failure_count += 1;

        // Return retry decision with backoff
        RetryDecision::Retry {
            after: state.next_backoff(),
        }
    }

    /// Reset retry state for a phase group (after success).
    pub fn reset(&mut self, group_name: &str) {
        self.states.remove(group_name);
    }

    /// Get the retry count for a phase group.
    pub fn retry_count(&self, group_name: &str) -> u32 {
        self.states
            .get(group_name)
            .map(|s| s.attempts)
            .unwrap_or(0)
    }

    /// Check if we're in a rapid failure pattern.
    ///
    /// Returns true if there have been multiple failures across
    /// different groups in a short time span.
    pub fn is_rapid_failure_pattern(&self) -> bool {
        self.states.values().any(|s| s.is_rapid_failure)
    }

    /// Get the total failure count across all groups.
    pub fn total_failures(&self) -> u32 {
        self.global_failure_count
    }

    /// Get the current backoff for a group.
    pub fn current_backoff(&self, group_name: &str) -> Option<Duration> {
        self.states.get(group_name).map(|s| s.next_backoff())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_class_default_backoff() {
        assert_eq!(ErrorClass::Crash.default_backoff(), Duration::from_secs(30));
        assert_eq!(
            ErrorClass::ApiOverload.default_backoff(),
            Duration::from_secs(15 * 60)
        );
        assert_eq!(ErrorClass::AuthError.default_backoff(), Duration::ZERO);
    }

    #[test]
    fn test_error_class_max_retries() {
        assert_eq!(ErrorClass::Crash.max_retries(), 3);
        assert_eq!(ErrorClass::Stuck.max_retries(), 2);
        assert_eq!(ErrorClass::ApiOverload.max_retries(), 4);
        assert_eq!(ErrorClass::AuthError.max_retries(), 0);
        assert_eq!(ErrorClass::BootstrapError.max_retries(), 0);
    }

    #[test]
    fn test_error_class_backoff_for_attempt_api_overload() {
        let error = ErrorClass::ApiOverload;

        assert_eq!(error.backoff_for_attempt(0), Duration::from_secs(15 * 60));
        assert_eq!(error.backoff_for_attempt(1), Duration::from_secs(30 * 60));
        assert_eq!(error.backoff_for_attempt(2), Duration::from_secs(60 * 60));
        assert_eq!(error.backoff_for_attempt(3), Duration::from_secs(120 * 60));
        assert_eq!(error.backoff_for_attempt(10), Duration::from_secs(120 * 60)); // Capped
    }

    #[test]
    fn test_error_class_from_pane_output_billing_with_context() {
        // Billing keyword WITH error context → match
        let output = "Error: billing payment required";
        assert_eq!(
            ErrorClass::from_pane_output(output, false),
            Some(ErrorClass::AuthError)
        );
    }

    #[test]
    fn test_error_class_from_pane_output_billing_no_context() {
        // Billing keyword WITHOUT error context → no match (avoids false positives
        // when Claude writes about billing in code or plans)
        let output = "The retry module handles billing with backoff";
        assert_eq!(ErrorClass::from_pane_output(output, false), None);
    }

    #[test]
    fn test_error_class_from_pane_output_auth_in_plan() {
        // Claude enriching a plan that mentions auth patterns → no match
        // Real plan output discusses these as code concepts, not API errors
        let output = "- **Already has** patterns (billing, auth)\n\
                      - ErrorClass enum with AuthError, Crash, Stuck\n\
                      - `from_pane_output()` duplicates patterns from detect.rs";
        assert_eq!(ErrorClass::from_pane_output(output, false), None);
    }

    #[test]
    fn test_error_class_from_pane_output_auth_in_code_output() {
        // Claude writing code that references auth/billing keywords → no match
        // Tool output markers show Claude is actively working
        let output = "⏺ Update(src/engine/retry.rs)\n\
                      ⎿  if output_lower.contains(\"billing\") {\n\
                      ⎿      return Some(ErrorClass::AuthError);\n\
                      ⎿  }";
        assert_eq!(ErrorClass::from_pane_output(output, false), None);
    }

    #[test]
    fn test_error_class_from_pane_output_rate_limit_with_context() {
        let output = "Error: rate_limit exceeded (429)";
        assert_eq!(
            ErrorClass::from_pane_output(output, false),
            Some(ErrorClass::ApiOverload)
        );
    }

    #[test]
    fn test_error_class_from_pane_output_unknown() {
        let output = "Some random output without error patterns";
        assert_eq!(ErrorClass::from_pane_output(output, false), None);
    }

    #[test]
    fn test_error_class_from_pane_output_ignores_old_scrollback() {
        // Error in early lines (beyond last 10) should be ignored
        let mut output = "Error: billing payment required\n".to_string();
        for i in 0..20 {
            output.push_str(&format!("normal output line {}\n", i));
        }
        assert_eq!(ErrorClass::from_pane_output(&output, false), None);
    }

    #[test]
    fn test_error_class_skips_plan() {
        assert!(ErrorClass::AuthError.skips_plan());
        assert!(ErrorClass::RequestTooLarge.skips_plan());
        assert!(!ErrorClass::Crash.skips_plan());
        assert!(!ErrorClass::Timeout.skips_plan());
        assert!(!ErrorClass::NetworkError.skips_plan());
        assert!(!ErrorClass::PermissionBlocked.skips_plan());
    }

    // --- New ErrorClass variant tests ---

    #[test]
    fn test_new_error_class_backoffs() {
        assert_eq!(ErrorClass::NetworkError.default_backoff(), Duration::from_secs(30));
        assert_eq!(ErrorClass::PermissionBlocked.default_backoff(), Duration::from_secs(30));
        assert_eq!(ErrorClass::RequestTooLarge.default_backoff(), Duration::ZERO);
    }

    #[test]
    fn test_new_error_class_max_retries() {
        assert_eq!(ErrorClass::NetworkError.max_retries(), 3);
        assert_eq!(ErrorClass::PermissionBlocked.max_retries(), 2);
        assert_eq!(ErrorClass::RequestTooLarge.max_retries(), 0);
    }

    // --- Bootstrap vs runtime pattern filtering ---

    #[test]
    fn test_plan_not_found_detected_in_bootstrap() {
        let output = "Error: plan not found at plans/missing.md";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::BootstrapError));
    }

    #[test]
    fn test_plan_not_found_skipped_during_runtime() {
        // D4: bootstrap_only — must NOT trigger during runtime (mid-arc)
        let output = "Error: plan not found at plans/missing.md";
        assert_eq!(ErrorClass::from_pane_output(output, true), None);
    }

    #[test]
    fn test_plugin_missing_skipped_during_runtime() {
        // D5: bootstrap_only — must NOT trigger during runtime (mid-arc)
        let output = "Error: skill not found: rune:arc";
        assert_eq!(ErrorClass::from_pane_output(output, true), None);
    }

    #[test]
    fn test_api_errors_still_detected_during_runtime() {
        let output = "Error: overloaded_error: API is overloaded";
        assert_eq!(ErrorClass::from_pane_output(output, true), Some(ErrorClass::ApiOverload));
    }

    // --- Network error detection ---

    #[test]
    fn test_network_error_connection_refused() {
        let output = "Error: connection refused to api.anthropic.com";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::NetworkError));
    }

    #[test]
    fn test_network_error_econnreset() {
        let output = "Error: ECONNRESET during API call";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::NetworkError));
    }

    #[test]
    fn test_network_error_dns() {
        let output = "Error: dns resolution failed for api.anthropic.com";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::NetworkError));
    }

    // --- Permission blocked detection ---

    #[test]
    fn test_permission_denied_detected() {
        let output = "Error: permission denied for tool Bash";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::PermissionBlocked));
    }

    #[test]
    fn test_access_denied_detected() {
        let output = "Error: access denied to resource";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::PermissionBlocked));
    }

    // --- Request too large detection ---

    #[test]
    fn test_request_too_large_detected() {
        let output = "Error: request too large, reduce context";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::RequestTooLarge));
    }

    #[test]
    fn test_payload_too_large_detected() {
        let output = "Error: payload too large for API";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::RequestTooLarge));
    }

    // --- Anchored HTTP code tests (ported from torrent) ---

    #[test]
    fn test_500_without_anchor_is_none() {
        // "500" alone should NOT trigger — could be "Processing 500 items"
        let output = "Error: Processing 500 items in batch";
        assert_ne!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::ApiOverload));
    }

    #[test]
    fn test_500_with_anchor_triggers_api_overload() {
        let output = "Error: HTTP 500 api_error: internal server error";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::ApiOverload));
    }

    #[test]
    fn test_429_without_anchor_is_none() {
        // "429" without error context anchor should not trigger
        let output = "Error: ticket #429 assigned to user";
        // rate_limit not in text, so anchored pattern won't match
        assert_ne!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::ApiOverload));
    }

    #[test]
    fn test_429_with_anchor_triggers_overload() {
        let output = "Error: status code 429: rate limit exceeded";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::ApiOverload));
    }

    #[test]
    fn test_413_with_anchor_triggers_request_too_large() {
        let output = "Error: status code 413 request too large";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::RequestTooLarge));
    }

    // --- Service unavailable ---

    #[test]
    fn test_bad_gateway_detected() {
        let output = "Error: bad gateway from upstream";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::ApiOverload));
    }

    #[test]
    fn test_service_unavailable_detected() {
        let output = "Error: service unavailable, try again later";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::ApiOverload));
    }

    // --- Priority ordering ---

    #[test]
    fn test_billing_takes_priority_over_auth() {
        let output = "Error: billing error and also unauthorized access";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::AuthError));
    }

    #[test]
    fn test_auth_takes_priority_over_rate_limit() {
        let output = "Error: unauthorized and rate_limit_error together";
        assert_eq!(ErrorClass::from_pane_output(output, false), Some(ErrorClass::AuthError));
    }

    // --- Crash grace period constant ---

    #[test]
    fn test_crash_grace_period_constant() {
        assert_eq!(CRASH_GRACE_SECS, 300);
    }

    // --- ErrorEvidence confidence tests ---

    #[test]
    fn test_evidence_keyword_alone_not_enough() {
        // Keyword match alone with fresh checkpoint = 0.2 - 0.2 = 0.0 → should NOT act
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 0,
            checkpoint_stale_secs: Some(0),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert_eq!(evidence.confidence(), 0.0);
        assert!(!evidence.should_act());
        assert!(evidence.classify().is_none());
    }

    #[test]
    fn test_evidence_stall_alone_not_enough() {
        // Screen stall + fresh checkpoint = 0.3 - 0.2 = 0.1 → should NOT act
        // (also blocked by no-keyword gate)
        let evidence = ErrorEvidence {
            keyword_match: None,
            screen_stall_secs: 120,
            checkpoint_stale_secs: Some(0),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert!(evidence.confidence() < 0.5);
        assert!(!evidence.should_act());
    }

    #[test]
    fn test_evidence_keyword_plus_stall_fresh_checkpoint_not_enough() {
        // Keyword + stall but checkpoint fresh = 0.2 + 0.3 - 0.2 = 0.3 → NOT enough
        // This is the key scenario: Claude waiting for teammates with auth keyword in output
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 120,
            checkpoint_stale_secs: Some(0),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert!(evidence.confidence() < 0.5);
        assert!(!evidence.should_act());
    }

    #[test]
    fn test_evidence_keyword_plus_stall_stale_checkpoint_acts() {
        // Keyword + stall + stale checkpoint = 0.2 + 0.3 + 0.3 = 0.8 → ACT
        // All signals agree: real error
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 60,
            checkpoint_stale_secs: Some(60),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert_eq!(evidence.confidence(), 0.8);
        assert!(evidence.should_act());
        assert_eq!(evidence.classify(), Some(ErrorClass::AuthError));
    }

    #[test]
    fn test_evidence_stall_plus_checkpoint_stale_no_keyword_no_action() {
        // Screen stall + checkpoint stale but no keyword and process alive
        // → should NOT act. This is idle behavior, not an error.
        // The idle detection system (nudge at 180s, kill at 300s) handles this.
        let evidence = ErrorEvidence {
            keyword_match: None,
            screen_stall_secs: 120,
            checkpoint_stale_secs: Some(120),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert_eq!(evidence.confidence(), 0.6);
        assert!(!evidence.should_act()); // No error indicator → idle, not error
        assert!(evidence.classify().is_none());
    }

    #[test]
    fn test_evidence_stall_plus_checkpoint_stale_process_dead_acts() {
        // Screen stall + checkpoint stale + process dead → should act
        // Process death IS an error indicator
        let evidence = ErrorEvidence {
            keyword_match: None,
            screen_stall_secs: 120,
            checkpoint_stale_secs: Some(120),
            process_alive: false,
            artifacts_active: false,
            swarm_active: false,
        };
        assert!(evidence.confidence() >= 0.5);
        assert!(evidence.should_act());
        assert_eq!(evidence.classify(), Some(ErrorClass::Crash));
    }

    #[test]
    fn test_evidence_process_dead_high_confidence() {
        // Process dead overrides checkpoint freshness → at least 0.5
        let evidence = ErrorEvidence {
            keyword_match: None,
            screen_stall_secs: 0,
            checkpoint_stale_secs: Some(0),
            process_alive: false,
            artifacts_active: false,
            swarm_active: false,
        };
        assert!(evidence.confidence() >= 0.5);
        assert!(evidence.should_act());
    }

    #[test]
    fn test_evidence_all_signals_capped_at_1() {
        // All signals → capped at 1.0
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::ApiOverload),
            screen_stall_secs: 120,
            checkpoint_stale_secs: Some(120),
            process_alive: false,
            artifacts_active: false,
            swarm_active: false,
        };
        assert_eq!(evidence.confidence(), 1.0);
        assert!(evidence.should_act());
        assert_eq!(evidence.classify(), Some(ErrorClass::ApiOverload));
    }

    #[test]
    fn test_evidence_no_signals_fresh_checkpoint() {
        // No signals + fresh checkpoint = -0.2, clamped to 0.0
        let evidence = ErrorEvidence {
            keyword_match: None,
            screen_stall_secs: 0,
            checkpoint_stale_secs: Some(0),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert_eq!(evidence.confidence(), 0.0);
        assert!(!evidence.should_act());
        assert!(evidence.classify().is_none());
    }

    #[test]
    fn test_evidence_no_checkpoint_exists() {
        // No checkpoint at all = None → no checkpoint contribution (positive or negative)
        // Keyword + stall (≥60s threshold) = 0.2 + 0.3 = 0.5 → should act
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 120,
            checkpoint_stale_secs: None,
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert_eq!(evidence.confidence(), 0.5);
        assert!(evidence.should_act());
    }

    #[test]
    fn test_evidence_claude_waiting_for_teammates() {
        // Claude teamlead waiting: screen idle, keyword in output, BUT checkpoint fresh
        // This is normal during agent team execution → should NOT kill
        // keyword=0.2 + stall(120s>=60s)=0.3 + checkpoint_fresh=-0.2 = 0.3 < 0.5
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 120, // 2 min stall
            checkpoint_stale_secs: Some(5), // checkpoint updated 5s ago (fresh)
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert!(evidence.confidence() < 0.5);
        assert!(!evidence.should_act());
    }

    #[test]
    fn test_evidence_claude_thinking_with_keyword() {
        // Claude is thinking (screen stall < 30s) with keyword in output → no action
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 15,
            checkpoint_stale_secs: Some(10),
            process_alive: true,
            artifacts_active: false,
            swarm_active: false,
        };
        assert!(evidence.confidence() < 0.5);
        assert!(!evidence.should_act());
    }

    #[test]
    fn test_evidence_artifacts_active_reduces_confidence() {
        // Keyword + stall + stale checkpoint = 0.8, but artifacts active = -0.2 → 0.6
        // Still above threshold, but demonstrates the artifact signal works
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 60,
            checkpoint_stale_secs: Some(60),
            process_alive: true,
            artifacts_active: true,
            swarm_active: false,
        };
        let conf = evidence.confidence();
        assert!(conf > 0.5 && conf < 0.7, "expected ~0.6, got {}", conf);
        assert!(evidence.should_act()); // keyword present → should_act gate passes
    }

    #[test]
    fn test_evidence_artifacts_active_prevents_action() {
        // Keyword + stall + fresh checkpoint + artifacts active
        // = 0.2 + 0.3 - 0.2 - 0.2 = 0.1 → NOT enough
        // Agents writing files + checkpoint updating = definitely still working
        let evidence = ErrorEvidence {
            keyword_match: Some(ErrorClass::AuthError),
            screen_stall_secs: 120,
            checkpoint_stale_secs: Some(5),
            process_alive: true,
            artifacts_active: true,
            swarm_active: false,
        };
        assert!(evidence.confidence() < 0.5);
        assert!(!evidence.should_act());
    }

    #[test]
    fn test_backoff_strategy_fixed() {
        let strategy = BackoffStrategy::Fixed(Duration::from_secs(30));

        assert_eq!(strategy.duration_for_attempt(0), Duration::from_secs(30));
        assert_eq!(strategy.duration_for_attempt(5), Duration::from_secs(30));
    }

    #[test]
    fn test_backoff_strategy_exponential() {
        let strategy = BackoffStrategy::Exponential {
            initial: Duration::from_secs(10),
            multiplier: 2.0,
            max: Duration::from_secs(100),
        };

        assert_eq!(strategy.duration_for_attempt(0), Duration::from_secs(10));
        assert_eq!(strategy.duration_for_attempt(1), Duration::from_secs(20));
        assert_eq!(strategy.duration_for_attempt(2), Duration::from_secs(40));
        assert_eq!(strategy.duration_for_attempt(3), Duration::from_secs(80));
        assert_eq!(strategy.duration_for_attempt(4), Duration::from_secs(100)); // Capped
    }

    #[test]
    fn test_backoff_strategy_custom() {
        let strategy = BackoffStrategy::Custom(vec![
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ]);

        assert_eq!(strategy.duration_for_attempt(0), Duration::from_secs(5));
        assert_eq!(strategy.duration_for_attempt(1), Duration::from_secs(10));
        assert_eq!(strategy.duration_for_attempt(2), Duration::from_secs(30));
        assert_eq!(strategy.duration_for_attempt(10), Duration::from_secs(30)); // Last value
    }

    #[test]
    fn test_retry_state_exhausted() {
        let mut state = RetryState::new(ErrorClass::Crash);

        assert!(!state.is_exhausted());

        state.record_failure(); // 1
        state.record_failure(); // 2
        state.record_failure(); // 3

        assert!(state.is_exhausted()); // Max 3 for Crash
    }

    #[test]
    fn test_retry_state_auth_never_exhausted() {
        let state = RetryState::new(ErrorClass::AuthError);
        // AuthError has 0 max retries, so it's immediately "exhausted"
        assert!(state.is_exhausted());
    }

    #[test]
    fn test_retry_coordinator_should_retry() {
        let mut coordinator = RetryCoordinator::new();

        // First failure should allow retry
        let decision = coordinator.should_retry("A", ErrorClass::Crash);
        assert!(matches!(decision, RetryDecision::Retry { .. }));

        // Second failure should allow retry
        let decision = coordinator.should_retry("A", ErrorClass::Crash);
        assert!(matches!(decision, RetryDecision::Retry { .. }));

        // Third failure should allow retry
        let decision = coordinator.should_retry("A", ErrorClass::Crash);
        assert!(matches!(decision, RetryDecision::Retry { .. }));

        // Fourth failure should be exhausted
        let decision = coordinator.should_retry("A", ErrorClass::Crash);
        assert_eq!(decision, RetryDecision::Exhausted);
    }

    #[test]
    fn test_retry_coordinator_auth_error_skips_plan() {
        let mut coordinator = RetryCoordinator::new();

        let decision = coordinator.should_retry("A", ErrorClass::AuthError);
        assert_eq!(decision, RetryDecision::SkipPlan);
    }

    #[test]
    fn test_retry_coordinator_reset() {
        let mut coordinator = RetryCoordinator::new();

        coordinator.should_retry("A", ErrorClass::Crash);
        assert_eq!(coordinator.retry_count("A"), 1);

        coordinator.reset("A");
        assert_eq!(coordinator.retry_count("A"), 0);
    }

    #[test]
    fn test_retry_coordinator_different_groups() {
        let mut coordinator = RetryCoordinator::new();

        coordinator.should_retry("A", ErrorClass::Crash);
        coordinator.should_retry("B", ErrorClass::Crash);

        assert_eq!(coordinator.retry_count("A"), 1);
        assert_eq!(coordinator.retry_count("B"), 1);
    }
}