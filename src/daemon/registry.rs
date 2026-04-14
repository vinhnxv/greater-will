//! Run registry: persistent tracking of arc runs.
//!
//! Each run is stored as `~/.gw/runs/<id>/meta.json` using atomic writes
//! (write-to-tmp + rename). Per-repo locking via `fs2` prevents concurrent
//! runs against the same repository.

use chrono::{DateTime, Utc};
use color_eyre::{eyre::WrapErr, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::daemon::protocol::{RunInfo, RunStatus};
use crate::daemon::state::gw_home;

/// Maximum pending runs across all repos.
const MAX_QUEUE_SIZE: usize = 50;

/// Consecutive spawn failures before the circuit breaker trips.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

// ── Pending run (in-memory queue entry) ────────────────────────────

/// In-memory record of a run waiting to be spawned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRun {
    pub run_id: String,
    pub plan_path: PathBuf,
    pub repo_dir: PathBuf,
    pub session_name: Option<String>,
    pub config_dir: Option<PathBuf>,
    pub verbose: u8,
    pub queued_at: DateTime<Utc>,
}

/// Snapshot of all pending queues and circuit-breaker state, persisted to
/// `~/.gw/queue.json` so queued runs survive daemon restarts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueueSnapshot {
    #[serde(default)]
    pub queues: HashMap<String, Vec<PendingRun>>,
    #[serde(default)]
    pub consecutive_failures: HashMap<String, u32>,
}

// ── Shared repo hash ────────────────────────────────────────────────

/// Compute the canonical repo hash used for daemon per-repo lock paths.
///
/// Shared between [`RunRegistry`] (which creates and holds the lock) and
/// `commands::run::check_daemon_repo_lock` (which probes the lock from the
/// foreground run path). Both call sites MUST use this function — any drift
/// would silently break the foreground-vs-daemon collision guard by making
/// it check a non-existent lock file path.
///
/// Format: SHA-256 of the canonical path as UTF-8, first 16 bytes hex-encoded
/// (32 characters). Canonicalization falls back to the input path if the
/// target doesn't exist, so the function is infallible and usable from any
/// call site, including pre-creation foreground guards.
pub(crate) fn repo_hash(repo_dir: &Path) -> String {
    let canonical = repo_dir
        .canonicalize()
        .unwrap_or_else(|_| repo_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..16])
}

// ── Run entry ───────────────────────────────────────────────────────

/// Persistent metadata for a single arc run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEntry {
    /// 8-character hex run identifier.
    pub run_id: String,
    /// Plan file that initiated this run.
    pub plan_path: PathBuf,
    /// Repository working directory.
    pub repo_dir: PathBuf,
    /// Logical session name (user-facing).
    pub session_name: String,
    /// Tmux session name (internal).
    pub tmux_session: Option<String>,
    /// Current run status.
    pub status: RunStatus,
    /// Current phase being executed.
    pub current_phase: Option<String>,
    /// When the run was submitted.
    pub started_at: DateTime<Utc>,
    /// When the run completed (success, failure, or stop).
    pub finished_at: Option<DateTime<Utc>>,
    /// Number of crash-restart cycles.
    pub crash_restarts: u32,
    /// Config directory override, if any.
    pub config_dir: Option<PathBuf>,
    /// Error message on failure.
    pub error_message: Option<String>,
    /// Whether this run can be re-spawned from scratch on daemon restart.
    /// Defaults to `true` for backward compatibility with existing meta.json.
    #[serde(default = "default_restartable")]
    pub restartable: bool,
    /// PID of the `claude` child process inside the tmux session.
    ///
    /// Stored after a successful spawn so the monitor loop can check whether
    /// the Claude process is alive without shell-wrangling tmux. `None` for
    /// runs that existed before this field was added (`#[serde(default)]`)
    /// and for runs whose spawn failed before a PID was captured.
    #[serde(default)]
    pub claude_pid: Option<u32>,
    /// Schedule ID that triggered this run, if any.
    #[serde(default)]
    pub schedule_id: Option<String>,
    /// When the current session started (reset on each recovery spawn).
    /// Used to compute per-session uptime for crash loop stability checks.
    /// `None` for runs created before this field was added.
    #[serde(default)]
    pub last_recovery_at: Option<DateTime<Utc>>,
    /// Monotonic write-epoch for idempotent flush. Bumped on every stage_status_locked mutation.
    /// INV-19: enables fsync-outside-mutex via epoch-based conflict resolution.
    #[serde(default)]
    pub write_epoch: u64,
}

fn default_restartable() -> bool {
    true
}

impl RunEntry {
    /// Convert to the wire format used in protocol responses.
    pub fn to_run_info(&self) -> RunInfo {
        let uptime = if matches!(self.status, RunStatus::Running | RunStatus::Queued) {
            Utc::now()
                .signed_duration_since(self.started_at)
                .num_seconds()
                .max(0) as u64
        } else {
            self.finished_at
                .map(|f| f.signed_duration_since(self.started_at).num_seconds().max(0) as u64)
                .unwrap_or(0)
        };

        RunInfo {
            run_id: self.run_id.clone(),
            plan_path: self.plan_path.clone(),
            repo_dir: self.repo_dir.clone(),
            session_name: self.session_name.clone(),
            status: self.status,
            current_phase: self.current_phase.clone(),
            started_at: self.started_at.to_rfc3339(),
            uptime_secs: uptime,
            schedule_id: self.schedule_id.clone(),
            waiting_for_network: false, // populated by server with shared state
        }
    }
}

// ── Repo lock guard ────────────────────────────────────────────────

/// RAII guard for per-repo locks. Releases the lock on drop unless `.commit()` is called.
/// Closes INV-7: prevents repo lock leaks when `write_meta` fails mid-promote.
pub(crate) struct RepoLockGuard {
    repo_hash: String,
    committed: bool,
}

impl RepoLockGuard {
    pub fn new(repo_hash: String) -> Self {
        Self {
            repo_hash,
            committed: false,
        }
    }

    /// Consume the guard, marking the lock as intentionally held.
    /// Must be called on the success path after write_meta succeeds.
    #[must_use = "RepoLockGuard::commit consumes self — assign to _ if intentional"]
    pub fn commit(mut self) {
        self.committed = true;
    }

    /// Release the repo lock if not committed. Call on error paths.
    pub fn release_from(self, map: &mut HashMap<String, fs::File>) {
        if !self.committed {
            map.remove(&self.repo_hash);
        }
    }
}

// ── Status update types ───────────────────────────────────────────

