//! Unix socket server for daemon IPC.
//!
//! Listens on a Unix domain socket, spawning a tokio task per connection.
//! Each connection uses length-prefixed JSON framing (see `protocol.rs`).

use color_eyre::{eyre::WrapErr, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

use crate::daemon::protocol::{
    read_message, write_message, ErrorCode, Request, Response, RunStatus,
};
use crate::daemon::registry::RunRegistry;

/// Per-request timeout: 30 seconds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ── Server ──────────────────────────────────────────────────────────

/// Daemon socket server.
pub struct DaemonServer {
    registry: Arc<Mutex<RunRegistry>>,
    socket_path: PathBuf,
    cancel: CancellationToken,
}

impl DaemonServer {
    /// Create a new server instance.
    pub fn new(
        registry: Arc<Mutex<RunRegistry>>,
        socket_path: PathBuf,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            registry,
            socket_path,
            cancel,
        }
    }

    /// Start listening for connections. Runs until cancellation.
    pub async fn start(&self) -> Result<()> {
        // Remove stale socket file if no daemon is running
        Self::cleanup_stale_socket(&self.socket_path)?;

        let listener = UnixListener::bind(&self.socket_path)
            .wrap_err_with(|| format!("failed to bind socket: {}", self.socket_path.display()))?;

        tracing::info!(path = %self.socket_path.display(), "daemon listening");

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!("server shutting down");
                    break;
                }
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let registry = Arc::clone(&self.registry);
                            let cancel = self.cancel.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, registry, cancel).await {
                                    tracing::warn!(error = %e, "connection handler error");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to accept connection");
                        }
                    }
                }
            }
        }

        // Clean up socket file on shutdown
        self.cleanup_socket();
        Ok(())
    }

    /// Remove stale socket file if it exists and no daemon owns it.
    fn cleanup_stale_socket(socket_path: &Path) -> Result<()> {
        if socket_path.exists() {
            // Check if a daemon PID file exists and process is alive
            let pid_path = socket_path.with_file_name("daemon.pid");
            let is_stale = if pid_path.exists() {
                match std::fs::read_to_string(&pid_path) {
                    Ok(content) => match content.trim().parse::<u32>() {
                        Ok(pid) => !crate::cleanup::process::is_pid_alive(pid),
                        Err(_) => true,
                    },
                    Err(_) => true,
                }
            } else {
                true // No PID file means stale
            };

            if is_stale {
                tracing::info!(path = %socket_path.display(), "removing stale socket");
                std::fs::remove_file(socket_path)
                    .wrap_err("failed to remove stale socket file")?;
            } else {
                return Err(color_eyre::eyre::eyre!(
                    "another daemon is already running (socket exists with live PID)"
                ));
            }
        }
        Ok(())
    }

    /// Best-effort socket cleanup on shutdown.
    fn cleanup_socket(&self) {
        if let Err(e) = std::fs::remove_file(&self.socket_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, "failed to remove socket file");
            }
        }
    }
}

// ── Connection handler ──────────────────────────────────────────────

async fn handle_connection(
    stream: tokio::net::UnixStream,
    registry: Arc<Mutex<RunRegistry>>,
    cancel: CancellationToken,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    loop {
        // Read request with timeout, also check for cancellation
        let request: Request = tokio::select! {
            _ = cancel.cancelled() => break,
            result = timeout(REQUEST_TIMEOUT, read_message(&mut reader)) => {
                match result {
                    Ok(Ok(req)) => req,
                    Ok(Err(e)) => {
                        // EOF or parse error — client disconnected
                        if e.to_string().contains("early eof")
                            || e.to_string().contains("unexpected eof")
                        {
                            break;
                        }
                        tracing::debug!(error = %e, "failed to read request");
                        break;
                    }
                    Err(_) => {
                        tracing::debug!("request timeout");
                        let resp = Response::Error {
                            code: ErrorCode::InternalError,
                            message: "request timeout".into(),
                        };
                        let _ = write_message(&mut writer, &resp).await;
                        break;
                    }
                }
            }
        };

        let response = dispatch_request(request, &registry, &cancel).await;

        if let Err(e) = write_message(&mut writer, &response).await {
            tracing::debug!(error = %e, "failed to write response");
            break;
        }
    }

    Ok(())
}

