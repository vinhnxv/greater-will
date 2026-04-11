pub mod drain;
pub mod executor;
pub mod heartbeat;
pub mod protocol;
pub mod reconciler;
pub mod registry;
pub mod server;
pub mod state;

use color_eyre::{eyre::WrapErr, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;

use crate::daemon::heartbeat::HeartbeatMonitor;
use crate::daemon::registry::RunRegistry;
use crate::daemon::server::DaemonServer;
use crate::daemon::state::{ensure_gw_home, GlobalConfig};
use crate::engine::crash_loop::{CrashLoopDecision, CrashLoopDetector};

// ── Crash loop flag ─────────────────────────────────────────────────

/// Sentinel file written at `~/.gw/crashloop.flag` when the daemon
/// detects a crash loop (too many unclean exits in a rolling window).
///
/// Presence of this file means `start_daemon` has refused to continue
/// so launchd's `KeepAlive` stops respawning the process.  The file is
/// also surfaced by `gw daemon status` so operators can see *why* the
/// daemon is not running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashLoopFlag {
    /// Unix epoch seconds when the flag was written.
    pub timestamp: u64,
    /// Number of crashes counted within the detector's rolling window.
    pub crash_count: u32,
    /// Optional last error, if one was captured by a previous process.
    pub last_error: Option<String>,
    /// Human-readable summary of the halt reason.
    pub message: String,
}

/// Return the standard path to the crash loop sentinel flag.
pub fn crashloop_flag_path(home: &Path) -> PathBuf {
    home.join("crashloop.flag")
}

/// Return the path to the "daemon running" marker file.
///
/// This file is written after the daemon finishes startup and removed
/// on clean shutdown.  If it is still present when a new daemon starts,
/// the previous instance exited uncleanly — which is how we detect
/// crashes across process boundaries.
fn running_marker_path(home: &Path) -> PathBuf {
    home.join("daemon.running")
}

/// Read and parse the crash loop flag at `~/.gw/crashloop.flag`, if present.
///
/// Returns `None` when the file is missing or unparseable.  Callers
/// generally treat a missing/malformed file as "no crash loop".
pub fn read_crashloop_flag(home: &Path) -> Option<CrashLoopFlag> {
    let path = crashloop_flag_path(home);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice::<CrashLoopFlag>(&bytes).ok()
}

/// Write the crash loop sentinel flag with a summary message.
///
/// Called only from the crash-detection path in `start_daemon` after the
/// `CrashLoopDetector` returns `StopCrashLoop`.
fn write_crashloop_flag(path: &Path, crash_count: u32) -> Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let flag = CrashLoopFlag {
        timestamp,
        crash_count,
        last_error: None,
        message: format!(
            "Crash loop detected: {crash_count} crashes within rolling window. \
             launchd respawn halted. Inspect daemon.log and delete \
             {} after fixing the root cause.",
            path.display(),
        ),
    };
    let json =
        serde_json::to_string_pretty(&flag).wrap_err("failed to serialize crash loop flag")?;

    // Atomic write: stage into a sibling tempfile, fsync, then POSIX
    // rename into place.  `rename(2)` within a single filesystem is
    // atomic, so `read_crashloop_flag` can never observe a torn write
    // — it sees either the old contents or the new, never partial JSON.
    // `std::fs::write` (the previous implementation) truncates-then-
    // writes, so an interrupted write (power loss, disk full, OOM kill)
    // would leave a half-written file that parses as None, silently
    // suppressing the crash-loop banner.
    let tmp_path = path.with_extension("flag.tmp");
    {
        let mut f = std::fs::File::create(&tmp_path).wrap_err_with(|| {
            format!(
                "failed to create crash loop flag tempfile at {}",
                tmp_path.display()
            )
        })?;
        f.write_all(json.as_bytes()).wrap_err_with(|| {
            format!(
                "failed to write crash loop flag tempfile at {}",
                tmp_path.display()
            )
        })?;
        f.sync_all().wrap_err_with(|| {
            format!(
                "failed to fsync crash loop flag tempfile at {}",
                tmp_path.display()
            )
        })?;
    }
    std::fs::rename(&tmp_path, path).wrap_err_with(|| {
        format!(
            "failed to atomically rename crash loop flag into {}",
            path.display()
        )
    })?;
    Ok(())
}

