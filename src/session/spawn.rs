//! Claude Code session spawning utilities.
//!
//! This module handles the creation and startup of Claude Code sessions
//! within tmux, including the critical Ink autocomplete workaround.
//!
//! # Ink Autocomplete Workaround
//!
//! Claude Code uses Ink (React-based terminal UI). When we send text
//! followed by Enter, Ink intercepts Enter for autocomplete suggestions.
//!
//! The workaround:
//! 1. Send text literally with `-l` flag
//! 2. Wait 300ms for autocomplete to render
//! 3. Send Escape to dismiss autocomplete
//! 4. Wait 100ms
//! 5. Send Enter to submit
//!
//! # Session Naming
//!
//! Sessions are named: `gw-{plan_hash}-{group}`
//!
//! Example: `gw-a1b2c3d4-A`
//!
//! # Example
//!
//! ```ignore
//! use greater_will::session::spawn::{spawn_claude_session, SpawnConfig};
//!
//! let config = SpawnConfig {
//!     session_id: "gw-a1b2c3d4-A",
//!     working_dir: PathBuf::from("/project"),
//!     config_dir: Some(PathBuf::from(".rune")),
//!     claude_path: "claude",
//!     mock: false,
//! };
//!
//! let pid = spawn_claude_session(&config)?;
//! println!("Claude Code started with PID: {}", pid);
//! ```

use color_eyre::eyre::{eyre, Context};
use color_eyre::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Delay in milliseconds for Ink autocomplete workaround.
/// 300ms is empirically determined - too short = autocomplete not rendered.
const SEND_DELAY_MS: u64 = 300;

/// Short delay after Escape for Ink processing.
const ESCAPE_DELAY_MS: u64 = 100;

/// Default wait time for Claude Code initialization (12 seconds).
const CLAUDE_INIT_SECS: u64 = 12;

/// Configuration for spawning a Claude Code session.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Unique session ID (e.g., "gw-a1b2c3d4-A").
    pub session_id: String,
    /// Working directory for the session.
    pub working_dir: PathBuf,
    /// Optional config directory for CLAUDE_CONFIG_DIR.
    pub config_dir: Option<PathBuf>,
    /// Path to claude executable.
    pub claude_path: String,
    /// Use mock script instead of real Claude.
    pub mock: bool,
}

impl SpawnConfig {
    /// Create a spawn config for a phase group.
    pub fn new(
        plan_hash: &str,
        group_name: &str,
        working_dir: PathBuf,
        config_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id: format!("gw-{}-{}", plan_hash, group_name),
            working_dir,
            config_dir,
            claude_path: "claude".to_string(),
            mock: false,
        }
    }

    /// Enable mock mode for testing.
    pub fn with_mock(mut self) -> Self {
        self.mock = true;
        self
    }

    /// Set a custom claude path.
    pub fn with_claude_path(mut self, path: impl Into<String>) -> Self {
        self.claude_path = path.into();
        self
    }

    /// Validate the session ID format.
    ///
    /// Session IDs must be:
    /// - Alphanumeric with hyphens and underscores
    /// - Maximum 64 characters
    pub fn validate_session_id(&self) -> Result<()> {
        if self.session_id.len() > 64 {
            return Err(eyre!(
                "Session ID too long: {} (max 64 chars)",
                self.session_id.len()
            ));
        }

        if !self
            .session_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(eyre!(
                "Session ID contains invalid characters: {}",
                self.session_id
            ));
        }

        Ok(())
    }
}

/// Spawn a Claude Code session in a new tmux session.
///
/// This function:
/// 1. Creates a new detached tmux session
/// 2. Starts Claude Code with `--dangerously-skip-permissions`
/// 3. Waits for initialization
/// 4. Returns the PID of the pane's child process
///
/// # Arguments
///
/// * `config` - Spawn configuration
///
/// # Returns
///
/// The PID of the Claude Code process (or shell PID if mock mode).
///
/// # Errors
///
/// Returns an error if:
/// - Session ID is invalid
/// - Tmux session creation fails
/// - Claude Code fails to start
pub fn spawn_claude_session(config: &SpawnConfig) -> Result<u32> {
    config.validate_session_id()?;

    info!(
        session_id = %config.session_id,
        working_dir = %config.working_dir.display(),
        mock = config.mock,
        "Spawning Claude Code session"
    );

    // Check if session already exists
    if has_session(&config.session_id) {
        return Err(eyre!(
            "Session '{}' already exists",
            config.session_id
        ));
    }

    // Create tmux session
    create_tmux_session(&config.session_id, &config.working_dir)?;

    // Start Claude Code (or mock)
    if config.mock {
        start_mock_session(&config.session_id)?;
    } else {
        start_claude(&config.session_id, &config.config_dir, &config.claude_path)?;
    }

    // Get the PID of the pane's child process
    let pid = get_pane_pid(&config.session_id)?;

    info!(
        session_id = %config.session_id,
        pid = pid,
        "Claude Code session started"
    );

    Ok(pid)
}

