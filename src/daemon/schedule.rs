//! Schedule registry: persistent tracking of scheduled plan executions.
//!
//! Supports three schedule kinds: cron (recurring), one-shot (fire at a
//! specific time), and delayed (fire after N seconds from creation).
//! Entries are persisted as JSON at `~/.gw/schedules.json` using atomic
//! writes (write-to-tmp + fsync + rename).

use chrono::{DateTime, Utc};
use color_eyre::{eyre::WrapErr, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── Schedule kind ──────────────────────────────────────────────────

/// The type of schedule trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScheduleKind {
    /// Recurring cron expression (7-field with seconds).
    Cron { expression: String },
    /// Fire once at a specific time.
    OneShot { at: DateTime<Utc> },
    /// Fire once after a delay from creation.
    Delayed { delay_secs: u64 },
}

// ── Schedule status ────────────────────────────────────────────────

/// Current lifecycle state of a schedule entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleStatus {
    /// Ready to fire on its next trigger time.
    Active,
    /// Temporarily suspended; will not fire until resumed.
    Paused,
    /// Terminal: the schedule has finished (one-shot/delayed after firing).
    Completed,
    /// Terminal: the schedule encountered an unrecoverable error.
    Failed,
}

// ── Schedule entry ─────────────────────────────────────────────────

/// A single scheduled plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    /// Unique schedule identifier (8-char hex).
    pub id: String,
    /// Path to the plan file to execute.
    pub plan_path: PathBuf,
    /// Repository working directory.
    pub repo_dir: PathBuf,
    /// Optional config directory override.
    pub config_dir: Option<PathBuf>,
    /// Verbosity level (0-3).
    pub verbose: u8,
    /// What triggers this schedule.
    pub kind: ScheduleKind,
    /// Current lifecycle status.
    pub status: ScheduleStatus,
    /// When this entry was created.
    pub created_at: DateTime<Utc>,
    /// When this schedule last fired.
    pub last_fired: Option<DateTime<Utc>>,
    /// When this schedule will next fire.
    pub next_fire: Option<DateTime<Utc>>,
    /// How many times this schedule has fired.
    pub fire_count: u32,
    /// Optional human-readable label.
    pub label: Option<String>,
}

// ── Schedule registry ──────────────────────────────────────────────

/// Persistent registry of all schedule entries.
///
/// Backed by a JSON file at the given `path`. All mutations persist
/// automatically via atomic write.
#[derive(Debug)]
pub struct ScheduleRegistry {
    entries: HashMap<String, ScheduleEntry>,
    path: PathBuf,
}

