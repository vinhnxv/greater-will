//! Session management module.
//!
//! Provides abstractions for managing Claude Code sessions via tmux.
//!
//! # Components
//!
//! - [`tmux`] - Basic tmux session management
//! - [`spawn`] - Claude Code startup sequence with Ink autocomplete workaround
//! - [`detect`] - Prompt detection and output velocity tracking

pub mod detect;
pub mod spawn;
mod tmux;

pub use detect::{PromptDetector, OutputVelocity, OutputUpdate, PaneSnapshot, capture_pane};
pub use spawn::{spawn_claude_session, kill_session, wait_for_prompt, send_keys_with_workaround, SpawnConfig};
pub use tmux::Tmux;