/// Errors from status update operations.
#[derive(Debug, thiserror::Error)]
pub enum UpdateStatusError {
    #[error("run not found: {0}")]
    NotFound(String),
    #[error("invalid transition: {from:?} → {to:?}")]
    InvalidTransition { from: RunStatus, to: RunStatus },
    #[error(transparent)]
    Io(#[from] color_eyre::eyre::Error),
}

/// Staged status change — result of in-memory mutation, ready for flush.
#[derive(Debug, Clone)]
pub struct StagedStatus {
    pub entry: RunEntry,
    pub epoch: u64,
}

// ── Registry ────────────────────────────────────────────────────────

/// In-memory registry of all known runs, backed by on-disk JSON files.
#[derive(Debug)]
pub struct RunRegistry {
    runs: HashMap<String, RunEntry>,
    /// Held file locks for per-repo exclusion (keyed by repo hash).
    #[allow(dead_code)]
    repo_locks: HashMap<String, fs::File>,
    /// Per-repo pending run queues (keyed by repo hash).
    pending_queues: HashMap<String, VecDeque<PendingRun>>,
    /// Consecutive spawn failure count per repo (for circuit breaker).
    consecutive_failures: HashMap<String, u32>,
}

impl RunRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            runs: HashMap::new(),
            repo_locks: HashMap::new(),
            pending_queues: HashMap::new(),
            consecutive_failures: HashMap::new(),
        }
    }

    /// Load all existing runs from `~/.gw/runs/*/meta.json`.
    pub fn load_from_disk() -> Result<Self> {
        let mut registry = Self::new();
        let runs_dir = gw_home().join("runs");

        if !runs_dir.exists() {
            return Ok(registry);
        }

        let entries = fs::read_dir(&runs_dir).wrap_err("failed to read runs directory")?;

        for entry in entries.flatten() {
            let meta_path = entry.path().join("meta.json");
            if !meta_path.exists() {
                continue;
            }

            match fs::read_to_string(&meta_path) {
                Ok(content) => match serde_json::from_str::<RunEntry>(&content) {
                    Ok(run) => {
                        tracing::debug!(run_id = %run.run_id, "loaded run from disk");
                        registry.runs.insert(run.run_id.clone(), run);
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %meta_path.display(),
                            error = %e,
                            "skipping malformed meta.json"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %meta_path.display(),
                        error = %e,
                        "failed to read meta.json"
                    );
                }
            }
        }

        tracing::info!(count = registry.runs.len(), "loaded runs from disk");
        Ok(registry)
    }

    /// Generate an 8-character hex run ID.
    ///
    /// Uses nanosecond timestamp + PID + random bytes to avoid collisions
    /// on systems with low-resolution clocks or rapid sequential calls.
    pub(crate) fn generate_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        // Add random bytes to prevent collisions on low-resolution clocks
        let random: u64 = {
            use std::collections::hash_map::RandomState;
            use std::hash::{BuildHasher, Hasher};
            let s = RandomState::new();
            let mut h = s.build_hasher();
            h.write_u128(nanos);
            h.finish()
        };
        let mut hasher = Sha256::new();
        hasher.update(nanos.to_le_bytes());
        hasher.update(pid.to_le_bytes());
        hasher.update(random.to_le_bytes());
        let hash = hasher.finalize();
        hex::encode(&hash[..4])
    }

    /// Register a new run. Creates the on-disk directory and acquires repo lock.
    pub fn register_run(
        &mut self,
        plan_path: PathBuf,
        repo_dir: PathBuf,
        session_name: Option<String>,
        config_dir: Option<PathBuf>,
    ) -> Result<String> {
        self.register_run_with_schedule(plan_path, repo_dir, session_name, config_dir, None)
    }

    /// Register a new run triggered by a schedule.
    pub fn register_run_with_schedule(
        &mut self,
        plan_path: PathBuf,
        repo_dir: PathBuf,
        session_name: Option<String>,
        config_dir: Option<PathBuf>,
        schedule_id: Option<String>,
    ) -> Result<String> {
        // Acquire per-repo lock first
        self.acquire_repo_lock(&repo_dir)?;

        let run_id = Self::generate_id();
        let session_name = session_name.unwrap_or_else(|| format!("gw-{}", &run_id));

        let entry = RunEntry {
            run_id: run_id.clone(),
            plan_path,
            repo_dir,
            session_name,
            tmux_session: None,
            status: RunStatus::Queued,
            current_phase: None,
            started_at: Utc::now(),
            finished_at: None,
            crash_restarts: 0,
            config_dir,
            error_message: None,
            restartable: true,
            claude_pid: None,
            schedule_id,
            last_recovery_at: None,
            write_epoch: 0,
        };

        // Persist to disk
        self.write_meta(&entry)?;
        self.runs.insert(run_id.clone(), entry);

        tracing::info!(run_id = %run_id, "registered new run");
        Ok(run_id)
    }

    /// Stage a status change in memory without flushing to disk.
    /// INV-19: callers drop the mutex guard after this returns, then call `flush_status`.
    /// INV-6: rejects backward transitions (Running→Queued) and terminal→non-terminal.
    pub fn stage_status_locked(
        &mut self,
        run_id: &str,
        status: RunStatus,
        phase: Option<String>,
        error: Option<String>,
    ) -> std::result::Result<StagedStatus, UpdateStatusError> {
        let entry = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| UpdateStatusError::NotFound(run_id.to_string()))?;

        // INV-6: prior-state guard — reject invalid transitions
        let from = entry.status;
        if from.is_terminal() && !status.is_terminal() {
            return Err(UpdateStatusError::InvalidTransition { from, to: status });
        }
        if from == RunStatus::Running && status == RunStatus::Queued {
            return Err(UpdateStatusError::InvalidTransition { from, to: status });
        }

        entry.status = status;
        entry.write_epoch += 1;
        if let Some(p) = phase {
            entry.current_phase = Some(p);
        }
        if let Some(e) = error {
            entry.error_message = Some(e);
        }

        // Mark finished time for terminal states
        if status.is_terminal() {
            entry.finished_at = Some(Utc::now());
            // Release repo lock on completion
            let rh = repo_hash(&entry.repo_dir);
            self.repo_locks.remove(&rh);
        }

        let staged = StagedStatus {
            entry: entry.clone(),
            epoch: entry.write_epoch,
        };

        tracing::debug!(run_id = %run_id, status = ?status, epoch = staged.epoch, "staged status update");
        Ok(staged)
    }

    /// Flush a staged status change to disk. Runs fsync via write_meta.
    /// INV-19: call this AFTER releasing the registry mutex.
    /// Epoch-guarded: skips write if on-disk epoch is newer (idempotent under retry).
    pub fn flush_status(&self, staged: &StagedStatus) -> Result<()> {
        self.write_meta(&staged.entry)
    }

    /// Update the status of a run (stage + flush in one call).
    /// Convenience wrapper — use stage_status_locked + flush_status when you need
    /// to release the mutex between mutation and fsync.
    pub fn update_status(
        &mut self,
        run_id: &str,
        status: RunStatus,
        phase: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let staged = self.stage_status_locked(run_id, status, phase, error)
            .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        self.flush_status(&staged)
    }

    /// Remove a run from registry and disk.
    // Future daemon-side cleanup API — currently only exercised by tests.
    #[allow(dead_code)]
    pub fn remove_run(&mut self, run_id: &str) -> Result<()> {
        if let Some(entry) = self.runs.remove(run_id) {
            let run_dir = gw_home().join("runs").join(run_id);
            if run_dir.exists() {
                fs::remove_dir_all(&run_dir).wrap_err_with(|| {
                    format!("failed to remove run directory: {}", run_dir.display())
                })?;
            }
            // Release repo lock if held
            let repo_hash = Self::repo_hash(&entry.repo_dir);
            self.repo_locks.remove(&repo_hash);
            tracing::info!(run_id = %run_id, "removed run");
        }
        Ok(())
    }

    /// List all runs, sorted by started_at descending (newest first).
    pub fn list_runs(&self, include_finished: bool) -> Vec<RunInfo> {
        let mut runs: Vec<_> = self
            .runs
            .values()
            .filter(|r| {
                include_finished
                    || matches!(r.status, RunStatus::Queued | RunStatus::Running)
            })
            .collect::<Vec<_>>();

        runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        runs.iter().map(|r| r.to_run_info()).collect()
    }

    /// Count runs without allocating RunInfo structs.
    pub fn count_runs(&self, include_finished: bool) -> usize {
        self.runs
            .values()
            .filter(|r| {
                include_finished
                    || matches!(r.status, RunStatus::Queued | RunStatus::Running)
            })
            .count()
    }

    /// Find a run by ID prefix (like git short SHA).
    pub fn find_by_prefix(&self, prefix: &str) -> Option<&RunEntry> {
        if prefix.is_empty() {
            return None;
        }
        let matches: Vec<_> = self
            .runs
            .values()
            .filter(|r| r.run_id.starts_with(prefix))
            .collect();

        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None // Ambiguous or not found
        }
    }

    /// Get a run by exact ID.
    pub fn get(&self, run_id: &str) -> Option<&RunEntry> {
        self.runs.get(run_id)
    }

    /// Get a mutable run by exact ID.
    pub fn get_mut(&mut self, run_id: &str) -> Option<&mut RunEntry> {
        self.runs.get_mut(run_id)
    }

    /// Acquire a repo lock for adoption. Unlike `acquire_repo_lock`, this does
    /// not fail if the lock is already held by this daemon instance (the same
    /// repo may have a terminated run that hasn't released its lock yet).
    pub fn acquire_repo_lock_for_adopt(&mut self, repo_dir: &Path) -> Result<()> {
        let repo_hash = Self::repo_hash(repo_dir);

        // Already holding — this is fine for adoption
        if self.repo_locks.contains_key(&repo_hash) {
            return Ok(());
        }

        let lock_dir = gw_home().join("repos").join(&repo_hash);
        fs::create_dir_all(&lock_dir).wrap_err("failed to create repo lock directory")?;

        let lock_path = lock_dir.join("lock");
        let lock_file = fs::File::create(&lock_path).wrap_err("failed to create repo lock file")?;

        lock_file.try_lock_exclusive().map_err(|_| {
            color_eyre::eyre::eyre!(
                "repository is locked by another daemon instance: {}",
                repo_dir.display()
            )
        })?;

        self.repo_locks.insert(repo_hash, lock_file);
        Ok(())
    }

    /// Adopt a run entry that was loaded from disk (e.g., during orphan recovery).
    ///
    /// Inserts the entry into the in-memory registry and persists it.
    /// Unlike `register_run`, this does NOT generate a new ID or acquire repo locks.
    pub fn adopt(&mut self, entry: RunEntry) {
        let run_id = entry.run_id.clone();
        if let Err(e) = self.write_meta(&entry) {
            tracing::warn!(run_id = %run_id, error = %e, "failed to persist adopted entry");
        }
        self.runs.insert(run_id, entry);
    }

    // ── Queue operations ────────────────────────────────────────────

    /// Enqueue a pending run for a repo. Returns queue position (1-indexed).
    pub fn enqueue_run(&mut self, pending: PendingRun) -> Result<usize> {
        // Global queue cap
        let total: usize = self.pending_queues.values().map(|q| q.len()).sum();
        if total >= MAX_QUEUE_SIZE {
            return Err(color_eyre::eyre::eyre!("queue full: {total} pending runs"));
        }
        let hash = repo_hash(&pending.repo_dir);

        // Duplicate detection (borrow pending_queues, then release)
        {
            let queue = self.pending_queues.entry(hash.clone()).or_default();
            if queue.iter().any(|p| p.plan_path == pending.plan_path) {
                return Err(color_eyre::eyre::eyre!(
                    "plan already queued: {}",
                    pending.plan_path.display()
                ));
            }
        }

        // Create a RunEntry for gw ps visibility
        let entry = RunEntry {
            run_id: pending.run_id.clone(),
            plan_path: pending.plan_path.clone(),
            repo_dir: pending.repo_dir.clone(),
            session_name: format!("gw-{}", &pending.run_id),
            tmux_session: None,
            status: RunStatus::Queued,
            current_phase: None,
            started_at: pending.queued_at,
            finished_at: None,
            crash_restarts: 0,
            config_dir: pending.config_dir.clone(),
            error_message: None,
            restartable: false,
            claude_pid: None,
            schedule_id: None,
            last_recovery_at: None,
            write_epoch: 0,
        };
        self.write_meta(&entry)?;
        self.runs.insert(pending.run_id.clone(), entry);

        let queue = self.pending_queues.entry(hash).or_default();
        queue.push_back(pending);
        let position = queue.len();
        self.save_queue()?;
        Ok(position)
    }

    /// Remove a queued run by ID. Returns the PendingRun if found.
    pub fn dequeue_run(&mut self, run_id: &str) -> Option<PendingRun> {
        // Collect pending entry first without holding a mutable borrow of
        // self.pending_queues while we call &mut self methods below.
        let pending = {
            let mut found: Option<PendingRun> = None;
            for queue in self.pending_queues.values_mut() {
                if let Some(pos) = queue.iter().position(|p| p.run_id == run_id) {
                    // VecDeque::remove returns Option<T>; position was just
                    // verified above so this is Some in practice.
                    found = queue.remove(pos);
                    break;
                }
            }
            found?
        };
        // Update the RunEntry to Stopped. Warn loudly on failure so the
        // disk meta.json doesn't drift from queue.json (which would leave a
        // ghost Queued entry that causes repo_lock leaks after daemon restart).
        if let Err(e) = self.update_status(
            run_id,
            RunStatus::Stopped,
            None,
            Some("removed from queue".into()),
        ) {
            tracing::warn!(
                run_id = %run_id,
                error = %e,
                "failed to flip meta.json status to Stopped after dequeue — meta may drift from queue.json"
            );
        }
        if let Err(e) = self.save_queue() {
            tracing::warn!(error = %e, "failed to persist queue after dequeue_run");
        }
        Some(pending)
    }

    /// Promote a queued run to the running spawn path.
    ///
    /// Invariant fix: before this method existed, the drain path called
    /// `register_run` which minted a fresh `run_id` and inserted a *second*
    /// RunEntry while the original Queued entry lingered in `self.runs`
    /// forever — producing a ghost row in `gw ps` (one Queued, one Running,
    /// same TMUX name). This method keeps the original `run_id` so the
    /// lifecycle is Queued → Running on a single entry.
    ///
    /// Steps:
    /// 1. Validate the entry exists and is still `Queued`.
    /// 2. Acquire the per-repo lock — `enqueue_run` intentionally does not
    ///    hold one, so acquisition happens here at promotion time.
    /// 3. Reset `started_at` to now so `uptime_secs` reflects spawn time
    ///    rather than time-spent-in-queue (matches the fresh-run path).
    /// 4. Set `restartable = true` so heartbeat crash recovery treats
    ///    drained-then-running entries like fresh registrations
    ///    (`enqueue_run` sets it `false` because a never-spawned run has
    ///    nothing to recover to).
    ///
    /// Status stays `Queued` on success — the subsequent tmux spawn in the
    /// executor flips it to `Running` via `update_status`, preserving the
    /// same two-step state machine used by `register_run` + `update_status`.
    pub fn promote_queued(&mut self, run_id: &str) -> Result<()> {
        let repo_dir = {
            let entry = self
                .runs
                .get(run_id)
                .ok_or_else(|| color_eyre::eyre::eyre!("run not found: {run_id}"))?;
            if entry.status != RunStatus::Queued {
                return Err(color_eyre::eyre::eyre!(
                    "run {run_id} is not queued (status: {:?})",
                    entry.status
                ));
            }
            entry.repo_dir.clone()
        };

        let rh = repo_hash(&repo_dir);
        self.acquire_repo_lock(&repo_dir)?;
        let guard = RepoLockGuard::new(rh);

        let entry = self
            .runs
            .get_mut(run_id)
            .expect("entry existence validated above under the same &mut self borrow");
        entry.started_at = Utc::now();
        entry.restartable = true;
        let entry_clone = entry.clone();

        match self.write_meta(&entry_clone) {
            Ok(()) => {
                let _ = guard.commit();
                tracing::info!(run_id = %run_id, "promoted queued run — repo lock acquired");
                Ok(())
            }
            Err(e) => {
                guard.release_from(&mut self.repo_locks);
                Err(e)
            }
        }
    }

    /// Pop the next pending run for a repo. Returns None if empty or circuit breaker tripped.
    pub fn drain_next(&mut self, repo_dir: &Path) -> Option<PendingRun> {
        let hash = repo_hash(repo_dir);
        if self
            .consecutive_failures
            .get(&hash)
            .copied()
            .unwrap_or(0)
            >= MAX_CONSECUTIVE_FAILURES
        {
            return None;
        }
        let result = self.pending_queues.get_mut(&hash).and_then(|q| q.pop_front());
        if result.is_some() {
            if let Err(e) = self.save_queue() {
                tracing::warn!(error = %e, "failed to persist queue after drain_next");
            }
        }
        result
    }

    /// Record a queue spawn failure for the circuit breaker.
    pub fn record_queue_failure(&mut self, repo_dir: &Path) {
        let hash = repo_hash(repo_dir);
        let count = self.consecutive_failures.entry(hash.clone()).or_insert(0);
        *count += 1;
        if *count >= MAX_CONSECUTIVE_FAILURES {
            tracing::warn!(repo_hash = %hash, "circuit breaker tripped — draining remaining queue");
            // Collect run IDs first to avoid borrowing self while iterating
            let run_ids: Vec<String> = self
                .pending_queues
                .get_mut(&hash)
                .map(|queue| queue.drain(..).map(|p| p.run_id).collect())
                .unwrap_or_default();
            for run_id in &run_ids {
                let _ = self.update_status(
                    run_id,
                    RunStatus::Stopped,
                    None,
                    Some("circuit breaker tripped".into()),
                );
            }
            self.consecutive_failures.remove(&hash);
        }
        if let Err(e) = self.save_queue() {
            tracing::warn!(error = %e, "failed to persist queue after record_queue_failure");
        }
    }

    /// Record a successful queue spawn, resetting the circuit breaker.
    pub fn record_queue_success(&mut self, repo_dir: &Path) {
        let hash = repo_hash(repo_dir);
        self.consecutive_failures.remove(&hash);
        if let Err(e) = self.save_queue() {
            tracing::warn!(error = %e, "failed to persist queue after record_queue_success");
        }
    }

    /// Check whether a repo lock is currently held.
    pub fn has_repo_lock(&self, repo_dir: &Path) -> bool {
        let hash = repo_hash(repo_dir);
        self.repo_locks.contains_key(&hash)
    }

    /// Total number of pending runs across all repos.
    pub fn total_pending_count(&self) -> usize {
        self.pending_queues.values().map(|q| q.len()).sum()
    }

    /// Count runs currently in `Running` status.
    pub fn count_running(&self) -> usize {
        self.runs
            .values()
            .filter(|r| r.status == RunStatus::Running)
            .count()
    }

    /// Check whether a run ID is present in any pending queue.
    pub fn is_in_pending_queue(&self, run_id: &str) -> bool {
        self.pending_queues
            .values()
            .any(|q| q.iter().any(|p| p.run_id == run_id))
    }

    /// Return references to all pending entries across all repo queues.
    pub fn pending_entries(&self) -> Vec<&PendingRun> {
        self.pending_queues
            .values()
            .flat_map(|q| q.iter())
            .collect()
    }

    /// Remove a pending entry by run ID from whichever queue contains it.
    pub fn remove_pending(&mut self, run_id: &str) {
        for queue in self.pending_queues.values_mut() {
            queue.retain(|p| p.run_id != run_id);
        }
    }

    /// Pop the next pending run from any repo queue whose circuit breaker
    /// has not tripped. Used to fill global capacity when the completing
    /// repo's own queue is empty.
    pub fn drain_any_ready(&mut self) -> Option<PendingRun> {
        let ready_hash = self
            .pending_queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .filter(|(hash, _)| {
                self.consecutive_failures
                    .get(*hash)
                    .copied()
                    .unwrap_or(0)
                    < MAX_CONSECUTIVE_FAILURES
            })
            .map(|(hash, _)| hash.clone())
            .next()?;
        let result = self.pending_queues.get_mut(&ready_hash)?.pop_front();
        if result.is_some() {
            if let Err(e) = self.save_queue() {
                tracing::warn!(error = %e, "failed to persist queue after drain_any_ready");
            }
        }
        result
    }

    /// List queued runs, optionally filtered by repo directory.
    /// Returns `QueuedRunInfo` entries with global position numbering.
    pub fn list_queue(
        &self,
        repo_filter: Option<&Path>,
    ) -> Vec<crate::daemon::protocol::QueuedRunInfo> {
        let mut entries = Vec::new();
        let mut position = 1usize;

        for queue in self.pending_queues.values() {
            for pending in queue {
                if let Some(filter) = repo_filter {
                    if pending.repo_dir != filter {
                        let canonical = filter.canonicalize().unwrap_or_else(|_| filter.to_path_buf());
                        if pending.repo_dir != canonical {
                            position += 1;
                            continue;
                        }
                    }
                }
                entries.push(crate::daemon::protocol::QueuedRunInfo {
                    run_id: pending.run_id.clone(),
                    plan_path: pending.plan_path.clone(),
                    repo_dir: pending.repo_dir.clone(),
                    position,
                    queued_at: pending.queued_at.to_rfc3339(),
                    session_name: pending.session_name.clone(),
                });
                position += 1;
            }
        }

        entries
    }

    /// Clear queued runs. If `repo_filter` is `Some`, only clear runs for
    /// that repo directory; otherwise clear all queues. Returns the count
    /// of removed entries.
    pub fn clear_queue(&mut self, repo_filter: Option<&Path>) -> usize {
        let mut removed = 0usize;

        match repo_filter {
            Some(filter) => {
                let canonical = filter.canonicalize().unwrap_or_else(|_| filter.to_path_buf());
                for queue in self.pending_queues.values_mut() {
                    let before = queue.len();
                    queue.retain(|p| p.repo_dir != canonical && p.repo_dir != filter);
                    removed += before - queue.len();
                }
            }
            None => {
                for queue in self.pending_queues.values_mut() {
                    removed += queue.len();
                    queue.clear();
                }
            }
        }

        removed
    }

    /// Find a pending run by ID prefix across all queues.
    /// Returns `(full_run_id, repo_hash)` if exactly one match is found.
    pub fn find_pending_by_prefix(&self, prefix: &str) -> Option<(String, String)> {
        let mut matches = Vec::new();
        for (hash, queue) in &self.pending_queues {
            for pending in queue {
                if pending.run_id.starts_with(prefix) {
                    matches.push((pending.run_id.clone(), hash.clone()));
                }
            }
        }
        if matches.len() == 1 {
            Some(matches.into_iter().next().unwrap())
        } else {
            None
        }
    }

    // ── Queue persistence ──────────────────────────────────────────

    /// Persist pending queues and circuit-breaker state to `~/.gw/queue.json`.
    ///
    /// Uses the same atomic write pattern as [`write_meta`]: write to a
    /// sibling tempfile, fsync, then rename into place.
    pub fn save_queue(&self) -> Result<()> {
        let snapshot = QueueSnapshot {
            queues: self
                .pending_queues
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            consecutive_failures: self.consecutive_failures.clone(),
        };
        let json = serde_json::to_string_pretty(&snapshot)
            .wrap_err("failed to serialize queue snapshot")?;

        let queue_path = gw_home().join("queue.json");
        let tmp_path = gw_home().join("queue.json.tmp");
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp_path).wrap_err_with(|| {
                format!("failed to create queue tmpfile: {}", tmp_path.display())
            })?;
            f.write_all(json.as_bytes())
                .wrap_err("failed to write queue.json")?;
            f.sync_all().wrap_err("failed to fsync queue.json")?;
        }
        fs::rename(&tmp_path, &queue_path)
            .wrap_err("failed to atomically rename queue.json")?;
        Ok(())
    }

    /// Load queue snapshot from `~/.gw/queue.json`.
    ///
    /// Returns an empty default on missing or corrupt files so the daemon
    /// always starts cleanly.
    pub fn load_queue(home: &Path) -> Result<QueueSnapshot> {
        let path = home.join("queue.json");
        match fs::read_to_string(&path) {
            Ok(content) => Ok(serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "corrupt queue.json — using empty defaults");
                QueueSnapshot::default()
            })),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, "failed to read queue.json — using empty defaults");
                }
                Ok(QueueSnapshot::default())
            }
        }
    }

    /// Populate in-memory pending queues and circuit-breaker state from a
    /// previously loaded [`QueueSnapshot`].
    pub fn restore_queue(&mut self, snapshot: QueueSnapshot) {
        for (hash, entries) in snapshot.queues {
            self.pending_queues
                .insert(hash, VecDeque::from(entries));
        }
        self.consecutive_failures = snapshot.consecutive_failures;
    }

    // ── Internal helpers ────────────────────────────────────────────

    /// Compute SHA-256 hash of a repo path for the lock filename.
    ///
    /// Delegates to the module-level [`repo_hash`] function so that the
    /// foreground collision guard at `commands::run::check_daemon_repo_lock`
    /// and the daemon's lock acquisition share exactly one definition.
    fn repo_hash(repo_dir: &Path) -> String {
        repo_hash(repo_dir)
    }

    /// Acquire a per-repo advisory lock.
    fn acquire_repo_lock(&mut self, repo_dir: &Path) -> Result<()> {
        let repo_hash = Self::repo_hash(repo_dir);

        // Already holding this lock
        if self.repo_locks.contains_key(&repo_hash) {
            return Err(color_eyre::eyre::eyre!(
                "repository already has an active run: {}",
                repo_dir.display()
            ));
        }

        let lock_dir = gw_home().join("repos").join(&repo_hash);
        fs::create_dir_all(&lock_dir).wrap_err("failed to create repo lock directory")?;

        let lock_path = lock_dir.join("lock");
        let lock_file = fs::File::create(&lock_path).wrap_err("failed to create repo lock file")?;

        lock_file.try_lock_exclusive().map_err(|_| {
            color_eyre::eyre::eyre!(
                "repository is locked by another daemon instance: {}",
                repo_dir.display()
            )
        })?;

        self.repo_locks.insert(repo_hash, lock_file);
        Ok(())
    }

    /// Atomically write run metadata to disk (write tmp + rename).
    fn write_meta(&self, entry: &RunEntry) -> Result<()> {
        let run_dir = gw_home().join("runs").join(&entry.run_id);
        fs::create_dir_all(&run_dir)
            .wrap_err_with(|| format!("failed to create run dir: {}", run_dir.display()))?;

        let meta_path = run_dir.join("meta.json");
        let tmp_path = run_dir.join("meta.json.tmp");

        let json = serde_json::to_string(entry).wrap_err("failed to serialize run entry")?;

        // Open → write → fsync → rename for crash safety.
        // Using a single fd avoids the TOCTOU of write-then-reopen.
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp_path)
                .wrap_err_with(|| format!("failed to create tmp meta: {}", tmp_path.display()))?;
            f.write_all(json.as_bytes())
                .wrap_err("failed to write meta.json")?;
            f.sync_all().wrap_err("failed to fsync meta.json")?;
        }

        fs::rename(&tmp_path, &meta_path).wrap_err("failed to atomically rename meta.json")?;

        Ok(())
    }
}

