//! Unix socket server for daemon IPC.
//!
//! Listens on a Unix domain socket, spawning a tokio task per connection.
//! Each connection uses length-prefixed JSON framing (see `protocol.rs`).

use color_eyre::{eyre::WrapErr, Result};
use std::os::unix::fs::PermissionsExt;
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

/// Poll interval for `--follow` mode (300ms).
const FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(300);

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

        // SEC-003: Restrict socket to owner-only (rw-------). Unix domain
        // sockets ignore `umask` on many platforms, so without this any local
        // user could connect and submit/cancel runs as the daemon owner.
        std::fs::set_permissions(
            &self.socket_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .wrap_err_with(|| {
            format!(
                "failed to set 0600 permissions on socket: {}",
                self.socket_path.display()
            )
        })?;

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
    ///
    /// SEC-006: There is a theoretical TOCTOU window between reading
    /// `daemon.pid`, deciding the socket is stale, and removing it. That
    /// window is closed by the `daemon.pid` flock acquired in
    /// `daemon/mod.rs` (see SEC-001): the flock guarantees that only one
    /// daemon process can reach this codepath for a given `socket_path`
    /// at a time, so no other daemon can race us to recreate the socket.
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

        // Follow mode: intercept GetLogs with follow=true and enter a
        // streaming loop instead of the normal single-response dispatch.
        if let Request::GetLogs {
            ref run_id,
            follow: true,
            tail,
            pane,
        } = request
        {
            tracing::info!(run_id = %run_id, "entering follow mode");
            match handle_follow_logs(
                &mut writer,
                &registry,
                &cancel,
                run_id,
                tail,
                pane,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "follow mode ended");
                }
            }
            // Follow mode consumes the connection — no further requests.
            break;
        }

        let request_type = request_type_name(&request);
        let start = std::time::Instant::now();
        let response = dispatch_request(request, &registry, &cancel).await;
        let elapsed_ms = start.elapsed().as_millis();

        tracing::info!(
            request_type = %request_type,
            elapsed_ms = elapsed_ms,
            "request handled"
        );

        if let Err(e) = write_message(&mut writer, &response).await {
            tracing::debug!(error = %e, "failed to write response");
            break;
        }
    }

    Ok(())
}

// ── Follow-mode handler ───────────────────────────────────────────

