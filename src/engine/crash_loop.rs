//! Windowed crash loop detection.
//!
//! Replaces the flat `crash_restarts` counter with a sliding-window approach.
//! Crashes outside the window are forgotten, and a stability period of healthy
//! running resets the crash history entirely.

use color_eyre::eyre::{self, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Decision from the crash loop detector after recording a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashLoopDecision {
    /// Restart allowed — crash count within budget.
    AllowRestart,
    /// Too many crashes in the rolling window — stop the pipeline.
    StopCrashLoop,
}

/// Sliding-window crash loop detector.
///
/// Tracks crash timestamps in a bounded deque. Only crashes within the
/// rolling `window` count toward the threshold. A sustained healthy period
/// (`stability_period`) clears the history entirely.
#[derive(Debug)]
pub struct CrashLoopDetector {
    /// Timestamps of recent crashes (within window).
    crash_times: VecDeque<Instant>,
    /// Max crashes allowed in the window before stopping.
    max_crashes: u32,
    /// Rolling window duration.
    window: Duration,
    /// How long healthy running must last to reset crash counters.
    stability_period: Duration,
    /// When the current healthy streak started.
    healthy_since: Option<Instant>,
    /// Total lifetime restarts (for PipelineResult compatibility).
    total_restarts: u32,
    /// Absolute ceiling on total restarts regardless of window.
    ///
    /// The sliding window alone is defeated when each recovery session survives
    /// long enough (3-10 min) to push earlier crashes out of the window — you
    /// can have infinite crashes as long as each run spaces them ~3+ min apart.
    /// This ceiling provides a hard stop after N total restarts per run.
    max_total_restarts: u32,
}

impl CrashLoopDetector {
    pub fn new(max_crashes: u32, window_secs: u64, stability_secs: u64) -> Self {
        Self::with_total_limit(max_crashes, window_secs, stability_secs, max_crashes * 2)
    }

    pub fn with_total_limit(
        max_crashes: u32,
        window_secs: u64,
        stability_secs: u64,
        max_total: u32,
    ) -> Self {
        // Enforce minimum stability period to prevent trivial crash-history resets (FLAW-007).
        // A 1-second healthy runtime should not clear crash counters.
        const MIN_STABILITY_SECS: u64 = 60;
        let effective_stability = if stability_secs > 0 && stability_secs < MIN_STABILITY_SECS {
            tracing::warn!(
                requested = stability_secs,
                enforced = MIN_STABILITY_SECS,
                "stability_period below minimum — enforcing floor"
            );
            MIN_STABILITY_SECS
        } else {
            stability_secs
        };
        Self {
            crash_times: VecDeque::new(),
            max_crashes,
            window: Duration::from_secs(window_secs),
            stability_period: Duration::from_secs(effective_stability),
            healthy_since: None,
            total_restarts: 0,
            max_total_restarts: max_total.max(max_crashes),
        }
    }

    pub fn from_watchdog(cfg: &crate::config::watchdog::WatchdogConfig) -> Self {
        Self::with_total_limit(
            cfg.max_crash_retries,
            cfg.crash_window_secs,
            cfg.crash_stability_secs,
            cfg.max_total_crash_retries,
        )
    }

    /// Record a crash/restart. Returns whether to continue or stop.
    ///
    /// The threshold check uses `>=`: the loop stops when crash count *reaches*
    /// `max_crashes`, not when it *exceeds* it.  With `max_crashes = 5` the
    /// detector allows crashes 1–4 and stops on the 5th.
    pub fn record_restart(&mut self) -> CrashLoopDecision {
        let now = Instant::now();
        self.total_restarts += 1;
        self.crash_times.push_back(now);
        self.healthy_since = None; // Reset healthy tracking

        // Hard ceiling: stop regardless of window timing.
        // The sliding window is defeated when each recovery session survives 3-10 min
        // (pushing earlier crashes out of the window). This ensures a finite upper bound.
        if self.total_restarts >= self.max_total_restarts {
            tracing::error!(
                total_restarts = self.total_restarts,
                max_total = self.max_total_restarts,
                "Crash loop detected — {} total restarts reached ceiling of {}",
                self.total_restarts, self.max_total_restarts
            );
            return CrashLoopDecision::StopCrashLoop;
        }

        // Prune entries outside the window (use checked_sub to handle early-boot / short uptime)
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        while self.crash_times.front().is_some_and(|&t| t < cutoff) {
            self.crash_times.pop_front();
        }

        if self.crash_times.len() >= self.max_crashes as usize {
            tracing::error!(
                crashes_in_window = self.crash_times.len(),
                max_crashes = self.max_crashes,
                window_secs = self.window.as_secs(),
                "Crash loop detected — {} crashes reached threshold of {} within {}s window",
                self.crash_times.len(), self.max_crashes, self.window.as_secs()
            );
            CrashLoopDecision::StopCrashLoop
        } else {
            CrashLoopDecision::AllowRestart
        }
    }

    /// Call periodically during healthy execution.
    /// If healthy for >= stability_period, reset crash counters.
    ///
    /// State transitions:
    ///   `healthy_since == None`  →  start tracking (`Some(now)`)
    ///   elapsed < stability      →  keep waiting (no-op)
    ///   elapsed >= stability     →  clear crash history, restart tracking
    ///
    /// Invariant: after `crash_times.clear()`, `healthy_since` is always
    /// reset to `Some(now)` so the next stability window starts fresh.
    pub fn record_healthy_tick(&mut self) {
        let now = Instant::now();
        match self.healthy_since {
            // Stability period reached — clear crash history and restart tracking.
            Some(since) if now.duration_since(since) >= self.stability_period => {
                tracing::info!(
                    stability_secs = self.stability_period.as_secs(),
                    cleared_crashes = self.crash_times.len(),
                    total_restarts_before = self.total_restarts,
                    "Session stable — resetting crash counters (including total_restarts)"
                );
                self.crash_times.clear();
                self.total_restarts = 0;
                self.healthy_since = Some(now);
            }
            // First healthy tick after a crash — begin tracking stability.
            None => {
                self.healthy_since = Some(now);
            }
            // Still within stability period — keep waiting.
            _ => {}
        }
    }

    /// Record healthy runtime duration from a completed session (GAP 2 parity).
    ///
    /// Unlike `record_healthy_tick()` which requires a live `Instant` and is
    /// called periodically, this method accepts a duration in seconds and is
    /// designed for the daemon path where the detector is recreated per-event.
    ///
    /// If `duration_secs >= stability_period`, crash history is cleared entirely.
    /// Otherwise, if no healthy tracking is active, it begins now.
    pub fn record_healthy_runtime(&mut self, duration_secs: u64) {
        // Guard: a session that ran for 0 seconds was not healthy (FLAW-002 fix).
        if duration_secs == 0 {
            return;
        }
        if duration_secs >= self.stability_period.as_secs() {
            tracing::info!(
                stability_secs = self.stability_period.as_secs(),
                runtime_secs = duration_secs,
                cleared_crashes = self.crash_times.len(),
                total_restarts_before = self.total_restarts,
                "Session ran long enough to reset crash counters (including total_restarts)"
            );
            self.crash_times.clear();
            self.total_restarts = 0;
            self.healthy_since = Some(Instant::now());
        } else if self.healthy_since.is_none() {
            self.healthy_since = Some(Instant::now());
        }
    }

    /// Total restarts across the lifetime (for PipelineResult).
    pub fn total_restarts(&self) -> u32 { self.total_restarts }

    /// Crashes within the current window.
    pub fn crashes_in_window(&self) -> usize { self.crash_times.len() }

    /// Persist crash history to disk so it survives gw restarts.
    ///
    /// Converts `Instant` timestamps to Unix epoch seconds for serialization.
    /// Only crashes within the current window are persisted.
    ///
    /// # Clock drift (VEIL-001)
    ///
    /// The conversion assumes `Instant` and `SystemTime` advance at the same
    /// rate.  During NTP adjustments or suspend/resume cycles the reconstructed
    /// epoch timestamps may drift by a few seconds.  This is acceptable because
    /// the crash window is large (typically 900 s) and a small drift will not
    /// materially affect crash-loop detection.
    ///
    /// # Concurrency (VEIL-003)
    ///
    /// No file-level locking is performed.  `InstanceLock` (acquired in batch
    /// mode) and the single-session design prevent concurrent `gw` processes in
    /// the same working directory, so the race window is theoretical.  The
    /// atomic write-to-tmp-then-rename pattern protects against partial reads.
    pub fn persist(&self, working_dir: &Path) -> color_eyre::Result<()> {
        let now_instant = Instant::now();
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| eyre::eyre!("System clock before UNIX epoch: {e}"))?
            .as_secs();

        // Convert Instants to epoch seconds
        let crash_epochs: Vec<u64> = self.crash_times.iter().map(|&t| {
            let age_secs = now_instant.duration_since(t).as_secs();
            now_epoch.saturating_sub(age_secs)
        }).collect();

        // Convert healthy_since Instant to epoch for persistence (GAP 2/3)
        let last_healthy_epoch = self.healthy_since.map(|since| {
            let age = now_instant.duration_since(since).as_secs();
            now_epoch.saturating_sub(age)
        });

        let record = CrashHistoryRecord {
            crash_epochs,
            total_restarts: self.total_restarts,
            window_secs: self.window.as_secs(),
            last_healthy_epoch,
        };

        let path = Self::history_path(working_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err("Failed to create crash history directory")?;
        }

        let json = serde_json::to_string_pretty(&record)
            .wrap_err("Failed to serialize crash history")?;

        // Atomic write: write to .tmp then rename to avoid corruption on crash
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)
            .wrap_err("Failed to write crash history tmp file")?;
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).wrap_err("Failed to rename crash history tmp file");
        }

        Ok(())
    }

    /// Load crash history from a previous gw run.
    ///
    /// Converts persisted epoch timestamps back to `Instant` (approximate).
    /// Crashes older than the window are automatically pruned.
    pub fn load_history(&mut self, working_dir: &Path) {
        let path = Self::history_path(working_dir);
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // No history file — first run
        };

        let record: CrashHistoryRecord = match serde_json::from_str(&contents) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse crash history — starting fresh");
                return;
            }
        };

        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .inspect_err(|e| tracing::warn!(error = %e, "System clock before UNIX epoch"))
            .unwrap_or_default()
            .as_secs();
        let now_instant = Instant::now();
        let window_secs = self.window.as_secs();

        if record.window_secs != window_secs {
            tracing::info!(
                persisted = record.window_secs,
                current = window_secs,
                "Crash history window size changed — pruning with current window"
            );
        }

        // Restore crashes that are still within our window
        let mut restored = 0u32;
        for epoch in &record.crash_epochs {
            let age_secs = now_epoch.saturating_sub(*epoch);
            if age_secs < window_secs {
                // Reconstruct approximate Instant
                let crash_instant = now_instant - Duration::from_secs(age_secs);
                self.crash_times.push_back(crash_instant);
                restored += 1;
            }
        }

        self.total_restarts = record.total_restarts;

        // Restore healthy_since if it's still within the stability period (GAP 2/3)
        if let Some(epoch) = record.last_healthy_epoch {
            let age = now_epoch.saturating_sub(epoch);
            if age < self.stability_period.as_secs() {
                self.healthy_since = Some(now_instant - Duration::from_secs(age));
            }
        }

        if restored > 0 {
            tracing::info!(
                restored = restored,
                total_restarts = record.total_restarts,
                "Loaded crash history from previous gw run"
            );
        }

        // Don't delete the file here — persist() will overwrite it when called,
        // and clear_history() handles intentional cleanup. Deleting on load means
        // a crash between load and the first persist loses all crash history,
        // allowing infinite restarts.
    }

    /// Remove persisted crash history (call on clean completion).
    ///
    /// `remove_file` is atomic on POSIX, and `InstanceLock` prevents concurrent
    /// `gw` processes in the same working directory, so no additional locking is
    /// needed.  A racing `load_history` call would either see the file or get
    /// `ENOENT` — both are safe.
    pub fn clear_history(working_dir: &Path) {
        let path = Self::history_path(working_dir);
        let _ = std::fs::remove_file(&path);
    }

    fn history_path(working_dir: &Path) -> std::path::PathBuf {
        working_dir.join(".gw").join("crash-history.json")
    }
}

