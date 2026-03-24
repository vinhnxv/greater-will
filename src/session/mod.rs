//! Session management module.
//!
//! Provides abstractions for managing Claude Code sessions via tmux.

mod tmux;

pub use tmux::Tmux;