// ── Daemon lifecycle ────────────────────────────────────────────────

/// Start the daemon process.
///
/// 1. Create ~/.gw/ directory hierarchy
/// 2. Check for existing daemon (PID file + alive check)
/// 3. Write PID file
/// 4. Init tracing to daemon.log (with daily rotation)
/// 5. Init RunRegistry (load from disk)
/// 6. Crash recovery reconciler (sync registry vs tmux reality)
/// 7. Start socket server + heartbeat monitor
/// 8. Wait for shutdown signal (SIGTERM/SIGINT)
/// 9. Graceful shutdown
pub async fn start_daemon(verbosity: u8) -> Result<()> {
    // 1. Ensure directory structure
    let home = ensure_gw_home()?;

    // 1a. Crash-loop detection.
    //
    // The daemon itself is a single process — by definition a crash ends
    // the process, so `record_restart` must be called *across* process
    // boundaries.  We detect an unclean previous exit by checking for the
    // "daemon.running" marker file (written after startup, removed on
    // clean shutdown).  CrashLoopDetector.load_history() restores the
    // rolling crash window from `~/.gw/crash-history.json`.
    //
    // On StopCrashLoop we write `~/.gw/crashloop.flag` and return an
    // error so launchd sees a non-zero exit and backs off further
    // respawns.
    let flag_path = crashloop_flag_path(&home);
    let running_marker = running_marker_path(&home);
    {
        let mut detector = CrashLoopDetector::new(5, 30, 300);
        detector.load_history(&home);

        if running_marker.exists() {
            match detector.record_restart() {
                CrashLoopDecision::StopCrashLoop => {
                    let crashes = detector.crashes_in_window() as u32;
                    write_crashloop_flag(&flag_path, crashes)?;
                    let _ = detector.persist(&home);
                    return Err(color_eyre::eyre::eyre!(
                        "crash loop detected: {} crashes within 30s window — \
                         wrote {} and halting launchd respawn",
                        crashes,
                        flag_path.display()
                    ));
                }
                CrashLoopDecision::AllowRestart => {
                    // Under threshold — persist updated history and continue.
                    let _ = detector.persist(&home);
                    warn!(
                        crashes_in_window = detector.crashes_in_window(),
                        "previous daemon exit was unclean (running marker present) — counted as crash"
                    );
                }
            }
        }
    }
    // Clean start path (or allowed restart): remove any stale crash-loop flag.
    if flag_path.exists() {
        if let Err(e) = std::fs::remove_file(&flag_path) {
            warn!(error = %e, path = %flag_path.display(), "failed to clear stale crashloop.flag");
        }
    }

    // 2. Open the PID file and acquire an exclusive OS-level lock.
    //    The lock is released automatically by the kernel when this process
    //    exits — clean shutdown or crash — so stale PID files cannot cause
    //    a lockout. `try_lock_exclusive` is non-blocking: failure means
    //    another daemon already holds the lock.
    let pid_path = home.join("daemon.pid");
    let mut pid_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&pid_path)
        .wrap_err("failed to open daemon PID file")?;

    if FileExt::try_lock_exclusive(&pid_file).is_err() {
        // Another daemon owns the lock — read its PID for a helpful error.
        let existing = std::fs::read_to_string(&pid_path).unwrap_or_default();
        return Err(color_eyre::eyre::eyre!(
            "daemon already running (PID {}). Stop it first with `gw daemon stop`.",
            existing.trim()
        ));
    }

    // 3. Lock acquired — write our PID atomically into the locked file.
    //    `pid_file` must remain in scope for the full daemon lifetime so
    //    the OS keeps the lock held.
    let our_pid = std::process::id();
    pid_file
        .set_len(0)
        .wrap_err("failed to truncate daemon PID file")?;
    pid_file
        .seek(SeekFrom::Start(0))
        .wrap_err("failed to seek daemon PID file")?;
    pid_file
        .write_all(our_pid.to_string().as_bytes())
        .wrap_err("failed to write daemon PID file")?;
    pid_file
        .flush()
        .wrap_err("failed to flush daemon PID file")?;
    pid_file
        .sync_all()
        .wrap_err("failed to fsync daemon PID file")?;

    // 3b. Write the "daemon.running" marker IMMEDIATELY after the PID
    //     flock is held, BEFORE tracing / registry / heartbeat / server
    //     initialization.  This closes the early-startup blind spot in
    //     crash-loop detection: any crash during the remaining startup
    //     steps (or later runtime) leaves the marker in place so the
    //     next process counts it via `CrashLoopDetector::record_restart`.
    //     Best-effort — failing to write the marker degrades crash-loop
    //     detection but must not prevent startup.  Tracing is not yet
    //     initialized, so `warn!` will no-op here; the error is still
    //     propagated to stderr via eprintln as a last-resort signal.
    if let Err(e) = std::fs::write(&running_marker, our_pid.to_string()) {
        eprintln!(
            "gw daemon: failed to write daemon.running marker at {}: {}",
            running_marker.display(),
            e
        );
        warn!(error = %e, path = %running_marker.display(), "failed to write daemon.running marker");
    }

    // 4. Init tracing to daemon.log with daily rotation.
    //    Verbosity controls the log level (matching `gw run -v/-vv/-vvv`):
    //    0 = info (daemon default), 1 = info, 2 = debug, 3+ = trace.
    //    RUST_LOG env var overrides when set.
    let level = match verbosity {
        0 | 1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(level));

    let log_dir = home.clone();
    let file_appender = rolling::daily(&log_dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .finish();
    // Only set if not already set (tests may have their own subscriber)
    let _ = tracing::subscriber::set_global_default(subscriber);

    // 5. Load config and registry from disk
    let config = GlobalConfig::load()?;

    info!(
        pid = our_pid,
        home = %home.display(),
        socket = %config.socket_path().display(),
        "daemon starting"
    );
    let registry = RunRegistry::load_from_disk()?;
    let registry = Arc::new(Mutex::new(registry));

    // 6. Crash recovery reconciler
    // NOTE: reconcile() calls `tmux list-sessions` via std::process::Command (blocking).
    // Acceptable here: runs once at startup before any concurrent server tasks exist.
    // For hot-path usage, would need spawn_blocking or async tmux interaction.
    {
        let mut reg_guard = registry.lock().await;
        let report = reconciler::reconcile(&mut reg_guard);
        tracing::debug!(?report, "reconciliation complete");
    } // drop lock before heartbeat/server start

    // 7. Start socket server
    let cancel = CancellationToken::new();
    let socket_path = config.socket_path();

    let server = DaemonServer::new(Arc::clone(&registry), socket_path, cancel.clone());

    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.start().await {
            error!(error = %e, "server exited with error");
        }
    });

    // 7b. Start heartbeat monitor (captures pane logs, tracks phases, detects crashes)
    let heartbeat = HeartbeatMonitor::new(Arc::clone(&registry));
    let heartbeat_handle = heartbeat.start();
    info!("heartbeat monitor spawned");

    // Note: the `daemon.running` marker was written earlier (step 3b)
    // immediately after the PID flock.  Do NOT write it here — that would
    // reintroduce the early-startup crash blind spot that CDX-GAP-001 fixed.

    // 8. Wait for shutdown signal
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("received shutdown signal");
        shutdown_cancel.cancel();
    });

    #[cfg(unix)]
    {
        let term_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to register SIGTERM handler");
            sigterm.recv().await;
            info!("received SIGTERM");
            term_cancel.cancel();
        });
    }

    // Wait for server to finish (it exits when cancel fires)
    let _ = server_handle.await;

    // 9. Graceful shutdown: stop heartbeat, drain running sessions, clean up.
    heartbeat_handle.abort(); // Stop heartbeat before draining to avoid interference
    info!("daemon shutting down — draining running sessions");
    let drained = drain::drain_running_sessions(Arc::clone(&registry)).await;
    info!(drained = drained, "drain complete");

    state::cleanup_old_runs(config.retention)?;

    // Clean shutdown — remove the running marker so the next start is
    // not mistakenly counted as a crash.  Best-effort.
    if let Err(e) = std::fs::remove_file(&running_marker) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(error = %e, path = %running_marker.display(), "failed to remove daemon.running marker");
        }
    }

    // Explicitly release the exclusive lock before unlinking the PID file.
    // (Drop would also release it, but being explicit documents the intent.)
    let _ = FileExt::unlock(&pid_file);
    drop(pid_file);

    if let Err(e) = std::fs::remove_file(&pid_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(error = %e, "failed to remove PID file");
        }
    }

    info!("daemon stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writing a well-formed crash loop flag should round-trip through
    /// `read_crashloop_flag` and return all fields intact.
    #[test]
    fn read_crashloop_flag_parses_written_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let flag_path = crashloop_flag_path(dir.path());

        // Write a flag via the internal writer and parse it back.
        write_crashloop_flag(&flag_path, 5).expect("write flag");

        let parsed = read_crashloop_flag(dir.path()).expect("flag should parse");
        assert_eq!(parsed.crash_count, 5);
        assert!(parsed.message.contains("Crash loop detected"));
        assert!(parsed.message.contains("5 crashes"));
        assert!(parsed.timestamp > 0);
    }

    /// When the flag file is missing, `read_crashloop_flag` must return
    /// `None` — callers rely on this to mean "daemon is not in a crash
    /// loop state".
    #[test]
    fn read_crashloop_flag_missing_file_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_crashloop_flag(dir.path()).is_none());
    }

    /// Malformed JSON must also return `None`, not panic or bubble up an
    /// error — the status command should degrade gracefully.
    #[test]
    fn read_crashloop_flag_malformed_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let flag_path = crashloop_flag_path(dir.path());
        std::fs::write(&flag_path, b"not json {{{").unwrap();
        assert!(read_crashloop_flag(dir.path()).is_none());
    }

    /// Regression test for CDX-GAP-002.  A pre-existing torn / partial
    /// write at the flag path (simulating a crash during a prior
    /// `write_crashloop_flag` via `std::fs::write`) must be fully
    /// replaced by a subsequent atomic write — not merged with or
    /// silently shadowed by the partial content.
    ///
    /// Before the fix, `std::fs::write` would truncate-then-write and a
    /// crash mid-write could leave the file unparseable (making
    /// `read_crashloop_flag` return None).  The tempfile+rename pattern
    /// guarantees the next read sees either the old contents or the
    /// fully-written new contents — never a torn state.
    #[test]
    fn write_crashloop_flag_replaces_partial_file_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let flag_path = crashloop_flag_path(dir.path());

        // Simulate a torn prior write: unparseable partial JSON at the target.
        std::fs::write(&flag_path, b"{\"timestamp\": 42, \"crash_coun").unwrap();
        // Sanity check: the partial must not parse.
        assert!(
            read_crashloop_flag(dir.path()).is_none(),
            "test precondition: partial file should not parse"
        );

        // Atomic write must fully replace the partial content.
        write_crashloop_flag(&flag_path, 7).expect("atomic write should succeed");

        let parsed = read_crashloop_flag(dir.path())
            .expect("flag should parse after atomic write replaces partial file");
        assert_eq!(parsed.crash_count, 7);
        assert!(parsed.message.contains("7 crashes"));
        assert!(parsed.timestamp > 0);

        // The sibling tempfile must not linger after a successful rename.
        let tmp_path = flag_path.with_extension("flag.tmp");
        assert!(
            !tmp_path.exists(),
            "tempfile {} should be gone after atomic rename",
            tmp_path.display()
        );
    }
}
