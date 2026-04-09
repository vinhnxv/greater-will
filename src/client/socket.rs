//! Unix socket client for communicating with the gw daemon.
//!
//! Provides a synchronous API that wraps async socket I/O using a
//! short-lived tokio runtime. Each request opens a new connection,
//! sends one message, reads one response, and closes.
//!
//! # Timeouts
//!
//! - Connect: 5 seconds
//! - Response: 30 seconds
//! - Probe (is_daemon_running): 1 second

use crate::daemon::protocol::{read_message, write_message, Request, Response};
use crate::daemon::state::{gw_home, GlobalConfig};
use color_eyre::{eyre::eyre, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tracing::{debug, warn};

/// Timeout for establishing a connection to the daemon.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for receiving a response after sending a request.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Quick timeout for connectivity probes (`is_daemon_running`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Client for sending requests to the gw daemon over a Unix socket.
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Create a new client using the configured socket path.
    pub fn new() -> Result<Self> {
        let config = GlobalConfig::load()?;
        Ok(Self {
            socket_path: config.socket_path(),
        })
    }

    /// Create a client pointing at a specific socket path.
    pub fn with_path(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Check whether the daemon socket exists and is connectable.
    ///
    /// Performs a quick connect + DaemonStatus ping with a 1-second timeout.
    /// Returns `false` if the socket doesn't exist, connection fails, or
    /// the daemon doesn't respond in time.
    pub fn is_daemon_running() -> bool {
        let config = match GlobalConfig::load() {
            Ok(c) => c,
            Err(_) => return false,
        };
        let path = config.socket_path();
        if !path.exists() {
            return false;
        }
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(_) => return false,
        };
        rt.block_on(async {
            let connect_result =
                tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(&path)).await;
            match connect_result {
                Ok(Ok(mut stream)) => {
                    // Send a status ping to verify it's a real daemon
                    let req = Request::DaemonStatus;
                    if write_message(&mut stream, &req).await.is_ok() {
                        let resp: std::result::Result<Response, _> =
                            read_message(&mut stream).await;
                        resp.is_ok()
                    } else {
                        false
                    }
                }
                _ => false,
            }
        })
    }

    /// Send a request and return the response.
    ///
    /// Applies a 5-second connect timeout and a 30-second response timeout.
    /// Returns helpful error messages when the daemon is unreachable.
    pub fn send(&self, request: Request) -> Result<Response> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(self.send_async(request))
    }

    /// Async implementation of send with timeouts.
    async fn send_async(&self, request: Request) -> Result<Response> {
        debug!(?request, path = %self.socket_path.display(), "sending request to daemon");

        if !self.socket_path.exists() {
            return Err(eyre!(
                "Daemon socket not found at {}.\n\
                 Is the daemon running? Start it with: gw daemon start",
                self.socket_path.display()
            ));
        }

        // Connect with timeout
        let mut stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            UnixStream::connect(&self.socket_path),
        )
        .await
        .map_err(|_| {
            eyre!(
                "Connection to daemon timed out after {}s.\n\
                 The daemon may be overloaded or hung. Check: gw daemon status",
                CONNECT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| {
            eyre!(
                "Cannot connect to daemon at {}: {e}\n\
                 Start the daemon with: gw daemon start",
                self.socket_path.display()
            )
        })?;

        write_message(&mut stream, &request).await?;

        // Read response with timeout
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, async {
            read_message::<_, Response>(&mut stream).await
        })
        .await
        .map_err(|_| {
            eyre!(
                "Daemon did not respond within {}s. It may be busy processing a run.",
                RESPONSE_TIMEOUT.as_secs()
            )
        })??;

        debug!(?response, "received response from daemon");
        Ok(response)
    }

    /// Send a request and stream log chunks via a callback.
    ///
    /// Used for `gw logs --follow` where the daemon sends multiple
    /// `LogChunk` responses until the run completes or the callback
    /// returns `false`.
    pub fn send_streaming<F>(&self, request: Request, mut on_chunk: F) -> Result<()>
    where
        F: FnMut(Response) -> bool, // return false to stop
    {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            if !self.socket_path.exists() {
                return Err(eyre!(
                    "Daemon socket not found at {}.\n\
                     Start the daemon with: gw daemon start",
                    self.socket_path.display()
                ));
            }

            let mut stream = tokio::time::timeout(
                CONNECT_TIMEOUT,
                UnixStream::connect(&self.socket_path),
            )
            .await
            .map_err(|_| {
                eyre!(
                    "Connection to daemon timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| {
                eyre!("Cannot connect to daemon: {e}")
            })?;

            write_message(&mut stream, &request).await?;

            loop {
                match read_message::<_, Response>(&mut stream).await {
                    Ok(resp) => {
                        if !on_chunk(resp) {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "stream ended");
                        break;
                    }
                }
            }
            Ok(())
        })
    }

    /// Stream logs for a run, calling `on_line` for each line received.
    ///
    /// This is a convenience wrapper around `send_streaming` that extracts
    /// log data from `LogChunk` responses and splits into lines.
    ///
    /// Returns when the daemon closes the connection (run completed/failed)
    /// or `on_line` returns `false`.
    pub fn stream_logs<F>(&self, run_id: String, mut on_line: F) -> Result<()>
    where
        F: FnMut(&str) -> bool, // return false to stop
    {
        let request = Request::GetLogs {
            run_id,
            follow: true,
            tail: None,
        };

        self.send_streaming(request, |resp| match resp {
            Response::LogChunk { data, .. } => {
                for line in data.lines() {
                    if !on_line(line) {
                        return false;
                    }
                }
                true
            }
            Response::Error { message, .. } => {
                warn!(message = %message, "error from daemon during log streaming");
                false
            }
            _ => true, // ignore unexpected response types
        })
    }

    /// Return the socket path for display purposes.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }
}

/// Default socket path for display in error messages.
pub fn default_socket_path() -> PathBuf {
    gw_home().join("daemon.sock")
}
