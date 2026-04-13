//! Command implementations for the gw CLI.
//!
//! Each subcommand has its own module with an `execute()` function.
//! The `Commands` enum is re-exported from `config::cli_args`.

pub mod clean;
pub mod daemon;
pub mod elden;
pub mod history;
pub mod logs;
pub mod ps;
pub mod queue;
pub mod replay;
pub mod schedule;
pub mod run;
pub mod status;
pub mod stop;
pub(crate) mod util;

// Re-export Commands from cli_args for convenience
pub use crate::config::cli_args::Commands;