impl ScheduleRegistry {
    /// Create an empty registry that will persist to `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            entries: HashMap::new(),
            path,
        }
    }

    /// Add a schedule entry and persist. Returns the entry ID.
    pub fn add(&mut self, entry: ScheduleEntry) -> Result<String> {
        let id = entry.id.clone();
        self.entries.insert(id.clone(), entry);
        self.save()?;
        Ok(id)
    }

    /// Remove a schedule entry by ID and persist.
    pub fn remove(&mut self, id: &str) -> Result<()> {
        self.entries
            .remove(id)
            .ok_or_else(|| color_eyre::eyre::eyre!("schedule '{}' not found", id))?;
        self.save()
    }

    /// Pause a schedule: set status to `Paused`, clear `next_fire`, and persist.
    pub fn pause(&mut self, id: &str) -> Result<()> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| color_eyre::eyre::eyre!("schedule '{}' not found", id))?;
        entry.status = ScheduleStatus::Paused;
        entry.next_fire = None;
        self.save()
    }

    /// Resume a schedule: set status to `Active`, recompute `next_fire`, and persist.
    pub fn resume(&mut self, id: &str) -> Result<()> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| color_eyre::eyre::eyre!("schedule '{}' not found", id))?;
        entry.status = ScheduleStatus::Active;
        entry.next_fire = compute_next_fire(&entry.kind, Utc::now());
        self.save()
    }

    /// Return all schedule entries.
    pub fn list(&self) -> Vec<&ScheduleEntry> {
        self.entries.values().collect()
    }

    /// Return active entries whose `next_fire` is at or before now.
    pub fn get_ready_schedules(&self) -> Vec<ScheduleEntry> {
        let now = Utc::now();
        self.entries
            .values()
            .filter(|e| {
                e.status == ScheduleStatus::Active
                    && e.next_fire.is_some_and(|nf| nf <= now)
            })
            .cloned()
            .collect()
    }

    /// Record that a schedule has fired: update counters and compute next fire time.
    ///
    /// For `OneShot` and `Delayed` kinds, the status is set to `Completed`
    /// after firing since they only fire once.
    pub fn record_fired(&mut self, id: &str) -> Result<()> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| color_eyre::eyre::eyre!("schedule '{}' not found", id))?;

        let now = Utc::now();
        entry.last_fired = Some(now);
        entry.fire_count += 1;

        match &entry.kind {
            ScheduleKind::OneShot { .. } | ScheduleKind::Delayed { .. } => {
                entry.status = ScheduleStatus::Completed;
                entry.next_fire = None;
            }
            ScheduleKind::Cron { .. } => {
                entry.next_fire = compute_next_fire(&entry.kind, now);
            }
        }

        self.save()
    }

    /// Find a schedule entry by ID prefix (same pattern as `RunRegistry`).
    ///
    /// Returns `Some` only when exactly one entry matches the prefix.
    /// Returns `None` for empty prefix or ambiguous matches.
    pub fn find_by_prefix(&self, prefix: &str) -> Option<&ScheduleEntry> {
        if prefix.is_empty() {
            return None;
        }
        let matches: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.id.starts_with(prefix))
            .collect();

        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None
        }
    }

    /// Count entries with `Active` status.
    pub fn count_active(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.status == ScheduleStatus::Active)
            .count()
    }

    /// Load a registry from a JSON file.
    ///
    /// Returns an empty registry if the file does not exist.
    /// Returns an error on parse failure.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                entries: HashMap::new(),
                path: path.to_path_buf(),
            });
        }

        let bytes = std::fs::read(path)
            .wrap_err_with(|| format!("failed to read schedule registry at {}", path.display()))?;

        let entries: HashMap<String, ScheduleEntry> = serde_json::from_slice(&bytes)
            .wrap_err_with(|| {
                format!(
                    "failed to parse schedule registry at {}",
                    path.display()
                )
            })?;

        Ok(Self {
            entries,
            path: path.to_path_buf(),
        })
    }

    /// Persist the registry to disk using atomic write.
    ///
    /// Writes to a `.json.tmp` sibling, fsyncs, then renames into place.
    /// This guarantees readers never see a torn/partial file.
    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.entries)
            .wrap_err("failed to serialize schedule registry")?;

        let tmp_path = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp_path).wrap_err_with(|| {
                format!(
                    "failed to create schedule registry tempfile at {}",
                    tmp_path.display()
                )
            })?;
            f.write_all(json.as_bytes()).wrap_err_with(|| {
                format!(
                    "failed to write schedule registry tempfile at {}",
                    tmp_path.display()
                )
            })?;
            f.sync_all().wrap_err_with(|| {
                format!(
                    "failed to fsync schedule registry tempfile at {}",
                    tmp_path.display()
                )
            })?;
        }
        std::fs::rename(&tmp_path, &self.path).wrap_err_with(|| {
            format!(
                "failed to atomically rename schedule registry into {}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

// ── Schedule runner ────────────────────────────────────────────────

/// Run the schedule polling loop.
/// Ticks every `interval_secs` seconds, checking for ready schedules.
/// When a schedule fires, submits a run via the existing executor path.
pub async fn run_schedule_loop(
    schedule_registry: Arc<Mutex<ScheduleRegistry>>,
    run_registry: Arc<Mutex<crate::daemon::registry::RunRegistry>>,
    cancel: CancellationToken,
    interval_secs: u64,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    // Skip: don't catch up on missed ticks — fire once on next tick instead.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                let ready = {
                    let sched = schedule_registry.lock().await;
                    sched.get_ready_schedules()
                };
                for entry in ready {
                    let result = crate::daemon::executor::spawn_run(
                        Arc::clone(&run_registry),
                        &entry.plan_path,
                        &entry.repo_dir,
                        None,
                        entry.config_dir.clone(),
                        entry.verbose,
                    )
                    .await;
                    let mut sched = schedule_registry.lock().await;
                    match result {
                        Ok(run_id) => {
                            tracing::info!(schedule_id = %entry.id, run_id = %run_id, "schedule fired");
                            // Tag the run with the schedule ID for `gw ps` annotation
                            {
                                let mut reg = run_registry.lock().await;
                                if let Some(run_entry) = reg.get_mut(&run_id) {
                                    run_entry.schedule_id = Some(entry.id.clone());
                                }
                            }
                            sched.record_fired(&entry.id).ok();
                        }
                        Err(e) => {
                            tracing::warn!(schedule_id = %entry.id, error = %e, "schedule fire failed");
                        }
                    }
                }
            }
        }
    }
}

