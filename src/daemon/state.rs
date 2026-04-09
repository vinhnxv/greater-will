//! Daemon state directory management.
//!
//! Manages the `~/.gw/` directory hierarchy where the daemon stores
//! run state, configuration, and per-repo metadata.

use color_eyre::{eyre::WrapErr, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{debug, warn};

/// Default retention limit for completed runs.
const DEFAULT_RETENTION: usize = 50;

/// Default socket filename within GW_HOME.
const DEFAULT_SOCKET_NAME: &str = "daemon.sock";

// ── Path helpers ─────────────────────────────────────────────────────

/// Resolve the GW home directory: `$GW_HOME` or `~/.gw/`.
pub fn gw_home() -> PathBuf {
    if let Ok(home) = std::env::var("GW_HOME") {
        PathBuf::from(home)
    } else {
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(".gw")
    }
}

/// Create the standard directory hierarchy under GW_HOME:
/// - `~/.gw/`
/// - `~/.gw/runs/`
/// - `~/.gw/repos/`
pub fn ensure_gw_home() -> Result<PathBuf> {
    let home = gw_home();
    let dirs = [home.clone(), home.join("runs"), home.join("repos")];

    for dir in &dirs {
        std::fs::create_dir_all(dir)
            .wrap_err_with(|| format!("failed to create directory: {}", dir.display()))?;
        debug!("ensured directory: {}", dir.display());
    }

    Ok(home)
}

// ── Global configuration ─────────────────────────────────────────────

/// Global daemon configuration, loaded from `~/.gw/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Maximum number of completed runs to retain.
    #[serde(default = "default_retention")]
    pub retention: usize,

    /// Unix socket filename (relative to GW_HOME).
    #[serde(default = "default_socket_name")]
    pub socket_name: String,

    /// Maximum concurrent runs (0 = unlimited).
    #[serde(default)]
    pub max_concurrent_runs: usize,
}

fn default_retention() -> usize {
    DEFAULT_RETENTION
}

fn default_socket_name() -> String {
    DEFAULT_SOCKET_NAME.to_string()
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            retention: DEFAULT_RETENTION,
            socket_name: DEFAULT_SOCKET_NAME.to_string(),
            max_concurrent_runs: 0,
        }
    }
}

impl GlobalConfig {
    /// Load config from `~/.gw/config.toml`, falling back to defaults.
    pub fn load() -> Result<Self> {
        let path = gw_home().join("config.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            let config: GlobalConfig = toml::from_str(&content)
                .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
            debug!("loaded config from {}", path.display());
            Ok(config)
        } else {
            debug!("no config file found, using defaults");
            Ok(GlobalConfig::default())
        }
    }

    /// Resolve the full socket path.
    pub fn socket_path(&self) -> PathBuf {
        gw_home().join(&self.socket_name)
    }
}

// ── Cleanup ──────────────────────────────────────────────────────────

/// Remove completed run directories beyond the retention limit.
///
/// Runs are sorted by modification time (oldest first), and excess
/// completed runs are removed until we're within the retention limit.
pub fn cleanup_old_runs(retention: usize) -> Result<()> {
    let runs_dir = gw_home().join("runs");
    if !runs_dir.exists() {
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&runs_dir)
        .wrap_err("failed to read runs directory")?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    if entries.len() <= retention {
        return Ok(());
    }

    // Sort by modification time, oldest first
    entries.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });

    let to_remove = entries.len() - retention;
    for entry in entries.into_iter().take(to_remove) {
        let path = entry.path();
        debug!("removing old run directory: {}", path.display());
        if let Err(e) = std::fs::remove_dir_all(&path) {
            warn!("failed to remove {}: {e}", path.display());
        }
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: set GW_HOME to a temp directory for isolated testing.
    fn with_temp_gw_home(f: impl FnOnce(&TempDir)) {
        let tmp = TempDir::new().unwrap();
        // Safety: tests run single-threaded with `cargo test -- --test-threads=1`
        // or this env var is scoped enough for our purposes.
        unsafe { std::env::set_var("GW_HOME", tmp.path()) };
        f(&tmp);
        unsafe { std::env::remove_var("GW_HOME") };
    }

    #[test]
    fn gw_home_respects_env() {
        with_temp_gw_home(|tmp| {
            assert_eq!(gw_home(), tmp.path());
        });
    }

    #[test]
    fn ensure_gw_home_creates_dirs() {
        with_temp_gw_home(|tmp| {
            ensure_gw_home().unwrap();
            assert!(tmp.path().join("runs").is_dir());
            assert!(tmp.path().join("repos").is_dir());
        });
    }

    #[test]
    fn global_config_defaults() {
        let cfg = GlobalConfig::default();
        assert_eq!(cfg.retention, 50);
        assert_eq!(cfg.socket_name, "daemon.sock");
        assert_eq!(cfg.max_concurrent_runs, 0);
    }

    #[test]
    fn global_config_load_missing_file() {
        with_temp_gw_home(|_| {
            let cfg = GlobalConfig::load().unwrap();
            assert_eq!(cfg.retention, DEFAULT_RETENTION);
        });
    }

    #[test]
    fn global_config_load_from_file() {
        with_temp_gw_home(|tmp| {
            let config_content = r#"
retention = 10
socket_name = "custom.sock"
max_concurrent_runs = 4
"#;
            std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();
            let cfg = GlobalConfig::load().unwrap();
            assert_eq!(cfg.retention, 10);
            assert_eq!(cfg.socket_name, "custom.sock");
            assert_eq!(cfg.max_concurrent_runs, 4);
        });
    }

    #[test]
    fn cleanup_old_runs_removes_excess() {
        with_temp_gw_home(|tmp| {
            let runs_dir = tmp.path().join("runs");
            std::fs::create_dir_all(&runs_dir).unwrap();

            // Create 5 run directories
            for i in 0..5 {
                let run_dir = runs_dir.join(format!("run-{i:03}"));
                std::fs::create_dir(&run_dir).unwrap();
                // Touch a file so the dir isn't empty
                std::fs::write(run_dir.join("state.json"), "{}").unwrap();
            }

            assert_eq!(std::fs::read_dir(&runs_dir).unwrap().count(), 5);

            // Retain only 2
            cleanup_old_runs(2).unwrap();

            assert_eq!(std::fs::read_dir(&runs_dir).unwrap().count(), 2);
        });
    }

    #[test]
    fn cleanup_old_runs_noop_within_limit() {
        with_temp_gw_home(|tmp| {
            let runs_dir = tmp.path().join("runs");
            std::fs::create_dir_all(&runs_dir).unwrap();

            for i in 0..3 {
                std::fs::create_dir(runs_dir.join(format!("run-{i}"))).unwrap();
            }

            cleanup_old_runs(5).unwrap();
            assert_eq!(std::fs::read_dir(&runs_dir).unwrap().count(), 3);
        });
    }
}
