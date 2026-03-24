//! Structured logging setup using tracing-subscriber.
//!
//! This module configures the tracing subscriber for structured output to stdout.
//!
//! # Log Levels
//!
//! The verbosity level is controlled by the `-v` flag count:
//! - No `-v`: WARN level (errors and warnings only)
//! - `-v`: INFO level
//! - `-vv`: DEBUG level
//! - `-vvv` or more: TRACE level

use tracing_subscriber::{fmt, EnvFilter};

/// Initialize the tracing subscriber with stdout output.
///
/// # Arguments
///
/// * `verbosity` - Number of `-v` flags (0-3+)
///
/// # Example
///
/// ```ignore
/// // In main.rs after parsing CLI args:
/// crate::log::init(cli.verbose);
/// ```
pub fn init(verbosity: u8) {
    // Determine log level from verbosity
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    // Build the filter, allowing RUST_LOG env var to override
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new(level));

    // Install the subscriber
    fmt()
        .with_env_filter(filter)
        .with_target(false) // Don't show target module path
        .with_thread_ids(false) // Don't show thread IDs
        .with_thread_names(false) // Don't show thread names
        .with_line_number(false) // Don't show line numbers (cleaner output)
        .init();
}