//! Prompt detection and output velocity tracking.
//!
//! This module provides utilities for detecting Claude Code's prompt (❯)
//! and tracking output velocity to detect idle sessions.
//!
//! # Prompt Detection
//!
//! Claude Code's prompt is the `❯` character. We detect it in the last
//! non-empty line of the pane to determine if Claude is ready for input
//! or waiting for something.
//!
//! # Output Velocity
//!
//! We track how quickly the pane content changes to detect idle sessions.
//! A session that stops producing output for an extended period may be
//! stuck or waiting for input.
//!
//! # Example
//!
//! ```ignore
//! use greater_will::session::detect::{PromptDetector, OutputVelocity};
//!
//! let mut detector = PromptDetector::new("gw-session-A");
//!
//! // Check for prompt
//! if detector.detect_prompt()? {
//!     println!("Claude is ready for input");
//! }
//!
//! // Track output velocity
//! let velocity = detector.compute_velocity()?;
//! if velocity.is_idle(Duration::from_secs(180)) {
//!     println!("Session is idle");
//! }
//! ```

use color_eyre::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::Command;
use std::time::{Duration, Instant};

/// The prompt character used by Claude Code.
const PROMPT_CHAR: char = '❯';

/// Number of lines to check for prompt.
const PROMPT_CHECK_LINES: usize = 5;

/// Prompt detector for Claude Code sessions.
///
/// Tracks prompt detection state and output velocity.
#[derive(Debug)]
pub struct PromptDetector {
    /// Session ID to monitor.
    session_id: String,
    /// Hash of last captured content.
    last_hash: Option<u64>,
    /// Time of last content change.
    last_change: Instant,
    /// Last captured content (for debugging).
    last_content: Option<String>,
}

impl PromptDetector {
    /// Create a new prompt detector.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            last_hash: None,
            last_change: Instant::now(),
            last_content: None,
        }
    }

    /// Detect if the Claude Code prompt is visible.
    ///
    /// Checks the last few non-empty lines for the `❯` character.
    pub fn detect_prompt(&self) -> Result<bool> {
        let content = capture_pane(&self.session_id)?;
        Ok(has_prompt_in_content(&content))
    }

    /// Update the detector with current pane content.
    ///
    /// Call this periodically to track output velocity.
    pub fn update(&mut self) -> Result<OutputUpdate> {
        let content = capture_pane(&self.session_id)?;
        let current_hash = compute_content_hash(&content);
        let now = Instant::now();

        let changed = match self.last_hash {
            Some(last) => current_hash != last,
            None => true, // First update always counts as change
        };

        if changed {
            self.last_change = now;
        }

        self.last_hash = Some(current_hash);
        self.last_content = Some(content.clone());

        Ok(OutputUpdate {
            changed,
            hash: current_hash,
            idle_duration: now.duration_since(self.last_change),
            has_prompt: has_prompt_in_content(&content),
        })
    }

    /// Compute current output velocity.
    ///
    /// Returns information about how active the session is.
    pub fn compute_velocity(&self) -> Result<OutputVelocity> {
        let content = capture_pane(&self.session_id)?;
        let current_hash = compute_content_hash(&content);

        Ok(OutputVelocity {
            current_hash,
            last_hash: self.last_hash,
            idle_duration: self.last_change.elapsed(),
            content_length: content.len(),
        })
    }

    /// Check if the session has been idle for the given duration.
    pub fn is_idle(&self, threshold: Duration) -> Result<bool> {
        let content = capture_pane(&self.session_id)?;
        let current_hash = compute_content_hash(&content);

        let idle = match self.last_hash {
            Some(last) if current_hash == last => self.last_change.elapsed() > threshold,
            _ => false,
        };

        Ok(idle)
    }

    /// Get the time since the last content change.
    pub fn time_since_change(&self) -> Duration {
        self.last_change.elapsed()
    }

    /// Get the last captured content hash.
    pub fn last_hash(&self) -> Option<u64> {
        self.last_hash
    }

    /// Capture the current pane content.
    pub fn capture_current(&self) -> Result<String> {
        capture_pane(&self.session_id)
    }

    /// Get the last line of the pane (skipping empty lines).
    pub fn get_last_line(&self) -> Result<Option<String>> {
        let content = capture_pane(&self.session_id)?;

        let last_line = content
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.to_string());

        Ok(last_line)
    }
}

/// Result of an output update.
#[derive(Debug, Clone)]
pub struct OutputUpdate {
    /// Whether the content changed since last update.
    pub changed: bool,
    /// Hash of current content.
    pub hash: u64,
    /// How long the session has been idle.
    pub idle_duration: Duration,
    /// Whether a prompt is visible.
    pub has_prompt: bool,
}

