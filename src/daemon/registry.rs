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
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::daemon::protocol::{RunInfo, RunStatus};
use crate::daemon::state::gw_home;

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
        }
    }
}

// ── Registry ────────────────────────────────────────────────────────

/// In-memory registry of all known runs, backed by on-disk JSON files.
#[derive(Debug)]
pub struct RunRegistry {
    runs: HashMap<String, RunEntry>,
    /// Held file locks for per-repo exclusion (keyed by repo hash).
    #[allow(dead_code)]
    repo_locks: HashMap<String, fs::File>,
}

impl RunRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            runs: HashMap::new(),
            repo_locks: HashMap::new(),
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
    fn generate_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let mut hasher = Sha256::new();
        hasher.update(nanos.to_le_bytes());
        hasher.update(pid.to_le_bytes());
        let hash = hasher.finalize();
        hex::encode(&hash[..4])
    }

    /// Register a new run. Creates the on-disk directory and acquires repo lock.
    pub fn register_run(
        &mut self,
        plan_path: PathBuf,
        repo_dir: PathBuf,
        session_name: Option<String>,
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
            config_dir: None,
            error_message: None,
            restartable: true,
        };

        // Persist to disk
        self.write_meta(&entry)?;
        self.runs.insert(run_id.clone(), entry);

        tracing::info!(run_id = %run_id, "registered new run");
        Ok(run_id)
    }

    /// Update the status of a run (atomic write).
    pub fn update_status(
        &mut self,
        run_id: &str,
        status: RunStatus,
        phase: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let entry = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| color_eyre::eyre::eyre!("run not found: {run_id}"))?;

        entry.status = status;
        if let Some(p) = phase {
            entry.current_phase = Some(p);
        }
        if let Some(e) = error {
            entry.error_message = Some(e);
        }

        // Mark finished time for terminal states
        if matches!(
            status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Stopped
        ) {
            entry.finished_at = Some(Utc::now());
            // Release repo lock on completion
            let repo_hash = Self::repo_hash(&entry.repo_dir);
            self.repo_locks.remove(&repo_hash);
        }

        let entry_clone = entry.clone();
        self.write_meta(&entry_clone)?;

        tracing::debug!(run_id = %run_id, status = ?status, "updated run status");
        Ok(())
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

    /// Find a run by ID prefix (like git short SHA).
    pub fn find_by_prefix(&self, prefix: &str) -> Option<&RunEntry> {
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

    // ── Internal helpers ────────────────────────────────────────────

    /// Compute SHA-256 hash of a repo path for the lock filename.
    fn repo_hash(repo_dir: &Path) -> String {
        let canonical = repo_dir
            .canonicalize()
            .unwrap_or_else(|_| repo_dir.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        hex::encode(&hasher.finalize()[..16])
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

        let json = serde_json::to_string_pretty(entry).wrap_err("failed to serialize run entry")?;

        fs::write(&tmp_path, &json)
            .wrap_err_with(|| format!("failed to write tmp meta: {}", tmp_path.display()))?;

        // Fsync before rename for crash safety
        let f = fs::File::open(&tmp_path)?;
        f.sync_all().wrap_err("failed to fsync meta.json")?;

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
        use std::sync::{Mutex, OnceLock};
        static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        // Recover from poison: a panicking test already fails; propagating
        // lock poison would cascade failures to unrelated tests.
        let _guard = MUTEX
            .get_or_init(|| Mutex::new(()))
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
            )
            .unwrap();

            // Second run against same repo should fail
            let result = reg.register_run(
                PathBuf::from("plans/f.md"),
                PathBuf::from("/tmp/repo-lock-test"),
                None,
            );
            assert!(result.is_err());
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
                )
                .unwrap();
            let _id2 = reg
                .register_run(
                    PathBuf::from("p2.md"),
                    PathBuf::from("/tmp/r2"),
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
}
