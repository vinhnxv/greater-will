//! Stop a running arc (`gw stop`).
//!
//! Sends a StopRun request to the daemon to gracefully halt
//! a specific arc run, preserving its last checkpoint.
//!
//! Supports two modes:
//! - Default: kills the tmux session (with confirmation unless `--force`)
//! - `--detach`: stops GW tracking but keeps the tmux session alive

use crate::client::socket::DaemonClient;
use crate::daemon::protocol::{Request, Response};
use crate::output::tags::tag;
use color_eyre::Result;
use std::io::{self, Write};

/// Execute the `gw stop` command.
///
/// Sends a StopRun request for the given run ID and displays
/// the result, including the last checkpoint phase if available.
pub fn execute(run_id: String, force: bool, detach: bool) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let short = crate::commands::util::short_id(&run_id);

    // Confirmation prompt (unless --force)
    if !force {
        if detach {
            println!(
                "{} This will stop tracking run {} but keep its tmux session alive.",
                tag("WARN"),
                short,
            );
            println!("  The session can be re-adopted on next daemon restart.");
        } else {
            println!(
                "{} This will stop run {} and kill its tmux session.",
                tag("WARN"),
                short,
            );
            println!("  Any in-progress work that hasn't been committed will be lost.");
            println!("  Use --detach to keep the tmux session alive instead.");
        }

        print!("Continue? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    if detach {
        // Detach mode: stop tracking, keep tmux alive
        println!("Detaching run {}...", short);

        let client = DaemonClient::new()?;
        match client.send(Request::DetachRun { run_id: run_id.clone() })? {
            Response::Ok { message } => {
                println!("{} {}", tag("OK"), message);
                println!("  tmux session kept alive — re-adopt with `gw daemon restart`");
            }
            Response::Error { code, message } => {
                use crate::daemon::protocol::ErrorCode;
                match code {
                    ErrorCode::RunNotFound => {
                        println!("{} Run {} not found.", tag("WARN"), short);
                        println!("  Use `gw ps --all` to see all runs.");
                    }
                    _ => {
                        println!("{} {}", tag("FAIL"), message);
                        println!("  {}", code.suggestion(&run_id));
                    }
                }
            }
            _ => {
                println!("{} Unexpected response from daemon", tag("WARN"));
            }
        }
    } else {
        // Default mode: stop and kill tmux
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
                        println!("  {}", code.suggestion(&run_id));
                    }
                }
            }
            _ => {
                println!("{} Unexpected response from daemon", tag("WARN"));
            }
        }
    }

    Ok(())
}