/// Stream log output for a run, polling for new bytes at a fixed interval.
///
/// Tracks a byte offset into the log file and on each tick reads only the
/// newly-appended bytes. Incomplete trailing lines (no terminating newline)
/// are buffered until the next tick so the client never sees a partial line.
///
/// Handles log rotation (file truncated / replaced) by resetting the offset
/// to zero when the file shrinks.
async fn handle_follow_logs<W>(
    writer: &mut W,
    registry: &Arc<Mutex<RunRegistry>>,
    cancel: &CancellationToken,
    run_id: &str,
    tail: Option<usize>,
    pane: bool,
) -> Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    use std::io::{Read, Seek, SeekFrom};

    // Resolve the run_id prefix once and determine the log path.
    let (actual_id, log_path) = {
        let reg = registry.lock().await;
        match reg.find_by_prefix(run_id) {
            Some(entry) => {
                let log_dir = crate::daemon::state::gw_home()
                    .join("runs")
                    .join(&entry.run_id)
                    .join("logs");
                let path = if pane {
                    log_dir.join("pane.log")
                } else {
                    log_dir.join("events.jsonl")
                };
                (entry.run_id.clone(), path)
            }
            None => {
                let resp = Response::Error {
                    code: ErrorCode::RunNotFound,
                    message: format!("no run matching prefix: {run_id}"),
                };
                write_message(writer, &resp).await?;
                return Ok(());
            }
        }
    };

    // Send the initial tail chunk so the client sees recent history first.
    let initial_data = read_log_tail(&log_path, tail);
    if !initial_data.is_empty() {
        let resp = Response::LogChunk {
            run_id: actual_id.clone(),
            data: initial_data,
        };
        write_message(writer, &resp).await?;
    }

    // Initialise the byte offset to the current end of file.
    let mut last_offset: u64 = std::fs::metadata(&log_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Buffer for bytes that don't end with a newline (incomplete line).
    let mut pending = Vec::new();

    let mut interval = tokio::time::interval(FOLLOW_POLL_INTERVAL);
    // The first tick fires immediately; skip it so we don't duplicate the
    // initial tail chunk we just sent.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                // Open the file each tick — handles rotation (new inode).
                let mut file = match std::fs::File::open(&log_path) {
                    Ok(f) => f,
                    Err(_) => continue, // file doesn't exist (yet)
                };

                let file_len = match file.metadata() {
                    Ok(m) => m.len(),
                    Err(_) => continue,
                };

                // Rotation detection: file shrank → reset.
                if file_len < last_offset {
                    tracing::debug!(
                        old_offset = last_offset,
                        new_len = file_len,
                        "log rotation detected, resetting offset"
                    );
                    last_offset = 0;
                    pending.clear();
                }

                // No new data since last poll.
                if file_len == last_offset {
                    continue;
                }

                // Seek to where we left off and read new bytes.
                if file.seek(SeekFrom::Start(last_offset)).is_err() {
                    continue;
                }

                let bytes_available = file_len - last_offset;
                let mut buf = Vec::with_capacity(bytes_available as usize);
                if file.take(bytes_available).read_to_end(&mut buf).is_err() {
                    continue;
                }

                last_offset = file_len;

                // Prepend any buffered incomplete line from the previous tick.
                if !pending.is_empty() {
                    let mut merged = std::mem::take(&mut pending);
                    merged.extend_from_slice(&buf);
                    buf = merged;
                }

                // Split off an incomplete trailing line (no terminating \n).
                if let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') {
                    // Everything after the last newline is incomplete.
                    if last_nl + 1 < buf.len() {
                        pending = buf[last_nl + 1..].to_vec();
                        buf.truncate(last_nl + 1);
                    }
                } else {
                    // No newline at all — buffer everything for next tick.
                    pending = buf;
                    continue;
                }

                if buf.is_empty() {
                    continue;
                }

                let data = String::from_utf8_lossy(&buf).into_owned();
                let resp = Response::LogChunk {
                    run_id: actual_id.clone(),
                    data,
                };
                // Client disconnect will surface as a write error.
                if let Err(e) = write_message(writer, &resp).await {
                    tracing::debug!(error = %e, "follow: client disconnected");
                    return Ok(());
                }
            }
        }
    }

    // Send a final message so the client knows the stream ended cleanly.
    let _ = write_message(
        writer,
        &Response::Ok {
            message: "follow ended".into(),
        },
    )
    .await;

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
            // SEC-008: Validate repo_dir first, then ensure the plan_path
            // resolves *inside* the canonicalized repo root. Without this a
            // client could direct the daemon to spawn `/etc/shadow` or any
            // other file outside the project.
            let canonical_repo = match validate_repo_dir(&repo_dir) {
                Ok(p) => p,
                Err(e) => {
                    return Response::Error {
                        code: ErrorCode::InvalidRequest,
                        message: format!("invalid repo_dir: {e}"),
                    };
                }
            };
            let canonical_plan = match validate_plan_path(&plan_path, &canonical_repo) {
                Ok(p) => p,
                Err(e) => {
                    return Response::Error {
                        code: ErrorCode::InvalidRequest,
                        message: format!("invalid plan_path: {e}"),
                    };
                }
            };

            // BACK-003: Delegate to `executor::spawn_run`, which registers
            // the run AND spawns the tmux session. Previously the handler
            // only called `reg.register_run()` and never started tmux, so
            // every submitted run sat in the Queued state forever.
            //
            // Note: `executor::spawn_run` internally calls `register_run`,
            // so we MUST NOT call it ourselves beforehand — the per-repo
            // flock would reject the nested registration. The call is
            // awaited inline (not wrapped in `tokio::spawn`) because the
            // registry lock is only held briefly inside the executor, the
            // tmux work is sub-second, and we need the returned run_id for
            // the synchronous response.
            match crate::daemon::executor::spawn_run(
                Arc::clone(registry),
                &canonical_plan,
                &canonical_repo,
                session_name,
            )
            .await
            {
                Ok(run_id) => Response::RunSubmitted { run_id },
                Err(e) => {
                    // QUAL-005: classify errors into typed ErrorCode
                    // variants instead of a naive `contains("locked")`.
                    let code = classify_spawn_error(&e);
                    Response::Error {
                        code,
                        message: e.to_string(),
                    }
                }
            }
        }

        Request::ListRuns { all } => {
            let reg = registry.lock().await;
            let runs = reg.list_runs(all);
            Response::RunList { runs }
        }

        Request::GetLogs {
            run_id,
            follow: _,
            tail,
            pane,
        } => {
            let reg = registry.lock().await;
            match reg.find_by_prefix(&run_id) {
                Some(entry) => {
                    let log_dir = crate::daemon::state::gw_home()
                        .join("runs")
                        .join(&entry.run_id)
                        .join("logs");
                    // Default: structured event log (events.jsonl)
                    // --pane: raw tmux pane capture (pane.log)
                    let log_path = if pane {
                        log_dir.join("pane.log")
                    } else {
                        log_dir.join("events.jsonl")
                    };
                    // SEC-004: Read only the last `tail` bytes (or a 1 MB
                    // cap when unset) to prevent OOM on large log files.
                    let data = read_log_tail(&log_path, tail);
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
            // Resolve the prefix to a full run_id under a short-lived lock,
            // then release the lock before delegating to the executor (which
            // re-acquires it briefly at the end).
            let actual_id = {
                let reg = registry.lock().await;
                match reg.find_by_prefix(&run_id) {
                    Some(entry) => entry.run_id.clone(),
                    None => {
                        return Response::Error {
                            code: ErrorCode::RunNotFound,
                            message: format!("no run matching prefix: {run_id}"),
                        };
                    }
                }
            };

            // SEC-002: Delegate to `executor::stop_run`, which sends /exit
            // to the tmux session, waits for graceful shutdown, force-kills
            // on timeout, and then updates the registry. The previous code
            // only mutated the registry — it left the tmux session (and
            // its arc worker) running, leaking resources and making the
            // "stopped" state a lie.
            match crate::daemon::executor::stop_run(Arc::clone(registry), &actual_id).await {
                Ok(()) => Response::Ok {
                    message: format!("run {actual_id} stopped"),
                },
                Err(e) => Response::Error {
                    code: ErrorCode::InternalError,
                    message: e.to_string(),
                },
            }
        }

        Request::DetachRun { run_id } => {
            // Detach: stop tracking but keep the tmux session alive.
            // The session can be re-adopted on the next daemon restart.

            // Step 1: Resolve prefix and snapshot pane under one lock scope
            let resolved = {
                let reg = registry.lock().await;
                reg.find_by_prefix(&run_id).map(|entry| {
                    (entry.run_id.clone(), entry.tmux_session.clone())
                })
            };

            let (actual_id, tmux) = match resolved {
                Some(pair) => pair,
                None => {
                    return Response::Error {
                        code: ErrorCode::RunNotFound,
                        message: format!("no run matching prefix: {run_id}"),
                    };
                }
            };

            // Step 2: Drain snapshot (re-acquires lock internally)
            crate::daemon::drain::drain_running_sessions(Arc::clone(registry)).await;

            // Step 3: Mark as Stopped but do NOT kill tmux
            {
                let mut reg = registry.lock().await;
                if let Err(e) = reg.update_status(
                    &actual_id,
                    RunStatus::Stopped,
                    Some("detached".to_string()),
                    Some("detached by user — tmux session kept alive".to_string()),
                ) {
                    return Response::Error {
                        code: ErrorCode::InternalError,
                        message: e.to_string(),
                    };
                }
            }

            tracing::info!(
                run_id = %actual_id,
                tmux = ?tmux,
                "run detached — tmux session preserved"
            );

            Response::Ok {
                message: format!(
                    "run {actual_id} detached (tmux session kept alive)"
                ),
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

/// Extract a short type label from a request for structured logging.
fn request_type_name(req: &Request) -> &'static str {
    match req {
        Request::SubmitRun { .. } => "SubmitRun",
        Request::ListRuns { .. } => "ListRuns",
        Request::GetLogs { .. } => "GetLogs",
        Request::StopRun { .. } => "StopRun",
        Request::DetachRun { .. } => "DetachRun",
        Request::DaemonStatus => "DaemonStatus",
        Request::Shutdown => "Shutdown",
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Upper bound on log bytes returned when a client does not specify `tail`.
/// Chosen at 1 MB — large enough to be useful for most cases, small enough
/// that we will not exhaust daemon memory on a multi-GB rogue log.
const DEFAULT_LOG_TAIL_BYTES: u64 = 1024 * 1024;

/// Read the trailing bytes of a log file without loading the whole file.
///
/// If `tail` is `Some(n)` with `n > 0`, reads the last `n` bytes; otherwise
/// defaults to [`DEFAULT_LOG_TAIL_BYTES`]. Missing / unreadable files yield
/// an empty string so the wire protocol stays on the happy path — callers
/// can distinguish "no logs yet" from "run missing" via the `RunNotFound`
/// branch above.
///
/// SEC-004: Bounds the read so an attacker (or a runaway process) cannot
/// force the daemon to allocate arbitrary memory for a single IPC call.
fn read_log_tail(path: &Path, tail: Option<usize>) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return String::new(),
    };

    let want_bytes: u64 = match tail {
        Some(n) if n > 0 => n as u64,
        _ => DEFAULT_LOG_TAIL_BYTES,
    };

    let start = file_len.saturating_sub(want_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }

    let capacity = want_bytes.min(file_len) as usize;
    let mut buf = Vec::with_capacity(capacity);
    if file.take(want_bytes).read_to_end(&mut buf).is_err() {
        return String::new();
    }

    String::from_utf8_lossy(&buf).into_owned()
}

/// Validate a repo directory path supplied by an IPC client.
///
/// SEC-008: Rejects any `..` components in the *raw* input (defense-in-depth,
/// even if canonicalization would resolve them) and then canonicalizes,
/// which implicitly enforces that the directory exists and is accessible.
fn validate_repo_dir(repo_dir: &Path) -> Result<PathBuf> {
    for component in repo_dir.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(color_eyre::eyre::eyre!(
                "repo_dir contains '..' component"
            ));
        }
    }
    repo_dir
        .canonicalize()
        .wrap_err_with(|| format!("repo_dir does not exist: {}", repo_dir.display()))
}

/// Validate a plan file path and enforce that it resolves inside `allowed_root`.
///
/// SEC-008: The `..`-component rejection is defense-in-depth on top of the
/// canonical-path prefix check, which is the real security boundary. A
/// plan_path that canonicalizes outside the repo root (e.g., via a symlink)
/// will be rejected here.
fn validate_plan_path(plan_path: &Path, allowed_root: &Path) -> Result<PathBuf> {
    for component in plan_path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(color_eyre::eyre::eyre!(
                "plan_path contains '..' component"
            ));
        }
    }

    let canonical = plan_path
        .canonicalize()
        .wrap_err_with(|| format!("plan_path does not exist: {}", plan_path.display()))?;

    if !canonical.starts_with(allowed_root) {
        return Err(color_eyre::eyre::eyre!(
            "plan_path {} is outside allowed root {}",
            canonical.display(),
            allowed_root.display()
        ));
    }

    Ok(canonical)
}

