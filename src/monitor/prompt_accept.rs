//! Auto-accept permission prompts in Claude Code sessions.
//!
//! When Claude Code asks "Allow tool? (y/n)", the session stalls if no human
//! is present. This module detects permission prompt patterns in pane output
//! and auto-sends Enter via tmux to unblock the session.
//!
//! # Safety
//!
//! - Shell prompts (`❯`, `$`, `#`, `%`) are explicitly blocked to prevent
//!   sending Enter to a bare terminal
//! - Debounce prevents rapid-fire accepts within a configurable interval
//! - Uses bare `tmux send-keys Enter` (NOT `send_keys_with_workaround`)
//!   because permission prompts are native terminal prompts, not Ink-rendered

use std::time::{Duration, Instant};

/// Patterns that indicate a permission/confirmation prompt.
///
/// Checked against the last non-empty line of pane output (lowercased).
const PROMPT_PATTERNS: &[&str] = &[
    "? (y/n)",
    "? (yes/no)",
    "(y/n)",
    "(yes/no)",
    "allow tool",
    "allow edit",
    "allow write",
    "allow bash",
    "allow read",
    "allow mcp",
    "do you want to proceed",
    "do you want to continue",
    "press enter to continue",
    "press enter",
    "continue?",
];

/// Shell prompt patterns — NEVER auto-accept these.
///
/// If the last line looks like a shell prompt, the session is idle at a
/// terminal, not waiting for permission.
const SHELL_PROMPTS: &[&str] = &["❯", "$ ", "# ", "% "];

/// Detects permission prompts in pane output and auto-sends Enter.
#[derive(Debug)]
pub struct PromptAcceptor {
    /// When the last accept was sent (for debounce).
    last_accept: Option<Instant>,
    /// Minimum interval between auto-accepts.
    debounce: Duration,
    /// Whether auto-accept is enabled.
    enabled: bool,
    /// Total number of prompts accepted this session.
    accept_count: u32,
}

impl PromptAcceptor {
    /// Create a new acceptor.
    ///
    /// `debounce_secs` controls the minimum interval between auto-accepts.
    pub fn new(enabled: bool, debounce_secs: u64) -> Self {
        Self {
            last_accept: None,
            debounce: Duration::from_secs(debounce_secs),
            enabled,
            accept_count: 0,
        }
    }

    /// Check the last line of pane content for a permission prompt.
    ///
    /// Returns `true` if a prompt was detected and Enter was sent.
    pub fn check_and_accept(
        &mut self,
        pane_content: &str,
        session_name: &str,
    ) -> bool {
        if !self.enabled {
            return false;
        }

        let last_line = pane_content
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        // Safety: never accept shell prompts
        if SHELL_PROMPTS.iter().any(|p| last_line.contains(p)) {
            return false;
        }

        // Check for permission patterns
        if !PROMPT_PATTERNS.iter().any(|p| last_line.contains(p)) {
            return false;
        }

        // Debounce: max 1 accept per interval
        if let Some(last) = self.last_accept {
            if last.elapsed() < self.debounce {
                return false;
            }
        }

        if send_enter(session_name).is_ok() {
            self.last_accept = Some(Instant::now());
            self.accept_count += 1;
            tracing::info!(
                pattern = %last_line.trim(),
                count = self.accept_count,
                "Auto-accepted permission prompt"
            );
            println!("[gw] Auto-accepted: {}", last_line.trim());
            return true;
        }
        false
    }

    /// Total number of prompts accepted this session.
    pub fn accept_count(&self) -> u32 {
        self.accept_count
    }
}

/// Send bare Enter key to tmux session.
///
/// Uses raw `tmux send-keys Enter` rather than `send_keys_with_workaround`
/// because permission prompts are native terminal prompts, not Ink-rendered.
fn send_enter(session_name: &str) -> color_eyre::Result<()> {
    let output = std::process::Command::new("tmux")
        .args(["send-keys", "-t", session_name, "Enter"])
        .output()?;
    if !output.status.success() {
        color_eyre::eyre::bail!("tmux send-keys failed: {:?}", output.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: check if the detection logic matches a given pane content.
    ///
    /// Isolates pattern detection from tmux side-effects so tests
    /// run without a real tmux session.
    fn detects_prompt(content: &str) -> bool {
        let last_line = content
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        if SHELL_PROMPTS.iter().any(|p| last_line.contains(p)) {
            return false;
        }

        PROMPT_PATTERNS.iter().any(|p| last_line.contains(p))
    }

    #[test]
    fn test_prompt_patterns_match() {
        for pattern in PROMPT_PATTERNS {
            let content = format!("Some output\n{}", pattern);
            assert!(
                detects_prompt(&content),
                "Pattern should match: {}",
                pattern
            );
        }
    }

    #[test]
    fn test_shell_prompts_never_match() {
        // Shell prompts should be blocked even if they contain a pattern keyword
        for prompt in SHELL_PROMPTS {
            let content = format!("output\n{}", prompt);
            assert!(
                !detects_prompt(&content),
                "Shell prompt should be blocked: {}",
                prompt
            );
        }
    }

    #[test]
    fn test_debounce_prevents_spam() {
        let mut acceptor = PromptAcceptor::new(true, 60);
        // Simulate a recent accept
        acceptor.last_accept = Some(Instant::now());
        // Should be blocked by debounce (send_enter will also fail without tmux,
        // but debounce check runs first)
        let result = acceptor.check_and_accept("Allow tool? (y/n)", "fake-session");
        assert!(!result, "Should be blocked by debounce");
    }

    #[test]
    fn test_disabled_returns_false() {
        let mut acceptor = PromptAcceptor::new(false, 0);
        let result = acceptor.check_and_accept("Allow tool? (y/n)", "fake-session");
        assert!(!result, "Disabled acceptor should return false");
    }

    #[test]
    fn test_empty_content_returns_false() {
        assert!(!detects_prompt(""));
        assert!(!detects_prompt("   \n  \n  "));
    }

    #[test]
    fn test_normal_output_not_matched() {
        assert!(!detects_prompt("Building project...\nCompilation succeeded"));
        assert!(!detects_prompt("Running tests\n42 passed, 0 failed"));
    }
}