/// Serializable crash history for persistence across gw restarts.
#[derive(Debug, Serialize, Deserialize)]
struct CrashHistoryRecord {
    /// Crash timestamps as Unix epoch seconds.
    crash_epochs: Vec<u64>,
    /// Total lifetime restarts.
    total_restarts: u32,
    /// Window size used when this history was written.
    window_secs: u64,
    /// When healthy running started (Unix epoch). `None` if no healthy period
    /// was active when the history was persisted. Added for GAP 2/3 parity —
    /// old files without this field deserialize correctly (`Option` defaults to `None`).
    #[serde(default)]
    last_healthy_epoch: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_under_threshold_allows_restart() {
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        for _ in 0..4 {
            assert!(matches!(d.record_restart(), CrashLoopDecision::AllowRestart));
        }
    }

    #[test]
    fn test_at_threshold_stops() {
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        for _ in 0..4 {
            d.record_restart();
        }
        assert!(matches!(d.record_restart(), CrashLoopDecision::StopCrashLoop));
    }

    #[test]
    fn test_total_restarts_tracks_lifetime() {
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.record_restart();
        d.record_restart();
        d.record_restart();
        assert_eq!(d.total_restarts(), 3);
    }

    #[test]
    fn test_crashes_in_window() {
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.record_restart();
        d.record_restart();
        assert_eq!(d.crashes_in_window(), 2);
    }

