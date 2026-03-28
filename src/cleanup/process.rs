#![allow(dead_code)]
//! Process tree management and kill logic.
//!
//! Provides utilities for traversing process trees and terminating
//! all descendants of a given process.

use color_eyre::Result;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Create a new process system instance.
///
/// On macOS, this requires multiple refreshes for accurate CPU values.
/// This function performs the necessary initialization sequence.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::create_process_system;
///
/// let sys = create_process_system();
/// ```
pub fn create_process_system() -> System {
    let mut sys = System::new_all();
    sys.refresh_all();

    // macOS requires delay for CPU values to populate
    std::thread::sleep(Duration::from_millis(200));
    sys.refresh_processes(ProcessesToUpdate::All, true);

    sys
}

/// Refresh process information with minimal overhead.
///
/// Updates only memory, CPU, and command/exe information.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::{create_process_system, refresh_process_system};
///
/// let mut sys = create_process_system();
/// // ... later
/// refresh_process_system(&mut sys);
/// ```
pub fn refresh_process_system(sys: &mut System) {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet),
    );
}

/// Check if a process is alive using `libc::kill(pid, 0)`.
///
/// This has zero subprocess overhead compared to spawning `ps` commands.
/// Returns `true` if the process exists (including EPERM cases where
/// the process is owned by another user).
///
/// # Arguments
///
/// * `pid` - Process ID to check
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::is_pid_alive;
///
/// if is_pid_alive(12345) {
///     println!("Process 12345 is running");
/// }
/// ```
pub fn is_pid_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }

    // SAFETY: libc::kill with signal 0 is a no-op that just checks process existence
    let ret = unsafe { libc::kill(pid as i32, 0) };

    if ret == 0 {
        return true;
    }

    // EPERM means process exists but we don't have permission to signal it
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Collect all descendant PIDs of a given root PID.
///
/// Uses a depth-first search after building a parent→children index
/// in O(n) time complexity.
///
/// # Arguments
///
/// * `sys` - System instance with process information
/// * `root_pid` - Root process ID to start traversal from
///
/// # Returns
///
/// Vector of all descendant PIDs (not including the root).
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::{create_process_system, collect_descendants};
///
/// let sys = create_process_system();
/// let children = collect_descendants(&sys, 12345);
/// println!("Found {} child processes", children.len());
/// ```
pub fn collect_descendants(sys: &System, root_pid: u32) -> Vec<u32> {
    // Build parent→children index in a single pass
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();

    for (pid, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children_map
                .entry(parent.as_u32())
                .or_default()
                .push(pid.as_u32());
        }
    }

    // DFS traversal with deduplication
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![root_pid];

    while let Some(parent) = stack.pop() {
        if let Some(kids) = children_map.get(&parent) {
            for &child_pid in kids {
                if child_pid != root_pid && visited.insert(child_pid) {
                    result.push(child_pid);
                    stack.push(child_pid);
                }
            }
        }
    }

    result
}

/// Kill a process and all its descendants.
///
/// First sends SIGTERM, waits for graceful termination, then
/// force-kills any remaining processes with SIGKILL.
///
/// # Arguments
///
/// * `sys` - System instance for process tracking
/// * `root_pid` - Root process ID to kill
///
/// # Returns
///
/// Number of processes killed.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::{create_process_system, kill_process_tree};
///
/// let mut sys = create_process_system();
/// let killed = kill_process_tree(&mut sys, 12345)?;
/// println!("Killed {} processes", killed);
/// ```
pub fn kill_process_tree(sys: &mut System, root_pid: u32) -> Result<usize> {
    // Collect all descendants first
    let descendants = collect_descendants(sys, root_pid);

    // Send SIGTERM to all processes (children first, then root)
    for pid in &descendants {
        unsafe {
            libc::kill(*pid as i32, libc::SIGTERM);
        }
    }
    unsafe {
        libc::kill(root_pid as i32, libc::SIGTERM);
    }

    // Wait for graceful termination
    std::thread::sleep(Duration::from_secs(2));

    // Check which processes are still alive
    refresh_process_system(sys);
    let still_alive: Vec<u32> = descendants
        .iter()
        .chain(std::iter::once(&root_pid))
        .filter(|&&pid| is_pid_alive(pid))
        .copied()
        .collect();

    let killed_count = descendants.len() + 1 - still_alive.len();

    if still_alive.is_empty() {
        return Ok(killed_count);
    }

    // Force-kill remaining processes (ignore ESRCH — process may have exited between check and kill)
    tracing::warn!(pids = ?still_alive, "Force-killing remaining processes");
    for pid in &still_alive {
        let ret = unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                tracing::warn!(pid = pid, error = %err, "SIGKILL failed for unexpected reason");
            }
        }
    }

    std::thread::sleep(Duration::from_secs(1));

    Ok(descendants.len() + 1)
}

