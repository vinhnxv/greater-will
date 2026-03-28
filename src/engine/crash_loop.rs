//! Windowed crash loop detection.
//!
//! Replaces the flat `crash_restarts` counter with a sliding-window approach.
//! Crashes outside the window are forgotten, and a stability period of healthy
//! running resets the crash history entirely.

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
}

impl CrashLoopDetector {
    pub fn new(max_crashes: u32, window_secs: u64, stability_secs: u64) -> Self {
        Self {
            crash_times: VecDeque::new(),
            max_crashes,
            window: Duration::from_secs(window_secs),
            stability_period: Duration::from_secs(stability_secs),
            healthy_since: None,
            total_restarts: 0,
        }
    }

    pub fn from_watchdog(cfg: &crate::config::watchdog::WatchdogConfig) -> Self {
        Self::new(cfg.max_crash_retries, cfg.crash_window_secs, cfg.crash_stability_secs)
    }

    /// Record a crash/restart. Returns whether to continue or stop.
    pub fn record_restart(&mut self) -> CrashLoopDecision {
        let now = Instant::now();
        self.total_restarts += 1;
        self.crash_times.push_back(now);
        self.healthy_since = None; // Reset healthy tracking

        // Prune entries outside the window
        let cutoff = now - self.window;
        while self.crash_times.front().map_or(false, |&t| t < cutoff) {
            self.crash_times.pop_front();
        }

        if self.crash_times.len() >= self.max_crashes as usize {
            tracing::error!(
                crashes_in_window = self.crash_times.len(),
                window_secs = self.window.as_secs(),
                "Crash loop detected — too many crashes in window"
            );
            CrashLoopDecision::StopCrashLoop
        } else {
            CrashLoopDecision::AllowRestart
        }
    }

    /// Call periodically during healthy execution.
    /// If healthy for >= stability_period, reset crash counters.
    pub fn record_healthy_tick(&mut self) {
        let now = Instant::now();
        match self.healthy_since {
            Some(since) if now.duration_since(since) >= self.stability_period => {
                tracing::info!(
                    stability_secs = self.stability_period.as_secs(),
                    cleared_crashes = self.crash_times.len(),
                    "Session stable — resetting crash counters"
                );
                self.crash_times.clear();
                self.healthy_since = Some(now);
            }
            None => {
                self.healthy_since = Some(now);
            }
            _ => {} // Still within stability period
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
    pub fn persist(&self, working_dir: &Path) {
        let now_instant = Instant::now();
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Convert Instants to epoch seconds
        let crash_epochs: Vec<u64> = self.crash_times.iter().map(|&t| {
            let age_secs = now_instant.duration_since(t).as_secs();
            now_epoch.saturating_sub(age_secs)
        }).collect();

        let record = CrashHistoryRecord {
            crash_epochs,
            total_restarts: self.total_restarts,
            window_secs: self.window.as_secs(),
        };

        let path = Self::history_path(working_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match serde_json::to_string_pretty(&record) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(error = %e, "Failed to persist crash history");
                }
            }
            Err(e) => tracing::warn!(error = %e, "Failed to serialize crash history"),
        }
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

        if restored > 0 {
            tracing::info!(
                restored = restored,
                total_restarts = record.total_restarts,
                "Loaded crash history from previous gw run"
            );
        }

        // Clean up the history file after loading
        let _ = std::fs::remove_file(&path);
    }

    /// Remove persisted crash history (call on clean completion).
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
    fn test_from_watchdog_config() {
        let cfg = crate::config::watchdog::WatchdogConfig::from_env();
        let d = CrashLoopDetector::from_watchdog(&cfg);
        assert_eq!(d.max_crashes, cfg.max_crash_retries);
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
        d1.persist(dir.path());
        assert!(dir.path().join(".gw").join("crash-history.json").exists());

        // Load into a fresh detector
        let mut d2 = CrashLoopDetector::new(5, 900, 1800);
        d2.load_history(dir.path());
        assert_eq!(d2.total_restarts(), 2);
        assert_eq!(d2.crashes_in_window(), 2);

        // History file should be cleaned up after load
        assert!(!dir.path().join(".gw").join("crash-history.json").exists());
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
        d.persist(dir.path());
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
}
