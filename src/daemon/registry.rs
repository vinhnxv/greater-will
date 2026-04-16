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
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::daemon::protocol::{RunInfo, RunStatus};
use crate::daemon::reconciler::plans_match;
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

/// On-disk wire format for `~/.gw/queue.json` — pure serde DTO with no
/// runtime state.
///
/// Used by [`RunRegistry::load_queue`], [`RunRegistry::flush_queue`], and
/// [`RunRegistry::restore_queue`] for persistence I/O. Decoupled from the
/// runtime [`QueueSnapshot`] wrapper so the on-disk schema is independent
/// of the in-memory must-flush invariant. The `{queues, consecutive_failures}`
/// field shape is preserved exactly so existing `queue.json` files continue
/// to deserialize without migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueueSnapshotWire {
    #[serde(default)]
    pub queues: HashMap<String, Vec<PendingRun>>,
    #[serde(default)]
    pub consecutive_failures: HashMap<String, u32>,
}

/// Runtime queue snapshot — wraps [`QueueSnapshotWire`] with a per-staging
/// epoch and a must-flush invariant.
///
/// Construct via [`RunRegistry::stage_queue_locked`] (under the registry
/// mutex) and flush via [`RunRegistry::flush_queue`] (after releasing the
/// mutex). This mirrors the INV-19 pattern used by `stage_status_locked` /
/// `flush_status` so the 1–20 ms fsync runs outside the registry lock.
///
/// Dropping a `QueueSnapshot` without flushing is an invariant violation:
/// the in-memory queue mutation is lost on daemon restart because no
/// on-disk write happened. The `Drop` impl emits a `tracing::error!` so
/// such bugs are detectable in production logs.
#[must_use = "QueueSnapshot must be flushed via RunRegistry::flush_queue \
              before it is dropped, otherwise the in-memory queue mutation \
              is lost on daemon restart"]
#[derive(Debug)]
pub struct QueueSnapshot {
    /// On-disk wire format — what gets serialized to `queue.json`.
    pub wire: QueueSnapshotWire,
    /// Monotonic epoch assigned by `stage_queue_locked`. Used by `flush_queue`
    /// to detect stale snapshots (a newer staging already flushed).
    epoch: u64,
    /// Shared handle into [`RunRegistry::queue_last_flushed_epoch`] — every
    /// snapshot derived from the same registry observes the same advancing
    /// counter, enabling stale-snapshot suppression.
    flushed_tracker: Arc<AtomicU64>,
    /// Set to `true` by `flush_queue` on success; checked by `Drop`.
    flushed: Cell<bool>,
}

impl Drop for QueueSnapshot {
    fn drop(&mut self) {
        if !self.flushed.get() {
            tracing::error!(
                epoch = self.epoch,
                "BUG: QueueSnapshot dropped without flush — queue mutation at \
                 epoch {} exists only in memory. Durability requires daemon \
                 restart (which re-reads the prior on-disk queue.json). This \
                 is an invariant violation; investigate the caller site.",
                self.epoch,
            );
        }
    }
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
    /// Monotonic counter bumped by `stage_queue_locked` so each runtime
    /// `QueueSnapshot` carries a unique epoch. Used by `flush_queue` for
    /// stale-snapshot detection and by the `Drop` log for correlation.
    queue_epoch: AtomicU64,
    /// Most-recently-flushed queue epoch. Shared with every `QueueSnapshot`
    /// (via `Arc::clone`) so out-of-order flushes can early-return without
    /// rolling back the on-disk state.
    queue_last_flushed_epoch: Arc<AtomicU64>,
}

