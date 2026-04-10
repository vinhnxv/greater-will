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
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_appender::rolling;

use crate::daemon::heartbeat::HeartbeatMonitor;
use crate::daemon::registry::RunRegistry;
use crate::daemon::server::DaemonServer;
use crate::daemon::state::{ensure_gw_home, GlobalConfig};

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
pub async fn start_daemon() -> Result<()> {
    // 1. Ensure directory structure
    let home = ensure_gw_home()?;

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

    // 4. Init tracing to daemon.log with daily rotation
    let log_dir = home.clone();
    let file_appender = rolling::daily(&log_dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(non_blocking)
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
    let _heartbeat_handle = heartbeat.start();
    info!("heartbeat monitor spawned");

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

    // 9. Graceful shutdown: drain running sessions, clean up old runs, release lock, remove PID file.
    info!("daemon shutting down — draining running sessions");
    let drained = drain::drain_running_sessions(Arc::clone(&registry)).await;
    info!(drained = drained, "drain complete");

    state::cleanup_old_runs(config.retention)?;

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

