//! Elden command — Claude Code hook context injection.
//!
//! When registered as a `SessionStart` hook in `.claude/settings.json`,
//! this command injects workspace state, role context, and current task
//! into the Claude Code context window via stdout.
//!
//! # How it works
//!
//! Claude Code captures hook command stdout and injects it into the model's
//! context as a `<system-reminder>`. This is the same mechanism used by
//! gastown's `gt prime`.
//!
//! # Hook registration
//!
//! ```json
//! {
//!   "hooks": {
//!     "SessionStart": [{
//!       "matcher": "",
//!       "hooks": [{
//!         "type": "command",
//!         "command": "gw elden"
//!       }]
//!     }]
//!   }
//! }
//! ```
//!
//! # Stdin protocol
//!
//! Claude Code pipes JSON to stdin:
//! ```json
//! {"session_id": "uuid", "source": "startup", "hook_event_name": "SessionStart"}
//! ```
//!
//! `source` is one of: `startup`, `resume`, `compact`

use color_eyre::Result;
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

/// Hook event source — how the session was initiated.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookSource {
    Startup,
    Resume,
    Compact,
    #[serde(other)]
    Unknown,
}

/// JSON payload that Claude Code sends on stdin to hook commands.
#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    source: Option<HookSource>,
}

/// Execute the elden command.
///
/// Reads hook input from stdin, detects workspace state,
/// and prints context to stdout for Claude Code to capture.
pub fn execute() -> Result<()> {
    // 1. Read hook input from stdin (non-blocking with timeout)
    let hook_input = read_hook_input();

    let session_id = hook_input
        .as_ref()
        .and_then(|h| h.session_id.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let source = hook_input
        .as_ref()
        .and_then(|h| h.source.clone())
        .unwrap_or(HookSource::Startup);

    // 2. Route by source — compact/resume get brief context
    match source {
        HookSource::Compact | HookSource::Resume => {
            print_brief_context(&session_id, &source)?;
        }
        _ => {
            print_full_context(&session_id)?;
        }
    }

    Ok(())
}

/// Print full startup context (fires on fresh session start).
fn print_full_context(session_id: &str) -> Result<()> {
    // Header
    println!("[Greater-Will] session:{}", session_id);
    println!();

    // Role context
    println!("# Greater-Will Managed Session");
    println!();
    println!("You are running inside a Greater-Will orchestrated tmux session.");
    println!("Greater-Will monitors this session for crashes, stuck states, and timeouts.");
    println!();

    // Detect and print batch state
    if let Some(state) = detect_batch_state() {
        println!("## Current Batch");
        println!();
        println!("- Batch ID: {}", state.batch_id);
        println!(
            "- Progress: {}/{} plans",
            state.current_index,
            state.plans.len()
        );

        if let Some(current_plan) = state.plans.get(state.current_index) {
            println!("- Current plan: `{}`", current_plan);
            println!();
            println!("## Instructions");
            println!();
            println!(
                "Run the arc pipeline for the current plan:"
            );
            println!("```");
            println!("/rune:arc {} --resume", current_plan);
            println!("```");
        } else if state.current_index >= state.plans.len() {
            println!("- Status: All plans completed");
        }

        // Show recent results
        let recent: Vec<_> = state.results.iter().rev().take(3).collect();
        if !recent.is_empty() {
            println!();
            println!("## Recent Results");
            println!();
            for r in recent {
                let icon = match r.outcome.as_str() {
                    "Passed" => "OK",
                    "Failed" => "FAIL",
                    "Skipped" => "SKIP",
                    _ => "?",
                };
                println!("- [{}] {} ({:.0}s)", icon, r.plan, r.duration_secs);
            }
        }
    } else {
        // No batch state — check for single-session mode
        println!("## Mode: Single Session");
        println!();
        println!("No active batch detected. This session is running in single-session mode.");
        println!("Greater-Will will restart this session with `--resume` if it crashes.");
    }

    // Detect arc checkpoint
    if let Some(checkpoint_info) = detect_arc_checkpoint() {
        println!();
        println!("## Arc Checkpoint");
        println!();
        println!("- Plan: `{}`", checkpoint_info.plan_file);
        println!("- Phase: {}", checkpoint_info.current_phase);
        println!("- Completed: {}/{}", checkpoint_info.completed, checkpoint_info.total);

        if checkpoint_info.completed < checkpoint_info.total {
            println!();
            println!(
                "Resume from phase `{}`. Run `/rune:arc --resume` to continue.",
                checkpoint_info.current_phase
            );
        }
    }

    // Protocol reminders
    println!();
    println!("## Protocol");
    println!();
    println!("- Do NOT exit the session manually — Greater-Will manages the lifecycle");
    println!("- If stuck, Greater-Will will nudge after 5 minutes and restart after 10 minutes");
    println!("- All work is persisted via git commits and Rune checkpoints");

    Ok(())
}

/// Print brief context for resume/compact (minimize context usage).
fn print_brief_context(session_id: &str, source: &HookSource) -> Result<()> {
    let source_str = match source {
        HookSource::Resume => "resume",
        HookSource::Compact => "compact",
        _ => "unknown",
    };

    println!(
        "[Greater-Will] session:{} source:{} — Managed session, crash recovery active.",
        session_id, source_str
    );

    // On compact/resume, just remind about the current plan
    if let Some(state) = detect_batch_state() {
        if let Some(current_plan) = state.plans.get(state.current_index) {
            println!("Current plan: `{}`. Continue with `/rune:arc --resume`.", current_plan);
        }
    } else if let Some(checkpoint_info) = detect_arc_checkpoint() {
        println!(
            "Plan: `{}`, phase: {}. Continue with `/rune:arc --resume`.",
            checkpoint_info.plan_file, checkpoint_info.current_phase
        );
    }

    Ok(())
}

/// Read hook input from stdin (Claude Code sends JSON).
fn read_hook_input() -> Option<HookInput> {
    let mut input = String::new();

    // Read with a size limit to avoid hanging on empty stdin
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();

    // Try to read up to 4KB
    let mut buf = vec![0u8; 4096];
    match handle.read(&mut buf) {
        Ok(0) => return None,
        Ok(n) => {
            input = String::from_utf8_lossy(&buf[..n]).to_string();
        }
        Err(_) => return None,
    }

    // Parse JSON — be lenient (may have extra whitespace/newlines)
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    serde_json::from_str(trimmed).ok()
}

/// Minimal batch state for context injection (avoids importing full BatchState).
#[derive(Debug, Deserialize)]
struct MiniBatchState {
    batch_id: String,
    plans: Vec<String>,
    current_index: usize,
    #[serde(default)]
    results: Vec<MiniPlanResult>,
}

#[derive(Debug, Deserialize)]
struct MiniPlanResult {
    plan: String,
    outcome: String,
    #[serde(default)]
    duration_secs: f64,
}

/// Detect batch state from `.gw/batch-state.json`.
fn detect_batch_state() -> Option<MiniBatchState> {
    let state_path = Path::new(".gw/batch-state.json");
    if !state_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(state_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Minimal arc checkpoint info for context display.
struct ArcCheckpointInfo {
    plan_file: String,
    current_phase: String,
    completed: usize,
    total: usize,
}

/// Detect the most recent arc checkpoint.
fn detect_arc_checkpoint() -> Option<ArcCheckpointInfo> {
    // Look for checkpoint files in .rune/arc/*/checkpoint.json
    let arc_dir = Path::new(".rune/arc");
    if !arc_dir.exists() {
        // Also check tmp/arc/ (Rune's default location)
        return detect_arc_checkpoint_in(Path::new("tmp/arc"));
    }
    detect_arc_checkpoint_in(arc_dir)
}

fn detect_arc_checkpoint_in(arc_dir: &Path) -> Option<ArcCheckpointInfo> {
    if !arc_dir.exists() {
        return None;
    }

    // Find most recent checkpoint by modification time
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;

    if let Ok(entries) = std::fs::read_dir(arc_dir) {
        for entry in entries.flatten() {
            let cp_path = entry.path().join("checkpoint.json");
            if cp_path.exists() {
                if let Ok(meta) = cp_path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
                            newest = Some((modified, cp_path));
                        }
                    }
                }
            }
        }
    }

    let (_, cp_path) = newest?;
    let content = std::fs::read_to_string(&cp_path).ok()?;
    let checkpoint: serde_json::Value = serde_json::from_str(&content).ok()?;

    let plan_file = checkpoint
        .get("plan_file")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let phases = checkpoint.get("phases")?.as_object()?;
    let total = phases.len();
    let completed = phases
        .values()
        .filter(|v| {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            status == "completed" || status == "skipped"
        })
        .count();

    // Find first pending/in_progress phase
    // Use a simple heuristic: find phases that aren't completed/skipped
    let current_phase = phases
        .iter()
        .find(|(_, v)| {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            status != "completed" && status != "skipped"
        })
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| "all_complete".to_string());

    Some(ArcCheckpointInfo {
        plan_file,
        current_phase,
        completed,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_hook_input_empty() {
        // Empty stdin should return None
        let result: Option<HookInput> = serde_json::from_str("").ok();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_hook_input() {
        let json = r#"{"session_id":"abc-123","source":"startup"}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id.unwrap(), "abc-123");
        assert!(matches!(input.source.unwrap(), HookSource::Startup));
    }

    #[test]
    fn test_parse_hook_input_resume() {
        let json = r#"{"session_id":"xyz","source":"resume"}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert!(matches!(input.source.unwrap(), HookSource::Resume));
    }

    #[test]
    fn test_parse_hook_input_compact() {
        let json = r#"{"session_id":"xyz","source":"compact"}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert!(matches!(input.source.unwrap(), HookSource::Compact));
    }

    #[test]
    fn test_parse_hook_input_unknown_source() {
        let json = r#"{"session_id":"xyz","source":"something_new"}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert!(matches!(input.source.unwrap(), HookSource::Unknown));
    }

    #[test]
    fn test_parse_hook_input_minimal() {
        let json = r#"{}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert!(input.session_id.is_none());
        assert!(input.source.is_none());
    }

    #[test]
    fn test_detect_batch_state_no_file() {
        // No .gw/batch-state.json in test env
        let result = detect_batch_state();
        // May or may not exist depending on test env — just verify no panic
        let _ = result;
    }

    #[test]
    fn test_detect_arc_checkpoint_no_dir() {
        let result = detect_arc_checkpoint_in(Path::new("/nonexistent/path"));
        assert!(result.is_none());
    }
}