/// Check if a tmux session exists.
pub fn has_session(session_id: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session_id])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a new detached tmux session.
fn create_tmux_session(session_id: &str, working_dir: &Path) -> Result<()> {
    debug!(
        session_id = %session_id,
        working_dir = %working_dir.display(),
        "Creating tmux session"
    );

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",                       // Detached
            "-s", session_id,           // Session name
            "-x", "200",                // Width (stable output)
            "-y", "50",                 // Height
            "-c", &working_dir.to_string_lossy(), // Working directory
        ])
        .output()
        .wrap_err_with(|| format!("Failed to create tmux session '{}'", session_id))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("Failed to create tmux session '{}': {}", session_id, stderr));
    }

    Ok(())
}

/// Start Claude Code in an existing tmux session.
///
/// Uses the Ink autocomplete workaround for command submission.
fn start_claude(
    session_id: &str,
    config_dir: &Option<PathBuf>,
    claude_path: &str,
) -> Result<()> {
    let mut cmd = String::new();

    // Set CLAUDE_CONFIG_DIR if non-default
    if let Some(dir) = config_dir {
        let is_default = dir
            .file_name()
            .map(|n| n == ".claude")
            .unwrap_or(false);

        if !is_default {
            cmd.push_str(&format!(
                "CLAUDE_CONFIG_DIR={} ",
                shell_escape(&dir.to_string_lossy())
            ));
        }
    }

    // Add claude command with skip permissions
    // Note: don't shell_escape the claude path — it's sent via tmux send-keys
    // which interprets literally. Escaping wraps it in quotes that break execution.
    cmd.push_str(&format!("{} --dangerously-skip-permissions", claude_path));

    debug!(
        session_id = %session_id,
        command = %cmd,
        "Starting Claude Code"
    );

    // Send command directly — this is a shell command, not Claude Code Ink UI.
    // The Ink workaround (Escape before Enter) is only needed INSIDE Claude Code.
    // For starting Claude itself, plain send-keys + Enter works.
    send_simple_command(session_id, &cmd)?;

    // Wait for initialization
    info!("Waiting {}s for Claude Code initialization...", CLAUDE_INIT_SECS);
    std::thread::sleep(Duration::from_secs(CLAUDE_INIT_SECS));

    Ok(())
}

/// Start a mock session for testing.
fn start_mock_session(session_id: &str) -> Result<()> {
    // Send a simple sleep command that acts as a mock
    let cmd = "echo 'Mock Claude session ready' && sleep 3600";

    send_keys_with_workaround(session_id, cmd)?;

    // Short wait for mock
    std::thread::sleep(Duration::from_secs(1));

    Ok(())
}

/// Send keys with the Ink autocomplete workaround.
///
/// The workaround sequence:
/// 1. Send text literally (no Enter)
/// 2. Wait 300ms for autocomplete to render
/// 3. Send Escape to dismiss autocomplete
/// 4. Wait 100ms
/// 5. Send Enter to submit
pub fn send_keys_with_workaround(session_id: &str, text: &str) -> Result<()> {
    debug!(
        session_id = %session_id,
        text = %text,
        "Sending keys with Ink workaround"
    );

    // Step 1: Send text literally (no Enter)
    Command::new("tmux")
        .args(["send-keys", "-t", session_id, "-l", text])
        .output()
        .wrap_err("Failed to send text literally")?;

    // Step 2: Wait for autocomplete to render
    std::thread::sleep(Duration::from_millis(SEND_DELAY_MS));

    // Step 3: Send Escape to dismiss autocomplete
    Command::new("tmux")
        .args(["send-keys", "-t", session_id, "Escape"])
        .output()
        .wrap_err("Failed to send Escape")?;

    // Step 4: Brief wait for Ink to process
    std::thread::sleep(Duration::from_millis(ESCAPE_DELAY_MS));

    // Step 5: Send Enter to submit
    Command::new("tmux")
        .args(["send-keys", "-t", session_id, "Enter"])
        .output()
        .wrap_err("Failed to send Enter")?;

    Ok(())
}

/// Send a simple command (without workaround) to the session.
pub fn send_simple_command(session_id: &str, cmd: &str) -> Result<()> {
    Command::new("tmux")
        .args(["send-keys", "-t", session_id, cmd, "Enter"])
        .output()
        .wrap_err("Failed to send command")?;

    Ok(())
}

