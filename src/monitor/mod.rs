//! Monitor module for phase group execution monitoring.
//!
//! This module provides the monitoring infrastructure for tracking
//! phase group progress and detecting issues.
//!
//! # Components
//!
//! - [`checkpoint_poll`] - Checkpoint file watcher (polls every 3s)
//! - [`nudge`] - Idle detection and nudge sending
//!
//! # Monitoring Strategy
//!
//! The monitor uses a layered approach:
//!
//! 1. **Checkpoint Polling (PRIMARY)**: Watch checkpoint.json for phase status changes
//! 2. **Idle Detection**: Monitor pane output for changes, nudge if stagnant
//!
//! # Example
//!
//! ```ignore
//! use greater_will::monitor::{CheckpointWatcher, NudgeManager};
//!
//! let watcher = CheckpointWatcher::new(".rune/arc/arc-123/checkpoint.json");
//! let nudge_mgr = NudgeManager::new("gw-session-A");
//!
//! loop {
//!     // Check checkpoint
//!     if watcher.has_progressed()? {
//!         println!("Phase progressed");
//!     }
//!
//!     // Check for idle
//!     if nudge_mgr.should_nudge()? {
//!         nudge_mgr.send_nudge()?;
//!     }
//! }
//! ```

pub mod checkpoint_poll;
pub mod loop_state;
pub mod nudge;
pub mod prompt_accept;
pub mod session_owner;

