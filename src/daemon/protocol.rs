//! IPC protocol types and length-prefixed framing for daemon communication.
//!
//! Messages are framed as: 4-byte big-endian length + JSON payload.
//! This avoids delimiter ambiguity and allows precise buffer allocation.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum message size: 16 MiB (prevents unbounded allocation from malformed frames).
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

// ── Request types ────────────────────────────────────────────────────

/// Client-to-daemon request messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Request {
    /// Submit a new arc run for execution.
    SubmitRun {
        plan_path: PathBuf,
        repo_dir: PathBuf,
        session_name: Option<String>,
        /// Optional CLAUDE_CONFIG_DIR override.
        #[serde(default)]
        config_dir: Option<PathBuf>,
        /// Verbosity level (0=warn, 1=info, 2=debug, 3+=trace).
        /// Controls per-run log detail in daemon event logs.
        #[serde(default)]
        verbose: u8,
    },
    /// List all known runs (optionally filtered by status).
    ListRuns {
        all: bool,
    },
    /// Retrieve log output for a specific run.
    GetLogs {
        run_id: String,
        follow: bool,
        tail: Option<usize>,
        /// If true, return raw tmux pane capture; otherwise return structured events.
        pane: bool,
    },
    /// Stop a running arc (kills the tmux session).
    StopRun {
        run_id: String,
    },
    /// Detach a run: stop GW tracking but keep the tmux session alive.
    /// The session can be re-adopted on next daemon restart via reconciler.
    DetachRun {
        run_id: String,
    },
    /// Query daemon health and version.
    DaemonStatus,
    /// Request graceful daemon shutdown.
    Shutdown,
    /// Add a new schedule.
    AddSchedule {
        plan_path: PathBuf,
        repo_dir: PathBuf,
        kind: crate::daemon::schedule::ScheduleKind,
        #[serde(default)]
        config_dir: Option<PathBuf>,
        #[serde(default)]
        verbose: u8,
        label: Option<String>,
    },
    /// List all schedules.
    ListSchedules,
    /// Remove a schedule by ID.
    RemoveSchedule { id: String },
    /// Pause a schedule.
    PauseSchedule { id: String },
    /// Resume a paused schedule.
    ResumeSchedule { id: String },
    /// List all queued (pending) runs.
    ListQueue,
    /// Remove a specific queued run by ID.
    RemoveQueued { run_id: String },
    /// Clear queued runs, optionally filtered by repo directory.
    ClearQueue { repo_dir: Option<PathBuf> },
}

// ── Response types ───────────────────────────────────────────────────

/// Daemon-to-client response messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Response {
    /// A run was successfully queued.
    RunSubmitted { run_id: String },
    /// A run was queued behind an active run in the same repo.
    RunQueued { run_id: String, position: usize },
    /// List of runs.
    RunList { runs: Vec<RunInfo> },
    /// A chunk of log output.
    LogChunk { run_id: String, data: String },
    /// Generic success acknowledgement.
    Ok { message: String },
    /// An error occurred.
    Error {
        code: ErrorCode,
        message: String,
    },
    /// List of schedules.
    ScheduleList {
        schedules: Vec<ScheduleInfo>,
    },
    /// A schedule was created.
    ScheduleAdded {
        id: String,
        next_fire: Option<String>,
    },
    /// List of queued runs.
    QueueList { entries: Vec<QueuedRunInfo> },
}

// ── Supporting types ─────────────────────────────────────────────────

/// Information about a single arc run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunInfo {
    pub run_id: String,
    pub plan_path: PathBuf,
    pub repo_dir: PathBuf,
    pub session_name: String,
    pub status: RunStatus,
    pub current_phase: Option<String>,
    pub started_at: String,
    pub uptime_secs: u64,
    /// Schedule ID that triggered this run, if any.
    #[serde(default)]
    pub schedule_id: Option<String>,
    /// Whether the daemon is currently waiting for network connectivity.
    #[serde(default)]
    pub waiting_for_network: bool,
}

/// Status of an arc run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Stopped,
}

impl RunStatus {
    /// Returns true for terminal states (Succeeded, Failed, Stopped).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Stopped)
    }
}

/// Structured error codes for programmatic handling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ErrorCode {
    RepoLocked,
    RunNotFound,
    DaemonBusy,
    InvalidRequest,
    InternalError,
    QueueFull,
    ScheduleNotFound,
}

