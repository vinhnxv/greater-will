//! Stop a running arc (`gw stop`).
//!
//! Sends a StopRun request to the daemon to gracefully halt
//! a specific arc run, preserving its last checkpoint.

use crate::client::socket::DaemonClient;
use crate::daemon::protocol::{Request, Response};
use crate::output::tags::tag;
use color_eyre::Result;

/// Execute the `gw stop` command.
///
/// Sends a StopRun request for the given run ID and displays
/// the result, including the last checkpoint phase if available.
pub fn execute(run_id: String) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        return Ok(());
    }

    let short = if run_id.len() > 8 { &run_id[..8] } else { &run_id };
    println!("Stopping run {}...", short);

    let client = DaemonClient::new()?;
    match client.send(Request::StopRun { run_id: run_id.clone() })? {
        Response::Ok { message } => {
            println!("{} {}", tag("OK"), message);
        }
        Response::Error { code, message } => {
            use crate::daemon::protocol::ErrorCode;
            match code {
                ErrorCode::RunNotFound => {
                    println!("{} Run {} not found. It may have already completed.", tag("WARN"), short);
                    println!("  Use `gw ps --all` to see completed runs.");
                }
                _ => {
                    println!("{} {}", tag("FAIL"), message);
                }
            }
        }
        _ => {
            println!("{} Unexpected response from daemon", tag("WARN"));
        }
    }

    Ok(())
}