/// Classify an error returned from `executor::spawn_run` (or its inner
/// `register_run` call) into a wire `ErrorCode`.
///
/// QUAL-005: Improves over the previous naive `message.contains("locked")`
/// by recognising both known lock-error phrasings and the executor's
/// pre-flight messages. A fully typed error hierarchy would require
/// refactoring `RunRegistry::register_run`, which is out of scope for this
/// file's fix group.
fn classify_spawn_error(e: &color_eyre::Report) -> ErrorCode {
    let msg = e.to_string().to_lowercase();
    if msg.contains("locked")
        || msg.contains("already has an active run")
    {
        ErrorCode::RepoLocked
    } else if msg.contains("plan file not found")
        || msg.contains("repository directory not found")
    {
        ErrorCode::InvalidRequest
    } else {
        ErrorCode::InternalError
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{write_message, read_message};
    use tempfile::TempDir;

    #[tokio::test]
    async fn server_accepts_and_responds() {
        // Set up GW_HOME under the serializing mutex (sync section)
        let tmp = {
            use std::sync::{Mutex as StdMutex, OnceLock};
            static MUTEX: OnceLock<StdMutex<()>> = OnceLock::new();
            let _guard = MUTEX
                .get_or_init(|| StdMutex::new(()))
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let tmp = TempDir::new().unwrap();
            unsafe { std::env::set_var("GW_HOME", tmp.path()) };
            crate::daemon::state::ensure_gw_home().unwrap();
            tmp
            // _guard drops here — OK because we only needed GW_HOME set for ensure_gw_home
        };

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

        // Clean up GW_HOME
        unsafe { std::env::remove_var("GW_HOME") };
    }
}
