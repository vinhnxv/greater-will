//! Command implementations for the gw CLI.
//!
//! Each subcommand has its own module with an `execute()` function.
//! The `Commands` enum is re-exported from `config::cli_args`.

pub mod clean;
pub mod elden;
pub mod replay;
pub mod run;
pub mod status;

// Re-export Commands from cli_args for convenience
pub use crate::config::cli_args::Commands;