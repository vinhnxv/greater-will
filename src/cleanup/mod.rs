//! Process cleanup module for Greater-Will.
//!
//! Provides aggressive cleanup between phase groups and between plans.
//! Ensures no orphaned processes remain after session termination.
//!
//! # Components
//!
//! - [`process`] - Process tree traversal and kill logic
//! - [`health`] - System health gates (RAM/CPU checks)
//! - [`tmux_cleanup`] - Stale tmux session cleanup

pub mod health;
pub mod process;
pub mod tmux_cleanup;


use color_eyre::Result;

/// Execute pre-phase cleanup.
///
/// Kills any remaining Claude processes and stale tmux sessions
/// before starting a new phase group.
///
/// # Errors
///
/// Returns an error if cleanup fails critically (e.g., permission denied).
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::pre_phase_cleanup;
///
/// pre_phase_cleanup("a1b2c3d4", "A")?;
/// ```
pub fn pre_phase_cleanup(plan_hash: &str, group_letter: &str) -> Result<()> {
    tracing::info!(
        plan_hash = %plan_hash,
        group = %group_letter,
        "Starting pre-phase cleanup"
    );

    // 1. Kill any stale tmux sessions from previous runs
    let stale_count = tmux_cleanup::cleanup_stale_sessions()?;
    if stale_count > 0 {
        tracing::info!(count = stale_count, "Cleaned up stale tmux sessions");
    }

    // 2. Kill only Claude processes owned by gw-* tmux sessions
    let killed_pids = process::kill_gw_owned_claude_processes()?;
    if !killed_pids.is_empty() {
        tracing::info!(
            pids = ?killed_pids,
            "Killed gw-owned Claude processes"
        );
    }

    tracing::info!("Pre-phase cleanup completed");
    Ok(())
}

/// Execute post-phase cleanup.
///
/// Verifies zero children remain after session kill, with force-kill
/// fallback for any remaining processes.
///
/// # Arguments
///
/// * `session_id` - The tmux session ID that was killed
/// * `pane_pid` - The PID of the pane's child process tree
///
/// # Returns
///
/// `true` if all processes were cleaned up successfully.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::post_phase_cleanup;
///
/// let clean = post_phase_cleanup("gw-a1b2c3d4-A", 12345)?;
/// if !clean {
///     tracing::warn!("Some processes could not be cleaned up");
/// }
/// ```
pub fn post_phase_cleanup(session_id: &str, pane_pid: u32) -> Result<bool> {
    use std::time::Duration;

    tracing::info!(
        session_id = %session_id,
        pane_pid = pane_pid,
        "Starting post-phase cleanup"
    );

    let mut sys = process::create_process_system();

    // Wait 2s for process tree to terminate
    std::thread::sleep(Duration::from_secs(2));

    // Refresh and check for remaining processes
    process::refresh_process_system(&mut sys);
    let remaining = process::collect_descendants(&sys, pane_pid);

    if remaining.is_empty() {
        tracing::info!("All child processes terminated cleanly");
        return Ok(true);
    }

    tracing::warn!(
        pids = ?remaining,
        "Force-killing remaining child processes"
    );

    // Force-kill remaining processes
    for pid in &remaining {
        unsafe {
            libc::kill(*pid as i32, libc::SIGKILL);
        }
    }

    // Wait 1s and verify
    std::thread::sleep(Duration::from_secs(1));
    process::refresh_process_system(&mut sys);
    let final_remaining = process::collect_descendants(&sys, pane_pid);

    if final_remaining.is_empty() {
        tracing::info!("All child processes terminated after force-kill");
        Ok(true)
    } else {
        tracing::error!(
            pids = ?final_remaining,
            "Failed to kill some child processes"
        );
        Ok(false)
    }
}

/// Execute startup cleanup.
///
/// Detects and kills stale sessions from crashed previous runs.
/// Should be called at the start of `gw run`.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::startup_cleanup;
///
/// startup_cleanup()?;
/// ```
pub fn startup_cleanup() -> Result<()> {
    tracing::info!("Running startup cleanup");

    let stale_count = tmux_cleanup::cleanup_stale_sessions()?;
    if stale_count > 0 {
        tracing::info!(count = stale_count, "Cleaned up stale sessions from previous runs");
    }

    Ok(())
}