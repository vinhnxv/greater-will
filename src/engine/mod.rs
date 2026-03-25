//! Engine module for Greater-Will PhaseSession Executor.
//!
//! This module provides the core execution engine that runs phase groups
//! in isolated Claude Code sessions.
//!
//! # Components
//!
//! - [`completion`] - 4-layer completion detection (checkpoint polling, timeout, idle nudge, prompt return)
//! - [`retry`] - Per-group retry with exponential backoff
//! - [`phase_executor`] - Core state machine and execution loop
//!
//! # Architecture
//!
//! ```text
//! PhaseGroupExecutor
//!    │
//!    ├── PhaseGroupState (state machine)
//!    │       ├── Pending → PreFlight → Spawning → WaitingReady
//!    │       └── Dispatching → Running → Completing → Succeeded/Failed/Skipped
//!    │
//!    ├── CompletionDetector (4-layer monitoring)
//!    │       ├── Layer 1: Checkpoint polling (every 3s)
//!    │       ├── Layer 2: Phase timeout (configurable per group)
//!    │       ├── Layer 3: Idle detection + nudge (3min idle → nudge)
//!    │       └── Layer 4: Prompt return detection (❯ symbol)
//!    │
//!    └── RetryCoordinator
//!            └── ErrorClass → Backoff → MaxRetries mapping
//! ```
//!
//! # Example
//!
//! ```ignore
//! use greater_will::engine::PhaseGroupExecutor;
//! use greater_will::config::PhaseConfig;
//!
//! let config = PhaseConfig::from_file("config/default-phases.toml")?;
//! let executor = PhaseGroupExecutor::new(config);
//!
//! executor.execute_plan("plans/feature.md")?;
//! ```

pub mod completion;
pub mod phase_executor;
pub mod retry;

pub use completion::{CompletionDetector, CompletionEvent, CompletionState};
pub use phase_executor::{PhaseGroupExecutor, PhaseGroupResult, PhaseGroupState, ExecutorConfig, append_result_jsonl};
pub use retry::{BackoffStrategy, ErrorClass, RetryCoordinator, RetryDecision};