impl ErrorCode {
    /// Return an actionable suggestion for the user based on the error variant.
    ///
    /// `context` is typically a run ID or resource name to interpolate into the
    /// message so the user gets a copy-pasteable remediation command.
    pub fn suggestion(&self, context: &str) -> String {
        match self {
            ErrorCode::RepoLocked => {
                format!("Repository is locked by {context}. Wait or run: `gw stop {context}`")
            }
            ErrorCode::RunNotFound => {
                format!("No run matching '{context}'. Run `gw ps` to see active runs.")
            }
            ErrorCode::DaemonBusy => {
                "Daemon is busy. Try again in a moment.".to_string()
            }
            ErrorCode::InvalidRequest => {
                format!("Invalid request: {context}. Run `gw --help` for usage.")
            }
            ErrorCode::InternalError => {
                "Internal daemon error. Check with: `gw daemon status`".to_string()
            }
            ErrorCode::QueueFull => {
                "Queue is full. Wait for runs to complete or cancel queued entries with `gw stop`.".to_string()
            }
            ErrorCode::ScheduleNotFound => {
                format!("No schedule matching '{context}'. Run `gw schedule list` to see schedules.")
            }
        }
    }
}

// ── Schedule wire types ─────────────────────────────────────────────

/// Wire representation of a schedule entry for IPC responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleInfo {
    pub id: String,
    pub plan_path: PathBuf,
    pub repo_dir: PathBuf,
    pub kind: ScheduleKindInfo,
    pub status: crate::daemon::schedule::ScheduleStatus,
    pub next_fire: Option<String>,
    pub last_fired: Option<String>,
    pub fire_count: u32,
    pub label: Option<String>,
}

/// Wire representation of schedule trigger kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ScheduleKindInfo {
    Cron { expression: String },
    OneShot { at: String },
    Delayed { fires_at: String },
}

// ── Queue wire types ──────────────────────────────────────────────

/// Wire representation of a queued (pending) run for IPC responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedRunInfo {
    pub run_id: String,
    pub plan_path: PathBuf,
    pub repo_dir: PathBuf,
    pub position: usize,
    pub queued_at: String,
    pub session_name: Option<String>,
}

// ── Length-prefixed framing ──────────────────────────────────────────