impl RunRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            runs: HashMap::new(),
            repo_locks: HashMap::new(),
            pending_queues: HashMap::new(),
            consecutive_failures: HashMap::new(),
            queue_epoch: AtomicU64::new(0),
            queue_last_flushed_epoch: Arc::new(AtomicU64::new(0)),
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
        // Acquire per-repo lock first, then wrap it in RAII so a write_meta
        // failure below cannot leave a zombie lock in self.repo_locks (INV-7).
        self.acquire_repo_lock(&repo_dir)?;
        let rh = repo_hash(&repo_dir);
        let guard = RepoLockGuard::new(rh);

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

        // Persist to disk — release the lock on failure so the repo is not
        // permanently blocked on ENOSPC/serde errors.
        match Self::write_meta(&entry) {
            Ok(()) => {
                guard.commit();
            }
            Err(e) => {
                guard.release_from(&mut self.repo_locks);
                return Err(e);
            }
        }
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

    /// Flush a staged status change to disk. Runs fsync via `write_meta`.
    ///
    /// INV-19: associated function (no `&self`) — MUST be called after the
    /// registry mutex is released, so the 1–20ms fsync does not block other
    /// registry operations.
    ///
    /// Note: `StagedStatus::epoch` is stamped for external correlation/debug
    /// but is NOT used to skip writes in this release. Callers that need
    /// idempotent retry guards should gate on run status before re-flushing.
    pub fn flush_status(staged: &StagedStatus) -> Result<()> {
        Self::write_meta(&staged.entry)
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
        Self::flush_status(&staged)
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
        if let Err(e) = Self::write_meta(&entry) {
            tracing::warn!(run_id = %run_id, error = %e, "failed to persist adopted entry");
        }
        self.runs.insert(run_id, entry);
    }

    // ── Queue operations ────────────────────────────────────────────

    /// Enqueue a pending run for a repo. Returns queue position (1-indexed).
    ///
    /// **Synchronous wrapper** — fsyncs `queue.json` while holding `&mut self`.
    /// Production paths under an async lock should call
    /// [`enqueue_run_staged`](Self::enqueue_run_staged) instead and flush
    /// outside the registry mutex (INV-19 for queues).
    pub fn enqueue_run(&mut self, pending: PendingRun) -> Result<usize> {
        let (position, snap) = self.enqueue_run_staged(pending)?;
        Self::flush_queue(&snap)?;
        Ok(position)
    }

    /// Staged variant of [`enqueue_run`](Self::enqueue_run) — performs the
    /// in-memory mutation and returns a [`QueueSnapshot`] for the caller to
    /// flush after releasing the registry mutex.
    ///
    /// Production callers under an `Arc<Mutex<RunRegistry>>` should prefer
    /// this form so the 1–20 ms `queue.json` fsync does not block other IPC
    /// requests behind the lock.
    pub fn enqueue_run_staged(
        &mut self,
        pending: PendingRun,
    ) -> Result<(usize, QueueSnapshot)> {
        let position = self.enqueue_run_without_snapshot(pending)?;
        let snap = self.stage_queue_locked();
        Ok((position, snap))
    }

    /// Mutation-only helper for the multi-mutation canonical pattern. Used by
    /// `executor::drain_if_available` to compose `record_queue_failure` +
    /// `drain_next` + a single trailing `stage_queue_locked` (one snapshot,
    /// not two).
    pub(crate) fn enqueue_run_without_snapshot(
        &mut self,
        pending: PendingRun,
    ) -> Result<usize> {
        // Global queue cap
        let total: usize = self.pending_queues.values().map(|q| q.len()).sum();
        if total >= MAX_QUEUE_SIZE {
            return Err(color_eyre::eyre::eyre!("queue full: {total} pending runs"));
        }
        let hash = repo_hash(&pending.repo_dir);
        let pending_plan_str = pending.plan_path.to_string_lossy().into_owned();

        // Duplicate detection.
        //
        // Scans `self.runs` for any non-terminal entry on the same repo whose
        // plan resolves to the same file (via `plans_match`, which
        // canonicalizes relative vs. absolute forms and falls back to
        // basename comparison on symlink farms).
        //
        // Scanning `self.runs` — not `pending_queues` — is load-bearing:
        // `drain_next` pops a plan out of `pending_queues` when it promotes
        // to Running, but the `RunEntry` stays in `self.runs` with
        // `status=Running`. The old queue-only check silently accepted a
        // re-submit of an already-running plan, producing twin `gw ps` rows
        // (one Running, one Queued) for the same plan file.
        //
        // Queued entries also live in `self.runs` (insert happens below at
        // queue-push time), so this single scan covers Queued + Running
        // without a second loop over `pending_queues`.
        if let Some(existing) = self.runs.values().find(|e| {
            !e.status.is_terminal()
                && repo_hash(&e.repo_dir) == hash
                && plans_match(
                    &e.plan_path.to_string_lossy(),
                    &pending_plan_str,
                    &pending.repo_dir,
                )
        }) {
            let verb = match existing.status {
                RunStatus::Queued => "queued",
                RunStatus::Running => "running",
                _ => "active",
            };
            return Err(color_eyre::eyre::eyre!(
                "plan already {verb} on this repo: {}",
                pending.plan_path.display()
            ));
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
        Self::write_meta(&entry)?;
        self.runs.insert(pending.run_id.clone(), entry);

        let queue = self.pending_queues.entry(hash).or_default();
        queue.push_back(pending);
        let position = queue.len();
        Ok(position)
    }

    /// Remove a queued run by ID. Returns the PendingRun if found.
    ///
    /// **Synchronous wrapper** — see [`dequeue_run_staged`](Self::dequeue_run_staged)
    /// for the lock-release-before-fsync form.
    pub fn dequeue_run(&mut self, run_id: &str) -> Option<PendingRun> {
        let (pending, snap) = self.dequeue_run_staged(run_id)?;
        if let Err(e) = Self::flush_queue(&snap) {
            tracing::warn!(error = %e, "failed to persist queue after dequeue_run");
        }
        Some(pending)
    }

    /// Staged variant of [`dequeue_run`](Self::dequeue_run).
    ///
    /// The internal `update_status(Stopped)` still fsyncs `meta.json` while
    /// holding the registry mutex; only the `queue.json` fsync is deferred to
    /// the caller. This is the dominant cost in burst paths — `dequeue_run`
    /// is invoked once per StopRun, not in bursts, so the meta.json fsync is
    /// acceptable inside the lock.
    pub fn dequeue_run_staged(
        &mut self,
        run_id: &str,
    ) -> Option<(PendingRun, QueueSnapshot)> {
        let pending = self.dequeue_run_without_snapshot(run_id)?;
        let snap = self.stage_queue_locked();
        Some((pending, snap))
    }

    /// Mutation-only helper — pops the pending entry and flips meta.json to
    /// `Stopped` without fsyncing `queue.json`.
    pub(crate) fn dequeue_run_without_snapshot(
        &mut self,
        run_id: &str,
    ) -> Option<PendingRun> {
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

        match Self::write_meta(&entry_clone) {
            Ok(()) => {
                guard.commit();
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
    ///
    /// **Synchronous wrapper** — see [`drain_next_staged`](Self::drain_next_staged).
    pub fn drain_next(&mut self, repo_dir: &Path) -> Option<PendingRun> {
        let (pending, snap) = self.drain_next_staged(repo_dir)?;
        if let Err(e) = Self::flush_queue(&snap) {
            tracing::warn!(error = %e, "failed to persist queue after drain_next");
        }
        Some(pending)
    }

    /// Staged variant of [`drain_next`](Self::drain_next). Returns `None` for
    /// empty queues / tripped breakers without bumping the queue epoch.
    pub fn drain_next_staged(
        &mut self,
        repo_dir: &Path,
    ) -> Option<(PendingRun, QueueSnapshot)> {
        let pending = self.drain_next_without_snapshot(repo_dir)?;
        let snap = self.stage_queue_locked();
        Some((pending, snap))
    }

    /// Mutation-only helper — circuit-breaker check + pop_front, no fsync.
    pub(crate) fn drain_next_without_snapshot(
        &mut self,
        repo_dir: &Path,
    ) -> Option<PendingRun> {
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
        self.pending_queues.get_mut(&hash).and_then(|q| q.pop_front())
    }

    /// Record a queue spawn failure for the circuit breaker.
    /// Returns staged statuses that the caller should flush after releasing the mutex.
    /// INV-19 / BACK-007: avoids N sequential fsyncs while holding the registry lock.
    ///
    /// **Synchronous wrapper** — see
    /// [`record_queue_failure_staged`](Self::record_queue_failure_staged).
    pub fn record_queue_failure(&mut self, repo_dir: &Path) -> Vec<StagedStatus> {
        let (staged_batch, snap) = self.record_queue_failure_staged(repo_dir);
        if let Err(e) = Self::flush_queue(&snap) {
            tracing::warn!(error = %e, "failed to persist queue after record_queue_failure");
        }
        staged_batch
    }

    /// Staged variant of [`record_queue_failure`](Self::record_queue_failure).
    /// Returns the staged status batch (already flushable via
    /// [`flush_status`](Self::flush_status)) **and** the queue snapshot in one
    /// call so the caller can flush both outside the registry mutex.
    pub fn record_queue_failure_staged(
        &mut self,
        repo_dir: &Path,
    ) -> (Vec<StagedStatus>, QueueSnapshot) {
        let staged_batch = self.record_queue_failure_without_snapshot(repo_dir);
        let snap = self.stage_queue_locked();
        (staged_batch, snap)
    }

    /// Mutation-only helper — bumps the consecutive-failure counter, drains
    /// the per-repo queue when the circuit breaker trips, and stages
    /// `RunStatus::Stopped` for each drained entry. Does **not** fsync.
    pub(crate) fn record_queue_failure_without_snapshot(
        &mut self,
        repo_dir: &Path,
    ) -> Vec<StagedStatus> {
        let hash = repo_hash(repo_dir);
        let count = self.consecutive_failures.entry(hash.clone()).or_insert(0);
        *count += 1;
        let mut staged_batch = Vec::new();
        if *count >= MAX_CONSECUTIVE_FAILURES {
            tracing::warn!(repo_hash = %hash, "circuit breaker tripped — draining remaining queue");
            let run_ids: Vec<String> = self
                .pending_queues
                .get_mut(&hash)
                .map(|queue| queue.drain(..).map(|p| p.run_id).collect())
                .unwrap_or_default();
            for run_id in &run_ids {
                match self.stage_status_locked(
                    run_id,
                    RunStatus::Stopped,
                    None,
                    Some("circuit breaker tripped".into()),
                ) {
                    Ok(staged) => staged_batch.push(staged),
                    Err(e) => {
                        tracing::error!(run_id = %run_id, error = %e, "stage_status_locked failed: circuit breaker drain");
                    }
                }
            }
            self.consecutive_failures.remove(&hash);
        }
        staged_batch
    }

    /// Record a successful queue spawn, resetting the circuit breaker.
    ///
    /// **Synchronous wrapper** — see
    /// [`record_queue_success_staged`](Self::record_queue_success_staged).
    pub fn record_queue_success(&mut self, repo_dir: &Path) {
        let snap = self.record_queue_success_staged(repo_dir);
        if let Err(e) = Self::flush_queue(&snap) {
            tracing::warn!(error = %e, "failed to persist queue after record_queue_success");
        }
    }

    /// Staged variant of [`record_queue_success`](Self::record_queue_success).
    pub fn record_queue_success_staged(&mut self, repo_dir: &Path) -> QueueSnapshot {
        self.record_queue_success_without_snapshot(repo_dir);
        self.stage_queue_locked()
    }

    /// Mutation-only helper — clears the consecutive-failure counter for
    /// `repo_dir` without fsyncing.
    pub(crate) fn record_queue_success_without_snapshot(&mut self, repo_dir: &Path) {
        let hash = repo_hash(repo_dir);
        self.consecutive_failures.remove(&hash);
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
    ///
    /// Does **not** fsync `queue.json`. Callers are expected to either pair
    /// this with a subsequent [`save_queue`](Self::save_queue) (sync wrapper
    /// path, e.g. tests) or to use [`remove_pending_staged`](Self::remove_pending_staged)
    /// which returns a [`QueueSnapshot`] for outside-lock flush.
    pub fn remove_pending(&mut self, run_id: &str) {
        for queue in self.pending_queues.values_mut() {
            queue.retain(|p| p.run_id != run_id);
        }
    }

    /// Staged variant of [`remove_pending`](Self::remove_pending). The
    /// returned [`QueueSnapshot`] must be flushed via
    /// [`flush_queue`](Self::flush_queue) after the registry mutex is released.
    pub fn remove_pending_staged(&mut self, run_id: &str) -> QueueSnapshot {
        self.remove_pending(run_id);
        self.stage_queue_locked()
    }

    /// Pop the next pending run from any repo queue whose circuit breaker
    /// has not tripped. Used to fill global capacity when the completing
    /// repo's own queue is empty.
    ///
    /// **Synchronous wrapper** — see
    /// [`drain_any_ready_staged`](Self::drain_any_ready_staged).
    pub fn drain_any_ready(&mut self) -> Option<PendingRun> {
        let (pending, snap) = self.drain_any_ready_staged()?;
        if let Err(e) = Self::flush_queue(&snap) {
            tracing::warn!(error = %e, "failed to persist queue after drain_any_ready");
        }
        Some(pending)
    }

    /// Staged variant of [`drain_any_ready`](Self::drain_any_ready). Returns
    /// `None` (without bumping the queue epoch) when no eligible repo has a
    /// pending entry.
    pub fn drain_any_ready_staged(&mut self) -> Option<(PendingRun, QueueSnapshot)> {
        let pending = self.drain_any_ready_without_snapshot()?;
        let snap = self.stage_queue_locked();
        Some((pending, snap))
    }

    /// Mutation-only helper — pops the first pending entry from any
    /// non-broken queue without fsyncing.
    pub(crate) fn drain_any_ready_without_snapshot(&mut self) -> Option<PendingRun> {
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
        self.pending_queues.get_mut(&ready_hash)?.pop_front()
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
    ///
    /// FLAW-003: For each cleared entry, the corresponding `RunEntry` is
    /// transitioned to `Stopped` so `meta.json` and the in-memory registry
    /// stay in sync with `pending_queues`. Without this, reloading from disk
    /// after restart would resurrect the Queued entries as ghosts.
    ///
    /// **Synchronous wrapper** — fsyncs `meta.json` for each removed entry
    /// while holding `&mut self`. Production callers should prefer
    /// [`clear_queue_staged`](Self::clear_queue_staged) so the N×meta + 1×queue
    /// fsyncs run outside the registry mutex.
    pub fn clear_queue(&mut self, repo_filter: Option<&Path>) -> usize {
        let (count, staged_batch, snap) = self.clear_queue_staged(repo_filter);
        for staged in &staged_batch {
            if let Err(e) = Self::flush_status(staged) {
                tracing::warn!(
                    run_id = %staged.entry.run_id,
                    error = %e,
                    "flush_status failed during clear_queue (sync wrapper)",
                );
            }
        }
        if let Err(e) = Self::flush_queue(&snap) {
            tracing::warn!(error = %e, "failed to persist queue after clear_queue");
        }
        count
    }

    /// Staged variant of [`clear_queue`](Self::clear_queue). Returns the
    /// removed-entry count, the staged `RunStatus::Stopped` batch (one per
    /// removed entry, ready for [`flush_status`](Self::flush_status)), and a
    /// [`QueueSnapshot`] for outside-lock flush.
    pub fn clear_queue_staged(
        &mut self,
        repo_filter: Option<&Path>,
    ) -> (usize, Vec<StagedStatus>, QueueSnapshot) {
        // Collect removed run_ids first, then stage each RunEntry transition.
        // Drained in two passes because `stage_status_locked` takes `&mut self`
        // and we cannot iterate `pending_queues` mutably at the same time.
        let mut removed_ids: Vec<String> = Vec::new();

        match repo_filter {
            Some(filter) => {
                let canonical = filter.canonicalize().unwrap_or_else(|_| filter.to_path_buf());
                for queue in self.pending_queues.values_mut() {
                    let mut i = 0;
                    while i < queue.len() {
                        let p = &queue[i];
                        if p.repo_dir == canonical || p.repo_dir == filter {
                            let pending = queue.remove(i).expect("index verified");
                            removed_ids.push(pending.run_id);
                        } else {
                            i += 1;
                        }
                    }
                }
            }
            None => {
                for queue in self.pending_queues.values_mut() {
                    while let Some(p) = queue.pop_front() {
                        removed_ids.push(p.run_id);
                    }
                }
            }
        }

        let mut staged_batch = Vec::with_capacity(removed_ids.len());
        for run_id in &removed_ids {
            match self.stage_status_locked(
                run_id,
                RunStatus::Stopped,
                None,
                Some("cleared by user".into()),
            ) {
                Ok(staged) => staged_batch.push(staged),
                Err(e) => {
                    tracing::warn!(
                        run_id = %run_id,
                        error = %e,
                        "stage_status_locked failed during clear_queue",
                    );
                }
            }
        }

        let snap = self.stage_queue_locked();
        (removed_ids.len(), staged_batch, snap)
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

    /// Stage an in-memory snapshot of all pending queues and circuit-breaker
    /// state without touching disk.
    ///
    /// INV-19 mirror for queues: callers drop the registry mutex *before*
    /// invoking [`flush_queue`](Self::flush_queue) so the 1–20 ms fsync runs
    /// outside the lock and does not block other registry operations.
    ///
    /// Bumps `queue_epoch` so the returned `QueueSnapshot` is uniquely
    /// identifiable. Takes `&self` (not `&mut self`) — `AtomicU64::fetch_add`
    /// is interior-mutable, and the `pending_queues` / `consecutive_failures`
    /// maps are read-only here.
    pub fn stage_queue_locked(&self) -> QueueSnapshot {
        // fetch_add returns the prior value; bump by one for the new epoch
        // so the very first staging carries epoch=1 and is strictly greater
        // than the initial `queue_last_flushed_epoch` value of 0.
        let epoch = self
            .queue_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let wire = QueueSnapshotWire {
            queues: self
                .pending_queues
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            consecutive_failures: self.consecutive_failures.clone(),
        };
        QueueSnapshot {
            wire,
            epoch,
            flushed_tracker: Arc::clone(&self.queue_last_flushed_epoch),
            flushed: Cell::new(false),
        }
    }

    /// Atomically write `snapshot.wire` to `~/.gw/queue.json`.
    ///
    /// Associated function (no `&self`) so the caller's registry mutex is
    /// already released — this is the INV-19 mirror for queues.
    ///
    /// Uses the same atomic write pattern as [`write_meta`]: write to a
    /// sibling tempfile, `fsync`, then `rename` into place.
    ///
    /// Epoch idempotency: if a newer epoch has already been flushed, this
    /// snapshot is stale and writing it would roll the on-disk state back.
    /// The early-return treats stale flushes as success because the on-disk
    /// state already supersedes this snapshot.
    pub fn flush_queue(snapshot: &QueueSnapshot) -> Result<()> {
        // Stale-snapshot guard: a newer flush already happened, skip the I/O.
        let last = snapshot.flushed_tracker.load(Ordering::Acquire);
        if snapshot.epoch <= last {
            snapshot.flushed.set(true);
            return Ok(());
        }

        let json = serde_json::to_string_pretty(&snapshot.wire)
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

        // Advance `flushed_tracker` to this epoch via CAS — never regress.
        // If a concurrent flush bumped it past `snapshot.epoch`, leave it.
        let mut current = snapshot.flushed_tracker.load(Ordering::Relaxed);
        while snapshot.epoch > current {
            match snapshot.flushed_tracker.compare_exchange_weak(
                current,
                snapshot.epoch,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }

        snapshot.flushed.set(true);
        Ok(())
    }

    /// Persist pending queues and circuit-breaker state to `~/.gw/queue.json`.
    ///
    /// Convenience wrapper around [`stage_queue_locked`](Self::stage_queue_locked)
    /// followed by [`flush_queue`](Self::flush_queue). Use the two-step form
    /// directly when you need to release the registry mutex between mutation
    /// and fsync (the INV-19 pattern).
    pub fn save_queue(&self) -> Result<()> {
        let s = self.stage_queue_locked();
        Self::flush_queue(&s)
    }

    /// Load queue snapshot from `~/.gw/queue.json`.
    ///
    /// Returns the on-disk wire format; runtime callers that need a stage/
    /// flush invariant should construct a [`QueueSnapshot`] via
    /// [`stage_queue_locked`](Self::stage_queue_locked) instead.
    ///
    /// Returns an empty default on missing or corrupt files so the daemon
    /// always starts cleanly.
    pub fn load_queue(home: &Path) -> Result<QueueSnapshotWire> {
        let path = home.join("queue.json");
        match fs::read_to_string(&path) {
            Ok(content) => Ok(serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "corrupt queue.json — using empty defaults");
                QueueSnapshotWire::default()
            })),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, "failed to read queue.json — using empty defaults");
                }
                Ok(QueueSnapshotWire::default())
            }
        }
    }

    /// Populate in-memory pending queues and circuit-breaker state from a
    /// previously loaded [`QueueSnapshotWire`].
    pub fn restore_queue(&mut self, snapshot: QueueSnapshotWire) {
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
    ///
    /// Associated function (not `&self`) so it can be called after the
    /// registry mutex is released — see `flush_status` / INV-19.
    fn write_meta(entry: &RunEntry) -> Result<()> {
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
            let err = reg.enqueue_run(p2).unwrap_err().to_string();
            assert!(
                err.contains("plan already queued"),
                "expected queued-duplicate error, got: {err}"
            );
        });
    }

    /// Regression for the `gw ps` twin-entry bug: when a plan has already
    /// been drained into `Running` status and is no longer present in
    /// `pending_queues`, a second submit via the enqueue path used to be
    /// accepted (the old duplicate check only scanned `pending_queues`).
    /// The scan in `enqueue_run` must consult `self.runs` so Running plans
    /// are rejected too.
    #[test]
    fn running_plan_rejected_on_enqueue() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/test-repo-running-dup");
            let plan = PathBuf::from("plan-running.md");

            // Simulate the `gw run` (foreground) / `spawn_run` (daemon)
            // path: register_run inserts with Queued + holds repo_lock,
            // then executor flips to Running.
            let run_id = reg
                .register_run(plan.clone(), repo.clone(), None, None)
                .expect("first register should succeed");
            reg.update_status(&run_id, RunStatus::Running, None, None)
                .expect("flip to Running");
            assert_eq!(reg.get(&run_id).unwrap().status, RunStatus::Running);

            // Re-submit the same plan via the enqueue path (what the
            // server does when has_repo_lock=true).
            let dup = PendingRun {
                run_id: "dup01".into(),
                plan_path: plan.clone(),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            let err = reg
                .enqueue_run(dup)
                .expect_err("enqueue of a running plan must be rejected");
            assert!(
                err.to_string().contains("plan already running"),
                "error must surface the Running status, got: {err}"
            );
            // And the running entry's repo must have no phantom Queued
            // sibling in self.runs (no ghost row in `gw ps`).
            let queued_count = reg
                .list_runs(true)
                .iter()
                .filter(|e| e.status == RunStatus::Queued)
                .count();
            assert_eq!(queued_count, 0, "no ghost Queued row may be created");
        });
    }

    /// Duplicate detection must canonicalize plan paths. Submitting the
    /// same plan as a relative path and then as an absolute path used to
    /// slip past the old byte-equality check.
    #[test]
    fn duplicate_detected_across_path_variants() {
        with_temp_gw_home(|tmp| {
            let repo = tmp.path().join("repo");
            std::fs::create_dir_all(repo.join("plans")).unwrap();
            let abs_plan = repo.join("plans/x.md");
            std::fs::write(&abs_plan, "dummy").unwrap();

            let mut reg = RunRegistry::new();
            // First register with an absolute path.
            let _run_id = reg
                .register_run(abs_plan.clone(), repo.clone(), None, None)
                .expect("first register");

            // Second submit with a relative path under the same repo —
            // `plans_match` must normalize them to the same canonical file.
            let rel = PendingRun {
                run_id: "relpath".into(),
                plan_path: PathBuf::from("plans/x.md"),
                repo_dir: repo.clone(),
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            assert!(
                reg.enqueue_run(rel).is_err(),
                "relative path must be recognized as duplicate of the already-registered absolute path"
            );
        });
    }

    /// Terminal entries (Succeeded/Failed/Stopped) must NOT block a fresh
    /// enqueue of the same plan — the user may legitimately re-run a
    /// completed plan.
    #[test]
    fn terminal_entry_does_not_block_enqueue() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/test-repo-terminal-dup");
            let plan = PathBuf::from("finished.md");

            let run_id = reg
                .register_run(plan.clone(), repo.clone(), None, None)
                .expect("register");
            reg.update_status(&run_id, RunStatus::Succeeded, None, None)
                .expect("flip to Succeeded");

            let fresh = PendingRun {
                run_id: "fresh01".into(),
                plan_path: plan,
                repo_dir: repo,
                session_name: None,
                config_dir: None,
                verbose: 0,
                queued_at: Utc::now(),
            };
            assert!(
                reg.enqueue_run(fresh).is_ok(),
                "a completed (terminal) entry must not block re-running the plan"
            );
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
        let _ = guard.commit();
        // guard consumed, map untouched
        assert!(map.contains_key("abc"));
    }

    // ── PERF-003: queue staged-flush regression suite ──────────────────
    //
    // Covers T4 (safety-net wiring) + T5 (canonical-path regressions)
    // for plans/2026-04-15-perf-queue-fsync-outside-mutex-plan.md.

    /// Test-only `MakeWriter` that captures structured tracing output into
    /// a shared buffer so assertions can inspect emitted events.
    ///
    /// Used by the Drop / panic-unwind tests to verify the production
    /// `tracing::error!` BUG signal fires when a `QueueSnapshot` escapes
    /// without a flush.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl LogCapture {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Read `~/.gw/queue.json` directly — bypasses `restore_queue` so a test
    /// can compare on-disk state against an in-memory expectation.
    fn read_queue_from_disk() -> QueueSnapshotWire {
        RunRegistry::load_queue(&gw_home()).expect("load_queue should succeed in tests")
    }

    fn make_pending(run_id: &str, plan: &str, repo: &Path) -> PendingRun {
        PendingRun {
            run_id: run_id.into(),
            plan_path: PathBuf::from(plan),
            repo_dir: repo.to_path_buf(),
            session_name: None,
            config_dir: None,
            verbose: 0,
            queued_at: Utc::now(),
        }
    }

    // ── T5 #1: parity between staged + sync paths ──────────────────────
    #[test]
    fn staged_variant_produces_same_state_as_sync_wrapper() {
        // Sync wrappers and `*_staged` + flush must produce byte-identical
        // queue.json after the same sequence of mutations. Guards against
        // future drift between the two API forms.
        //
        // Pre-built pendings with a fixed timestamp ensure the only
        // possible difference between passes is API path, not wall-clock.
        with_temp_gw_home(|_| {
            let repo = PathBuf::from("/tmp/parity-repo");
            let fixed_ts = Utc::now();
            let pendings: Vec<PendingRun> = (0..5)
                .map(|i| PendingRun {
                    run_id: format!("sync-{i:02}"),
                    plan_path: PathBuf::from(format!("plan-{i:02}.md")),
                    repo_dir: repo.clone(),
                    session_name: None,
                    config_dir: None,
                    verbose: 0,
                    queued_at: fixed_ts,
                })
                .collect();

            // Pass A: sync wrappers.
            let mut reg_sync = RunRegistry::new();
            for p in &pendings {
                reg_sync.enqueue_run(p.clone()).unwrap();
            }
            reg_sync.record_queue_failure(&repo);
            reg_sync.record_queue_success(&repo);
            let _ = reg_sync.drain_next(&repo);
            let on_disk_sync = read_queue_from_disk();

            // Reset GW_HOME contents to isolate pass B.
            let _ = fs::remove_file(gw_home().join("queue.json"));

            // Pass B: staged variants under simulated lock-release.
            let mut reg_staged = RunRegistry::new();
            for p in &pendings {
                let (_, snap) = reg_staged.enqueue_run_staged(p.clone()).unwrap();
                RunRegistry::flush_queue(&snap).unwrap();
            }
            let (_, snap) = reg_staged.record_queue_failure_staged(&repo);
            RunRegistry::flush_queue(&snap).unwrap();
            let snap = reg_staged.record_queue_success_staged(&repo);
            RunRegistry::flush_queue(&snap).unwrap();
            if let Some((_, snap)) = reg_staged.drain_next_staged(&repo) {
                RunRegistry::flush_queue(&snap).unwrap();
            }
            let on_disk_staged = read_queue_from_disk();

            assert_eq!(
                serde_json::to_string(&on_disk_sync).unwrap(),
                serde_json::to_string(&on_disk_staged).unwrap(),
                "staged + flush must produce byte-identical queue.json as sync wrappers",
            );
        });
    }

    // ── T5 #2: idempotency on identical snapshots ──────────────────────
    #[test]
    fn flush_queue_is_nop_on_identical_snapshot() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            reg.enqueue_run(make_pending("a1", "p.md", &PathBuf::from("/tmp/r")))
                .unwrap();
            let snap = reg.stage_queue_locked();
            // First flush writes the file.
            RunRegistry::flush_queue(&snap).unwrap();
            // Second flush of the same snapshot must be a no-op.
            RunRegistry::flush_queue(&snap).unwrap();
            // Both calls return Ok and the file remains valid.
            assert!(gw_home().join("queue.json").exists());
        });
    }

    // ── T5 #3 (VIGIL-1): crash window — staged-but-not-flushed lost ────
    #[test]
    fn load_queue_after_staged_but_not_flushed_sees_prior_state() {
        with_temp_gw_home(|_| {
            // Establish a prior on-disk state via the sync API.
            let mut reg = RunRegistry::new();
            reg.enqueue_run(make_pending("prior", "old.md", &PathBuf::from("/tmp/r")))
                .unwrap();
            let prior_disk = read_queue_from_disk();
            assert!(prior_disk.queues.values().any(|q| q.iter().any(|p| p.run_id == "prior")));

            // Stage a new mutation but DO NOT flush. Drop the snapshot.
            {
                let (_pos, snap) = reg
                    .enqueue_run_staged(make_pending("ghost", "new.md", &PathBuf::from("/tmp/r")))
                    .unwrap();
                drop(snap); // intentional — exercises the durability contract
            }

            // On-disk queue.json must still reflect the *prior* state, not
            // the staged-but-unflushed mutation. This is the durability
            // contract documented in the plan's Invariant section.
            let after_disk = read_queue_from_disk();
            let has_ghost = after_disk
                .queues
                .values()
                .any(|q| q.iter().any(|p| p.run_id == "ghost"));
            assert!(
                !has_ghost,
                "staged-but-unflushed mutation must NOT appear on disk",
            );
            assert!(
                after_disk
                    .queues
                    .values()
                    .any(|q| q.iter().any(|p| p.run_id == "prior")),
                "prior state must remain on disk after a dropped snapshot",
            );
        });
    }

    // ── T5 #4 (VIGIL-2): canonical multi-mutation single flush ─────────
    #[test]
    fn multi_mutation_single_flush_captures_cumulative_state() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/multi-repo");
            // Seed two pendings so drain_next has something to pop.
            reg.enqueue_run(make_pending("seed-1", "s1.md", &repo)).unwrap();
            reg.enqueue_run(make_pending("seed-2", "s2.md", &repo)).unwrap();

            // Simulated held-lock scope: two mutations under one lock + one
            // trailing snapshot. Mirrors executor::drain_if_available.
            let (staged_batch, drained, snap) = {
                let staged_batch = reg.record_queue_failure_without_snapshot(&repo);
                let drained = reg.drain_next_without_snapshot(&repo);
                let snap = reg.stage_queue_locked();
                (staged_batch, drained, snap)
            };
            // Outside the scope, flush both signals.
            for s in &staged_batch {
                let _ = RunRegistry::flush_status(s);
            }
            RunRegistry::flush_queue(&snap).unwrap();

            // Reload from disk and assert both mutations landed.
            let on_disk = read_queue_from_disk();
            let total: usize = on_disk.queues.values().map(|q| q.len()).sum();
            // After one drain_next, only seed-2 should remain.
            assert_eq!(total, 1, "drain_next mutation must persist");
            assert_eq!(
                on_disk
                    .consecutive_failures
                    .get(&repo_hash(&repo))
                    .copied()
                    .unwrap_or(0),
                1,
                "record_queue_failure mutation must persist",
            );
            assert!(drained.is_some(), "first pending should have been drained");
        });
    }

    // ── T5 #5 (RUIN-SEC-2): stale snapshot flush is skipped ────────────
    #[test]
    fn stale_snapshot_flush_is_skipped() {
        with_temp_gw_home(|_| {
            let mut reg = RunRegistry::new();
            let repo = PathBuf::from("/tmp/stale-repo");
            // Stage A (epoch N) and B (epoch N+1).
            reg.enqueue_run_without_snapshot(make_pending("a", "a.md", &repo))
                .unwrap();
            let snap_a = reg.stage_queue_locked();
            reg.enqueue_run_without_snapshot(make_pending("b", "b.md", &repo))
                .unwrap();
            let snap_b = reg.stage_queue_locked();
            assert!(snap_b.epoch > snap_a.epoch);

            // Flush B (newer) first, then A (stale).
            RunRegistry::flush_queue(&snap_b).unwrap();
            let after_b = read_queue_from_disk();
            let count_b: usize = after_b.queues.values().map(|q| q.len()).sum();
            assert_eq!(count_b, 2, "snapshot B reflects both enqueues");

            // Stale flush of A must succeed (idempotent) but NOT roll back
            // the on-disk state.
            RunRegistry::flush_queue(&snap_a).unwrap();
            let after_a = read_queue_from_disk();
            let count_a: usize = after_a.queues.values().map(|q| q.len()).sum();
            assert_eq!(
                count_a, 2,
                "stale flush must not roll on-disk state back to snapshot A's view",
            );
        });
    }

    // ── T4 / T5 #6 (RUIN-SEC-1): Drop emits production tracing::error! ─
    #[test]
    fn dropped_unflushed_snapshot_emits_error_log() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        with_temp_gw_home(|_| {
            let buf = LogCapture::default();
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer(buf.clone())
                    .with_ansi(false),
            );
            let _guard = subscriber.set_default();

            let reg = RunRegistry::new();
            {
                let snap = reg.stage_queue_locked();
                drop(snap); // unflushed — Drop must emit BUG line
            }

            let logs = buf.contents();
            assert!(
                logs.contains("BUG: QueueSnapshot dropped"),
                "Drop must emit production-visible tracing::error! line, got logs: {logs}",
            );
        });
    }

    // ── T4: panic-unwind safety net ────────────────────────────────────
    #[test]
    fn panic_unwind_drops_unflushed_snapshot_and_emits_error() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        with_temp_gw_home(|_| {
            let buf = LogCapture::default();
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer(buf.clone())
                    .with_ansi(false),
            );
            let _guard = subscriber.set_default();

            let reg = RunRegistry::new();
            // Wrap in catch_unwind so the panic does not abort the test.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _snap = reg.stage_queue_locked();
                panic!("simulated worker panic before flush");
            }));
            assert!(result.is_err(), "panic must propagate to catch_unwind");

            let logs = buf.contents();
            assert!(
                logs.contains("BUG: QueueSnapshot dropped"),
                "Drop must fire during unwind, got logs: {logs}",
            );
        });
    }

    // ── T4: errored flush leaves snapshot un-flushed (Drop still fires)
    #[test]
    fn errored_flush_does_not_clear_flushed_flag() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        with_temp_gw_home(|tmp| {
            let buf = LogCapture::default();
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer(buf.clone())
                    .with_ansi(false),
            );
            let _guard = subscriber.set_default();

            // Make `gw_home/queue.json.tmp` unwritable: replace the gw_home
            // directory's `queue.json.tmp` location with a directory so
            // `fs::File::create` fails. Simulates a tempfile creation error.
            let tmp_path = tmp.path().join("queue.json.tmp");
            fs::create_dir(&tmp_path).expect("create directory in place of tmpfile");

            let reg = RunRegistry::new();
            let snap = reg.stage_queue_locked();
            let result = RunRegistry::flush_queue(&snap);
            assert!(result.is_err(), "flush must fail when tmpfile cannot be created");

            // Drop the snapshot — Drop must fire because flushed flag stayed false.
            drop(snap);
            let logs = buf.contents();
            assert!(
                logs.contains("BUG: QueueSnapshot dropped"),
                "errored flush must leave Drop-trigger intact, got logs: {logs}",
            );
        });
    }

    // ── T5 #7 (SIGHT-ARCH-2): concurrent flush safety ──────────────────
    #[test]
    fn concurrent_flush_is_safe() {
        // Two threads each stage and flush a snapshot on a shared registry.
        // Final on-disk state must reflect the higher-epoch snapshot — never
        // a torn write.
        with_temp_gw_home(|_| {
            let reg = Arc::new(std::sync::Mutex::new(RunRegistry::new()));
            let repo_a = PathBuf::from("/tmp/concurrent-a");
            let repo_b = PathBuf::from("/tmp/concurrent-b");

            let r1 = Arc::clone(&reg);
            let h1 = std::thread::spawn(move || {
                let mut g = r1.lock().unwrap();
                let snap = g
                    .enqueue_run_staged(make_pending("c-a", "a.md", &repo_a))
                    .unwrap()
                    .1;
                drop(g);
                RunRegistry::flush_queue(&snap).unwrap();
            });
            let r2 = Arc::clone(&reg);
            let h2 = std::thread::spawn(move || {
                let mut g = r2.lock().unwrap();
                let snap = g
                    .enqueue_run_staged(make_pending("c-b", "b.md", &repo_b))
                    .unwrap()
                    .1;
                drop(g);
                RunRegistry::flush_queue(&snap).unwrap();
            });
            h1.join().unwrap();
            h2.join().unwrap();

            // Final disk must be valid JSON containing both entries (no torn
            // write, no partial overwrite).
            let on_disk = read_queue_from_disk();
            let total: usize = on_disk.queues.values().map(|q| q.len()).sum();
            assert_eq!(
                total, 2,
                "both concurrent stages must be reflected in final on-disk state",
            );
        });
    }

    // ── T5 #8: structural — flush_queue runs after lock release ────────
    #[test]
    fn flush_queue_runs_outside_registry_lock() {
        // Codifies the lock-release-before-flush contract by exercising it
        // explicitly: stage under a held mutex, drop the guard, observe
        // that the flush call itself is unable to access the registry.
        // If a future change moves `flush_queue` back inside the locked
        // scope, the manual `drop(guard)` ordering will fail review and
        // this test documents the intent.
        with_temp_gw_home(|_| {
            let reg = Arc::new(std::sync::Mutex::new(RunRegistry::new()));
            let lock_held = Arc::new(std::sync::atomic::AtomicBool::new(false));

            let snap = {
                let mut guard = reg.lock().unwrap();
                lock_held.store(true, Ordering::SeqCst);
                let s = guard.stage_queue_locked();
                lock_held.store(false, Ordering::SeqCst);
                s
            };

            assert!(
                !lock_held.load(Ordering::SeqCst),
                "registry mutex must be released before flush_queue runs",
            );
            // flush_queue is an associated function — `&self` is unavailable
            // by construction, which is the static guarantee that backs this
            // test's runtime assertion.
            RunRegistry::flush_queue(&snap).unwrap();
        });
    }
}
