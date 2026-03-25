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
        Command::new("tmux")
            .args(["has-session", "-t", name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Create a new detached tmux session.
    ///
    /// The session is created in detached mode, ready for commands.
    ///
    /// # Errors
    ///
    /// Returns an error if tmux is not available or session creation fails.
    pub fn create_session(&self) -> Result<()> {
        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",           // Detached
                "-s", &self.name, // Session name
                "-x", "200",    // Width (stable output, matches spawn.rs)
                "-y", "50",     // Height
            ])
            .output()?;

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
        let output = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output()?;

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
        Command::new("tmux")
            .args(["send-keys", "-t", &self.name, cmd, "Enter"])
            .output()?;

        Ok(())
    }

    /// Capture the current pane content.
    ///
    /// Returns the visible text in the current pane.
    pub fn capture_pane(&self) -> Result<String> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", &self.name, "-p"])
            .output()?;

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