/// Dispatch a request to the appropriate handler.
async fn dispatch_request(
    request: Request,
    registry: &Arc<Mutex<RunRegistry>>,
    cancel: &CancellationToken,
) -> Response {
    match request {
        Request::SubmitRun {
            plan_path,
            repo_dir,
            session_name,
        } => {
            let mut reg = registry.lock().await;
            match reg.register_run(plan_path, repo_dir, session_name) {
                Ok(run_id) => Response::RunSubmitted { run_id },
                Err(e) => {
                    let message = e.to_string();
                    let code = if message.contains("locked") {
                        ErrorCode::RepoLocked
                    } else {
                        ErrorCode::InternalError
                    };
                    Response::Error { code, message }
                }
            }
        }

        Request::ListRuns { all } => {
            let reg = registry.lock().await;
            let runs = reg.list_runs(all);
            Response::RunList { runs }
        }

        Request::GetLogs { run_id, .. } => {
            let reg = registry.lock().await;
            match reg.find_by_prefix(&run_id) {
                Some(entry) => {
                    // Read log file for this run
                    let log_path = crate::daemon::state::gw_home()
                        .join("runs")
                        .join(&entry.run_id)
                        .join("output.log");
                    let data = std::fs::read_to_string(&log_path).unwrap_or_default();
                    Response::LogChunk {
                        run_id: entry.run_id.clone(),
                        data,
                    }
                }
                None => Response::Error {
                    code: ErrorCode::RunNotFound,
                    message: format!("no run matching prefix: {run_id}"),
                },
            }
        }

        Request::StopRun { run_id } => {
            let mut reg = registry.lock().await;
            match reg.find_by_prefix(&run_id) {
                Some(entry) => {
                    let actual_id = entry.run_id.clone();
                    match reg.update_status(&actual_id, RunStatus::Stopped, None, None) {
                        Ok(()) => Response::Ok {
                            message: format!("run {actual_id} stopped"),
                        },
                        Err(e) => Response::Error {
                            code: ErrorCode::InternalError,
                            message: e.to_string(),
                        },
                    }
                }
                None => Response::Error {
                    code: ErrorCode::RunNotFound,
                    message: format!("no run matching prefix: {run_id}"),
                },
            }
        }

        Request::DaemonStatus => {
            let reg = registry.lock().await;
            let active = reg.list_runs(false).len();
            let total = reg.list_runs(true).len();
            Response::Ok {
                message: format!(
                    "daemon running — {active} active run(s), {total} total, pid {}",
                    std::process::id()
                ),
            }
        }

        Request::Shutdown => {
            tracing::info!("shutdown requested via socket");
            cancel.cancel();
            Response::Ok {
                message: "shutting down".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{write_message, read_message};
    use tempfile::TempDir;

    fn with_temp_gw_home(f: impl FnOnce(&TempDir)) {
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        crate::daemon::state::ensure_gw_home().unwrap();
        f(&tmp);
        unsafe { std::env::remove_var("GW_HOME") };
    }

    #[tokio::test]
    async fn server_accepts_and_responds() {
        with_temp_gw_home(|tmp| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let socket_path = tmp.path().join("test.sock");
                let registry = Arc::new(Mutex::new(RunRegistry::new()));
                let cancel = CancellationToken::new();

                let server = DaemonServer::new(
                    Arc::clone(&registry),
                    socket_path.clone(),
                    cancel.clone(),
                );

                // Spawn server in background
                let server_handle = tokio::spawn(async move {
                    let _ = server.start().await;
                });

                // Give server time to bind
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Connect and send DaemonStatus
                let stream = tokio::net::UnixStream::connect(&socket_path)
                    .await
                    .unwrap();
                let (mut reader, mut writer) = stream.into_split();

                let req = Request::DaemonStatus;
                write_message(&mut writer, &req).await.unwrap();

                let resp: Response = read_message(&mut reader).await.unwrap();
                match resp {
                    Response::Ok { message } => {
                        assert!(message.contains("daemon running"));
                    }
                    other => panic!("unexpected response: {other:?}"),
                }

                // Shutdown
                cancel.cancel();
                let _ = server_handle.await;
            });
        });
    }
}
