#![allow(dead_code)]
//! Stale tmux session cleanup.
//!
//! Provides utilities for detecting and cleaning up orphaned tmux
//! sessions from crashed or interrupted Greater-Will runs.

use color_eyre::{eyre::eyre, Result};
use std::process::Command;

/// Prefix for Greater-Will tmux sessions.
const GW_SESSION_PREFIX: &str = "gw-";

/// Minimum session age (in seconds) to consider stale.
const STALE_THRESHOLD_SECS: u64 = 600; // 10 minutes

/// Error types for tmux cleanup.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TmuxCleanupError {
    /// Failed to list sessions.
    #[error("Failed to list tmux sessions: {message}")]
    ListFailed { message: String },

    /// Failed to kill session.
    #[error("Failed to kill tmux session '{session}': {message}")]
    KillFailed { session: String, message: String },

    /// Session does not exist.
    #[error("Tmux session '{session}' does not exist")]
    SessionNotFound { session: String },
}

/// Information about a tmux session.
#[derive(Debug, Clone)]
pub struct TmuxSession {
    /// Session name.
    pub name: String,
    /// Session creation time (Unix timestamp, if available).
    pub created: Option<u64>,
    /// Whether the session has a live process.
    pub has_process: bool,
}

/// List all Greater-Will tmux sessions.
///
/// Returns sessions matching the `gw-*` pattern.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::list_gw_sessions;
///
/// let sessions = list_gw_sessions()?;
/// for session in &sessions {
///     println!("Session: {}", session.name);
/// }
/// ```
pub fn list_gw_sessions() -> Result<Vec<TmuxSession>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            // tmux not installed or no sessions
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::debug!("tmux not found, returning empty session list");
                return Ok(Vec::new());
            }
            return Err(eyre!("Failed to run tmux: {}", e));
        }
    };

    // No sessions is not an error
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no server running")
            || stderr.contains("no sessions")
            || stderr.contains("No such file or directory")
        {
            return Ok(Vec::new());
        }
        return Err(eyre!("tmux list-sessions failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let sessions: Vec<TmuxSession> = stdout
        .lines()
        .filter(|line| line.starts_with(GW_SESSION_PREFIX))
        .map(|name| TmuxSession {
            name: name.to_string(),
            created: None,
            has_process: true, // Assume true until proven otherwise
        })
        .collect();

    Ok(sessions)
}

/// Check if a tmux session exists.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::session_exists;
///
/// if session_exists("gw-a1b2c3d4-A") {
///     println!("Session exists");
/// }
/// ```
pub fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Kill a tmux session.
///
/// # Arguments
///
/// * `name` - Session name to kill
///
/// # Errors
///
/// Returns an error if the session cannot be killed.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::kill_session;
///
/// kill_session("gw-a1b2c3d4-A")?;
/// ```
pub fn kill_session(name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Session already gone or no tmux server is not an error
        if stderr.contains("can't find session")
            || stderr.contains("no session")
            || stderr.contains("no server running")
        {
            return Ok(());
        }
        return Err(eyre!("Failed to kill session '{}': {}", name, stderr));
    }

    tracing::info!(session = %name, "Killed tmux session");
    Ok(())
}

