#![allow(dead_code)]
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
    ///
    /// Returns `false` until at least one [`update()`](Self::update) call has been
    /// made, since a baseline hash is needed before staleness can be detected.
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
        match self.last_hash {
            Some(last) => self.current_hash == last && self.idle_duration > threshold,
            None => false, // No prior observation — cannot determine idle
        }
    }

    /// Check if content is currently changing.
    ///
    /// Returns `true` when no prior observation exists (conservative default —
    /// we assume active until proven otherwise to avoid premature idle detection).
    pub fn is_active(&self) -> bool {
        match self.last_hash {
            Some(last) => self.current_hash != last,
            None => true, // No prior observation — assume active
        }
    }
}

/// Capture the current pane content.
///
/// Always targets window 0, pane 0 (`session:0.0`) to avoid capturing
/// output from teammate panes or windows that might be active.
/// Claude Code's main process runs in the first pane of the first window.
pub fn capture_pane(session_id: &str) -> Result<String> {
    let target = format!("{}:0.0", session_id);
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", &target, "-p"])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Capture the last N lines of the pane.
///
/// Always targets window 0, pane 0 to avoid teammate pane confusion.
pub fn capture_pane_lines(session_id: &str, lines: i32) -> Result<String> {
    let target = format!("{}:0.0", session_id);
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-t", &target,
            "-p",
            "-S", &format!("-{}", lines),
        ])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Capture the entire scrollback history of a tmux pane.
///
/// Unlike `capture_pane` (visible viewport only), this captures from the
/// beginning of scrollback (`-S -`) to the end, giving the complete history
/// of what happened in the session.
///
/// Always targets window 0, pane 0 to avoid teammate pane confusion.
pub fn capture_full_scrollback(session_id: &str) -> Result<String> {
    let target = format!("{}:0.0", session_id);
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-t", &target,
            "-p",
            "-S", "-",  // From start of scrollback
        ])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Save a crash dump of the tmux session before killing it.
///
/// Captures the full scrollback history and writes it to
/// `.gw/crash-dumps/<session_id>-<timestamp>.txt` in the working directory.
/// This preserves the Claude Code output for post-mortem debugging.
///
/// Returns the path to the dump file on success, or logs a warning and
/// returns None if capture fails (non-fatal — should never block session kill).
pub fn save_crash_dump(session_id: &str, working_dir: &std::path::Path, reason: &str) -> Option<std::path::PathBuf> {
    let dump_dir = working_dir.join(".gw").join("crash-dumps");
    if let Err(e) = std::fs::create_dir_all(&dump_dir) {
        tracing::warn!(error = %e, "Failed to create crash-dumps directory");
        return None;
    }

    // Restrict directory permissions so only the owner can list/enter it
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(&dump_dir) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o700);
            if let Err(e) = std::fs::set_permissions(&dump_dir, perms) {
                tracing::warn!(error = %e, "Failed to set crash-dumps directory permissions");
            }
        }
    }

    // Capture full scrollback. If the tmux session is already gone (the
    // common daemon vanish case) or the pane is unreadable, record that in
    // the dump header instead of bailing — a metadata-only report is still
    // strictly better than silently dropping the crash evidence.
    let (scrollback, scrollback_note) = match capture_full_scrollback(session_id) {
        Ok(content) if content.trim().is_empty() => (
            String::new(),
            Some("(scrollback empty — tmux pane produced no output)".to_string()),
        ),
        Ok(content) => (content, None),
        Err(e) => {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "Failed to capture scrollback for crash dump — writing metadata-only report"
            );
            (
                String::new(),
                Some(format!("(scrollback unavailable: {})", e)),
            )
        }
    };

    // Build dump filename: <session_id>-<timestamp>.txt
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let filename = format!("{}-{}.txt", session_id, timestamp);
    let dump_path = dump_dir.join(&filename);

    // Build dump content with metadata header. When scrollback is missing
    // (session already vanished), the header IS the report.
    let header = format!(
        "=== GW CRASH DUMP ===\n\
         Session:   {}\n\
         Timestamp: {}\n\
         Reason:    {}\n\
         =====================\n\n",
        session_id,
        chrono::Utc::now().to_rfc3339(),
        reason,
    );

    let content = match scrollback_note {
        Some(note) => format!("{}{}\n", header, note),
        None => format!("{}{}", header, scrollback),
    };

    match std::fs::write(&dump_path, &content) {
        Ok(()) => {
            // Restrict file permissions so only the owner can read/write it
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(metadata) = std::fs::metadata(&dump_path) {
                    let mut perms = metadata.permissions();
                    perms.set_mode(0o600);
                    if let Err(e) = std::fs::set_permissions(&dump_path, perms) {
                        tracing::warn!(error = %e, "Failed to set crash dump file permissions");
                    }
                }
            }

            tracing::info!(
                path = %dump_path.display(),
                bytes = content.len(),
                "Crash dump saved"
            );
            println!("[gw] Crash dump saved: {}", dump_path.display());
            // Rotate: keep only the 20 most recent dumps
            rotate_crash_dumps(&dump_dir, 20);
            Some(dump_path)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to write crash dump");
            None
        }
    }
}

/// Keep only the N most recent crash dump files (by modification time).
fn rotate_crash_dumps(dump_dir: &std::path::Path, keep: usize) {
    let mut entries: Vec<_> = match std::fs::read_dir(dump_dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };

    if entries.len() <= keep {
        return;
    }

    // Sort by modified time, oldest first.  Use filename as tiebreaker for
    // stable ordering when timestamps are equal (e.g. rapid crash dumps).
    entries.sort_by(|a, b| {
        let time_a = a.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let time_b = b.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        time_a.cmp(&time_b).then_with(|| a.file_name().cmp(&b.file_name()))
    });

    // Remove oldest entries beyond the keep limit
    let to_remove = entries.len() - keep;
    for entry in entries.into_iter().take(to_remove) {
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::debug!(error = %e, path = %entry.path().display(), "Failed to remove old crash dump");
        }
    }
}

/// Compute a hash of content for in-session change detection.
///
/// Uses `DefaultHasher` — not cryptographic and not stable across builds.
/// Suitable only for same-process comparisons, not for persistence.
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

    #[test]
    fn test_save_crash_dump_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        // save_crash_dump now writes a metadata-only report even when the
        // tmux session cannot be contacted — otherwise the daemon vanish
        // path leaves no forensic trail. The returned path must exist
        // and the file must contain the reason string.
        let result = save_crash_dump("gw-nonexistent-test", dir.path(), "test reason");
        let dump_path = result.expect("metadata-only crash dump should be written");
        assert!(dump_path.exists());
        assert!(dir.path().join(".gw").join("crash-dumps").exists());

        let contents = std::fs::read_to_string(&dump_path).unwrap();
        assert!(
            contents.contains("test reason"),
            "dump must include the crash reason; got: {}",
            contents
        );
        assert!(
            contents.contains("scrollback unavailable")
                || contents.contains("scrollback empty"),
            "dump must record why the scrollback was missing; got: {}",
            contents
        );
    }
}