/// Get the PID of the pane's child process.
///
/// This returns the PID of the process running in the tmux pane,
/// which is Claude Code (or the shell if mock mode).
pub fn get_pane_pid(session_id: &str) -> Result<u32> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t", session_id,
            "-F", "#{pane_pid}",
        ])
        .output()
        .wrap_err("Failed to get pane PID")?;

    let pid_str = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .ok_or_else(|| eyre!("No pane found for session '{}'", session_id))?
        .trim()
        .to_string();

    let pid: u32 = pid_str
        .parse()
        .wrap_err_with(|| format!("Invalid PID: {}", pid_str))?;

    Ok(pid)
}

/// Kill a tmux session.
///
/// This sends SIGTERM to the session, which propagates to child processes.
pub fn kill_session(session_id: &str) -> Result<()> {
    info!(session_id = %session_id, "Killing tmux session");

    let output = Command::new("tmux")
        .args(["kill-session", "-t", session_id])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            debug!(session_id = %session_id, "Session killed");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            warn!(session_id = %session_id, stderr = %stderr, "kill-session returned non-zero");
        }
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to kill session");
        }
    }

    Ok(())
}

/// Shell-escape a string for safe command-line use.
///
/// Wraps in single quotes and escapes internal single quotes.
pub fn shell_escape(s: &str) -> String {
    if s.contains('\'') {
        // Replace ' with '\''
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        format!("'{}'", s)
    }
}

/// Wait for the Claude Code prompt to appear.
///
/// Polls the session until the `❯` prompt is detected,
/// indicating Claude Code is ready to receive commands.
///
/// # Arguments
///
/// * `session_id` - The tmux session ID
/// * `timeout` - Maximum time to wait
///
/// # Returns
///
/// `Ok(())` if prompt was detected, `Err` if timeout.
pub fn wait_for_prompt(session_id: &str, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(500);

    info!(
        session_id = %session_id,
        timeout_secs = timeout.as_secs(),
        "Waiting for Claude Code prompt"
    );

    while start.elapsed() < timeout {
        if has_prompt(session_id)? {
            info!(session_id = %session_id, "Prompt detected");
            return Ok(());
        }

        std::thread::sleep(poll_interval);
    }

    Err(eyre!(
        "Timeout waiting for prompt in session '{}'",
        session_id
    ))
}

/// Check if the prompt (❯) is visible in the session.
fn has_prompt(session_id: &str) -> Result<bool> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", session_id, "-p"])
        .output()
        .wrap_err("Failed to capture pane")?;

    let content = String::from_utf8_lossy(&output.stdout);

    // Check last few non-empty lines for prompt
    let has_prompt = content
        .lines()
        .rev()
        .take(5)
        .any(|line| line.contains('❯'));

    Ok(has_prompt)
}

/// Capture the current pane content.
pub fn capture_pane(session_id: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", session_id, "-p"])
        .output()
        .wrap_err("Failed to capture pane")?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_config_new() {
        let config = SpawnConfig::new(
            "a1b2c3d4",
            "A",
            PathBuf::from("/project"),
            Some(PathBuf::from(".rune")),
        );

        assert_eq!(config.session_id, "gw-a1b2c3d4-A");
        assert_eq!(config.working_dir, PathBuf::from("/project"));
        assert!(!config.mock);
    }

    #[test]
    fn test_spawn_config_with_mock() {
        let config = SpawnConfig::new(
            "test",
            "B",
            PathBuf::from("/tmp"),
            None,
        ).with_mock();

        assert!(config.mock);
    }

    #[test]
    fn test_spawn_config_validate_session_id_valid() {
        let config = SpawnConfig::new(
            "abc123",
            "A",
            PathBuf::from("/tmp"),
            None,
        );

        assert!(config.validate_session_id().is_ok());
    }

    #[test]
    fn test_spawn_config_validate_session_id_with_hyphens() {
        let config = SpawnConfig {
            session_id: "gw-test-123-ABC".to_string(),
            working_dir: PathBuf::from("/tmp"),
            config_dir: None,
            claude_path: "claude".to_string(),
            mock: false,
        };

        assert!(config.validate_session_id().is_ok());
    }

    #[test]
    fn test_spawn_config_validate_session_id_too_long() {
        let config = SpawnConfig {
            session_id: "a".repeat(100),
            working_dir: PathBuf::from("/tmp"),
            config_dir: None,
            claude_path: "claude".to_string(),
            mock: false,
        };

        assert!(config.validate_session_id().is_err());
    }

    #[test]
    fn test_spawn_config_validate_session_id_invalid_chars() {
        let config = SpawnConfig {
            session_id: "invalid@session!".to_string(),
            working_dir: PathBuf::from("/tmp"),
            config_dir: None,
            claude_path: "claude".to_string(),
            mock: false,
        };

        assert!(config.validate_session_id().is_err());
    }

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn test_shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_escape_with_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_has_session_nonexistent() {
        // This session should not exist
        assert!(!has_session("gw-nonexistent-test-session-xyz-999"));
    }
}