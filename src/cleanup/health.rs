//! System health checks for process spawning.
//!
//! Provides health gates to ensure system resources are available
//! before spawning new Claude Code sessions.

use color_eyre::{eyre::eyre, Result};
use std::time::{Duration, Instant};
use sysinfo::System;

/// Minimum free RAM required (500 MB).
const MIN_FREE_RAM_MB: u64 = 500;

/// Maximum CPU usage percentage allowed.
const MAX_CPU_PERCENT: f32 = 90.0;

/// Maximum time to wait for resources to become available.
const HEALTH_WAIT_TIMEOUT_SECS: u64 = 60;

/// Interval between health check retries.
const HEALTH_CHECK_INTERVAL_SECS: u64 = 5;

/// Error types for health checks.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HealthError {
    /// Insufficient RAM available.
    #[error("Insufficient RAM: {available_mb}MB available, {required_mb}MB required")]
    InsufficientRam {
        available_mb: u64,
        required_mb: u64,
    },

    /// CPU usage too high.
    #[error("CPU usage too high: {usage}% (max {max}%)")]
    HighCpuUsage { usage: f32, max: f32 },

    /// Timeout waiting for resources.
    #[error("Timeout waiting for resources after {seconds}s")]
    Timeout { seconds: u64 },
}

/// System health information.
#[derive(Debug, Clone)]
pub struct SystemHealth {
    /// Total RAM in bytes.
    pub total_ram: u64,
    /// Available RAM in bytes.
    pub available_ram: u64,
    /// Free RAM in bytes.
    pub free_ram: u64,
    /// CPU usage percentage (0.0 - 100.0).
    pub cpu_usage: f32,
    /// Whether the system passed health checks.
    pub is_healthy: bool,
    /// Reason for unhealthy status (if applicable).
    pub unhealthy_reason: Option<String>,
}

impl SystemHealth {
    /// Create a system health snapshot.
    pub fn snapshot(sys: &System) -> Self {
        let total_ram = sys.total_memory();
        let available_ram = sys.available_memory();
        let free_ram = sys.free_memory();

        // Calculate aggregate CPU usage
        let cpu_usage = sys.global_cpu_usage();

        Self {
            total_ram,
            available_ram,
            free_ram,
            cpu_usage,
            is_healthy: true,
            unhealthy_reason: None,
        }
    }

    /// Check if available RAM meets minimum requirement.
    pub fn has_sufficient_ram(&self) -> bool {
        let available_mb = self.available_ram / 1024 / 1024;
        available_mb >= MIN_FREE_RAM_MB
    }

    /// Check if CPU usage is within acceptable limits.
    pub fn has_acceptable_cpu(&self) -> bool {
        self.cpu_usage < MAX_CPU_PERCENT
    }

    /// Get available RAM in megabytes.
    pub fn available_ram_mb(&self) -> u64 {
        self.available_ram / 1024 / 1024
    }
}

/// Health check coordinator.
///
/// Provides methods to check system health and wait for resources.
pub struct HealthCheck {
    /// Minimum free RAM required in bytes.
    min_free_ram: u64,
    /// Maximum CPU usage percentage allowed.
    max_cpu_percent: f32,
    /// Maximum time to wait for resources.
    wait_timeout: Duration,
    /// Interval between retries.
    check_interval: Duration,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            min_free_ram: MIN_FREE_RAM_MB * 1024 * 1024,
            max_cpu_percent: MAX_CPU_PERCENT,
            wait_timeout: Duration::from_secs(HEALTH_WAIT_TIMEOUT_SECS),
            check_interval: Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS),
        }
    }
}

impl HealthCheck {
    /// Create a new health check with default thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a health check with custom thresholds.
    pub fn with_thresholds(min_ram_mb: u64, max_cpu: f32) -> Self {
        Self {
            min_free_ram: min_ram_mb * 1024 * 1024,
            max_cpu_percent: max_cpu,
            ..Self::default()
        }
    }

