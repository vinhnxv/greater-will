//! Structured logging setup using tracing-subscriber.
//!
//! This module configures the tracing subscriber for structured output to stdout,
//! with an optional file sink controlled by `GW_LOG_FILE` environment variable.
//!
//! # Log Levels
//!
//! The verbosity level is controlled by the `-v` flag count:
//! - No `-v`: WARN level (errors and warnings only)
//! - `-v`: INFO level
//! - `-vv`: DEBUG level
//! - `-vvv` or more: TRACE level
//!
//! # File Logging
//!
//! Set `GW_LOG_FILE` to a directory path to enable file logging:
//! ```bash
//! GW_LOG_FILE=/tmp/gw-logs gw run --dry-run plans/*.md
//! ```
//! Log files are written as `gw.log` in the specified directory with daily rotation.

use tracing_subscriber::{fmt, EnvFilter, prelude::*};

/// Initialize the tracing subscriber with stdout output and optional file sink.
///
/// # Arguments
///
/// * `verbosity` - Number of `-v` flags (0-3+)
///
/// # File Sink
///
/// When `GW_LOG_FILE` is set to a directory path, logs are also written to
/// `{GW_LOG_FILE}/gw.log` with daily rotation.
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

    // Check for file sink directory
    if let Ok(log_dir) = std::env::var("GW_LOG_FILE") {
        let file_appender = tracing_appender::rolling::daily(&log_dir, "gw.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        // Leak the guard so it lives for the program's lifetime.
        // This is intentional — the guard must outlive all logging calls.
        std::mem::forget(_guard);

        let stdout_layer = fmt::layer()
            .with_target(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_line_number(false);

        let file_layer = fmt::layer()
            .with_writer(non_blocking)
            .with_target(true)
            .with_thread_ids(true)
            .with_ansi(false); // No ANSI colors in file output

        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .with(file_layer)
            .init();
    } else {
        // Stdout-only mode
        fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_line_number(false)
            .init();
    }
}