/// Write a serializable message with 4-byte big-endian length prefix.
pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> color_eyre::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(msg)?;
    // Enforce the same upper bound read_message enforces, so an oversized
    // message is rejected before truncating via the `as u32` cast below.
    if payload.len() > MAX_MESSAGE_SIZE as usize {
        return Err(color_eyre::eyre::eyre!(
            "message size {} exceeds maximum {MAX_MESSAGE_SIZE}",
            payload.len()
        ));
    }
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed message, deserializing from JSON.
pub async fn read_message<R, T>(reader: &mut R) -> color_eyre::Result<T>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_MESSAGE_SIZE {
        return Err(color_eyre::eyre::eyre!(
            "message size {len} exceeds maximum {MAX_MESSAGE_SIZE}"
        ));
    }

    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    let msg = serde_json::from_slice(&payload)?;
    Ok(msg)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn request_round_trip() {
        let cases = vec![
            Request::SubmitRun {
                plan_path: PathBuf::from("/tmp/plan.md"),
                repo_dir: PathBuf::from("/home/user/repo"),
                session_name: Some("test-session".into()),
                config_dir: Some(PathBuf::from("/custom/.claude")),
                verbose: 2,
            },
            Request::ListRuns { all: true },
            Request::GetLogs {
                run_id: "run-123".into(),
                follow: true,
                tail: Some(50),
                pane: false,
            },
            Request::StopRun {
                run_id: "run-456".into(),
            },
            Request::DetachRun {
                run_id: "run-789".into(),
            },
            Request::DaemonStatus,
            Request::Shutdown,
            Request::AddSchedule {
                plan_path: PathBuf::from("/tmp/plan.md"),
                repo_dir: PathBuf::from("/home/user/repo"),
                kind: crate::daemon::schedule::ScheduleKind::Cron {
                    expression: "0 */5 * * * * *".into(),
                },
                config_dir: None,
                verbose: 1,
                label: Some("nightly build".into()),
            },
            Request::ListSchedules,
            Request::RemoveSchedule {
                id: "sched-abc".into(),
            },
            Request::PauseSchedule {
                id: "sched-abc".into(),
            },
            Request::ResumeSchedule {
                id: "sched-abc".into(),
            },
            Request::ListQueue,
            Request::RemoveQueued {
                run_id: "run-q1".into(),
            },
            Request::ClearQueue {
                repo_dir: Some(PathBuf::from("/home/user/repo")),
            },
        ];

        for req in &cases {
            let json = serde_json::to_string(req).unwrap();
            let deser: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(req, &deser, "round-trip failed for {json}");
        }
    }

    #[test]
    fn response_round_trip() {
        let cases = vec![
            Response::RunSubmitted {
                run_id: "run-789".into(),
            },
            Response::RunList {
                runs: vec![RunInfo {
                    run_id: "run-001".into(),
                    plan_path: PathBuf::from("plans/feat.md"),
                    repo_dir: PathBuf::from("/repo"),
                    session_name: "sess-001".into(),
                    status: RunStatus::Running,
                    current_phase: Some("phase_2_work".into()),
                    started_at: "2026-04-10T02:00:00Z".into(),
                    uptime_secs: 120,
                    schedule_id: None,
                    waiting_for_network: false,
                }],
            },
            Response::LogChunk {
                run_id: "run-001".into(),
                data: "Phase completed successfully\n".into(),
            },
            Response::Ok {
                message: "done".into(),
            },
            Response::Error {
                code: ErrorCode::RunNotFound,
                message: "no such run".into(),
            },
            Response::RunQueued {
                run_id: "run-queue-1".into(),
                position: 2,
            },
            Response::ScheduleList {
                schedules: vec![ScheduleInfo {
                    id: "sched-001".into(),
                    plan_path: PathBuf::from("plans/nightly.md"),
                    repo_dir: PathBuf::from("/repo"),
                    kind: ScheduleKindInfo::Cron {
                        expression: "0 0 * * * * *".into(),
                    },
                    status: crate::daemon::schedule::ScheduleStatus::Active,
                    next_fire: Some("2026-04-13T03:00:00Z".into()),
                    last_fired: None,
                    fire_count: 0,
                    label: Some("nightly".into()),
                }],
            },
            Response::ScheduleAdded {
                id: "sched-002".into(),
                next_fire: Some("2026-04-13T04:00:00Z".into()),
            },
            Response::QueueList {
                entries: vec![QueuedRunInfo {
                    run_id: "run-q1".into(),
                    plan_path: PathBuf::from("plans/feat.md"),
                    repo_dir: PathBuf::from("/repo"),
                    position: 1,
                    queued_at: "2026-04-13T05:00:00Z".into(),
                    session_name: Some("sess-q1".into()),
                }],
            },
        ];

        for resp in &cases {
            let json = serde_json::to_string(resp).unwrap();
            let deser: Response = serde_json::from_str(&json).unwrap();
            assert_eq!(resp, &deser, "round-trip failed for {json}");
        }
    }

    #[test]
    fn run_status_round_trip() {
        for status in [
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Stopped,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deser: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deser);
        }
    }

    #[test]
    fn error_code_round_trip() {
        for code in [
            ErrorCode::RepoLocked,
            ErrorCode::RunNotFound,
            ErrorCode::DaemonBusy,
            ErrorCode::InvalidRequest,
            ErrorCode::InternalError,
            ErrorCode::QueueFull,
            ErrorCode::ScheduleNotFound,
        ] {
            let json = serde_json::to_string(&code).unwrap();
            let deser: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(code, deser);
        }
    }

    #[test]
    fn run_info_waiting_for_network_round_trip() {
        let info = RunInfo {
            run_id: "net-001".into(),
            plan_path: PathBuf::from("plans/net.md"),
            repo_dir: PathBuf::from("/repo"),
            session_name: "sess-net".into(),
            status: RunStatus::Running,
            current_phase: Some("phase_work".into()),
            started_at: "2026-04-13T10:00:00Z".into(),
            uptime_secs: 300,
            schedule_id: Some("sched-net".into()),
            waiting_for_network: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        let deser: RunInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, deser);
        assert!(deser.waiting_for_network);
    }

    #[test]
    fn queued_run_info_serde_round_trip() {
        let entry = QueuedRunInfo {
            run_id: "q-rt1".into(),
            plan_path: PathBuf::from("plans/queued.md"),
            repo_dir: PathBuf::from("/repo/queued"),
            position: 3,
            queued_at: "2026-04-13T08:00:00Z".into(),
            session_name: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deser: QueuedRunInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, deser);
        assert_eq!(deser.session_name, None);
    }

    #[tokio::test]
    async fn framing_round_trip() {
        let req = Request::SubmitRun {
            plan_path: PathBuf::from("/tmp/plan.md"),
            repo_dir: PathBuf::from("/repo"),
            session_name: None,
            config_dir: None,
            verbose: 0,
        };

        // Write to an in-memory buffer
        let mut buf = Vec::new();
        write_message(&mut buf, &req).await.unwrap();

        // Read back from the buffer
        let mut cursor = &buf[..];
        let deser: Request = read_message(&mut cursor).await.unwrap();
        assert_eq!(req, deser);
    }

    #[tokio::test]
    async fn framing_round_trip_response() {
        let resp = Response::RunList {
            runs: vec![RunInfo {
                run_id: "r1".into(),
                plan_path: PathBuf::from("p.md"),
                repo_dir: PathBuf::from("/r"),
                session_name: "s1".into(),
                status: RunStatus::Succeeded,
                current_phase: None,
                started_at: "2026-01-01T00:00:00Z".into(),
                uptime_secs: 60,
                schedule_id: None,
                waiting_for_network: false,
            }],
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &resp).await.unwrap();

        let mut cursor = &buf[..];
        let deser: Response = read_message(&mut cursor).await.unwrap();
        assert_eq!(resp, deser);
    }
}