/// Kill Claude processes owned by Greater-Will tmux sessions.
///
/// Only kills processes that are descendants of stale `gw-*` tmux sessions.
/// This is safe because it never touches Claude Code processes running
/// outside of Greater-Will (e.g., user's interactive sessions, IDE extensions,
/// Desktop app).
///
/// # Safety
///
/// This function exclusively targets processes within `gw-*` tmux sessions
/// that are considered stale (no active process + older than threshold).
/// It will NOT kill:
/// - User's interactive `claude` CLI sessions
/// - Claude Code Desktop app processes
/// - VS Code / JetBrains Claude extensions
/// - Other tools that spawn Claude Code
///
/// # Returns
///
/// Vector of PIDs that were killed.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::kill_gw_owned_claude_processes;
///
/// let killed = kill_gw_owned_claude_processes()?;
/// println!("Killed {} owned processes", killed.len());
/// ```
pub fn kill_gw_owned_claude_processes() -> Result<Vec<u32>> {
    let gw_sessions = crate::cleanup::tmux_cleanup::list_gw_sessions()?;
    let mut killed_pids = Vec::new();

    if gw_sessions.is_empty() {
        return Ok(killed_pids);
    }

    let mut sys = create_process_system();

    for session in &gw_sessions {
        // Only target stale sessions — active sessions are still doing work
        if !crate::cleanup::tmux_cleanup::is_session_stale(&session.name, 600) {
            tracing::debug!(session = %session.name, "Session still active, skipping");
            continue;
        }

        // Get the pane PID — this is the root of the process tree we own
        if let Some(pane_pid) = crate::cleanup::tmux_cleanup::get_session_pid(&session.name) {
            if !is_pid_alive(pane_pid) {
                continue;
            }

            // Kill the entire process tree rooted at our pane PID
            let descendants = collect_descendants(&sys, pane_pid);
            tracing::info!(
                session = %session.name,
                pane_pid = pane_pid,
                descendants = descendants.len(),
                "Killing stale gw-owned process tree"
            );

            // SIGTERM descendants first (children before parent)
            for &pid in &descendants {
                unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                killed_pids.push(pid);
            }
            // SIGTERM the root pane process
            unsafe { libc::kill(pane_pid as i32, libc::SIGTERM); }
            killed_pids.push(pane_pid);
        }

        // Kill the tmux session itself
        let _ = crate::cleanup::tmux_cleanup::kill_session(&session.name);
    }

    if !killed_pids.is_empty() {
        // Wait for graceful termination, then force-kill stragglers
        std::thread::sleep(Duration::from_secs(2));
        refresh_process_system(&mut sys);

        for &pid in &killed_pids {
            if is_pid_alive(pid) {
                tracing::warn!(pid = pid, "Force-killing straggler process");
                unsafe { libc::kill(pid as i32, libc::SIGKILL); }
            }
        }
    }

    Ok(killed_pids)
}

/// Resource snapshot for a process tree.
///
/// Aggregates CPU and memory usage across a root process and all descendants.
#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    /// Total CPU usage percentage (0.0 - 100.0 per core)
    pub cpu_percent: f32,
    /// Total memory usage in bytes
    pub memory_bytes: u64,
    /// Number of child processes
    pub child_count: u32,
    /// Process start time (Unix timestamp)
    pub start_time: u64,
}

/// Take a resource snapshot of a process tree.
///
/// Aggregates CPU, memory, and child count across the root process
/// and all its descendants.
///
/// # Arguments
///
/// * `sys` - System instance with process information
/// * `pid` - Root process ID
///
/// # Returns
///
/// `Some(ResourceSnapshot)` if the process exists, `None` otherwise.
///
/// # Example
///
/// ```no_run
/// use greater_will::cleanup::process::{create_process_system, snapshot};
///
/// let sys = create_process_system();
/// if let Some(snap) = snapshot(&sys, 12345) {
///     println!("CPU: {}%, Memory: {} bytes", snap.cpu_percent, snap.memory_bytes);
/// }
/// ```
pub fn snapshot(sys: &System, pid: u32) -> Option<ResourceSnapshot> {
    let root = sys.process(Pid::from_u32(pid))?;

    let mut total_cpu = root.cpu_usage();
    let mut total_mem = root.memory();
    let mut child_count = 0u32;

    let descendants = collect_descendants(sys, pid);
    for desc_pid in &descendants {
        if let Some(proc_) = sys.process(Pid::from_u32(*desc_pid)) {
            total_cpu += proc_.cpu_usage();
            total_mem += proc_.memory();
            child_count += 1;
        }
    }

    Some(ResourceSnapshot {
        cpu_percent: total_cpu,
        memory_bytes: total_mem,
        child_count,
        start_time: root.start_time(),
    })
}