/// Output velocity information.
#[derive(Debug, Clone)]
pub struct OutputVelocity {
    /// Hash of current content.
    pub current_hash: u64,
    /// Hash of last captured content.
    pub last_hash: Option<u64>,
    /// How long since the last change.
    pub idle_duration: Duration,
    /// Length of current content.
    pub content_length: usize,
}

impl OutputVelocity {
    /// Check if the session is idle (content unchanged for threshold duration).
    pub fn is_idle(&self, threshold: Duration) -> bool {
        self.current_hash == self.last_hash.unwrap_or(0) && self.idle_duration > threshold
    }

    /// Check if content is currently changing.
    pub fn is_active(&self) -> bool {
        self.current_hash != self.last_hash.unwrap_or(0)
    }
}

/// Capture the current pane content.
pub fn capture_pane(session_id: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", session_id, "-p"])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Capture the last N lines of the pane.
pub fn capture_pane_lines(session_id: &str, lines: i32) -> Result<String> {
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-t", session_id,
            "-p",
            "-S", &format!("-{}", lines),
        ])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Compute a hash of content for change detection.
pub fn compute_content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Check if the prompt character is in the content.
pub fn has_prompt_in_content(content: &str) -> bool {
    content
        .lines()
        .rev()
        .take(PROMPT_CHECK_LINES)
        .any(|line| line.contains(PROMPT_CHAR))
}

/// Get the last non-empty line from the content.
pub fn get_last_non_empty_line(content: &str) -> Option<String> {
    content
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
}

/// Check if the last line contains a prompt.
pub fn has_prompt_in_last_line(session_id: &str) -> Result<bool> {
    let content = capture_pane(session_id)?;
    let last_line = get_last_non_empty_line(&content);

    match last_line {
        Some(line) => Ok(line.contains(PROMPT_CHAR)),
        None => Ok(false),
    }
}

/// Detect error patterns in pane output.
///
/// Returns an error class if a known error pattern is found.
///
/// Only scans the last 10 lines of pane output to avoid false positives
/// from code snippets, documentation, or log history in the scrollback.
pub fn detect_error_pattern(content: &str) -> Option<String> {
    // Only check the tail of the pane — errors that matter are recent.
    // Scanning the full pane causes false positives when Claude's output
    // contains error keywords in code, docs, or logs.
    let tail: String = content
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
    let error_indicators = ["error", "failed", "fatal", "exception", "❌"];
    let has_error_context = error_indicators.iter().any(|ind| tail.contains(ind));

    // Billing/auth patterns — these are specific enough to match without extra context
    let fatal_patterns = [
        ("billing", "billing_error"),
        ("payment required", "billing_error"),
        ("subscription expired", "billing_error"),
        ("authentication_error", "auth_error"),
        ("invalid_api_key", "auth_error"),
    ];

    for (pattern, error_type) in fatal_patterns {
        if tail.contains(pattern) {
            return Some(error_type.to_string());
        }
    }

    // Rate limit / overload patterns — require error context to avoid false positives
    // from code that mentions "429" or "rate_limit" as strings.
    if has_error_context {
        let contextual_patterns = [
            ("rate_limit", "rate_limit"),
            ("too many requests", "rate_limit"),
            ("overloaded", "api_overload"),
            ("server_error", "api_overload"),
        ];

        for (pattern, error_type) in contextual_patterns {
            if tail.contains(pattern) {
                return Some(error_type.to_string());
            }
        }

        // HTTP status codes need even more care — only match as standalone tokens
        let status_code_patterns = [
            ("429", "rate_limit"),
            ("529", "api_overload"),
            ("502", "api_overload"),
            ("503", "api_overload"),
        ];

        for (code, error_type) in status_code_patterns {
            // Check that the code isn't part of a longer number (e.g., port 5293)
            for word in tail.split_whitespace() {
                if word.trim_matches(|c: char| !c.is_ascii_digit()) == code {
                    return Some(error_type.to_string());
                }
            }
        }
    }

    None
}

/// Wait for the prompt to appear with timeout.
///
/// Polls the session until the prompt is detected or timeout expires.
pub fn wait_for_prompt(session_id: &str, timeout: Duration) -> Result<bool> {
    let start = Instant::now();
    let poll_interval = Duration::from_millis(500);

    while start.elapsed() < timeout {
        if has_prompt_in_last_line(session_id)? {
            return Ok(true);
        }
        std::thread::sleep(poll_interval);
    }

    Ok(false)
}

/// Pane content snapshot for comparison.
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    /// Captured content.
    pub content: String,
    /// Content hash.
    pub hash: u64,
    /// When the snapshot was taken.
    pub timestamp: Instant,
    /// Whether a prompt was detected.
    pub has_prompt: bool,
}

