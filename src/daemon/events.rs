//! Structured event logging for daemon runs.
//!
//! Each run has an `events.jsonl` file under `~/.gw/runs/<run_id>/logs/`
//! where one JSON object per line records timestamped events (started,
//! stopped, phase_change, crash_loop, etc.). This is the primary data
//! source for `gw logs <id>`.
//!
//! Extracted from `daemon::heartbeat` to break a module cycle:
//! `heartbeat → server → heartbeat` (via `drain_if_available`). Keeping
//! event helpers in a leaf module lets executor, reconciler, run_monitor,
//! and heartbeat all depend on `events` without depending on each other.

use crate::daemon::state::gw_home;
use tracing::debug;

/// Append a structured event to the run's event log (events.jsonl).
///
/// Each line is a JSON object with timestamp, event type, and message.
/// This is the primary data source for `gw logs <id>`.
///
/// INV-19: Blocking I/O hoisted off tokio reactor via spawn_blocking
/// (fire-and-forget). The write is best-effort — errors are logged but
/// do not propagate to callers, matching the original contract.
pub fn append_event(run_id: &str, event: &str, message: &str) {
    // When a tokio runtime is available (daemon hot path), fire-and-forget
    // on the blocking pool.  When no runtime exists (unit tests, CLI
    // one-shots), fall back to synchronous inline execution.
    if tokio::runtime::Handle::try_current().is_ok() {
        let run_id = run_id.to_string();
        let event = event.to_string();
        let message = message.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            append_event_sync(&run_id, &event, &message);
        });
    } else {
        append_event_sync(run_id, event, message);
    }
}

/// Synchronous implementation of event append (runs on blocking thread pool).
fn append_event_sync(run_id: &str, event: &str, message: &str) {
    let log_dir = gw_home().join("runs").join(run_id).join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        debug!(error = %e, "failed to create log directory for events");
        return;
    }

    let log_path = log_dir.join("events.jsonl");
    let timestamp = chrono::Utc::now().to_rfc3339();
    let line = serde_json::json!({
        "ts": timestamp,
        "event": event,
        "msg": message,
    });

    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", line);
        }
        Err(e) => {
            debug!(error = %e, "failed to append event");
        }
    }
}

/// Log the initial "run started" event. Called from executor after spawn.
pub fn log_run_started(run_id: &str, plan_path: &str) {
    append_event(run_id, "started", &format!("plan: {plan_path}"));
}

/// Log a run stopped event.
pub fn log_run_stopped(run_id: &str) {
    append_event(run_id, "stopped", "stopped by user");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::gw_home_test_mutex;

    /// `append_event` must create the run's log directory if it does not
    /// already exist and write a JSON line with `ts`, `event`, `msg` keys.
    #[test]
    fn append_event_creates_log_and_writes_json_line() {
        let _guard = gw_home_test_mutex().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("GW_HOME", dir.path());

        append_event("run-abc", "phase_change", "loop -> waiting_pr");

        let log_path = dir
            .path()
            .join("runs")
            .join("run-abc")
            .join("logs")
            .join("events.jsonl");
        let contents = std::fs::read_to_string(&log_path).expect("events.jsonl must exist");
        let line = contents.lines().next().expect("one line expected");
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("valid JSON line expected");
        assert_eq!(parsed["event"], "phase_change");
        assert_eq!(parsed["msg"], "loop -> waiting_pr");
        assert!(parsed["ts"].is_string());

        std::env::remove_var("GW_HOME");
    }

    /// `log_run_started` must write an event with type `started` and a
    /// message that includes the supplied plan path.
    #[test]
    fn log_run_started_writes_started_event_with_plan_path() {
        let _guard = gw_home_test_mutex().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("GW_HOME", dir.path());

        log_run_started("run-xyz", "/repos/demo/arc-plan.md");

        let log_path = dir
            .path()
            .join("runs")
            .join("run-xyz")
            .join("logs")
            .join("events.jsonl");
        let contents = std::fs::read_to_string(&log_path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["event"], "started");
        assert!(parsed["msg"]
            .as_str()
            .unwrap()
            .contains("/repos/demo/arc-plan.md"));

        std::env::remove_var("GW_HOME");
    }

    /// `log_run_stopped` must write an event with type `stopped` and the
    /// canonical "stopped by user" message.
    #[test]
    fn log_run_stopped_writes_stopped_event() {
        let _guard = gw_home_test_mutex().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("GW_HOME", dir.path());

        log_run_stopped("run-zzz");

        let log_path = dir
            .path()
            .join("runs")
            .join("run-zzz")
            .join("logs")
            .join("events.jsonl");
        let contents = std::fs::read_to_string(&log_path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["event"], "stopped");
        assert_eq!(parsed["msg"], "stopped by user");

        std::env::remove_var("GW_HOME");
    }
}
