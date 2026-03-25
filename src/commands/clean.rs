//! Clean command implementation.
//!
//! Cleans up temporary files and tmux sessions from Greater-Will runs.

use color_eyre::Result;

use crate::cleanup;

/// Execute the clean command.
///
/// Removes:
/// - All `gw-*` tmux sessions
/// - Orphaned Claude processes
/// - Temporary checkpoint files
pub fn execute() -> Result<()> {
    tracing::info!("Starting cleanup");

    // 1. Kill all gw-* tmux sessions
    let sessions_killed = cleanup::tmux_cleanup::kill_all_gw_sessions()?;
    if sessions_killed > 0 {
        println!("Killed {} tmux session(s)", sessions_killed);
    } else {
        println!("No active Greater-Will sessions found");
    }

    // 2. Kill only Claude processes owned by gw-* tmux sessions
    let processes_killed = cleanup::process::kill_gw_owned_claude_processes()?;
    if !processes_killed.is_empty() {
        println!("Killed {} gw-owned Claude process(es)", processes_killed.len());
        for pid in &processes_killed {
            tracing::info!(pid = pid, "Killed gw-owned process");
        }
    }

    // 3. Report final status
    let status = cleanup::tmux_cleanup::get_sessions_status()?;
    if status.is_empty() {
        println!("All Greater-Will resources cleaned up");
    } else {
        println!("Warning: {} session(s) could not be cleaned:", status.len());
        for (name, has_process) in &status {
            let status_str = if *has_process { "active" } else { "idle" };
            println!("  - {} ({})", name, status_str);
        }
    }

    tracing::info!("Cleanup completed");
    Ok(())
}