    #[test]
    fn test_stability_reset() {
        let mut d = CrashLoopDetector::new(5, 900, 0); // 0 = immediate stability
        d.record_restart();
        d.record_restart();
        assert_eq!(d.crashes_in_window(), 2);
        // Simulate healthy period — stability_period is 0, so immediate reset
        d.healthy_since = Some(Instant::now() - Duration::from_secs(1));
        d.record_healthy_tick();
        assert_eq!(d.crashes_in_window(), 0);
    }

    #[test]
    fn test_total_ceiling_stops_spaced_crashes() {
        // Simulates the real bug: crashes spaced far enough apart that the
        // sliding window never fills, but total restarts hit the ceiling.
        // With max_total=6, window=5, window never triggers but total does.
        let mut d = CrashLoopDetector::with_total_limit(5, 900, 1800, 6);

        // Simulate 5 crashes that individually fall out of the window
        // (by clearing window state between each, mimicking time passage)
        for i in 0..5 {
            let decision = d.record_restart();
            assert!(
                matches!(decision, CrashLoopDecision::AllowRestart),
                "Crash {} should be allowed", i + 1
            );
            // Simulate time passage: clear window entries (as if they expired)
            d.crash_times.clear();
        }

        // 6th crash hits the total ceiling
        assert!(matches!(d.record_restart(), CrashLoopDecision::StopCrashLoop));
        assert_eq!(d.total_restarts(), 6);
    }

