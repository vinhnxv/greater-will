//! Batch queue manager for sequential multi-plan execution.
//!
//! Enables `gw run plans/*.md` to execute multiple plans sequentially
//! with circuit breaker protection, atomic state persistence, and resume support.
//!
//! # Components
//!
//! - [`queue`] - BatchRunner with VecDeque queue and circuit breaker
//! - [`state`] - Persistent batch state with atomic writes
//! - [`lock`] - Instance lock preventing concurrent gw instances

pub mod lock;
pub mod queue;
pub mod state;

pub use lock::InstanceLock;
pub use queue::{BatchRunner, BatchSummary};
pub use state::BatchState;
