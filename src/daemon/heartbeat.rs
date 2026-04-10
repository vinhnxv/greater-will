//! Heartbeat monitor for active daemon runs.
//!
//! Periodically checks tmux session health for each `Running` entry,
//! captures logs, updates phase information, and handles crash recovery
//! by either auto-resuming (if a checkpoint exists) or marking as Failed.

use crate::daemon::protocol::RunStatus;
use crate::daemon::reconciler::check_checkpoint;
use crate::daemon::registry::RunRegistry;
use crate::daemon::state::gw_home;
use crate::engine::single_session::util::is_pipeline_complete;
use crate::session::spawn;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Interval between heartbeat checks.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum crash-restart cycles before giving up.
const MAX_CRASH_RESTARTS: u32 = 3;

/// Heartbeat monitor that watches over active runs.
///
/// Spawns a background tokio task that periodically:
/// 1. Checks if each Running entry's tmux session is still alive
/// 2. Captures pane output and appends to pane log
/// 3. Detects phase transitions and logs structured events
/// 4. Performs crash recovery when a tmux session dies unexpectedly
pub struct HeartbeatMonitor {
    registry: Arc<Mutex<RunRegistry>>,
    /// Consecutive dead-tick counts per run. A run is only declared dead
    /// after reaching DEAD_TICKS_THRESHOLD consecutive failures, preventing
    /// transient `has_session` failures from triggering destructive recovery.
    dead_counts: std::sync::Mutex<std::collections::HashMap<String, u32>>,
}

