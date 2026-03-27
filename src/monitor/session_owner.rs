//! Session ownership tracking for gw crash recovery.
//!
//! When gw starts monitoring a session, it writes `.gw/session-owner.json`
//! with its own PID and session metadata. On next startup, gw reads this
//! file to determine if the previous gw is dead (orphaned session) or alive
//! (concurrent gw — blocked by InstanceLock).
//!
//! This solves the "gw crashes but tmux+Claude still running" problem:
//! - Previous gw dead + tmux alive + Claude alive → adopt session
//! - Previous gw dead + tmux alive + Claude dead → kill + respawn
//! - Previous gw dead + tmux dead → fresh start
//! - Previous gw alive → impossible (InstanceLock prevents)

use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Session ownership record written to `.gw/session-owner.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOwner {
    /// PID of the gw process that owns this session.
    pub gw_pid: u32,
    /// Name of the tmux session (e.g., "gw-feat-auth").
    pub session_name: String,
    /// Path to the plan file being executed.
    pub plan_file: String,
    /// PID of the Claude Code process inside tmux.
    pub claude_pid: u32,
    /// Unix timestamp when the session was started.
    pub started_at: u64,
}

/// Result of checking the previous session owner on startup.
#[derive(Debug)]
pub enum OwnerCheck {
    /// No previous session owner file found — fresh start.
    NoPrevious,
    /// Previous gw is dead, tmux session is alive with a live Claude process.
    /// The new gw should adopt this session instead of spawning fresh.
    OrphanedAlive {
        owner: SessionOwner,
    },
    /// Previous gw is dead, tmux session exists but Claude process is dead.
    /// Kill the stale session and spawn fresh.
    OrphanedStaleClaude {
        owner: SessionOwner,
    },
    /// Previous gw is dead, tmux session no longer exists.
    /// Clean start — just remove the stale owner file.
    OrphanedDead {
        owner: SessionOwner,
    },
}

/// Path to the session owner file.
fn owner_path(working_dir: &Path) -> PathBuf {
    working_dir.join(".gw").join("session-owner.json")
}

/// Write the session owner file.
///
/// Called after spawning (or adopting) a tmux session.
pub fn write_session_owner(
    working_dir: &Path,
    session_name: &str,
    plan_file: &str,
    claude_pid: u32,
) -> Result<()> {
    let path = owner_path(working_dir);

    // Ensure .gw directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let owner = SessionOwner {
        gw_pid: std::process::id(),
        session_name: session_name.to_string(),
        plan_file: plan_file.to_string(),
        claude_pid,
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };

    let json = serde_json::to_string_pretty(&owner)?;
    std::fs::write(&path, json)?;

    info!(
        gw_pid = owner.gw_pid,
        session = %session_name,
        claude_pid = claude_pid,
        "Wrote session owner file"
    );

    Ok(())
}

/// Read the session owner file, if it exists.
pub fn read_session_owner(working_dir: &Path) -> Option<SessionOwner> {
    let path = owner_path(working_dir);
    let contents = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&contents) {
        Ok(owner) => Some(owner),
        Err(e) => {
            warn!(error = %e, "Failed to parse session-owner.json — treating as missing");
            None
        }
    }
}

/// Remove the session owner file.
pub fn remove_session_owner(working_dir: &Path) {
    let path = owner_path(working_dir);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(error = %e, "Failed to remove session-owner.json");
        } else {
            debug!("Removed session-owner.json");
        }
    }
}

/// Check the previous session owner and determine recovery action.
///
/// This is called early in `run_single_session` before spawning.
/// It uses the gw PID (not Claude's PID) to detect orphaned sessions.
pub fn check_previous_owner(working_dir: &Path) -> OwnerCheck {
    let owner = match read_session_owner(working_dir) {
        Some(o) => o,
        None => return OwnerCheck::NoPrevious,
    };

    // Check if previous gw process is alive
    if crate::cleanup::process::is_pid_alive(owner.gw_pid) {
        // This should be impossible — InstanceLock blocks concurrent gw.
        // But if we somehow get here, treat as fresh start with a warning.
        warn!(
            gw_pid = owner.gw_pid,
            "Previous gw appears alive but we acquired InstanceLock — treating as stale"
        );
        // Fall through to orphan checks — the PID may have been recycled
    }

    // Previous gw is dead. Check tmux session state.
    let session_alive = crate::session::spawn::has_session(&owner.session_name);
    if !session_alive {
        info!(
            session = %owner.session_name,
            gw_pid = owner.gw_pid,
            "Previous session no longer exists — clean start"
        );
        remove_session_owner(working_dir);
        return OwnerCheck::OrphanedDead { owner };
    }

    // Tmux session exists. Check if Claude process inside is alive.
    let claude_alive = crate::cleanup::process::is_pid_alive(owner.claude_pid);
    if claude_alive {
        info!(
            session = %owner.session_name,
            gw_pid = owner.gw_pid,
            claude_pid = owner.claude_pid,
            "Orphaned session with live Claude — candidate for adoption"
        );
        OwnerCheck::OrphanedAlive { owner }
    } else {
        info!(
            session = %owner.session_name,
            gw_pid = owner.gw_pid,
            claude_pid = owner.claude_pid,
            "Orphaned session but Claude is dead — will kill and respawn"
        );
        OwnerCheck::OrphanedStaleClaude { owner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_write_and_read_session_owner() {
        let dir = TempDir::new().unwrap();
        write_session_owner(dir.path(), "gw-test", "plans/test.md", 12345).unwrap();

        let owner = read_session_owner(dir.path()).unwrap();
        assert_eq!(owner.session_name, "gw-test");
        assert_eq!(owner.plan_file, "plans/test.md");
        assert_eq!(owner.claude_pid, 12345);
        assert_eq!(owner.gw_pid, std::process::id());
        assert!(owner.started_at > 0);
    }

    #[test]
    fn test_read_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_session_owner(dir.path()).is_none());
    }

    #[test]
    fn test_read_corrupt_returns_none() {
        let dir = TempDir::new().unwrap();
        let gw_dir = dir.path().join(".gw");
        std::fs::create_dir_all(&gw_dir).unwrap();
        std::fs::write(gw_dir.join("session-owner.json"), "not json").unwrap();
        assert!(read_session_owner(dir.path()).is_none());
    }

    #[test]
    fn test_remove_session_owner() {
        let dir = TempDir::new().unwrap();
        write_session_owner(dir.path(), "gw-test", "plans/test.md", 99).unwrap();
        assert!(read_session_owner(dir.path()).is_some());

        remove_session_owner(dir.path());
        assert!(read_session_owner(dir.path()).is_none());
    }

    #[test]
    fn test_remove_nonexistent_is_noop() {
        let dir = TempDir::new().unwrap();
        remove_session_owner(dir.path()); // should not panic
    }

    #[test]
    fn test_check_previous_owner_no_file() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            check_previous_owner(dir.path()),
            OwnerCheck::NoPrevious
        ));
    }
}
