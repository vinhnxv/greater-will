//! Connectivity probing for internet recovery.
//!
//! Provides non-blocking connectivity checks used by the daemon to
//! detect network outages and wait for restoration before retrying
//! failed runs.
//!
//! ## Design
//!
//! - TCP connect (not ICMP) — no root privileges needed.
//! - Primary probe: `api.anthropic.com:443` — the actual endpoint Claude
//!   Code connects to. If this specific host is down, sessions will fail
//!   regardless of general internet availability.
//! - Fallback: `1.1.1.1:53` — Cloudflare DNS, highly available.
//! - All blocking I/O wrapped in `spawn_blocking` to avoid stalling the
//!   tokio runtime (matches `run_monitor.rs` precedent for `capture_pane`).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Probe targets in priority order. First reachable = online.
const PROBE_TARGETS: &[(&str, u16)] = &[
    ("api.anthropic.com", 443),
    ("1.1.1.1", 53),
];

/// TCP connect timeout per target.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default polling interval for `wait_for_connectivity`.
pub const CONNECTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum wait time for connectivity restoration (30 minutes).
pub const CONNECTIVITY_MAX_WAIT: Duration = Duration::from_secs(30 * 60);

/// Check if internet connectivity is available (blocking).
///
/// Tries each probe target in order. Returns `true` on first successful
/// TCP connect. DNS resolution failure counts as "offline".
pub fn is_online() -> bool {
    PROBE_TARGETS.iter().any(|(host, port)| {
        let addr = format!("{}:{}", host, port);
        // DNS resolution can itself block and fail — both count as offline.
        let resolved: Vec<SocketAddr> = match addr.to_socket_addrs() {
            Ok(addrs) => addrs.collect(),
            Err(_) => return false,
        };
        resolved.iter().any(|sa| {
            TcpStream::connect_timeout(sa, PROBE_TIMEOUT).is_ok()
        })
    })
}

/// Async-safe connectivity check via `spawn_blocking`.
///
/// MUST use spawn_blocking — `TcpStream::connect_timeout` blocks the
/// calling thread for up to `PROBE_TIMEOUT` (5s). Blocking a tokio
/// worker thread on a 4-core machine loses 25% async capacity.
pub async fn is_online_async() -> bool {
    tokio::task::spawn_blocking(is_online)
        .await
        .unwrap_or(false)
}

/// Wait until connectivity returns, polling every `interval`.
///
/// Returns `true` if connectivity restored, `false` if cancelled or
/// `max_wait` exceeded. Uses `tokio::select!` with the cancellation
/// token so `gw stop` or daemon shutdown can interrupt the wait.
pub async fn wait_for_connectivity(
    interval: Duration,
    max_wait: Duration,
    cancel: CancellationToken,
) -> bool {
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut poll_interval = tokio::time::interval(interval);
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the initial immediate tick
    poll_interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return false,
            _ = tokio::time::sleep_until(deadline) => {
                // Final attempt at deadline
                return is_online_async().await;
            }
            _ = poll_interval.tick() => {
                if is_online_async().await {
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: we cannot unit-test actual network connectivity in CI.
    // These tests validate the module's public API shape and basic logic.

    #[test]
    fn probe_targets_are_configured() {
        assert!(!PROBE_TARGETS.is_empty());
        assert_eq!(PROBE_TARGETS[0].0, "api.anthropic.com");
        assert_eq!(PROBE_TARGETS[0].1, 443);
    }

    #[test]
    fn probe_timeout_is_reasonable() {
        assert!(PROBE_TIMEOUT.as_secs() >= 1);
        assert!(PROBE_TIMEOUT.as_secs() <= 30);
    }

    #[test]
    fn is_online_returns_bool() {
        // Basic smoke test: is_online returns a bool without panicking.
        // Actual result depends on CI/dev environment connectivity.
        let result = is_online();
        // Type assertion — if this compiles, the return type is bool.
        let _: bool = result;
    }

    #[tokio::test]
    async fn wait_for_connectivity_cancelled() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Cancel immediately before waiting
        cancel_clone.cancel();

        let result = wait_for_connectivity(
            Duration::from_millis(100),
            Duration::from_secs(60),
            cancel,
        )
        .await;

        assert!(!result, "should return false when cancelled");
    }

    #[test]
    fn network_state_transitions() {
        use crate::daemon::state::NetworkState;

        // Start Online
        let mut state = NetworkState::Online;
        assert_eq!(state, NetworkState::Online);

        // Transition to WaitingForNetwork
        let now = chrono::Utc::now();
        state = NetworkState::WaitingForNetwork { since: now };
        assert!(matches!(state, NetworkState::WaitingForNetwork { .. }));

        // Display shows "waiting"
        let display = format!("{state}");
        assert!(
            display.contains("waiting"),
            "WaitingForNetwork display should contain 'waiting', got: {display}"
        );

        // Transition back to Online
        state = NetworkState::Online;
        assert_eq!(state, NetworkState::Online);
        assert_eq!(format!("{state}"), "online");
    }

    #[test]
    fn network_state_default_is_online() {
        use crate::daemon::state::NetworkState;
        let state = NetworkState::default();
        assert_eq!(state, NetworkState::Online);
    }

    #[test]
    fn network_state_serde_round_trip() {
        use crate::daemon::state::NetworkState;

        let online = NetworkState::Online;
        let json = serde_json::to_string(&online).unwrap();
        let deser: NetworkState = serde_json::from_str(&json).unwrap();
        assert_eq!(online, deser);

        let waiting = NetworkState::WaitingForNetwork {
            since: chrono::Utc::now(),
        };
        let json2 = serde_json::to_string(&waiting).unwrap();
        let deser2: NetworkState = serde_json::from_str(&json2).unwrap();
        assert_eq!(waiting, deser2);
    }
}