    #[test]
    fn test_from_watchdog_config() {
        let cfg = crate::config::watchdog::WatchdogConfig::from_env();
        let d = CrashLoopDetector::from_watchdog(&cfg);
        assert_eq!(d.max_crashes, cfg.max_crash_retries);
        assert_eq!(d.max_total_restarts, cfg.max_total_crash_retries);
    }

    #[test]
    fn test_persist_and_load_history() {
        let dir = tempfile::TempDir::new().unwrap();

        // Create a detector with some crashes
        let mut d1 = CrashLoopDetector::new(5, 900, 1800);
        d1.record_restart();
        d1.record_restart();
        assert_eq!(d1.total_restarts(), 2);
        assert_eq!(d1.crashes_in_window(), 2);

        // Persist
        d1.persist(dir.path()).unwrap();
        assert!(dir.path().join(".gw").join("crash-history.json").exists());

        // Load into a fresh detector
        let mut d2 = CrashLoopDetector::new(5, 900, 1800);
        d2.load_history(dir.path());
        assert_eq!(d2.total_restarts(), 2);
        assert_eq!(d2.crashes_in_window(), 2);

        // History file is preserved until persist() overwrites or clear_history() deletes
        assert!(dir.path().join(".gw").join("crash-history.json").exists());
    }