/// Registry of PIDs that Greater-Will has spawned.
///
/// Provides a second safety layer: even if tmux session detection fails,
/// we only ever kill PIDs that we explicitly registered after spawning.
/// This prevents accidental kills of user's interactive Claude sessions,
/// IDE extensions, or Desktop app processes.
///
/// # Invariant
///
/// A PID is only added via `register()` immediately after `spawn_claude_session()`
/// returns it. No PID scanning or pattern matching is used.
#[derive(Debug, Default)]
pub struct OwnedProcessRegistry {
    /// PIDs registered by this gw instance, keyed by tmux session name.
    pids: HashMap<String, u32>,
}

impl OwnedProcessRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self { pids: HashMap::new() }
    }

    /// Register a PID as owned by this gw instance.
    ///
    /// Call this immediately after spawning a tmux session.
    pub fn register(&mut self, session_name: String, pid: u32) {
        tracing::debug!(session = %session_name, pid = pid, "Registered owned PID");
        self.pids.insert(session_name, pid);
    }

    /// Unregister a session (after successful cleanup).
    pub fn unregister(&mut self, session_name: &str) {
        self.pids.remove(session_name);
    }

    /// Check if a PID is owned by this gw instance.
    pub fn is_owned(&self, pid: u32) -> bool {
        self.pids.values().any(|&p| p == pid)
    }

    /// Kill all owned processes and their descendants.
    ///
    /// Only kills PIDs that were explicitly registered via `register()`.
    /// Returns the number of processes killed.
    pub fn kill_all_owned(&mut self) -> Result<usize> {
        let mut sys = create_process_system();
        let mut total_killed = 0;

        let sessions: Vec<(String, u32)> = self.pids.drain().collect();

        for (session_name, root_pid) in sessions {
            if !is_pid_alive(root_pid) {
                tracing::debug!(session = %session_name, pid = root_pid, "Owned PID already dead");
                continue;
            }

            tracing::info!(session = %session_name, pid = root_pid, "Killing owned process tree");
            match kill_process_tree(&mut sys, root_pid) {
                Ok(count) => total_killed += count,
                Err(e) => tracing::warn!(
                    session = %session_name,
                    pid = root_pid,
                    error = %e,
                    "Failed to kill owned process tree"
                ),
            }
        }

        Ok(total_killed)
    }

    /// Get the number of registered PIDs.
    pub fn len(&self) -> usize {
        self.pids.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }
}

/// Process cleanup coordinator.
///
/// Provides high-level cleanup operations used by the Greater-Will engine.
pub struct ProcessCleanup {
    sys: System,
}

impl ProcessCleanup {
    /// Create a new process cleanup instance.
    pub fn new() -> Self {
        Self {
            sys: create_process_system(),
        }
    }

    /// Refresh process information.
    pub fn refresh(&mut self) {
        refresh_process_system(&mut self.sys);
    }

    /// Check if a process is alive.
    pub fn is_alive(&self, pid: u32) -> bool {
        is_pid_alive(pid)
    }

    /// Get all descendants of a process.
    pub fn get_descendants(&self, pid: u32) -> Vec<u32> {
        collect_descendants(&self.sys, pid)
    }

    /// Kill a process tree.
    pub fn kill_tree(&mut self, pid: u32) -> Result<usize> {
        kill_process_tree(&mut self.sys, pid)
    }

    /// Get a resource snapshot for a process.
    pub fn get_snapshot(&self, pid: u32) -> Option<ResourceSnapshot> {
        snapshot(&self.sys, pid)
    }

    /// Verify cleanup was successful.
    ///
    /// Returns `true` if no descendants remain.
    pub fn verify_cleanup(&mut self, pid: u32) -> bool {
        std::thread::sleep(Duration::from_secs(2));
        self.refresh();
        let remaining = self.get_descendants(pid);

        if remaining.is_empty() {
            return true;
        }

        // Force-kill remaining
        for pid in &remaining {
            unsafe {
                libc::kill(*pid as i32, libc::SIGKILL);
            }
        }

        std::thread::sleep(Duration::from_secs(1));
        self.refresh();
        self.get_descendants(pid).is_empty()
    }
}

impl Default for ProcessCleanup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_pid_alive_invalid() {
        // PID 1 is always init, should be alive
        assert!(is_pid_alive(1));

        // Very high PIDs are unlikely to exist
        assert!(!is_pid_alive(u32::MAX));
    }

    #[test]
    fn test_create_process_system() {
        let sys = create_process_system();
        // Should have at least init process
        assert!(!sys.processes().is_empty());
    }

    #[test]
    fn test_collect_descendants_init() {
        let sys = create_process_system();
        // PID 1 (init) typically has children
        let children = collect_descendants(&sys, 1);
        // Just verify it doesn't panic and returns a vec
        assert!(children.iter().all(|&pid| pid != 1));
    }
}