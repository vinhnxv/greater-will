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
//! - `-v`: INFO level (monitoring status, phase transitions)
//! - `-vv`: DEBUG level (decision logs, kill reasons, evidence)
//! - `-vvv` or more: TRACE level (every poll cycle, hash changes)
//!
//! # File Logging
//!
//! **Auto-enabled with `-vv` or higher**: debug logs are automatically written
//! to `.gw/logs/gw.log` with timestamps. No env var needed.
//!
//! **Manual override**: Set `GW_LOG_FILE` to a directory path to control
//! where file logs are written (always at DEBUG level):
//! ```bash
//! GW_LOG_FILE=/tmp/gw-logs gw run plans/*.md
//! ```

use tracing_subscriber::{fmt, EnvFilter, prelude::*};

/// Default log directory (relative to working dir).
const DEFAULT_LOG_DIR: &str = ".gw/logs";

/// Initialize the tracing subscriber with stdout output and optional file sink.
///
/// # Dual-layer design
///
/// The stdout layer uses a human-friendly format without timestamps —
/// timestamps add noise when you're watching output live. The file
/// layer includes full structured metadata (target, thread IDs) and
/// disables ANSI escapes so log files are grep-friendly.
///
/// # Auto file logging
///
/// When verbosity >= 2 (`-vv`), file logging is automatically enabled
/// to `.gw/logs/gw.log`. This ensures debug logs survive tmux
/// session crashes and can be reviewed post-mortem.
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

    // Resolve file logging directory:
    // 1. GW_LOG_FILE env var (explicit override)
    // 2. Auto-enable with -vv or higher -> .gw/logs/
    // 3. None -> stdout-only
    let log_dir = std::env::var("GW_LOG_FILE").ok().or_else(|| {
        if verbosity >= 2 {
            Some(DEFAULT_LOG_DIR.to_string())
        } else {
            None
        }
    });

    if let Some(log_dir) = log_dir {
        // Ensure log directory exists
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!("[gw] Warning: failed to create log dir '{}': {}", log_dir, e);
            // Fall through to stdout-only
            init_stdout_only(stdout_filter);
            return;
        }

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

        // Log that file logging is active (visible at INFO+ on stdout)
        tracing::info!(log_dir = %log_dir, verbosity = verbosity, "File logging enabled");
    } else {
        init_stdout_only(stdout_filter);
    }
}

/// Initialize stdout-only logging (no file sink).
fn init_stdout_only(filter: EnvFilter) {
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_line_number(false)
        .without_time()
        .init();
}