    #[test]
    fn test_load_missing_history_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.load_history(dir.path()); // no file — should not panic
        assert_eq!(d.total_restarts(), 0);
    }

    #[test]
    fn test_clear_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.record_restart();
        d.persist(dir.path()).unwrap();
        assert!(dir.path().join(".gw").join("crash-history.json").exists());

        CrashLoopDetector::clear_history(dir.path());
        assert!(!dir.path().join(".gw").join("crash-history.json").exists());
    }

    #[test]
    fn test_persisted_crashes_respect_window() {
        let dir = tempfile::TempDir::new().unwrap();

        // Manually write a history with an old crash (expired) and a recent one
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let record = CrashHistoryRecord {
            crash_epochs: vec![
                now_epoch - 2000, // older than 900s window → should be pruned
                now_epoch - 10,   // recent → should be kept
            ],
            total_restarts: 5,
            window_secs: 900,
            last_healthy_epoch: None,
        };

        let gw_dir = dir.path().join(".gw");
        std::fs::create_dir_all(&gw_dir).unwrap();
        std::fs::write(
            gw_dir.join("crash-history.json"),
            serde_json::to_string(&record).unwrap(),
        ).unwrap();

        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.load_history(dir.path());
        assert_eq!(d.total_restarts(), 5); // total preserved
        assert_eq!(d.crashes_in_window(), 1); // only recent one survives window
    }

    #[test]
    fn test_record_healthy_runtime_zero_is_noop() {
        // FLAW-002: duration=0 should NOT clear crashes, even with stability_period=0
        let mut d = CrashLoopDetector::new(5, 900, 0); // stability_period=0
        d.record_restart();
        d.record_restart();
        assert_eq!(d.crashes_in_window(), 2);

        d.record_healthy_runtime(0); // instant crash — not healthy
        assert_eq!(d.crashes_in_window(), 2); // crashes preserved
    }

    #[test]
    fn test_record_healthy_runtime_resets_crashes() {
        // GAP 2: Long healthy runtime should clear crash counters
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.record_restart();
        d.record_restart();
        d.record_restart();
        d.record_restart();
        assert_eq!(d.crashes_in_window(), 4);

        // Runtime exceeds stability_period (1800s) → crashes cleared
        d.record_healthy_runtime(1800);
        assert_eq!(d.crashes_in_window(), 0);
        assert!(d.healthy_since.is_some());
    }

    #[test]
    fn test_record_healthy_runtime_short_starts_tracking() {
        // Short runtime below stability_period starts tracking but doesn't clear
        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.record_restart();
        d.record_restart();
        assert_eq!(d.crashes_in_window(), 2);
        assert!(d.healthy_since.is_none()); // reset by record_restart

        d.record_healthy_runtime(600); // below 1800s threshold
        assert_eq!(d.crashes_in_window(), 2); // crashes preserved
        assert!(d.healthy_since.is_some()); // tracking started
    }

    #[test]
    fn test_persist_and_load_with_healthy_epoch() {
        // GAP 2/3: Roundtrip test for last_healthy_epoch persistence
        let dir = tempfile::TempDir::new().unwrap();

        let mut d1 = CrashLoopDetector::new(5, 900, 1800);
        d1.record_restart();
        // Set healthy_since to now (simulating a healthy period started)
        d1.healthy_since = Some(Instant::now());
        d1.persist(dir.path()).unwrap();

        // Load into a fresh detector
        let mut d2 = CrashLoopDetector::new(5, 900, 1800);
        d2.load_history(dir.path());
        assert_eq!(d2.total_restarts(), 1);
        // healthy_since should be restored (within stability period)
        assert!(d2.healthy_since.is_some());
    }

    #[test]
    fn test_load_old_history_without_healthy_epoch() {
        // Backward compat: old files without last_healthy_epoch load correctly
        let dir = tempfile::TempDir::new().unwrap();
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Simulate an old-format history file (no last_healthy_epoch field)
        let json = serde_json::json!({
            "crash_epochs": [now_epoch - 10],
            "total_restarts": 1,
            "window_secs": 900
        });

        let gw_dir = dir.path().join(".gw");
        std::fs::create_dir_all(&gw_dir).unwrap();
        std::fs::write(
            gw_dir.join("crash-history.json"),
            json.to_string(),
        ).unwrap();

        let mut d = CrashLoopDetector::new(5, 900, 1800);
        d.load_history(dir.path());
        assert_eq!(d.total_restarts(), 1);
        assert_eq!(d.crashes_in_window(), 1);
        assert!(d.healthy_since.is_none()); // not set (old format)
    }
}