    /// Set the wait timeout for resource availability.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.wait_timeout = timeout;
        self
    }

    /// Check current system health.
    ///
    /// Returns a snapshot of system health without waiting.
    pub fn check(&self) -> SystemHealth {
        let mut sys = System::new_all();
        sys.refresh_all();
        sys.refresh_cpu_all();

        let mut health = SystemHealth::snapshot(&sys);

        // Check RAM
        if !health.has_sufficient_ram() {
            health.is_healthy = false;
            health.unhealthy_reason = Some(format!(
                "Insufficient RAM: {}MB available, {}MB required",
                health.available_ram_mb(),
                MIN_FREE_RAM_MB
            ));
        }

        // Check CPU
        if !health.has_acceptable_cpu() {
            health.is_healthy = false;
            health.unhealthy_reason = Some(format!(
                "CPU usage too high: {:.1}% (max {:.1}%)",
                health.cpu_usage, self.max_cpu_percent
            ));
        }

        health
    }

    /// Wait for system to become healthy.
    ///
    /// Polls system health until all thresholds are met or timeout expires.
    ///
    /// # Returns
    ///
    /// `Ok(SystemHealth)` when healthy, or `Err(HealthError)` on timeout.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use greater_will::cleanup::health::HealthCheck;
    ///
    /// let checker = HealthCheck::new();
    /// match checker.wait_for_health() {
    ///     Ok(health) => println!("System healthy: {}MB RAM available", health.available_ram_mb()),
    ///     Err(e) => println!("Health check failed: {}", e),
    /// }
    /// ```
    pub fn wait_for_health(&self) -> Result<SystemHealth, HealthError> {
        let start = Instant::now();

        loop {
            let health = self.check();

            if health.is_healthy {
                tracing::info!(
                    available_ram_mb = health.available_ram_mb(),
                    cpu_usage = health.cpu_usage,
                    "System health check passed"
                );
                return Ok(health);
            }

            let elapsed = start.elapsed();
            if elapsed >= self.wait_timeout {
                tracing::error!(
                    reason = ?health.unhealthy_reason,
                    elapsed_secs = elapsed.as_secs(),
                    "Health check timeout"
                );
                return Err(HealthError::Timeout {
                    seconds: self.wait_timeout.as_secs(),
                });
            }

            tracing::warn!(
                reason = ?health.unhealthy_reason,
                remaining_secs = (self.wait_timeout - elapsed).as_secs(),
                "Waiting for system resources"
            );

            std::thread::sleep(self.check_interval);
        }
    }

    /// Check health and wait if necessary.
    ///
    /// Convenience method that combines check and wait.
    ///
    /// # Returns
    ///
    /// `Ok(())` if healthy or became healthy within timeout.
    /// `Err(...)` if timeout expired.
    pub fn ensure_healthy(&self) -> Result<()> {
        self.wait_for_health()
            .map(|_| ())
            .map_err(|e| eyre!("Health check failed: {}", e))
    }

    /// Quick health check without waiting.
    ///
    /// Returns immediately with current health status.
    pub fn quick_check(&self) -> Result<SystemHealth> {
        let health = self.check();
        if health.is_healthy {
            Ok(health)
        } else {
            Err(eyre!(
                "System unhealthy: {}",
                health.unhealthy_reason.unwrap_or_else(|| "Unknown reason".to_string())
            ))
        }
    }
}

/// Global function for pre-spawn health gate.
///
/// Checks system health and waits up to 60 seconds for resources.
/// Returns `Ok(())` if healthy, or aborts with error.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::health::check_system_health;
///
/// check_system_health()?;
/// // Safe to spawn new session
/// ```
pub fn check_system_health() -> Result<SystemHealth> {
    let checker = HealthCheck::new();
    checker.wait_for_health().map_err(|e| eyre!("{}", e))
}

/// Quick health check that returns immediately.
///
/// Use this for non-critical checks where you just want to know
/// the current state without waiting.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::health::quick_health_check;
///
/// if let Ok(health) = quick_health_check() {
///     println!("CPU: {}%, RAM: {}MB free", health.cpu_usage, health.available_ram_mb());
/// }
/// ```
pub fn quick_health_check() -> Result<SystemHealth> {
    let checker = HealthCheck::new();
    checker.quick_check()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_check_new() {
        let checker = HealthCheck::new();
        assert_eq!(checker.min_free_ram, MIN_FREE_RAM_MB * 1024 * 1024);
        assert_eq!(checker.max_cpu_percent, MAX_CPU_PERCENT);
    }

    #[test]
    fn test_health_check_custom_thresholds() {
        let checker = HealthCheck::with_thresholds(1000, 80.0);
        assert_eq!(checker.min_free_ram, 1000 * 1024 * 1024);
        assert_eq!(checker.max_cpu_percent, 80.0);
    }

    #[test]
    fn test_system_health_snapshot() {
        let mut sys = System::new_all();
        sys.refresh_all();
        sys.refresh_cpu_all();

        let health = SystemHealth::snapshot(&sys);

        // Basic sanity checks
        assert!(health.total_ram > 0);
        assert!(health.available_ram <= health.total_ram);
        assert!(health.cpu_usage >= 0.0);
    }

    #[test]
    fn test_quick_check() {
        let result = quick_health_check();
        // Just verify it doesn't panic
        // The actual result depends on system state
        assert!(result.is_ok() || result.is_err());
    }
}