//! Structured logging setup using tracing-subscriber.
//!
//! This module configures a dual-layer tracing subscriber:
//!
//! - **stdout** — human-readable, no timestamps (for interactive use)
//! - **file** — structured with timestamps (for post-run analysis)
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
/// # Dual-layer design
///
/// The stdout layer uses a human-friendly format without timestamps —
/// timestamps add noise when you're watching output live. The file
/// layer includes full structured metadata (target, thread IDs) and
/// disables ANSI escapes so log files are grep-friendly.
///
/// Each layer gets its own `EnvFilter` so you can keep stdout quiet
/// (`WARN`) while the file captures everything (`DEBUG`).
///
/// # Arguments
///
/// * `verbosity` - Number of `-v` flags (0-3+)
pub fn init(verbosity: u8) {
    // Determine log level from verbosity
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    // Build the stdout filter, allowing RUST_LOG env var to override
    let stdout_filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new(level));

    // Check for file sink directory
    if let Ok(log_dir) = std::env::var("GW_LOG_FILE") {
        let file_appender = tracing_appender::rolling::daily(&log_dir, "gw.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        // Leak the guard so it lives for the program's lifetime.
        // This is intentional — the guard must outlive all logging calls.
        std::mem::forget(_guard);

        // File layer gets its own filter: default to DEBUG for richer post-run analysis
        let file_filter = EnvFilter::try_from_env("GW_FILE_LOG")
            .unwrap_or_else(|_| EnvFilter::new("debug"));

        // Stdout: human-readable, no timestamps, no target noise
        let stdout_layer = fmt::layer()
            .with_target(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_line_number(false)
            .without_time()
            .with_filter(stdout_filter);

        // File: structured with full metadata, no ANSI colors
        let file_layer = fmt::layer()
            .with_writer(non_blocking)
            .with_target(true)
            .with_thread_ids(true)
            .with_ansi(false)
            .with_filter(file_filter);

        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(file_layer)
            .init();
    } else {
        // Stdout-only mode: human-readable, no timestamps
        fmt()
            .with_env_filter(stdout_filter)
            .with_target(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_line_number(false)
            .without_time()
            .init();
    }
}
