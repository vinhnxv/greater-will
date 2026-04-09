pub mod protocol;
pub mod registry;
pub mod server;
pub mod state;

use color_eyre::{eyre::WrapErr, Result};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_appender::rolling;

use crate::daemon::registry::RunRegistry;
use crate::daemon::server::DaemonServer;
use crate::daemon::state::{gw_home, ensure_gw_home, GlobalConfig};

// ── Daemon lifecycle ────────────────────────────────────────────────

/// Start the daemon process.
///
/// 1. Create ~/.gw/ directory hierarchy
/// 2. Check for existing daemon (PID file + alive check)
/// 3. Write PID file
/// 4. Init tracing to daemon.log (with daily rotation)
/// 5. Init RunRegistry (load from disk)
/// 6. Start socket server
/// 7. Wait for shutdown signal (SIGTERM/SIGINT)
/// 8. Graceful shutdown
pub async fn start_daemon() -> Result<()> {
    // 1. Ensure directory structure
    let home = ensure_gw_home()?;

    // 2. Check for existing daemon
    let pid_path = home.join("daemon.pid");
    if pid_path.exists() {
        let content = std::fs::read_to_string(&pid_path).unwrap_or_default();
        if let Ok(pid) = content.trim().parse::<u32>() {
            if crate::cleanup::process::is_pid_alive(pid) {
                return Err(color_eyre::eyre::eyre!(
                    "daemon already running (PID {pid}). Stop it first with `gw daemon stop`."
                ));
            }
            tracing::info!(stale_pid = pid, "removing stale PID file");
        }
    }

    // 3. Write our PID file
    let our_pid = std::process::id();
    std::fs::write(&pid_path, our_pid.to_string())
        .wrap_err("failed to write daemon PID file")?;

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

    tracing::info!(pid = our_pid, home = %home.display(), "daemon starting");

    // 5. Load run registry from disk
    let config = GlobalConfig::load()?;
    let registry = RunRegistry::load_from_disk()?;
    let registry = Arc::new(Mutex::new(registry));

    // 6. Start socket server
    let cancel = CancellationToken::new();
    let socket_path = config.socket_path();

    let server = DaemonServer::new(Arc::clone(&registry), socket_path, cancel.clone());

    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.start().await {
            tracing::error!(error = %e, "server exited with error");
        }
    });

    // 7. Wait for shutdown signal
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received shutdown signal");
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
            tracing::info!("received SIGTERM");
            term_cancel.cancel();
        });
    }

    // Wait for server to finish (it exits when cancel fires)
    let _ = server_handle.await;

    // 8. Graceful shutdown: clean up old runs, remove PID file
    tracing::info!("daemon shutting down");
    state::cleanup_old_runs(config.retention)?;

    if let Err(e) = std::fs::remove_file(&pid_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, "failed to remove PID file");
        }
    }

    tracing::info!("daemon stopped");
    Ok(())
}

/// Stop a running daemon by reading its PID and sending SIGTERM.
///
/// Falls back to SIGKILL after 5 seconds if the process doesn't exit.
pub fn stop_daemon() -> Result<()> {
    let pid_path = gw_home().join("daemon.pid");

    if !pid_path.exists() {
        return Err(color_eyre::eyre::eyre!("no daemon PID file found — is the daemon running?"));
    }

    let content =
        std::fs::read_to_string(&pid_path).wrap_err("failed to read daemon PID file")?;
    let pid: u32 = content
        .trim()
        .parse()
        .wrap_err("invalid PID in daemon.pid")?;

    if !crate::cleanup::process::is_pid_alive(pid) {
        // Stale PID file — clean up
        let _ = std::fs::remove_file(&pid_path);
        return Err(color_eyre::eyre::eyre!(
            "daemon PID {pid} is not running (cleaned up stale PID file)"
        ));
    }

    tracing::info!(pid = pid, "sending SIGTERM to daemon");
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    // Wait up to 5 seconds for graceful shutdown
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !crate::cleanup::process::is_pid_alive(pid) {
            tracing::info!(pid = pid, "daemon stopped");
            let _ = std::fs::remove_file(&pid_path);
            return Ok(());
        }
    }

    // Force kill
    tracing::warn!(pid = pid, "daemon did not stop gracefully, sending SIGKILL");
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    let _ = std::fs::remove_file(&pid_path);

    Ok(())
}

/// Check daemon status: PID, uptime, whether it's actually running.
pub fn daemon_status() -> Result<DaemonInfo> {
    let pid_path = gw_home().join("daemon.pid");

    if !pid_path.exists() {
        return Ok(DaemonInfo {
            running: false,
            pid: None,
            uptime_secs: None,
            socket_exists: false,
        });
    }

    let content = std::fs::read_to_string(&pid_path).unwrap_or_default();
    let pid: Option<u32> = content.trim().parse().ok();
    let running = pid.map(crate::cleanup::process::is_pid_alive).unwrap_or(false);

    let config = GlobalConfig::load().unwrap_or_default();
    let socket_exists = config.socket_path().exists();

    let uptime_secs = if running {
        // Estimate uptime from PID file modification time
        std::fs::metadata(&pid_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| {
                std::time::SystemTime::now()
                    .duration_since(modified)
                    .ok()
                    .map(|d| d.as_secs())
            })
    } else {
        None
    };

    Ok(DaemonInfo {
        running,
        pid,
        uptime_secs,
        socket_exists,
    })
}

/// Daemon status information.
#[derive(Debug)]
pub struct DaemonInfo {
    /// Whether the daemon process is alive.
    pub running: bool,
    /// PID from the PID file, if readable.
    pub pid: Option<u32>,
    /// Estimated uptime in seconds.
    pub uptime_secs: Option<u64>,
    /// Whether the socket file exists.
    pub socket_exists: bool,
}