/// Get the PID of the main process in a tmux session pane.
///
/// This is the child process that would be running Claude Code.
///
/// # Arguments
///
/// * `session` - Session name
///
/// # Returns
///
/// `Some(pid)` if a process is running, `None` otherwise.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::get_session_pid;
///
/// if let Some(pid) = get_session_pid("gw-a1b2c3d4-A") {
///     println!("Session has process with PID {}", pid);
/// }
/// ```
pub fn get_session_pid(session: &str) -> Option<u32> {
    let output = Command::new("tmux")
        .args(["display-message", "-t", session, "-p", "#{pane_pid}"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let pid_str = String::from_utf8_lossy(&output.stdout);
    pid_str.trim().parse::<u32>().ok()
}

/// Get session creation time.
///
/// Returns the session's creation timestamp if available.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::get_session_created;
///
/// if let Some(created) = get_session_created("gw-a1b2c3d4-A") {
///     println!("Session created at {}", created);
/// }
/// ```
pub fn get_session_created(session: &str) -> Option<u64> {
    // tmux stores creation time, but accessing it requires specific format strings
    // For simplicity, we'll use the session's activity time
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-t",
            session,
            "-p",
            "#{session_created}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let created_str = String::from_utf8_lossy(&output.stdout);
    created_str.trim().parse::<u64>().ok()
}

/// Check if a session is stale (no active process and older than threshold).
///
/// # Arguments
///
/// * `session` - Session name
/// * `stale_threshold_secs` - Age threshold in seconds
///
/// # Returns
///
/// `true` if the session is considered stale.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::is_session_stale;
///
/// if is_session_stale("gw-a1b2c3d4-A", 600) {
///     println!("Session is stale");
/// }
/// ```
pub fn is_session_stale(session: &str, stale_threshold_secs: u64) -> bool {
    // Check if there's an active process
    if let Some(pid) = get_session_pid(session) {
        // Check if process is alive
        if crate::cleanup::process::is_pid_alive(pid) {
            return false;
        }
    }

    // No active process - check age
    if let Some(created) = get_session_created(session) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let age = now.saturating_sub(created);
        return age >= stale_threshold_secs;
    }

    // If we can't determine age, assume stale
    true
}

/// Clean up all stale Greater-Will sessions.
///
/// Kills sessions that have no active process and are older than
/// the stale threshold (default 10 minutes).
///
/// # Returns
///
/// Number of sessions cleaned up.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::cleanup_stale_sessions;
///
/// let cleaned = cleanup_stale_sessions()?;
/// println!("Cleaned up {} stale sessions", cleaned);
/// ```
pub fn cleanup_stale_sessions() -> Result<usize> {
    let sessions = list_gw_sessions()?;
    let mut cleaned = 0;

    for session in sessions {
        if is_session_stale(&session.name, STALE_THRESHOLD_SECS) {
            tracing::info!(session = %session.name, "Cleaning up stale session");

            match kill_session(&session.name) {
                Ok(()) => cleaned += 1,
                Err(e) => {
                    tracing::warn!(session = %session.name, error = %e, "Failed to kill stale session");
                }
            }
        }
    }

    if cleaned > 0 {
        tracing::info!(count = cleaned, "Cleaned up stale sessions");
    }

    Ok(cleaned)
}

/// Kill all Greater-Will sessions.
///
/// Used by `gw clean` command to forcibly terminate all sessions.
///
/// # Returns
///
/// Number of sessions killed.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::kill_all_gw_sessions;
///
/// let killed = kill_all_gw_sessions()?;
/// println!("Killed {} sessions", killed);
/// ```
pub fn kill_all_gw_sessions() -> Result<usize> {
    let sessions = list_gw_sessions()?;
    let mut killed = 0;

    for session in sessions {
        tracing::info!(session = %session.name, "Killing session");

        match kill_session(&session.name) {
            Ok(()) => killed += 1,
            Err(e) => {
                tracing::warn!(session = %session.name, error = %e, "Failed to kill session");
            }
        }
    }

    Ok(killed)
}

/// Get detailed status of all Greater-Will sessions.
///
/// Returns session information including process status.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::tmux_cleanup::get_sessions_status;
///
/// let status = get_sessions_status()?;
/// for (name, has_process) in &status {
///     println!("{}: {}", name, if *has_process { "active" } else { "idle" });
/// }
/// ```
pub fn get_sessions_status() -> Result<Vec<(String, bool)>> {
    let sessions = list_gw_sessions()?;

    let status: Vec<(String, bool)> = sessions
        .iter()
        .map(|s| {
            let has_process = get_session_pid(&s.name)
                .map(crate::cleanup::process::is_pid_alive)
                .unwrap_or(false);
            (s.name.clone(), has_process)
        })
        .collect();

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_exists_nonexistent() {
        assert!(!session_exists("gw-nonexistent-test-xyz-12345"));
    }

    #[test]
    fn test_list_gw_sessions_empty() {
        // This test may find real sessions, which is fine
        let result = list_gw_sessions();
        assert!(result.is_ok());
    }

    #[test]
    fn test_kill_session_nonexistent() {
        // Killing a nonexistent session should succeed (no-op)
        let result = kill_session("gw-nonexistent-test-xyz-12345");
        assert!(result.is_ok());
    }
}