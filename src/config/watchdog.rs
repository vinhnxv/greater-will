//! Watchdog tuning parameters.
//!
//! All thresholds are configurable via environment variables with sensible
//! defaults. This module centralizes the knobs that control how aggressively
//! Greater-Will intervenes in Claude Code sessions.
//!
//! # Environment Variables
//!
//! | Variable | Default | Unit | Description |
//! |----------|---------|------|-------------|
//! | `GW_IDLE_NUDGE_SECS` | 300 | seconds | Send nudge after this idle time |
//! | `GW_IDLE_KILL_SECS` | 1800 | seconds | Kill session after this idle time |
//! | `GW_ERROR_STALL_THRESHOLD_SECS` | 60 | seconds | Screen/checkpoint stale threshold for error confidence |
//! | `GW_ARTIFACT_SCAN_INTERVAL_SECS` | 15 | seconds | How often to scan artifact directory |
//! | `GW_PIPELINE_TIMEOUT_SECS` | 21600 | seconds | Total pipeline timeout (6 hours) |
//! | `GW_CHECKPOINT_POLL_INTERVAL_SECS` | 10 | seconds | How often to check checkpoint file |
//! | `GW_PROMPT_ACCEPT` | 1 | bool (0/1) | Enable auto-accept for permission prompts |
//! | `GW_PROMPT_ACCEPT_DEBOUNCE_SECS` | 5 | seconds | Debounce between auto-accepts |
//! | `GW_CRASH_WINDOW_SECS` | 900 | seconds | Rolling window for crash loop detection |
//! | `GW_CRASH_STABILITY_SECS` | 1800 | seconds | Healthy running time to reset crash counters |
//! | `GW_LOOP_STATE_WARMUP_SECS` | 300 | seconds | Max wait for arc-phase-loop.local.md to appear |

use std::time::Duration;

/// Watchdog configuration — controls how aggressively GW intervenes.
///
/// Loaded from environment variables with defaults tuned for arc pipeline
/// execution where individual phases can take 30+ minutes.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// Idle time before sending a nudge (Enter key) to unstick session.
    pub idle_nudge_secs: u64,
    /// Idle time before killing a stuck session.
    /// Default 1800s (30 min) — allows long-running phases to complete.
    pub idle_kill_secs: u64,
    /// Screen/checkpoint staleness threshold for error confidence scoring.
    /// Below this, checkpoint is considered "fresh" (negative signal).
    pub error_stall_threshold_secs: u64,
    /// How often to scan the artifact directory for changes.
    pub artifact_scan_interval_secs: u64,
    /// Total pipeline timeout.
    pub pipeline_timeout: Duration,
    /// How often to check the checkpoint file.
    pub checkpoint_poll_interval_secs: u64,
    /// Maximum crash-recovery restarts before giving up.
    pub max_crash_retries: u32,
    /// Confirmation period for medium-confidence errors (0.5-0.8), in seconds.
    pub error_confirm_medium_secs: u64,
    /// Confirmation period for high-confidence errors (>= 0.8), in seconds.
    pub error_confirm_high_secs: u64,
    /// Whether to auto-accept permission prompts (y/n dialogs).
    /// Default: enabled. Set GW_PROMPT_ACCEPT=0 to disable.
    pub prompt_accept_enabled: bool,
    /// Debounce interval between auto-accepts (seconds).
    pub prompt_accept_debounce_secs: u64,
    /// Rolling window for crash loop detection (seconds). Default 900 (15 min).
    pub crash_window_secs: u64,
    /// How long healthy running resets crash counters (seconds). Default 1800 (30 min).
    pub crash_stability_secs: u64,
    /// Max time to wait for `arc-phase-loop.local.md` to appear after session start.
    /// If the file doesn't appear within this window, the session is treated as crashed.
    /// Default 180s (3 min) — used as a soft deadline. The warmup timeout only
    /// triggers if the session has exited or screen output is idle. While Claude
    /// Code is still alive and producing output, we keep waiting indefinitely.
    pub loop_state_warmup_secs: u64,
    /// Cooldown between crash-recovery restarts (seconds). Default 60s (1 min).
    /// Gives Claude Code time to fully initialize before the next attempt.
    pub restart_cooldown_secs: u64,
}

impl WatchdogConfig {
    /// Load configuration from environment variables with defaults.
    pub fn from_env() -> Self {
        Self {
            idle_nudge_secs: env_or("GW_IDLE_NUDGE_SECS", 300).clamp(30, 3600),   // 5 min
            idle_kill_secs: env_or("GW_IDLE_KILL_SECS", 1800).clamp(60, 86400), // 30 min
            error_stall_threshold_secs: env_or("GW_ERROR_STALL_THRESHOLD_SECS", 60).clamp(10, 3600),
            artifact_scan_interval_secs: env_or("GW_ARTIFACT_SCAN_INTERVAL_SECS", 15).clamp(5, 300),
            pipeline_timeout: Duration::from_secs(
                env_or("GW_PIPELINE_TIMEOUT_SECS", 6 * 3600).clamp(60, 86400),
            ),
            checkpoint_poll_interval_secs: env_or("GW_CHECKPOINT_POLL_INTERVAL_SECS", 10).clamp(1, 300),
            max_crash_retries: env_or("GW_MAX_CRASH_RETRIES", 5).clamp(1, 20) as u32,
            error_confirm_medium_secs: env_or("GW_ERROR_CONFIRM_MEDIUM_SECS", 15 * 60),
            error_confirm_high_secs: env_or("GW_ERROR_CONFIRM_HIGH_SECS", 5 * 60),
            prompt_accept_enabled: env_or("GW_PROMPT_ACCEPT", 1) != 0,
            prompt_accept_debounce_secs: env_or("GW_PROMPT_ACCEPT_DEBOUNCE_SECS", 5),
            crash_window_secs: env_or("GW_CRASH_WINDOW_SECS", 900).clamp(60, 7200),
            crash_stability_secs: env_or("GW_CRASH_STABILITY_SECS", 1800).clamp(60, 7200),
            loop_state_warmup_secs: env_or("GW_LOOP_STATE_WARMUP_SECS", 300).clamp(30, 1800),
            restart_cooldown_secs: env_or("GW_RESTART_COOLDOWN_SECS", 60).clamp(5, 600),
        }
    }
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

fn env_or(key: &str, default: u64) -> u64 {
    match std::env::var(key) {
        Ok(val) => match val.parse() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    key = %key,
                    value = ?val,
                    default = default,
                    "Invalid env var value, using default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let config = WatchdogConfig::from_env();
        // These are the defaults when no env vars are set
        assert_eq!(config.idle_nudge_secs, 300);
        assert_eq!(config.idle_kill_secs, 1800);
        assert_eq!(config.error_stall_threshold_secs, 60);
        assert_eq!(config.artifact_scan_interval_secs, 15);
        assert_eq!(config.checkpoint_poll_interval_secs, 10);
        assert_eq!(config.pipeline_timeout, Duration::from_secs(6 * 3600));
        assert_eq!(config.max_crash_retries, 5);
        assert_eq!(config.error_confirm_medium_secs, 15 * 60);
        assert_eq!(config.error_confirm_high_secs, 5 * 60);
        assert!(config.prompt_accept_enabled);
        assert_eq!(config.prompt_accept_debounce_secs, 5);
        assert_eq!(config.crash_window_secs, 900);
        assert_eq!(config.crash_stability_secs, 1800);
        assert_eq!(config.loop_state_warmup_secs, 300);
        assert_eq!(config.restart_cooldown_secs, 60);
    }
}
