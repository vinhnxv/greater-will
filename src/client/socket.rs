//! Unix socket client for communicating with the gw daemon.
//!
//! Provides a synchronous API that wraps async socket I/O using a
//! short-lived tokio runtime. Each request opens a new connection,
//! sends one message, reads one response, and closes.

use crate::daemon::protocol::{read_message, write_message, Request, Response};
use crate::daemon::state::{gw_home, GlobalConfig};
use color_eyre::Result;
use std::path::PathBuf;
use tokio::net::UnixStream;

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

    /// Check whether the daemon socket exists and is connectable.
    pub fn is_daemon_running() -> bool {
        let config = match GlobalConfig::load() {
            Ok(c) => c,
            Err(_) => return false,
        };
        let path = config.socket_path();
        if !path.exists() {
            return false;
        }
        // Try a quick connect to verify the daemon is actually listening
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(_) => return false,
        };
        rt.block_on(async {
            match UnixStream::connect(&path).await {
                Ok(mut stream) => {
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
                Err(_) => false,
            }
        })
    }

    /// Send a request and return the response.
    pub fn send(&self, request: Request) -> Result<Response> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(self.send_async(request))
    }

    /// Async implementation of send.
    async fn send_async(&self, request: Request) -> Result<Response> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        write_message(&mut stream, &request).await?;
        let response: Response = read_message(&mut stream).await?;
        Ok(response)
    }

    /// Send a request and stream log chunks via a callback.
    ///
    /// Used for `gw logs --follow` where the daemon sends multiple
    /// LogChunk responses until the run completes.
    pub fn send_streaming<F>(&self, request: Request, mut on_chunk: F) -> Result<()>
    where
        F: FnMut(Response) -> bool, // return false to stop
    {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut stream = UnixStream::connect(&self.socket_path).await?;
            write_message(&mut stream, &request).await?;

            loop {
                match read_message::<_, Response>(&mut stream).await {
                    Ok(resp) => {
                        if !on_chunk(resp) {
                            break;
                        }
                    }
                    Err(_) => break, // connection closed
                }
            }
            Ok(())
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