impl Default for RunRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn with_temp_gw_home(f: impl FnOnce(&TempDir)) {
        // Shared across `daemon::*` test modules — see
        // `daemon::state::gw_home_test_mutex` for why a common lock is
        // required. Recover from poison: a panicking test already fails;
        // propagating lock poison would cascade to unrelated tests.
        let _guard = crate::daemon::state::gw_home_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        crate::daemon::state::ensure_gw_home().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&tmp)));
        unsafe { std::env::remove_var("GW_HOME") };
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn register_and_list() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let id = reg
                .register_run(
                    PathBuf::from("plans/test.md"),
                    PathBuf::from("/tmp/repo"),
                    Some("test-session".into()),
                    None,
                )
                .unwrap();

            assert_eq!(id.len(), 8);

            let runs = reg.list_runs(true);
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].run_id, id);
            assert_eq!(runs[0].status, RunStatus::Queued);
        });
    }

    #[test]
    fn update_status_persists() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let id = reg
                .register_run(
                    PathBuf::from("plans/a.md"),
                    PathBuf::from("/tmp/repo-a"),
                    None,
                    None,
                )
                .unwrap();

            reg.update_status(&id, RunStatus::Running, Some("phase_1".into()), None)
                .unwrap();

            let entry = reg.get(&id).unwrap();
            assert_eq!(entry.status, RunStatus::Running);
            assert_eq!(entry.current_phase.as_deref(), Some("phase_1"));
        });
    }

    #[test]
    fn find_by_prefix_works() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let id = reg
                .register_run(
                    PathBuf::from("plans/b.md"),
                    PathBuf::from("/tmp/repo-b"),
                    None,
                    None,
                )
                .unwrap();

            // First 4 chars should be unique enough for a single run
            let found = reg.find_by_prefix(&id[..4]);
            assert!(found.is_some());
            assert_eq!(found.unwrap().run_id, id);
        });
    }

    #[test]
    fn remove_run_cleans_up() {
        with_temp_gw_home(|tmp| {
            let mut reg = RunRegistry::new();
            let id = reg
                .register_run(
                    PathBuf::from("plans/c.md"),
                    PathBuf::from("/tmp/repo-c"),
                    None,
                    None,
                )
                .unwrap();

            let run_dir = tmp.path().join("runs").join(&id);
            assert!(run_dir.exists());

            reg.remove_run(&id).unwrap();
            assert!(!run_dir.exists());
            assert!(reg.get(&id).is_none());
        });
    }

    #[test]
    fn load_from_disk_round_trip() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let id = reg
                .register_run(
                    PathBuf::from("plans/d.md"),
                    PathBuf::from("/tmp/repo-d"),
                    Some("disk-test".into()),
                    None,
                )
                .unwrap();

            // Mark as succeeded so the repo lock is released
            reg.update_status(&id, RunStatus::Succeeded, None, None)
                .unwrap();

            // Load fresh from disk
            let loaded = RunRegistry::load_from_disk().unwrap();
            let entry = loaded.get(&id).unwrap();
            assert_eq!(entry.session_name, "disk-test");
            assert_eq!(entry.status, RunStatus::Succeeded);
        });
    }

    #[test]
    fn repo_lock_prevents_duplicate() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            reg.register_run(
                PathBuf::from("plans/e.md"),
                PathBuf::from("/tmp/repo-lock-test"),
                None,
                None,
            )
            .unwrap();

            // Second run against same repo should fail
            let result = reg.register_run(
                PathBuf::from("plans/f.md"),
                PathBuf::from("/tmp/repo-lock-test"),
                None,
                None,
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn claude_pid_backward_compat_missing_field() {
        // Simulate a pre-shard-2 meta.json that predates the claude_pid field.
        // Deserializing it must succeed and yield None (#[serde(default)]),
        // preserving the on-disk format — old daemon files remain readable.
        let legacy_json = serde_json::json!({
            "run_id": "abcd1234",
            "plan_path": "plans/legacy.md",
            "repo_dir": "/tmp/repo-legacy",
            "session_name": "gw-abcd1234",
            "tmux_session": null,
            "status": "Running",
            "current_phase": null,
            "started_at": "2026-04-11T00:00:00Z",
            "finished_at": null,
            "crash_restarts": 0,
            "config_dir": null,
            "error_message": null,
            "restartable": true
            // NOTE: no claude_pid field
        });
        let entry: RunEntry = serde_json::from_value(legacy_json)
            .expect("legacy meta.json must deserialize with no claude_pid");
        assert_eq!(entry.claude_pid, None);
    }

    #[test]
    fn claude_pid_serializes_and_deserializes() {
        // New entries with claude_pid set round-trip through JSON correctly.
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let id = reg
                .register_run(
                    PathBuf::from("plans/pid-test.md"),
                    PathBuf::from("/tmp/repo-pid"),
                    None,
                    None,
                )
                .unwrap();

            // Mutate the PID as the executor would
            if let Some(entry) = reg.get_mut(&id) {
                entry.claude_pid = Some(98765);
            }
            // Force a disk write via update_status (it re-serializes)
            reg.update_status(&id, RunStatus::Running, Some("testing".into()), None)
                .unwrap();
            // Release lock so load_from_disk doesn't collide
            reg.update_status(&id, RunStatus::Succeeded, None, None)
                .unwrap();

            let loaded = RunRegistry::load_from_disk().unwrap();
            let entry = loaded.get(&id).unwrap();
            assert_eq!(entry.claude_pid, Some(98765));
        });
    }

    #[test]
    fn enqueue_and_drain_order() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/test-repo");
            // Enqueue two runs
            let p1 = PendingRun {
                run_id: "q1".into(),
                plan_path: PathBuf::from("plan1.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            let p2 = PendingRun {
                run_id: "q2".into(),
                plan_path: PathBuf::from("plan2.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            assert_eq!(reg.enqueue_run(p1).unwrap(), 1);
            assert_eq!(reg.enqueue_run(p2).unwrap(), 2);
            // Drain returns FIFO order
            let d1 = reg.drain_next(&repo).unwrap();
            assert_eq!(d1.run_id, "q1");
            let d2 = reg.drain_next(&repo).unwrap();
            assert_eq!(d2.run_id, "q2");
            assert!(reg.drain_next(&repo).is_none());
        });
    }

    #[test]
    fn promote_queued_flips_to_running_without_ghost() {
        // Regression test for `gw ps` ghost rows: before `promote_queued`
        // existed, the drain path minted a fresh run_id and left the
        // queued RunEntry orphaned. This test asserts the invariant:
        // after promotion + status update, there is EXACTLY ONE entry
        // for the run_id — not two.
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/test-repo-promote");
            let run_id = "prom1".to_string();
            let pending = PendingRun {
                run_id: run_id.clone(),
                plan_path: PathBuf::from("plan.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            reg.enqueue_run(pending).unwrap();

            // Before promotion: one Queued entry, not holding the repo lock.
            assert_eq!(reg.list_runs(true).len(), 1);
            assert!(!reg.has_repo_lock(&repo));

            // Simulate the drain path: pop from pending_queues then promote.
            let popped = reg.drain_next(&repo).unwrap();
            assert_eq!(popped.run_id, run_id);

            reg.promote_queued(&run_id).unwrap();

            // Entry count must remain 1 — no ghost from a second register_run.
            let all = reg.list_runs(true);
            assert_eq!(all.len(), 1, "promote must not create a second entry");
            assert_eq!(all[0].run_id, run_id);
            assert_eq!(all[0].status, RunStatus::Queued);

            // Lock must now be held (enqueue_run deliberately does not hold one).
            assert!(reg.has_repo_lock(&repo));

            // restartable must flip to true (enqueue sets false) so heartbeat
            // crash recovery treats this like a fresh-registered run.
            assert!(reg.get(&run_id).unwrap().restartable);

            // The subsequent status update is what actually flips to Running —
            // this mirrors what `spawn_after_register` does on the real path.
            reg.update_status(&run_id, RunStatus::Running, Some("starting".into()), None)
                .unwrap();
            assert_eq!(reg.get(&run_id).unwrap().status, RunStatus::Running);
            // Still exactly one entry.
            assert_eq!(reg.list_runs(true).len(), 1);
        });
    }

    #[test]
    fn promote_queued_rejects_non_queued_status() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            // A freshly registered run is Queued, then flipped to Running.
            let id = reg
                .register_run(
                    PathBuf::from("plans/p.md"),
                    PathBuf::from("/tmp/promote-non-queued"),
                    None,
                    None,
                )
                .unwrap();
            reg.update_status(&id, RunStatus::Running, None, None)
                .unwrap();

            let err = reg.promote_queued(&id).unwrap_err();
            assert!(
                err.to_string().contains("not queued"),
                "expected 'not queued' in error, got: {err}"
            );
        });
    }

    #[test]
    fn promote_queued_rejects_unknown_run() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let err = reg.promote_queued("does-not-exist").unwrap_err();
            assert!(err.to_string().contains("run not found"));
        });
    }

    #[test]
    fn circuit_breaker_trips_and_resets() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/test-repo-cb");
            // Enqueue a run so drain_next has something
            let p = PendingRun {
                run_id: "cb1".into(),
                plan_path: PathBuf::from("plan.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            let _ = reg.enqueue_run(p);
            // Record 3 failures — circuit breaker trips, queue is drained
            reg.record_queue_failure(&repo);
            reg.record_queue_failure(&repo);
            reg.record_queue_failure(&repo);
            // Queue should be empty and drain returns None
            assert!(reg.drain_next(&repo).is_none());
            // After trip, counter is reset — new enqueue should work
            let p2 = PendingRun {
                run_id: "cb2".into(),
                plan_path: PathBuf::from("plan2.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            assert_eq!(reg.enqueue_run(p2).unwrap(), 1);
            // One failure should not trip
            reg.record_queue_failure(&repo);
            assert!(reg.drain_next(&repo).is_some());
        });
    }

    #[test]
    fn duplicate_plan_rejected() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/test-repo-dup");
            let p1 = PendingRun {
                run_id: "d1".into(),
                plan_path: PathBuf::from("same.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            let p2 = PendingRun {
                run_id: "d2".into(),
                plan_path: PathBuf::from("same.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            assert!(reg.enqueue_run(p1).is_ok());
            assert!(reg.enqueue_run(p2).is_err()); // duplicate
        });
    }

    // ── A8: Queue Persistence Tests ──────────────────────────────────

    #[test]
    fn save_queue_load_queue_round_trip() {
        with_temp_gw_home(|tmp| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/rt-repo");

            let p1 = PendingRun {
                run_id: "rt01".into(),
                plan_path: PathBuf::from("plans/alpha.md"),
                repo_dir: repo.clone(),
                session_name: Some("sess-alpha".into()),
                config_dir: None,
                verbose: 1,
                queued_at: Utc::now(),
            };
            reg.enqueue_run(p1).unwrap();

            // save_queue is called internally by enqueue_run, but call explicitly
            reg.save_queue().unwrap();

            // Load from disk
            let snapshot = RunRegistry::load_queue(tmp.path()).unwrap();
            assert_eq!(snapshot.queues.len(), 1);
            let queue_entries: Vec<_> = snapshot.queues.values().next().unwrap().clone();
            assert_eq!(queue_entries.len(), 1);
            assert_eq!(queue_entries[0].run_id, "rt01");
            assert_eq!(queue_entries[0].plan_path, PathBuf::from("plans/alpha.md"));
            assert_eq!(queue_entries[0].session_name, Some("sess-alpha".into()));
            assert_eq!(queue_entries[0].verbose, 1);
        });
    }

    #[test]
    fn queue_survives_restart() {
        with_temp_gw_home(|tmp| {
            // First registry: enqueue and persist
            {
                let mut reg = RunRegistry::new();
                let repo = PathBuf::from("/tmp/survive-repo");
                let p = PendingRun {
                    run_id: "sv01".into(),
                    plan_path: PathBuf::from("plans/survive.md"),
                    repo_dir: repo,
                    session_name: None,
                    config_dir: None,
                    verbose: 0,
                    queued_at: Utc::now(),
                };
                reg.enqueue_run(p).unwrap();
                reg.save_queue().unwrap();
            }

            // Second registry: load and verify
            let mut reg2 = RunRegistry::new();
            let snapshot = RunRegistry::load_queue(tmp.path()).unwrap();
            reg2.restore_queue(snapshot);

            let pending = reg2.pending_entries();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].run_id, "sv01");
            assert_eq!(pending[0].plan_path, PathBuf::from("plans/survive.md"));
        });
    }

    #[test]
    fn stale_entry_pruning() {
        // Queue entries whose plan_path doesn't exist on disk should be
        // detected by callers. Verify that load_queue returns them (the
        // reconciler is responsible for pruning, but the data must round-trip).
        with_temp_gw_home(|tmp| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/stale-repo");

            // Use a plan_path that definitely doesn't exist
            let p = PendingRun {
                run_id: "stale1".into(),
                plan_path: PathBuf::from("/nonexistent/plan.md"),
                repo_dir: repo,
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            reg.enqueue_run(p).unwrap();
            reg.save_queue().unwrap();

            let snapshot = RunRegistry::load_queue(tmp.path()).unwrap();
            // Stale entry is loaded (callers validate plan_path existence)
            let total: usize = snapshot.queues.values().map(|v| v.len()).sum();
            assert_eq!(total, 1);

            // Verify the plan path doesn't exist — confirming it IS stale
            let entry = snapshot.queues.values().next().unwrap().first().unwrap();
            assert!(!entry.plan_path.exists());
        });
    }

    #[test]
    fn circuit_breaker_persists() {
        with_temp_gw_home(|_tmp| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/cb-persist-repo");

            // Enqueue enough runs so circuit breaker doesn't drain them all
            for i in 0..5 {
                let p = PendingRun {
                    run_id: format!("cbp{i}"),
                    plan_path: PathBuf::from(format!("plans/cb{i}.md")),
                    repo_dir: repo.clone(),
                    session_name: None,
                    config_dir: None,
                    verbose: 0,
                    queued_at: Utc::now(),
                };
                reg.enqueue_run(p).unwrap();
            }

            // Record 2 failures (below the trip threshold of 3)
            reg.record_queue_failure(&repo);
            reg.record_queue_failure(&repo);
            reg.save_queue().unwrap();

            // Load from gw_home() — the same path save_queue() writes to —
            // to avoid env var races when tests run in parallel.
            let snapshot = RunRegistry::load_queue(&crate::daemon::state::gw_home()).unwrap();
            let hash = repo_hash(&repo);
            assert_eq!(snapshot.consecutive_failures.get(&hash).copied().unwrap_or(0), 2);
        });
    }

    #[test]
    fn max_concurrent_runs_enforcement() {
        // When the global queue is full, additional enqueue attempts should fail.
        // We test the global cap (MAX_QUEUE_SIZE = 50).
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();

            // Fill queue to MAX_QUEUE_SIZE
            for i in 0..50 {
                let p = PendingRun {
                    run_id: format!("mx{i:03}"),
                    plan_path: PathBuf::from(format!("plans/mx{i:03}.md")),
                    repo_dir: PathBuf::from(format!("/tmp/repo-mx-{i}")),
                    session_name: None,
                    config_dir: None,
                    verbose: 0,
                    queued_at: Utc::now(),
                };
                reg.enqueue_run(p).unwrap();
            }

            // 51st enqueue should fail with "queue full"
            let overflow = PendingRun {
                run_id: "mx050".into(),
                plan_path: PathBuf::from("plans/overflow.md"),
                repo_dir: PathBuf::from("/tmp/repo-mx-overflow"),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            let result = reg.enqueue_run(overflow);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("queue full"),
                "expected 'queue full' error"
            );
        });
    }

    #[test]
    fn load_empty_queue_file() {
        with_temp_gw_home(|tmp| {
            // Case 1: missing queue.json returns empty snapshot (no panic)
            let snapshot = RunRegistry::load_queue(tmp.path()).unwrap();
            assert!(snapshot.queues.is_empty());
            assert!(snapshot.consecutive_failures.is_empty());

            // Case 2: empty file returns empty snapshot
            std::fs::write(tmp.path().join("queue.json"), "").unwrap();
            let snapshot2 = RunRegistry::load_queue(tmp.path()).unwrap();
            assert!(snapshot2.queues.is_empty());

            // Case 3: valid empty JSON object
            std::fs::write(tmp.path().join("queue.json"), "{}").unwrap();
            let snapshot3 = RunRegistry::load_queue(tmp.path()).unwrap();
            assert!(snapshot3.queues.is_empty());
        });
    }

    // ── B7: Queue CLI Tests (registry portion) ──────────────────────

    #[test]
    fn list_queue_with_repo_filter() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo_a = PathBuf::from("/tmp/filter-repo-a");
            let repo_b = PathBuf::from("/tmp/filter-repo-b");

            reg.enqueue_run(PendingRun {
                run_id: "flt1".into(),
                plan_path: PathBuf::from("plans/a1.md"),
                repo_dir: repo_a.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            })
            .unwrap();

            reg.enqueue_run(PendingRun {
                run_id: "flt2".into(),
                plan_path: PathBuf::from("plans/b1.md"),
                repo_dir: repo_b.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            })
            .unwrap();

            // No filter — both visible
            let all = reg.list_queue(None);
            assert_eq!(all.len(), 2);

            // Filter by repo_a
            let filtered_a = reg.list_queue(Some(&repo_a));
            assert_eq!(filtered_a.len(), 1);
            assert_eq!(filtered_a[0].run_id, "flt1");

            // Filter by repo_b
            let filtered_b = reg.list_queue(Some(&repo_b));
            assert_eq!(filtered_b.len(), 1);
            assert_eq!(filtered_b[0].run_id, "flt2");
        });
    }

    #[test]
    fn clear_queue_removes_and_persists() {
        with_temp_gw_home(|tmp| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/clear-repo");

            for i in 0..3 {
                reg.enqueue_run(PendingRun {
                    run_id: format!("clr{i}"),
                    plan_path: PathBuf::from(format!("plans/clr{i}.md")),
                    repo_dir: repo.clone(),
                    session_name: None,
                    config_dir: None,
                    verbose: 0,
                    queued_at: Utc::now(),
                })
                .unwrap();
            }

            assert_eq!(reg.total_pending_count(), 3);

            let removed = reg.clear_queue(None);
            assert_eq!(removed, 3);
            assert_eq!(reg.total_pending_count(), 0);

            // Verify persisted state is also empty
            reg.save_queue().unwrap();
            let snapshot = RunRegistry::load_queue(tmp.path()).unwrap();
            let total: usize = snapshot.queues.values().map(|v| v.len()).sum();
            assert_eq!(total, 0);
        });
    }

    #[test]
    fn find_pending_by_prefix_cases() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/prefix-repo");

            reg.enqueue_run(PendingRun {
                run_id: "abcd1234".into(),
                plan_path: PathBuf::from("plans/p1.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            })
            .unwrap();

            reg.enqueue_run(PendingRun {
                run_id: "abcd5678".into(),
                plan_path: PathBuf::from("plans/p2.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            })
            .unwrap();

            reg.enqueue_run(PendingRun {
                run_id: "efgh9999".into(),
                plan_path: PathBuf::from("plans/p3.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            })
            .unwrap();

            // Exact match
            let exact = reg.find_pending_by_prefix("abcd1234");
            assert!(exact.is_some());
            assert_eq!(exact.unwrap().0, "abcd1234");

            // Unique prefix match
            let unique = reg.find_pending_by_prefix("efgh");
            assert!(unique.is_some());
            assert_eq!(unique.unwrap().0, "efgh9999");

            // Ambiguous prefix — two runs start with "abcd"
            let ambiguous = reg.find_pending_by_prefix("abcd");
            assert!(ambiguous.is_none());

            // No match
            let none = reg.find_pending_by_prefix("zzzz");
            assert!(none.is_none());
        });
    }

    #[test]
    fn list_filters_finished() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let id1 = reg
                .register_run(
                    PathBuf::from("p1.md"),
                    PathBuf::from("/tmp/r1"),
                    None,
                    None,
                )
                .unwrap();
            let _id2 = reg
                .register_run(
                    PathBuf::from("p2.md"),
                    PathBuf::from("/tmp/r2"),
                    None,
                    None,
                )
                .unwrap();

            reg.update_status(&id1, RunStatus::Succeeded, None, None)
                .unwrap();

            // Without include_finished, only active runs
            let active = reg.list_runs(false);
            assert_eq!(active.len(), 1);

            // With include_finished, all runs
            let all = reg.list_runs(true);
            assert_eq!(all.len(), 2);
        });
    }

    // ── RepoLockGuard unit tests ───────────────────────────────────

    #[test]
    fn repo_lock_guard_releases_on_drop() {
        let mut map = HashMap::new();
        map.insert("abc".to_string(), tempfile::tempfile().unwrap());
        let guard = RepoLockGuard::new("abc".to_string());
        guard.release_from(&mut map);
        assert!(!map.contains_key("abc"));
    }

    #[test]
    fn repo_lock_guard_retains_on_commit() {
        let mut map = HashMap::new();
        map.insert("abc".to_string(), tempfile::tempfile().unwrap());
        let guard = RepoLockGuard::new("abc".to_string());
        guard.commit();
        // guard consumed, map untouched
        assert!(map.contains_key("abc"));
    }
}