impl PaneSnapshot {
    /// Capture a new snapshot of the pane.
    pub fn capture(session_id: &str) -> Result<Self> {
        let content = capture_pane(session_id)?;
        let hash = compute_content_hash(&content);

        Ok(Self {
            content,
            hash,
            timestamp: Instant::now(),
            has_prompt: false, // Will be computed if needed
        })
    }

    /// Capture a snapshot with prompt detection.
    pub fn capture_with_prompt(session_id: &str) -> Result<Self> {
        let content = capture_pane(session_id)?;
        let hash = compute_content_hash(&content);
        let has_prompt = has_prompt_in_content(&content);

        Ok(Self {
            content,
            hash,
            timestamp: Instant::now(),
            has_prompt,
        })
    }

    /// Check if this snapshot differs from another.
    pub fn differs_from(&self, other: &PaneSnapshot) -> bool {
        self.hash != other.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_prompt_in_content_present() {
        let content = "Some output\nMore output\n❯ ";
        assert!(has_prompt_in_content(content));
    }

    #[test]
    fn test_has_prompt_in_content_absent() {
        let content = "Some output\nMore output\nStill running...";
        assert!(!has_prompt_in_content(content));
    }

    #[test]
    fn test_has_prompt_in_content_in_middle() {
        // Prompt in middle shouldn't be detected (only checks last 5 lines)
        let content = "❯ prompt\nline1\nline2\nline3\nline4\nline5\nline6";
        // With 6 lines after the prompt, it should not find it
        assert!(!has_prompt_in_content(content));
    }

    #[test]
    fn test_has_prompt_in_content_in_last_5() {
        let content = "line1\nline2\n❯ prompt\nline3\nline4\nline5";
        assert!(has_prompt_in_content(content));
    }

    #[test]
    fn test_compute_content_hash() {
        let content1 = "Hello world";
        let content2 = "Hello world";
        let content3 = "Different";

        let hash1 = compute_content_hash(content1);
        let hash2 = compute_content_hash(content2);
        let hash3 = compute_content_hash(content3);

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_get_last_non_empty_line() {
        let content = "Line 1\nLine 2\n   \n\n";
        let last = get_last_non_empty_line(content);
        assert_eq!(last, Some("Line 2".to_string()));
    }

    #[test]
    fn test_get_last_non_empty_line_all_empty() {
        let content = "   \n\n   \n";
        let last = get_last_non_empty_line(content);
        assert_eq!(last, None);
    }

    #[test]
    fn test_detect_error_pattern_billing() {
        // Billing patterns are specific enough to match without error context
        let content = "billing payment required";
        assert_eq!(
            detect_error_pattern(content),
            Some("billing_error".to_string())
        );
    }

    #[test]
    fn test_detect_error_pattern_rate_limit_with_context() {
        // Rate limit requires error context (e.g., "error" keyword nearby)
        let content = "Error: rate_limit exceeded (429)";
        assert_eq!(
            detect_error_pattern(content),
            Some("rate_limit".to_string())
        );
    }

    #[test]
    fn test_detect_error_pattern_rate_limit_no_context() {
        // rate_limit mentioned without error context → no match (avoids false positives)
        let content = "The retry module handles rate_limit with backoff";
        assert_eq!(detect_error_pattern(content), None);
    }

    #[test]
    fn test_detect_error_pattern_none() {
        let content = "Normal output without errors";
        assert_eq!(detect_error_pattern(content), None);
    }

    #[test]
    fn test_detect_error_pattern_ignores_old_scrollback() {
        // Error in early lines (beyond last 10) should be ignored
        let mut content = "Error: rate_limit exceeded\n".to_string();
        for i in 0..20 {
            content.push_str(&format!("normal output line {}\n", i));
        }
        assert_eq!(detect_error_pattern(&content), None);
    }

    #[test]
    fn test_output_velocity_is_idle() {
        let velocity = OutputVelocity {
            current_hash: 123,
            last_hash: Some(123),
            idle_duration: Duration::from_secs(200),
            content_length: 1000,
        };

        assert!(velocity.is_idle(Duration::from_secs(180)));
        assert!(!velocity.is_idle(Duration::from_secs(300)));
    }

    #[test]
    fn test_output_velocity_is_active() {
        let velocity_active = OutputVelocity {
            current_hash: 123,
            last_hash: Some(456),
            idle_duration: Duration::from_secs(0),
            content_length: 1000,
        };

        let velocity_inactive = OutputVelocity {
            current_hash: 123,
            last_hash: Some(123),
            idle_duration: Duration::from_secs(10),
            content_length: 1000,
        };

        assert!(velocity_active.is_active());
        assert!(!velocity_inactive.is_active());
    }

    #[test]
    fn test_prompt_detector_new() {
        let detector = PromptDetector::new("test-session");
        assert!(detector.last_hash().is_none());
        // Time since change should be very small (< 100ms)
        assert!(detector.time_since_change() < Duration::from_millis(100));
    }
}