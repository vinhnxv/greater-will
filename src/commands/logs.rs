//! Log viewing command (`gw logs`).
//!
//! Retrieves log output for a specific arc run from the daemon.
//!
//! By default, shows structured event logs (phase transitions, status
//! changes, errors) formatted for human reading. Use `--pane` to view
//! raw tmux pane captures, or `--json` for raw JSONL output.
//!
//! Supports `--follow` mode for streaming output and `--tail` to
//! limit initial output.

use crate::client::socket::DaemonClient;
use crate::commands::util::short_id;
use crate::daemon::protocol::{Request, Response};
use crate::output::tags::tag;
use color_eyre::Result;
use console::Style;

/// Execute the `gw logs` command.
///
/// Sends a GetLogs request to the daemon and prints log output.
/// In follow mode, keeps the connection open and streams new lines.
pub fn execute(run_id: String, follow: bool, tail: Option<usize>, pane: bool) -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let request = Request::GetLogs {
        run_id: run_id.clone(),
        follow,
        tail,
        pane,
    };

    let log_type = if pane { "pane capture" } else { "event log" };

    if follow {
        println!(
            "{} Following {} for run {}... (Ctrl+C to stop)",
            tag("RUN"),
            log_type,
            short_id(&run_id)
        );
        println!();

        let client = DaemonClient::new()?;
        client.send_streaming(request, |resp| {
            match resp {
                Response::LogChunk { data, .. } => {
                    if pane {
                        print!("{}", data);
                    } else {
                        print_formatted_events(&data);
                    }
                    true
                }
                Response::Ok { message } => {
                    println!();
                    println!("{} {}", tag("DONE"), message);
                    false
                }
                Response::Error { code, message } => {
                    println!("{} {}", tag("FAIL"), message);
                    println!("  {}", code.suggestion(&run_id));
                    false
                }
                _ => false,
            }
        })?;
    } else {
        let client = DaemonClient::new()?;
        match client.send(request)? {
            Response::LogChunk { data, .. } => {
                if data.is_empty() {
                    println!(
                        "{} No {} available yet for run {}",
                        tag("WARN"),
                        log_type,
                        short_id(&run_id)
                    );
                    if !pane {
                        println!("  Tip: use --pane to see raw tmux output");
                    }
                } else if pane {
                    print!("{}", data);
                } else {
                    print_formatted_events(&data);
                }
            }
            Response::Error { code, message } => {
                println!("{} {}", tag("FAIL"), message);
                println!("  {}", code.suggestion(&run_id));
            }
            _ => {
                println!("{} Unexpected response from daemon", tag("WARN"));
            }
        }
    }

    Ok(())
}

/// Parse and pretty-print structured event lines from events.jsonl.
///
/// Input is newline-delimited JSON. Each line is formatted as:
///
///   14:01:56  [PHASE]  plan
///   14:02:30  [START]  plan: plans/feat.md
///   14:15:00  [DONE]   pipeline finished successfully
fn print_formatted_events(data: &str) {
    let dim = Style::new().dim();

    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse JSON event
        let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(line);
        match parsed {
            Ok(obj) => {
                let ts = obj.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                let event = obj.get("event").and_then(|v| v.as_str()).unwrap_or("?");
                let msg = obj.get("msg").and_then(|v| v.as_str()).unwrap_or("");

                let time_str = format_time(ts);
                let (event_tag, event_style) = event_display(event);

                println!(
                    "  {}  {}  {}",
                    dim.apply_to(&time_str),
                    event_style.apply_to(format!("{:<9}", event_tag)),
                    msg,
                );
            }
            Err(_) => {
                // Fallback: print raw line if not valid JSON
                println!("  {}", line);
            }
        }
    }
}

/// Extract HH:MM:SS from an RFC 3339 timestamp, or return as-is if unparseable.
fn format_time(ts: &str) -> String {
    // RFC 3339: "2026-04-10T14:01:56.481370+00:00"
    // We want just "14:01:56"
    if let Some(t_pos) = ts.find('T') {
        let after_t = &ts[t_pos + 1..];
        // Take up to the first dot or plus/minus (fractional seconds or tz offset)
        let end = after_t
            .find(|c: char| c == '.' || c == '+' || c == '-' || c == 'Z')
            .unwrap_or(after_t.len());
        after_t[..end].to_string()
    } else {
        ts.to_string()
    }
}

/// Map event type to a display tag and style.
fn event_display(event: &str) -> (&str, Style) {
    match event {
        "started" => ("START", Style::new().cyan().bold()),
        "phase_change" => ("PHASE", Style::new().blue().bold()),
        "completed" => ("DONE", Style::new().green().bold()),
        "failed" => ("FAIL", Style::new().red().bold()),
        "stopped" => ("STOP", Style::new().yellow().bold()),
        "crash_recovery" => ("CRASH", Style::new().red()),
        "recovery_started" => ("RECOV", Style::new().cyan()),
        "recovery_failed" => ("FAIL", Style::new().red().bold()),
        _ => (event, Style::new()),
    }
}
