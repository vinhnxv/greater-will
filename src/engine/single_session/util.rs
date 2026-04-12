//! Helper functions for single-session mode.
//!
//! Contains command builders, checkpoint resolution, pipeline completion
//! detection, and activity snapshot types.

use super::SingleSessionConfig;
use crate::checkpoint::schema::Checkpoint;
use crate::session::spawn::shell_escape;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// Build the `/rune:arc` command string for initial run.
pub(crate) fn build_arc_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {}", shell_escape(plan_path));

    if config.resume {
        cmd.push_str(" --resume");
    }

    for flag in &config.arc_flags {
        cmd.push(' ');
        cmd.push_str(&shell_escape(flag));
    }

    cmd
}

/// Build the `/rune:arc --resume` command for crash recovery.
pub(crate) fn build_resume_command(plan_path: &str, config: &SingleSessionConfig) -> String {
    let mut cmd = format!("/rune:arc {} --resume", shell_escape(plan_path));

    for flag in &config.arc_flags {
        cmd.push(' ');
        cmd.push_str(&shell_escape(flag));
    }

    cmd
}

// ---------------------------------------------------------------------------
// Restart decision
// ---------------------------------------------------------------------------

/// Decision for how to restart after a crash.
pub(crate) enum RestartDecision {
    /// Run the original command (no --resume).
    Fresh(String),
    /// Run with --resume.
    Resume(String),
    /// Pipeline already complete — don't restart at all.
    AlreadyDone,
}

/// Try to extract the checkpoint path from an inactive (active=false) loop state file.
///
/// When arc sets `active: false` during shutdown but the session is killed before
/// cleanup, the file still contains the checkpoint path needed for --resume.
/// This reads the raw file and extracts `checkpoint_path` regardless of `active` status.
fn try_checkpoint_from_inactive(working_dir: &Path) -> Option<PathBuf> {
    let loop_file = working_dir.join(".rune").join("arc-phase-loop.local.md");
    let contents = std::fs::read_to_string(&loop_file).ok()?;

    // Quick YAML parse — extract checkpoint_path from frontmatter
    let trimmed = contents.trim();
    let after_first = trimmed.strip_prefix("---")?;
    let end = after_first.find("---")?;
    let yaml = &after_first[..end];

    for line in yaml.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("checkpoint_path:") {
            let val = value.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() && val != "null" {
                let path = if val.starts_with('/') {
                    PathBuf::from(val)
                } else {
                    working_dir.join(val)
                };
                return Some(path);
            }
        }
    }
    None
}