// ── Time helpers ───────────────────────────────────────────────────

/// Compute the next fire time for a schedule kind after the given instant.
///
/// - **Cron**: parses the 7-field expression and returns the next upcoming time.
/// - **OneShot**: returns `at` if it is still in the future, else `None`.
/// - **Delayed**: returns `created_at + delay_secs` — but since we don't have
///   `created_at` here, the caller should set `next_fire` at creation time.
///   This function returns `None` for `Delayed` (it's only meaningful at creation).
pub fn compute_next_fire(kind: &ScheduleKind, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match kind {
        ScheduleKind::Cron { expression } => {
            let schedule = cron::Schedule::from_str(expression).ok()?;
            schedule.after(&after).next()
        }
        ScheduleKind::OneShot { at } => {
            if *at > after {
                Some(*at)
            } else {
                None
            }
        }
        ScheduleKind::Delayed { .. } => {
            // Delayed schedules have their next_fire set at creation time
            // (created_at + delay_secs). This function cannot compute it
            // without created_at, so callers handle it during entry creation.
            None
        }
    }
}

/// Normalize a cron expression to 7-field format (with seconds and year).
///
/// - 5 fields (standard cron): prepends `0` (seconds) and appends `*` (year).
/// - 7 fields (already full): returned as-is.
/// - Other field counts: returns an error.
pub fn normalize_cron_expression(input: &str) -> Result<String> {
    let fields: Vec<&str> = input.split_whitespace().collect();
    match fields.len() {
        5 => Ok(format!("0 {} *", input.trim())),
        7 => Ok(input.trim().to_string()),
        n => Err(color_eyre::eyre::eyre!(
            "invalid cron expression: expected 5 or 7 fields, got {}",
            n
        )),
    }
}

