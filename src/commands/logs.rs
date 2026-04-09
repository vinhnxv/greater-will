//! Log viewing command (`gw logs`).
//!
//! Retrieves log output for a specific arc run from the daemon.
//! Supports `--follow` mode for streaming output and `--tail` to
//! limit initial output.

use crate::client::socket::DaemonClient;
use crate::daemon::protocol::{Request, Response};
use crate::output::tags::tag;
use color_eyre::Result;

/// Execute the `gw logs` command.
///
/// Sends a GetLogs request to the daemon and prints log output.
/// In follow mode, keeps the connection open and streams new lines.
pub fn execute(run_id: String, follow: bool, tail: Option<usize>) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let request = Request::GetLogs {
        run_id: run_id.clone(),
        follow,
        tail,
    };

    if follow {
        // Streaming mode: keep connection open, print chunks as they arrive.
        // Ctrl+C (SIGINT) will break out of the loop via the connection dropping.
        println!("{} Following logs for run {}... (Ctrl+C to stop)", tag("RUN"), short_id(&run_id));
        println!();

        let client = DaemonClient::new()?;
        client.send_streaming(request, |resp| {
            match resp {
                Response::LogChunk { data, .. } => {
                    print!("{}", data);
                    true // continue
                }
                Response::Ok { message } => {
                    println!();
                    println!("{} {}", tag("DONE"), message);
                    false // run completed, stop
                }
                Response::Error { message, .. } => {
                    println!("{} {}", tag("FAIL"), message);
                    false
                }
                _ => false,
            }
        })?;
    } else {
        // One-shot mode: fetch all available logs and print.
        let client = DaemonClient::new()?;
        match client.send(request)? {
            Response::LogChunk { data, .. } => {
                print!("{}", data);
            }
            Response::Error { message, .. } => {
                println!("{} {}", tag("FAIL"), message);
            }
            _ => {
                println!("{} Unexpected response from daemon", tag("WARN"));
            }
        }
    }

    Ok(())
}

/// Shorten a run ID for display.
fn short_id(id: &str) -> &str {
    if id.len() > 8 { &id[..8] } else { id }
}