/// Determine the correct restart command based on checkpoint state.
///
/// Phase-aware logic:
/// - No checkpoint or no progress → fresh start
/// - Mid-arc with progress → --resume
/// - Ship phase with PR already merged → AlreadyDone
/// - Ship phase with PR open → --resume
pub(crate) fn resolve_restart_command(
    working_dir: &Path,
    plan_str: &str,
    config: &SingleSessionConfig,
    arc_command: &str,
    restart_count: u32,
) -> RestartDecision {
    use crate::engine::phase_profile::{self, RecoveryStrategy};

    // Read checkpoint path from arc-phase-loop.local.md — no directory scanning.
    // Check both active AND inactive states: when a session crashes after arc
    // sets active=false (e.g., during merge phase), the file still contains the
    // valid checkpoint path we need for --resume.
    let loop_state_read = crate::monitor::loop_state::read_arc_loop_state(working_dir);
    let cp_path = match loop_state_read.active() {
        Some(loop_state) => {
            let path = loop_state.resolve_checkpoint_path(working_dir);
            if path.exists() {
                path
            } else {
                info!(restart = restart_count, "Loop state exists but checkpoint not written yet — fresh start");
                println!("[gw] Checkpoint not written yet — running fresh (not --resume)");
                return RestartDecision::Fresh(arc_command.to_string());
            }
        }
        None => {
            // Try to extract checkpoint path from inactive file (active=false).
            // This handles the case where arc set active=false during its final
            // phases but the session was killed before cleanup completed.
            match try_checkpoint_from_inactive(working_dir) {
                Some(path) if path.exists() => {
                    info!(
                        restart = restart_count,
                        checkpoint = %path.display(),
                        "Loop state inactive but checkpoint exists — attempting resume"
                    );
                    println!("[gw] Found checkpoint from inactive loop state — attempting --resume");
                    path
                }
                _ => {
                    info!(restart = restart_count, "No loop state for plan '{}' — fresh start", plan_str);
                    println!("[gw] No active arc state found — running fresh (not --resume)");
                    return RestartDecision::Fresh(arc_command.to_string());
                }
            }
        }
    };

    let checkpoint = match crate::checkpoint::reader::read_checkpoint(&cp_path) {
        Ok(cp) => {
            let has_progress = cp.phases.values().any(|p| {
                p.status == "completed" || p.status == "in_progress" || p.status == "skipped"
            });
            if !has_progress {
                info!(restart = restart_count, "Checkpoint for plan '{}' has no progress — fresh start", plan_str);
                println!("[gw] Checkpoint exists but no progress — running fresh (not --resume)");
                return RestartDecision::Fresh(arc_command.to_string());
            }
            cp
        }
        Err(e) => {
            warn!(error = %e, "Failed to read checkpoint — fresh start");
            println!("[gw] Failed to read checkpoint — running fresh (not --resume)");
            return RestartDecision::Fresh(arc_command.to_string());
        }
    };

    // Check if already complete.
    // Defense-in-depth: also check terminal phase directly in case
    // is_complete() has edge cases with partial phase maps.
    if checkpoint.is_complete() || checkpoint.is_terminal_phase_completed() {
        info!(restart = restart_count, "Pipeline already complete (merge done) — skipping restart");
        println!("[gw] Pipeline already complete (merge phase done) — not restarting");
        return RestartDecision::AlreadyDone;
    }

    // Determine current phase and its recovery strategy.
    // Prefer inferred phase (scans actual statuses) over current_phase()
    // which relies on often-stale phase_sequence index.
    let current_phase = checkpoint.inferred_phase_name()
        .unwrap_or_else(|| checkpoint.current_phase().unwrap_or("unknown"));
    let profile = phase_profile::profile_for_phase(current_phase)
        .unwrap_or_else(phase_profile::default_profile);

    let completed = checkpoint.count_by_status("completed");
    let skipped = checkpoint.count_by_status("skipped");
    let total = checkpoint.phases.len();

    match profile.recovery {
        RecoveryStrategy::FreshStart => {
            info!(
                restart = restart_count,
                phase = current_phase,
                "Recovery strategy: fresh start for {:?} phase",
                profile.category,
            );
            println!("[gw] Phase '{}' recovery: fresh start", current_phase);
            RestartDecision::Fresh(arc_command.to_string())
        }
        RecoveryStrategy::Resume => {
            info!(
                restart = restart_count,
                phase = current_phase,
                progress = format!("{}/{}", completed + skipped, total),
                "Recovery strategy: --resume from {:?} phase",
                profile.category,
            );
            println!(
                "[gw] Phase '{}' ({}/{} done) — resuming with --resume",
                current_phase, completed + skipped, total,
            );
            RestartDecision::Resume(build_resume_command(plan_str, config))
        }
        RecoveryStrategy::ResumeCheckPR => {
            // Ship/merge phase — check if PR already exists and is merged
            if let Some(pr_url) = &checkpoint.pr_url {
                info!(
                    restart = restart_count,
                    phase = current_phase,
                    pr_url = %pr_url,
                    "Ship phase with existing PR — resuming to finish merge"
                );
                println!("[gw] Phase '{}' with PR {} — resuming", current_phase, pr_url);
            } else {
                info!(
                    restart = restart_count,
                    phase = current_phase,
                    "Ship phase without PR — resuming to create PR"
                );
            }
            RestartDecision::Resume(build_resume_command(plan_str, config))
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline completion detection
// ---------------------------------------------------------------------------

/// Check pane output for signals that the arc pipeline has completed.
///
/// Looks for common arc completion patterns in the visible pane content.
pub(crate) fn is_pipeline_complete(pane_content: &str) -> bool {
    // Check last ~20 lines for completion signals
    let last_lines: Vec<&str> = pane_content.lines().rev().take(20).collect();
    let tail = last_lines.join("\n").to_lowercase();

    // Arc completion signals
    tail.contains("arc completed")
        || tail.contains("all phases complete")
        || tail.contains("pipeline completed")
        || tail.contains("merge completed")
        || tail.contains("arc run finished")
        // Rune-specific completion markers
        || tail.contains("the tarnished rests")
        || tail.contains("arc result: success")
}

// ---------------------------------------------------------------------------
// Checkpoint resolution
// ---------------------------------------------------------------------------

/// Read checkpoint with three-strategy resolution.
///
/// Resolution order:
/// 1. Return from cache if path still exists
/// 2. Read `checkpoint_path` from `.rune/arc-phase-loop.local.md` (authoritative when active)
/// 3. Fall back to plan-based discovery (when loop state is absent or inactive)
///
/// Returns `None` if no checkpoint can be resolved yet (early arc init).
/// Once resolved, the path is cached until invalidated.
/// Only reads from arc-phase-loop.local.md — no directory scanning.
pub(crate) fn read_cached_checkpoint(
    working_dir: &Path,
    cached_path: &mut Option<PathBuf>,
) -> Option<Checkpoint> {
    /// Try reading a checkpoint and optionally cache the path on success.
    fn try_read_and_cache(
        cp_path: &Path,
        cached_path: &mut Option<PathBuf>,
        cache_on_success: bool,
        label: &str,
    ) -> Option<Checkpoint> {
        match crate::checkpoint::reader::read_checkpoint(cp_path) {
            Ok(cp) => {
                if cache_on_success {
                    *cached_path = Some(cp_path.to_path_buf());
                }
                Some(cp)
            }
            Err(e) => {
                debug!(error = %e, "{label}");
                None
            }
        }
    }

    // If we have a cached path, try reading directly — O(1)
    if let Some(cp_path) = cached_path.as_ref() {
        if cp_path.exists() {
            return match crate::checkpoint::reader::read_checkpoint(cp_path) {
                Ok(cp) => Some(cp),
                Err(e) => {
                    debug!(error = %e, "Could not read cached checkpoint (transient)");
                    None
                }
            };
        }
        // Cached path no longer exists — invalidate and re-discover
        debug!("Cached checkpoint path disappeared, re-discovering");
        *cached_path = None;
    }

    // Only source: arc-phase-loop.local.md — no directory scanning.
    // All checkpoint reads go through arc-phase-loop.local.md — no directory scanning.
    if let Some(loop_state) = crate::monitor::loop_state::read_arc_loop_state(working_dir).active() {
        let cp_path = loop_state.resolve_checkpoint_path(working_dir);
        if cp_path.exists() {
            info!(
                checkpoint = %cp_path.display(),
                arc_id = ?loop_state.arc_id(),
                iteration = loop_state.iteration,
                "Resolved checkpoint from arc-phase-loop.local.md"
            );
            return try_read_and_cache(&cp_path, cached_path, true, "Could not read checkpoint (transient)");
        }
        debug!(path = %cp_path.display(), "Loop state checkpoint_path doesn't exist yet");
    }

    None
}

/// Find artifact dir using the cached checkpoint path (avoids re-scanning).
pub(crate) fn find_artifact_dir_cached(cached_cp_path: &Option<PathBuf>, working_dir: &Path) -> Option<PathBuf> {
    let cp_path = cached_cp_path.as_ref()?;
    // checkpoint.json is at .rune/arc/arc-{id}/checkpoint.json
    // artifact dir is at   tmp/arc/arc-{id}/
    let arc_id = cp_path.parent()?.file_name()?;
    let artifact_dir = working_dir.join("tmp").join("arc").join(arc_id);
    if artifact_dir.is_dir() {
        Some(artifact_dir)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Activity snapshots
// ---------------------------------------------------------------------------

/// Swarm activity snapshot — number of active Claude teammates.
///
/// Claude Code spawns `tmux -L claude-swarm-{lead_pid}` for agent teams.
/// Each pane runs a teammate. If teammates are running, the system is working
/// even if the main screen is stalled (team lead waiting for agents).
///
/// # Teammate states
///
/// | State | Claude child | Output | Meaning |
/// |-------|-------------|--------|---------|
/// | Working | alive | changing | Teammate actively processing |
/// | Idle | alive | prompt (❯) visible | Teammate done, waiting for input |
/// | Done | dead | any | Teammate finished, pane still open |
/// | Stuck | alive | stalled (no prompt) | Teammate may be hung |
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwarmSnapshot {
    /// Number of tmux panes in the swarm server.
    pub pane_count: u32,
    /// Panes with a live Claude child process.
    pub alive_count: u32,
    /// Panes actively producing output (no prompt visible, process alive).
    /// This is the best signal that work is happening.
    pub working_count: u32,
    /// Panes where Claude child is alive but showing prompt (❯) — done/idle.
    pub idle_count: u32,
    /// Panes where Claude child has exited (shell still running).
    pub done_count: u32,
}

impl SwarmSnapshot {
    /// True if at least one teammate is genuinely working (not just alive).
    pub fn has_working_teammates(&self) -> bool {
        self.working_count > 0
    }

    /// True if all teammates have finished (all done or idle).
    pub fn all_finished(&self) -> bool {
        self.pane_count > 0 && self.working_count == 0
    }
}

/// Check swarm activity for a Claude Code process.
///
/// Looks for `claude-swarm-{pid}` tmux server and inspects each pane:
/// - Is the Claude child process alive?
/// - Is the pane showing a prompt (❯) — meaning teammate is idle/done?
/// - Or is output still being produced — meaning teammate is working?
///
/// Returns `None` if no swarm server exists (not running agent teams).
pub(crate) fn check_swarm_activity(claude_pid: u32) -> Option<SwarmSnapshot> {
    use std::process::Command;

    let socket_name = format!("claude-swarm-{}", claude_pid);

    // Check if the swarm server exists by listing panes with their PIDs
    // and session:window.pane identifiers for targeted capture
    let output = Command::new("tmux")
        .args(["-L", &socket_name, "list-panes", "-a",
               "-F", "#{pane_pid}\t#{session_name}:#{window_index}.#{pane_index}"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None; // No swarm server for this PID
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let panes: Vec<(u32, String)> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let pid: u32 = parts.next()?.trim().parse().ok()?;
            let target = parts.next()?.trim().to_string();
            Some((pid, target))
        })
        .collect();

    if panes.is_empty() {
        return None;
    }

    let mut alive_count = 0u32;
    let mut working_count = 0u32;
    let mut idle_count = 0u32;
    let mut done_count = 0u32;

    for (shell_pid, pane_target) in &panes {
        // Check if shell has a claude child via pgrep
        let child_alive = Command::new("pgrep")
            .args(["-P", &shell_pid.to_string(), "-x", "claude"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !child_alive {
            done_count += 1;
            continue;
        }

        alive_count += 1;

        // Capture last 5 lines of teammate pane to check for prompt
        let pane_output = Command::new("tmux")
            .args(["-L", &socket_name, "capture-pane", "-t", pane_target, "-p",
                   "-S", "-5"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        // Check if prompt (❯) is visible in last non-empty lines
        let has_prompt = pane_output
            .lines()
            .rev()
            .take(5)
            .any(|line| line.contains('❯'));

        if has_prompt {
            idle_count += 1; // Claude alive but showing prompt — done/waiting
        } else {
            working_count += 1; // Claude alive, no prompt — actively working
        }
    }

    Some(SwarmSnapshot {
        pane_count: panes.len() as u32,
        alive_count,
        working_count,
        idle_count,
        done_count,
    })
}

/// Lightweight snapshot of an artifact directory — file count and total size.
///
/// Used as an activity signal: if file_count or total_bytes changes between
/// poll cycles, agents are actively writing output (review files, TOME,
/// worker reports, etc.) even if the screen is stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtifactSnapshot {
    pub file_count: u64,
    pub total_bytes: u64,
}

/// Scan a directory recursively for file count and total size.
///
/// Lightweight: only calls `metadata()` per file, never reads content.
/// Returns `None` if the directory doesn't exist (normal for early phases).
pub(crate) fn scan_artifact_dir(dir: &Path) -> Option<ArtifactSnapshot> {
    if !dir.is_dir() {
        return None;
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    // Use a stack-based walk to avoid recursion depth issues
    let mut dirs_to_visit = vec![dir.to_path_buf()];
    while let Some(current) = dirs_to_visit.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                dirs_to_visit.push(entry.path());
            } else if ft.is_file() {
                file_count += 1;
                if let Ok(meta) = entry.metadata() {
                    total_bytes += meta.len();
                }
            }
        }
    }

    Some(ArtifactSnapshot {
        file_count,
        total_bytes,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_arc_command() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_arc_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test.md'");
    }

    #[test]
    fn test_build_arc_command_with_resume() {
        let config = SingleSessionConfig::new("/tmp").with_resume();
        let cmd = build_arc_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test.md' --resume");
    }

    #[test]
    fn test_build_resume_command() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_resume_command("plans/test.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test.md' --resume");
    }

    #[test]
    fn test_build_arc_command_escapes_special_chars() {
        let config = SingleSessionConfig::new("/tmp");
        let cmd = build_arc_command("plans/test; rm -rf /.md", &config);
        assert_eq!(cmd, "/rune:arc 'plans/test; rm -rf /.md'");
    }

    #[test]
    fn test_is_pipeline_complete() {
        assert!(is_pipeline_complete("some output\narc completed\n❯"));
        assert!(is_pipeline_complete("The Tarnished rests after a long journey"));
        assert!(!is_pipeline_complete("still working on phase 5..."));
        assert!(!is_pipeline_complete(""));
    }
}