/// Parse a duration string with a suffix into seconds.
///
/// Supported suffixes: `s` (seconds), `m` (minutes), `h` (hours), `d` (days).
///
/// # Examples
/// ```ignore
/// assert_eq!(parse_duration_suffix("30m").unwrap(), 1800);
/// assert_eq!(parse_duration_suffix("2h").unwrap(), 7200);
/// assert_eq!(parse_duration_suffix("1d").unwrap(), 86400);
/// ```
pub fn parse_duration_suffix(input: &str) -> Result<u64> {
    let input = input.trim();
    if input.is_empty() {
        return Err(color_eyre::eyre::eyre!("empty duration string"));
    }

    let (num_str, suffix) = input.split_at(input.len() - 1);
    let value: u64 = num_str
        .parse()
        .wrap_err_with(|| format!("invalid duration number: '{}'", num_str))?;

    let multiplier = match suffix {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => {
            return Err(color_eyre::eyre::eyre!(
                "unknown duration suffix '{}': expected s, m, h, or d",
                suffix
            ))
        }
    };

    Ok(value * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_5_field_cron() {
        let result = normalize_cron_expression("*/5 * * * *").unwrap();
        assert_eq!(result, "0 */5 * * * * *");
    }

    #[test]
    fn normalize_7_field_cron_passthrough() {
        let input = "0 */5 * * * * *";
        let result = normalize_cron_expression(input).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn normalize_invalid_field_count() {
        assert!(normalize_cron_expression("* * *").is_err());
        assert!(normalize_cron_expression("* * * * * *").is_err());
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration_suffix("30s").unwrap(), 30);
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration_suffix("30m").unwrap(), 1800);
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration_suffix("2h").unwrap(), 7200);
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(parse_duration_suffix("1d").unwrap(), 86400);
    }

    #[test]
    fn parse_duration_invalid_suffix() {
        assert!(parse_duration_suffix("10x").is_err());
    }

    #[test]
    fn parse_duration_empty() {
        assert!(parse_duration_suffix("").is_err());
    }

    #[test]
    fn registry_add_and_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        let entry = ScheduleEntry {
            id: "abcd1234".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::OneShot { at: Utc::now() + chrono::Duration::hours(1) },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
            fire_count: 0,
            label: Some("test".into()),
        };

        let id = reg.add(entry).unwrap();
        assert_eq!(id, "abcd1234");
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.count_active(), 1);
    }

    #[test]
    fn registry_remove() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        let entry = ScheduleEntry {
            id: "abcd1234".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::Delayed { delay_secs: 60 },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: None,
            fire_count: 0,
            label: None,
        };

        reg.add(entry).unwrap();
        reg.remove("abcd1234").unwrap();
        assert_eq!(reg.list().len(), 0);
    }

    #[test]
    fn registry_remove_missing_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);
        assert!(reg.remove("nonexistent").is_err());
    }

    #[test]
    fn registry_pause_and_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        let entry = ScheduleEntry {
            id: "abcd1234".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::Cron { expression: "0 */5 * * * * *".into() },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now()),
            fire_count: 0,
            label: None,
        };

        reg.add(entry).unwrap();

        reg.pause("abcd1234").unwrap();
        let e = reg.find_by_prefix("abcd").unwrap();
        assert_eq!(e.status, ScheduleStatus::Paused);
        assert!(e.next_fire.is_none());

        reg.resume("abcd1234").unwrap();
        let e = reg.find_by_prefix("abcd").unwrap();
        assert_eq!(e.status, ScheduleStatus::Active);
        assert!(e.next_fire.is_some());
    }

    #[test]
    fn registry_record_fired_oneshot_completes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        let entry = ScheduleEntry {
            id: "abcd1234".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::OneShot { at: Utc::now() },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now()),
            fire_count: 0,
            label: None,
        };

        reg.add(entry).unwrap();
        reg.record_fired("abcd1234").unwrap();

        let e = reg.find_by_prefix("abcd").unwrap();
        assert_eq!(e.status, ScheduleStatus::Completed);
        assert_eq!(e.fire_count, 1);
        assert!(e.last_fired.is_some());
        assert!(e.next_fire.is_none());
    }

    #[test]
    fn registry_find_by_prefix_empty_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let reg = ScheduleRegistry::new(path);
        assert!(reg.find_by_prefix("").is_none());
    }

    #[test]
    fn registry_load_missing_file_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        let reg = ScheduleRegistry::load(&path).unwrap();
        assert_eq!(reg.list().len(), 0);
    }

    #[test]
    fn registry_save_and_load_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path.clone());

        let entry = ScheduleEntry {
            id: "abcd1234".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::Cron { expression: "0 */5 * * * * *".into() },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now()),
            fire_count: 0,
            label: Some("roundtrip test".into()),
        };

        reg.add(entry).unwrap();

        let loaded = ScheduleRegistry::load(&path).unwrap();
        assert_eq!(loaded.list().len(), 1);
        assert_eq!(loaded.find_by_prefix("abcd").unwrap().label.as_deref(), Some("roundtrip test"));
    }

    #[test]
    fn registry_get_ready_returns_past_not_future() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        // Entry with next_fire in the past — should be ready.
        let past_entry = ScheduleEntry {
            id: "past0001".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::OneShot {
                at: Utc::now() - chrono::Duration::hours(1),
            },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now() - chrono::Duration::hours(1)),
            fire_count: 0,
            label: None,
        };

        // Entry with next_fire in the future — should NOT be ready.
        let future_entry = ScheduleEntry {
            id: "futr0001".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::OneShot {
                at: Utc::now() + chrono::Duration::hours(1),
            },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
            fire_count: 0,
            label: None,
        };

        reg.add(past_entry).unwrap();
        reg.add(future_entry).unwrap();

        let ready = reg.get_ready_schedules();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "past0001");
    }

    #[test]
    fn registry_record_fired_cron_advances_next_fire() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        let entry = ScheduleEntry {
            id: "cron0001".into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::Cron {
                expression: "0 * * * * * *".into(), // every minute
            },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: Some(Utc::now()),
            fire_count: 0,
            label: None,
        };

        reg.add(entry).unwrap();
        reg.record_fired("cron0001").unwrap();

        let e = reg.find_by_prefix("cron").unwrap();
        assert_eq!(e.status, ScheduleStatus::Active);
        assert_eq!(e.fire_count, 1);
        assert!(e.last_fired.is_some());
        // next_fire should advance to a future time
        assert!(e.next_fire.is_some());
        assert!(e.next_fire.unwrap() > Utc::now());
    }

    #[test]
    fn registry_find_by_prefix_unique_match() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("schedules.json");
        let mut reg = ScheduleRegistry::new(path);

        let make_entry = |id: &str| ScheduleEntry {
            id: id.into(),
            plan_path: PathBuf::from("plans/test.md"),
            repo_dir: PathBuf::from("/tmp/repo"),
            config_dir: None,
            verbose: 0,
            kind: ScheduleKind::Delayed { delay_secs: 60 },
            status: ScheduleStatus::Active,
            created_at: Utc::now(),
            last_fired: None,
            next_fire: None,
            fire_count: 0,
            label: None,
        };

        reg.add(make_entry("aabb1122")).unwrap();
        reg.add(make_entry("aacc3344")).unwrap();

        // Unique prefix → Some
        assert!(reg.find_by_prefix("aabb").is_some());
        assert_eq!(reg.find_by_prefix("aabb").unwrap().id, "aabb1122");

        // Ambiguous prefix → None
        assert!(reg.find_by_prefix("aa").is_none());

        // No match → None
        assert!(reg.find_by_prefix("zz").is_none());
    }

    #[test]
    fn parse_duration_45s() {
        assert_eq!(parse_duration_suffix("45s").unwrap(), 45);
    }

    #[test]
    fn parse_duration_invalid_no_number() {
        assert!(parse_duration_suffix("abc").is_err());
    }

    #[test]
    fn compute_next_fire_oneshot_future() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let kind = ScheduleKind::OneShot { at: future };
        assert_eq!(compute_next_fire(&kind, Utc::now()), Some(future));
    }

    #[test]
    fn compute_next_fire_oneshot_past() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let kind = ScheduleKind::OneShot { at: past };
        assert!(compute_next_fire(&kind, Utc::now()).is_none());
    }

    #[test]
    fn compute_next_fire_cron_returns_future() {
        let kind = ScheduleKind::Cron {
            expression: "0 * * * * * *".into(), // every minute
        };
        let next = compute_next_fire(&kind, Utc::now());
        assert!(next.is_some());
        assert!(next.unwrap() > Utc::now());
    }

    #[test]
    fn compute_next_fire_delayed_returns_none() {
        let kind = ScheduleKind::Delayed { delay_secs: 60 };
        assert!(compute_next_fire(&kind, Utc::now()).is_none());
    }
}