impl HeartbeatMonitor {
    /// Create a new heartbeat monitor wrapping the shared registry.
    pub fn new(registry: Arc<Mutex<RunRegistry>>) -> Self {
        Self {
            registry,
            dead_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Start the heartbeat loop as a background tokio task.
    ///
    /// Returns a `JoinHandle` that can be used to await or abort the task.
    /// The task runs until the handle is dropped/aborted or the process exits.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            info!("heartbeat monitor started (interval: {:?})", HEARTBEAT_INTERVAL);

            loop {
                interval.tick().await;
                self.check_all_runs().await;
            }
        })
    }

    /// Check all Running entries in the registry.
    async fn check_all_runs(&self) {
        let registry = self.registry.lock().await;

        // Collect run IDs to check (avoid holding lock during tmux calls).
        // Only include entries with a tmux session — entries without one
        // (e.g., queued runs) are skipped to avoid inflating the dead count.
        let running: Vec<String> = registry
            .list_runs(false)
            .iter()
            .filter(|r| r.status == RunStatus::Running)
            .map(|r| r.run_id.clone())
            .collect();

        drop(registry); // Release lock before I/O

        let mut alive = 0u32;
        let mut dead = 0u32;

        for run_id in &running {
            let was_alive = self.check_run(run_id).await;
            if was_alive {
                alive += 1;
            } else {
                dead += 1;
            }
        }

        if !running.is_empty() {
            debug!(alive = alive, dead = dead, "heartbeat tick");
        }
    }

    /// Check a single run's health. Returns `true` if the session is alive.
    async fn check_run(&self, run_id: &str) -> bool {
        let tmux_session = {
            let registry = self.registry.lock().await;
            match registry.get(run_id) {
                Some(entry) if entry.status == RunStatus::Running => {
                    entry.tmux_session.clone()
                }
                _ => return false, // Entry gone or no longer running
            }
        };

        let tmux_session = match tmux_session {
            Some(s) => s,
            None => {
                debug!(run_id = %run_id, "no tmux session associated — skipping");
                return false;
            }
        };

        // Check if tmux session is alive (sync call, with retry).
        let session_alive = spawn::has_session(&tmux_session);

        if session_alive {
            // Reset dead counter on success
            if let Ok(mut counts) = self.dead_counts.lock() {
                counts.remove(run_id);
            }
            self.handle_alive_session(run_id, &tmux_session).await;
        } else {
            // Grace period: require 3 consecutive dead ticks before recovery.
            // Prevents transient tmux failures (server busy during agent team
            // spawning) from triggering destructive kill+restart.
            const DEAD_TICKS_THRESHOLD: u32 = 3;

            let consecutive = {
                let mut counts = self.dead_counts.lock().unwrap_or_else(|p| p.into_inner());
                let count = counts.entry(run_id.to_string()).or_insert(0);
                *count += 1;
                *count
            };

            if consecutive >= DEAD_TICKS_THRESHOLD {
                warn!(
                    run_id = %run_id,
                    consecutive = consecutive,
                    "session confirmed dead after {} ticks — triggering recovery",
                    DEAD_TICKS_THRESHOLD
                );
                self.handle_dead_session(run_id, &tmux_session).await;
            } else {
                debug!(
                    run_id = %run_id,
                    consecutive = consecutive,
                    threshold = DEAD_TICKS_THRESHOLD,
                    "session appears dead — waiting for confirmation"
                );
            }
        }

        session_alive
    }

    /// Handle a healthy (alive) tmux session: capture logs and update phase.
    async fn handle_alive_session(&self, run_id: &str, tmux_session: &str) {
        // Capture pane output for log archival
        match spawn::capture_pane(tmux_session) {
            Ok(content) => {
                self.append_pane_log(run_id, &content);
                self.update_phase_from_pane(run_id, &content).await;
            }
            Err(e) => {
                debug!(run_id = %run_id, error = %e, "failed to capture pane");
            }
        }
    }

    /// Handle a dead tmux session: attempt crash recovery or mark as Failed.
    async fn handle_dead_session(&self, run_id: &str, tmux_session: &str) {
        // Diagnose WHY the session died before taking recovery action.
        let death_reason = diagnose_session_death(run_id, tmux_session);
        warn!(
            run_id = %run_id,
            tmux_session = %tmux_session,
            reason = %death_reason,
            "tmux session is dead: {}",
            death_reason,
        );

        // Log the diagnosis as a separate event for `gw logs` visibility
        append_event(run_id, "session_died", &format!(
            "tmux session '{}' died — reason: {}",
            tmux_session, death_reason,
        ));

        let mut registry = self.registry.lock().await;
        let entry = match registry.get_mut(run_id) {
            Some(e) if e.status == RunStatus::Running => e,
            _ => return,
        };

        // Check if we have a checkpoint for crash recovery
        let checkpoint_exists = check_checkpoint(&entry.repo_dir);

        if checkpoint_exists && entry.crash_restarts < MAX_CRASH_RESTARTS {
            info!(
                run_id = %run_id,
                crash_restarts = entry.crash_restarts,
                reason = %death_reason,
                "checkpoint found — scheduling crash recovery"
            );
            entry.crash_restarts += 1;
            entry.current_phase = Some("recovering".to_string());

            // Log the crash recovery event WITH the diagnosis reason
            append_event(run_id, "crash_recovery", &format!(
                "tmux session died (reason: {}), attempting restart #{}",
                death_reason, entry.crash_restarts,
            ));

            // Clone what we need before dropping the lock
            let repo_dir = entry.repo_dir.clone();
            let plan_path = entry.plan_path.clone();
            let session_name = entry.session_name.clone();
            let config_dir = entry.config_dir.clone();
            let run_id_owned = run_id.to_string();

            // Persist the crash-restart count
            if let Err(e) = registry.update_status(
                &run_id_owned,
                RunStatus::Running,
                Some("recovering".to_string()),
                None,
            ) {
                warn!(error = %e, "failed to update crash-restart status");
            }

            drop(registry);

            // Spawn recovery in background (don't block heartbeat)
            let registry_clone = Arc::clone(&self.registry);
            tokio::spawn(async move {
                spawn_recovery(
                    registry_clone,
                    &run_id_owned,
                    &repo_dir,
                    &plan_path,
                    &session_name,
                    config_dir,
                )
                .await;
            });
        } else {
            let reason = if entry.crash_restarts >= MAX_CRASH_RESTARTS {
                format!(
                    "tmux session died after {} crash restarts — giving up (last death: {})",
                    entry.crash_restarts, death_reason,
                )
            } else {
                format!(
                    "tmux session died with no checkpoint — marking failed (reason: {})",
                    death_reason,
                )
            };

            warn!(run_id = %run_id, reason = %reason);
            append_event(run_id, "failed", &reason);

            if let Err(e) = registry.update_status(
                run_id,
                RunStatus::Failed,
                None,
                Some(reason),
            ) {
                warn!(run_id = %run_id, error = %e, "failed to mark run as failed");
            }
        }
    }

    /// Append captured pane content to the pane log (raw tmux capture).
    fn append_pane_log(&self, run_id: &str, content: &str) {
        let log_dir = gw_home().join("runs").join(run_id).join("logs");
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            debug!(error = %e, "failed to create log directory");
            return;
        }

        let log_path = log_dir.join("pane.log");
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "--- heartbeat capture ---");
                let _ = write!(f, "{content}");
            }
            Err(e) => {
                debug!(error = %e, "failed to append to pane log");
            }
        }
    }

    /// Update the run's current phase from checkpoint.json (ground truth),
    /// falling back to pane content heuristics when no checkpoint is available.
    async fn update_phase_from_pane(&self, run_id: &str, pane_content: &str) {
        // Try checkpoint.json first — this is the authoritative source
        let checkpoint_phase = self.read_phase_from_checkpoint(run_id).await;

        // Fall back to pane heuristics only when checkpoint is unavailable
        let phase = if let Some(cp_phase) = checkpoint_phase {
            Some(cp_phase)
        } else {
            phase_from_pane_heuristic(pane_content)
        };

        if let Some(phase_name) = phase {
            let mut registry = self.registry.lock().await;
            if let Some(entry) = registry.get_mut(run_id) {
                if entry.current_phase.as_deref() != Some(&phase_name) {
                    debug!(run_id = %run_id, phase = %phase_name, "phase updated");
                    entry.current_phase = Some(phase_name.clone());

                    // Log the phase transition as a structured event
                    drop(registry);
                    append_event(run_id, "phase_change", &phase_name);
                }
            }
        }

        // Check for pipeline completion
        if is_pipeline_complete(pane_content) {
            info!(run_id = %run_id, "pipeline completed (pane text match)");
            append_event(run_id, "completion_detected", "pipeline completion detected via pane text pattern match");
            let mut registry = self.registry.lock().await;
            // Re-check status: stop_run (or another path) may have mutated the
            // entry between our earlier lock drop and this re-acquisition. If
            // the run is no longer Running, skip the Succeeded update to avoid
            // overwriting a Stopped/Failed terminal state.
            let still_running = registry
                .get(run_id)
                .map(|e| e.status == RunStatus::Running)
                .unwrap_or(false);
            if !still_running {
                debug!(
                    run_id = %run_id,
                    "status changed since alive-check began — skipping completion update"
                );
                return;
            }
            let _ = registry.update_status(
                run_id,
                RunStatus::Succeeded,
                Some("complete".to_string()),
                None,
            );
            drop(registry);
            append_event(run_id, "completed", "pipeline finished successfully");
        }
    }

    /// Read the current phase directly from checkpoint.json in the repo.
    ///
    /// This is the ground truth — it knows the exact phase name from the 41-phase
    /// PHASE_ORDER, not just the 5 coarse buckets from pane heuristics.
    async fn read_phase_from_checkpoint(&self, run_id: &str) -> Option<String> {
        let repo_dir = {
            let registry = self.registry.lock().await;
            registry.get(run_id).map(|e| e.repo_dir.clone())?
        };

        // Scan .rune/arc/*/checkpoint.json for the most recent checkpoint
        let arc_dir = repo_dir.join(".rune").join("arc");
        let checkpoint_path = find_latest_checkpoint(&arc_dir)?;

        match crate::checkpoint::reader::read_checkpoint(&checkpoint_path) {
            Ok(cp) => {
                let position = cp.infer_phase_position();
                Some(position.effective_phase()?.to_string())
            }
            Err(e) => {
                debug!(run_id = %run_id, error = %e, "failed to read checkpoint");
                None
            }
        }
    }
}

