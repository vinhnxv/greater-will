//! Tmux session management.
//!
//! This module provides a minimal interface for managing tmux sessions
//! used to isolate Claude Code instances during arc phase execution.
//!
//! # Session Naming Convention
//!
//! Sessions are named using the pattern: `gw-<phase>-<timestamp>`
//!
//! Example: `gw-forge-20260324-143052`

use color_eyre::Result;
use std::process::Command;
use std::time::Duration;

/// Timeout for tmux commands (10 seconds should be generous for any tmux op).
const TMUX_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Tmux session manager.
///
/// Provides methods to create, check, and destroy tmux sessions
/// for running Claude Code instances.
pub struct Tmux {
    /// Session name (e.g., "gw-forge-20260324-143052")
    name: String,
}

impl Tmux {
    /// Create a new tmux session manager.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique session name (alphanumeric, hyphens, underscores only, max 64 chars)
    ///
    /// # Errors
    ///
    /// Returns an error if the session name contains invalid characters.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.len() > 64 {
            color_eyre::eyre::bail!("Session name too long: {} (max 64 chars)", name.len());
        }
        if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            color_eyre::eyre::bail!(
                "Session name contains invalid characters: '{}'. Only alphanumeric, hyphens, and underscores allowed.",
                name
            );
        }
        Ok(Self { name })
    }

    /// Get the session name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Check if a tmux session with the given name exists.
    ///
    /// # Arguments
    ///
    /// * `name` - Session name to check
    pub fn has_session(name: &str) -> bool {
        Self::run_tmux_cmd(&["has-session", "-t", name])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run a tmux command with a timeout to prevent indefinite hangs.
    fn run_tmux_cmd(args: &[&str]) -> Result<std::process::Output> {
        let child = Command::new("tmux")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        // Capture child PID before moving ownership, so we can kill on timeout.
        let child_id = child.id();

        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = child.wait_with_output();
            let _ = tx.send(result);
        });

        match rx.recv_timeout(TMUX_CMD_TIMEOUT) {
            Ok(result) => {
                let _ = thread.join();
                Ok(result?)
            }
            Err(_) => {
                // Timeout — kill the child process by PID to avoid leaking
                // both the OS thread and the zombie tmux process.
                #[cfg(unix)]
                unsafe {
                    libc::kill(child_id as i32, libc::SIGKILL);
                }
                // Join the thread (it will return now that the child is killed)
                let _ = thread.join();
                color_eyre::eyre::bail!("tmux command timed out after {:?}", TMUX_CMD_TIMEOUT)
            }
        }
    }

    /// Create a new detached tmux session.
    ///
    /// The session is created in detached mode, ready for commands.
    ///
    /// # Errors
    ///
    /// Returns an error if tmux is not available or session creation fails.
    pub fn create_session(&self) -> Result<()> {
        let output = Self::run_tmux_cmd(&[
            "new-session",
            "-d",
            "-s", &self.name,
            "-x", "200",
            "-y", "50",
        ])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            color_eyre::eyre::bail!(
                "Failed to create tmux session '{}': {}",
                self.name,
                stderr.trim()
            );
        }

        Ok(())
    }

    /// Kill the tmux session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session doesn't exist or kill fails.
    pub fn kill_session(&self) -> Result<()> {
        let output = Self::run_tmux_cmd(&["kill-session", "-t", &self.name])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("Failed to kill tmux session '{}': {}", self.name, stderr);
        }

        Ok(())
    }

    /// Send a command to the tmux session.
    ///
    /// # Arguments
    ///
    /// * `cmd` - Command to send (Enter key is appended automatically)
    pub fn send_command(&self, cmd: &str) -> Result<()> {
        Self::run_tmux_cmd(&["send-keys", "-t", &self.name, cmd, "Enter"])?;

        Ok(())
    }

    /// Capture the current pane content.
    ///
    /// Returns the visible text in the current pane.
    pub fn capture_pane(&self) -> Result<String> {
        let output = Self::run_tmux_cmd(&["capture-pane", "-t", &self.name, "-p"])?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tmux_new() {
        let tmux = Tmux::new("test-session").unwrap();
        assert_eq!(tmux.name(), "test-session");
    }

    #[test]
    fn test_tmux_new_rejects_invalid_chars() {
        assert!(Tmux::new("test;injection").is_err());
        assert!(Tmux::new("test session").is_err());
        assert!(Tmux::new("test'quote").is_err());
    }

    #[test]
    fn test_tmux_new_rejects_long_name() {
        let long_name = "a".repeat(65);
        assert!(Tmux::new(long_name).is_err());
    }

    #[test]
    fn test_has_session_nonexistent() {
        // This session should not exist
        assert!(!Tmux::has_session("gw-nonexistent-test-session-xyz"));
    }
}