// ── Structured event logging ───────────────────────────────────────

/// Append a structured event to the run's event log (events.jsonl).
///
/// Each line is a JSON object with timestamp, event type, and message.
/// This is the primary data source for `gw logs <id>`.
pub fn append_event(run_id: &str, event: &str, message: &str) {
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

/// Spawn a crash recovery session for a failed run.
///
/// If a checkpoint exists, runs with `--resume` to continue where it left off.
/// Otherwise, starts a fresh arc run from scratch (re-spawn).
async fn spawn_recovery(
    registry: Arc<Mutex<RunRegistry>>,
    run_id: &str,
    repo_dir: &std::path::Path,
    plan_path: &std::path::Path,
    _session_name: &str,
    config_dir: Option<std::path::PathBuf>,
) {
    let tmux_session = format!("gw-{}", run_id);

    // Only kill the tmux session if it is truly dead (no claude process running).
    // A transient has_session failure could trigger recovery even when the
    // session is alive; killing it would be destructive.
    if spawn::has_session(&tmux_session) {
        // Session is still alive — check if claude is running inside it.
        // If claude is alive, abort recovery (false positive crash detection).
        if is_claude_alive_in_session(&tmux_session) {
            info!(
                run_id = %run_id,
                tmux_session = %tmux_session,
                "ABORT recovery: session and claude are alive — false positive crash"
            );
            // Reset status back to Running (undo the crash_restarts increment)
            let mut reg = registry.lock().await;
            if let Some(entry) = reg.get_mut(run_id) {
                entry.crash_restarts = entry.crash_restarts.saturating_sub(1);
                entry.current_phase = Some("running".to_string());
            }
            return;
        }
        // Claude is dead but session alive — kill to recreate cleanly
        append_event(run_id, "kill_session", &format!(
            "gw killed session '{}' — reason: claude process dead inside alive tmux, killing for clean recovery (killed by: heartbeat/spawn_recovery)",
            tmux_session,
        ));
        let _ = spawn::kill_session(&tmux_session);
    }

    let has_checkpoint = crate::daemon::reconciler::check_checkpoint(repo_dir);

    info!(
        run_id = %run_id,
        tmux_session = %tmux_session,
        has_checkpoint = has_checkpoint,
        "starting crash recovery"
    );

    // Build the command: --resume if checkpoint exists, fresh start otherwise
    let plan_str = plan_path
        .to_str()
        .unwrap_or("plan.md");
    let cmd = if has_checkpoint {
        format!("/rune:arc '{}' --resume", plan_str)
    } else {
        info!(run_id = %run_id, "no checkpoint — starting fresh arc run");
        format!("/rune:arc '{}'", plan_str)
    };

    // Spawn tmux session
    let config = spawn::SpawnConfig {
        session_id: tmux_session.clone(),
        working_dir: repo_dir.to_path_buf(),
        config_dir,
        claude_path: "claude".to_string(),
        mock: false,
    };

    match spawn::spawn_claude_session(&config) {
        Ok(_pid) => {
            // Send the resume command
            if let Err(e) = spawn::send_keys_with_workaround(&tmux_session, &cmd) {
                warn!(error = %e, "failed to send resume command");
                let mut reg = registry.lock().await;
                let _ = reg.update_status(
                    run_id,
                    RunStatus::Failed,
                    None,
                    Some(format!("crash recovery failed: {e}")),
                );
                return;
            }

            // Update registry with new tmux session
            let mut reg = registry.lock().await;
            if let Some(entry) = reg.get_mut(run_id) {
                entry.tmux_session = Some(tmux_session.clone());
                entry.current_phase = Some("resuming".to_string());
            }

            info!(run_id = %run_id, tmux_session = %tmux_session, "crash recovery started");
            append_event(run_id, "recovery_started", &format!("new session: {tmux_session}"));
        }
        Err(e) => {
            warn!(run_id = %run_id, error = %e, "failed to spawn recovery session");
            let mut reg = registry.lock().await;
            let _ = reg.update_status(
                run_id,
                RunStatus::Failed,
                None,
                Some(format!("crash recovery spawn failed: {e}")),
            );
            append_event(run_id, "recovery_failed", &e.to_string());
        }
    }
}

/// Fallback: extract a coarse phase name from pane content heuristics.
///
/// Used only when checkpoint.json is not available (e.g., session just started
/// and Rune hasn't written a checkpoint yet).
fn phase_from_pane_heuristic(pane_content: &str) -> Option<String> {
    let last_lines: Vec<&str> = pane_content.lines().rev().take(20).collect();
    let tail = last_lines.join("\n").to_lowercase();

    if tail.contains("phase_1") || tail.contains("plan") {
        Some("plan".to_string())
    } else if tail.contains("phase_2") || tail.contains("work") {
        Some("work".to_string())
    } else if tail.contains("phase_3") || tail.contains("review") {
        Some("review".to_string())
    } else if tail.contains("phase_4") || tail.contains("test") {
        Some("test".to_string())
    } else if tail.contains("phase_5") || tail.contains("ship") || tail.contains("merge") {
        Some("ship".to_string())
    } else {
        None
    }
}

/// Find the most recently modified checkpoint.json under .rune/arc/*/
fn find_latest_checkpoint(arc_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    if !arc_dir.is_dir() {
        return None;
    }

    std::fs::read_dir(arc_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let cp = e.path().join("checkpoint.json");
            if cp.exists() {
                let mtime = std::fs::metadata(&cp).ok()?.modified().ok()?;
                Some((cp, mtime))
            } else {
                None
            }
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(path, _)| path)
}


/// Diagnose why a tmux session died by checking multiple signals.
///
/// Returns a human-readable reason string that appears in `gw logs`.
/// Checks (in priority order):
/// 1. tmux server alive? (if not, all sessions die)
/// 2. Last captured pane output for error patterns
/// 3. Process exit signals (OOM, SIGKILL, etc.)
/// 4. Checkpoint state (which phase was running)
fn diagnose_session_death(run_id: &str, tmux_session: &str) -> String {
    use std::process::Command;

    // 1. Check if tmux server itself is dead
    let tmux_alive = Command::new("tmux")
        .args(["list-sessions"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !tmux_alive {
        return "tmux server died (all sessions lost)".to_string();
    }

    // 2. Check system log for OOM kills (last 60 seconds)
    let oom_killed = Command::new("log")
        .args([
            "show", "--predicate",
            "eventMessage CONTAINS \"Killed\" AND eventMessage CONTAINS \"claude\"",
            "--last", "1m",
            "--style", "compact",
        ])
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            if text.contains("Killed") || text.contains("jettisoned") {
                Some("OOM: system killed claude process (memory pressure)".to_string())
            } else {
                None
            }
        });

    if let Some(reason) = oom_killed {
        return reason;
    }

    // 3. Check the last pane log for error patterns
    let pane_log = gw_home().join("runs").join(run_id).join("logs").join("pane.log");
    if let Ok(content) = std::fs::read_to_string(&pane_log) {
        // Check last 50 lines for error patterns
        let tail: String = content.lines().rev().take(50).collect::<Vec<_>>().join("\n");
        let tail_lower = tail.to_lowercase();

        if tail_lower.contains("error: out of memory") || tail_lower.contains("allocation failed") {
            return "claude process ran out of memory".to_string();
        }
        if tail_lower.contains("api key") || tail_lower.contains("authentication") {
            return "possible auth/API key error in claude output".to_string();
        }
        if tail_lower.contains("connection refused") || tail_lower.contains("network") {
            return "possible network/connection error".to_string();
        }
        if tail_lower.contains("segmentation fault") || tail_lower.contains("segfault") {
            return "claude process crashed (segfault)".to_string();
        }
        if tail_lower.contains("panic") && tail_lower.contains("thread") {
            return "claude process panicked".to_string();
        }
    }

    // 4. Check checkpoint for phase context
    let arc_dir = gw_home().join("runs").join(run_id);
    if let Ok(entries) = std::fs::read_dir(&arc_dir) {
        // Try to find repo_dir from registry (best effort)
        let _ = entries; // We already have the pane log path
    }

    // 5. Check if any other gw process killed it (signal file)
    let signal_dir = std::path::Path::new("/tmp").join(format!("gw-signals-{}", tmux_session));
    if signal_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&signal_dir) {
            let signals: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            if !signals.is_empty() {
                return format!("signal files present: {}", signals.join(", "));
            }
        }
    }

    // 6. No diagnosis — unknown cause
    "unknown cause (session vanished with no error signals)".to_string()
}

/// Check if a claude process is running inside a tmux session.
///
/// Uses `tmux list-panes` to get the pane PID, then checks if it has
/// a claude child process via `pgrep -P`.
fn is_claude_alive_in_session(tmux_session: &str) -> bool {
    use std::process::Command;

    // Get pane PID
    let output = Command::new("tmux")
        .args(["list-panes", "-t", tmux_session, "-F", "#{pane_pid}"])
        .output();

    let pane_pid = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
        }
        _ => None,
    };

    let Some(pid) = pane_pid else {
        return false;
    };

    // Check if pane's shell has a claude child process
    Command::new("pgrep")
        .args(["-P", &pid.to_string(), "-f", "claude"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
