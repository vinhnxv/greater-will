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

use chrono::Utc;
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::checkpoint::phase_order::PHASE_ORDER;

// ─── Session ID Management ──────────────────────────────────────────

/// File where session UUID is persisted for cross-hook access.
const SESSION_ID_FILE: &str = ".gw/session_id";

/// Resolve session ID from available sources (priority order):
///   1. GW_SESSION_ID env var
///   2. Persisted .gw/session_id file (written by SessionStart hook)
///   3. Stdin JSON from Claude Code
///   4. "unknown" fallback
///
/// Same pattern as gastown's readHookSessionID().
fn resolve_session_id(hook_input: &Option<HookInput>) -> String {
    // 1. Env var (set by gw monitor or prior hook)
    if let Ok(id) = std::env::var("GW_SESSION_ID") {
        if !id.is_empty() {
            return id;
        }
    }

    // 2. Persisted file (written by SessionStart hook)
    if let Ok(content) = std::fs::read_to_string(SESSION_ID_FILE) {
        let id = content.lines().next().unwrap_or("").trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }

    // 3. Stdin JSON (Claude Code sends session_id)
    if let Some(input) = hook_input {
        if let Some(ref id) = input.session_id {
            if !id.is_empty() {
                return id.clone();
            }
        }
    }

    // 4. Fallback
    "unknown".to_string()
}

/// Persist session ID to file.
///
/// Called by SessionStart hook so subsequent hooks (Stop, StopFailure, etc.)
/// can find the session ID without needing stdin JSON.
fn persist_session_id(session_id: &str) {
    // Write to file
    if let Some(parent) = Path::new(SESSION_ID_FILE).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = format!("{}\n{}\n", session_id, Utc::now().to_rfc3339());
    let _ = std::fs::write(SESSION_ID_FILE, content);

    // Note: env var intentionally not set — std::env::set_var is unsound in
    // multi-threaded Rust. Session ID is persisted to file above instead.
}

/// Read the persisted session ID (for use by monitor loop).
pub fn read_persisted_session_id() -> Option<String> {
    let content = std::fs::read_to_string(SESSION_ID_FILE).ok()?;
    let id = content.lines().next()?.trim().to_string();
    if id.is_empty() { None } else { Some(id) }
}

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

    // 2. Resolve and persist session ID
    let session_id = resolve_session_id(&hook_input);
    persist_session_id(&session_id);

    let source = hook_input
        .as_ref()
        .and_then(|h| h.source.clone())
        .unwrap_or(HookSource::Startup);

    // 3. Route by source — compact/resume get brief context
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

// ─── Event-Specific Handlers ─────────────────────────────────────────

/// Directory for gw signal files.
/// Monitor loop watches these for reliable event detection.
const SIGNAL_DIR: &str = ".gw/signals";

/// Execute a specific hook event handler.
///
/// Called via `gw elden --event <name>`.
pub fn execute_event(event: &str) -> Result<()> {
    // Ensure signal directory exists with restrictive permissions
    std::fs::create_dir_all(SIGNAL_DIR)
        .wrap_err_with(|| format!("Failed to create {}", SIGNAL_DIR))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        if let Err(e) = std::fs::set_permissions(SIGNAL_DIR, perms) {
            tracing::warn!(error = %e, "Failed to set signal directory permissions");
        }
    }

    match event {
        "stop" => handle_stop_event(),
        "stop-failure" => handle_stop_failure_event(),
        "session-end" => handle_session_end_event(),
        "permission" => handle_permission_event(),
        _ => {
            tracing::warn!(event = %event, "Unknown elden event");
            Ok(())
        }
    }
}

/// Handle the `Stop` hook — Claude Code is stopping (normal or error).
///
/// Writes a signal file that the monitor loop can detect instantly,
/// instead of waiting for process death or text pattern matching.
fn handle_stop_event() -> Result<()> {
    let hook_input = read_hook_input();
    let session_id = resolve_session_id(&hook_input);

    let checkpoint_info = detect_arc_checkpoint();

    let signal = json!({
        "event": "stop",
        "session_id": session_id,
        "timestamp": Utc::now().to_rfc3339(),
        "checkpoint": checkpoint_info.as_ref().map(|ci| json!({
            "plan_file": ci.plan_file,
            "current_phase": ci.current_phase,
            "completed": ci.completed,
            "total": ci.total,
        })),
    });

    let signal_path = PathBuf::from(SIGNAL_DIR).join("session-stop.json");
    std::fs::write(&signal_path, serde_json::to_string_pretty(&signal)?)
        .wrap_err("Failed to write stop signal")?;

    Ok(())
}

/// Handle the `StopFailure` hook — turn ended due to an API error.
///
/// This is the most direct way to detect API errors (rate limit, auth,
/// overload). Much more reliable than pane text matching because Claude Code
/// itself tells us the turn failed. The monitor loop can immediately apply
/// the correct retry strategy without guessing from output text.
///
/// Note: Claude Code ignores stdout and exit code for this hook.
fn handle_stop_failure_event() -> Result<()> {
    let hook_input = read_hook_input();
    let session_id = resolve_session_id(&hook_input);

    let checkpoint_info = detect_arc_checkpoint();

    let signal = json!({
        "event": "stop_failure",
        "session_id": session_id,
        "timestamp": Utc::now().to_rfc3339(),
        "checkpoint": checkpoint_info.as_ref().map(|ci| json!({
            "plan_file": ci.plan_file,
            "current_phase": ci.current_phase,
            "completed": ci.completed,
            "total": ci.total,
        })),
    });

    let signal_path = PathBuf::from(SIGNAL_DIR).join("stop-failure.json");
    std::fs::write(&signal_path, serde_json::to_string_pretty(&signal)?)
        .wrap_err("Failed to write stop-failure signal")?;

    Ok(())
}

/// Handle the `SessionEnd` hook — session is closing.
///
/// Similar to Stop but fires specifically on session teardown.
/// Writes a distinct signal file so monitor can differentiate
/// clean exit from crash.
fn handle_session_end_event() -> Result<()> {
    let hook_input = read_hook_input();
    let session_id = resolve_session_id(&hook_input);

    let checkpoint_info = detect_arc_checkpoint();
    let is_complete = checkpoint_info
        .as_ref()
        .map(|ci| ci.completed >= ci.total && ci.total > 0)
        .unwrap_or(false);

    let signal = json!({
        "event": "session_end",
        "session_id": session_id,
        "timestamp": Utc::now().to_rfc3339(),
        "is_complete": is_complete,
        "checkpoint": checkpoint_info.as_ref().map(|ci| json!({
            "plan_file": ci.plan_file,
            "current_phase": ci.current_phase,
            "completed": ci.completed,
            "total": ci.total,
        })),
    });

    let signal_path = PathBuf::from(SIGNAL_DIR).join("session-end.json");
    std::fs::write(&signal_path, serde_json::to_string_pretty(&signal)?)
        .wrap_err("Failed to write session-end signal")?;

    Ok(())
}

/// Handle the `PermissionRequest` hook — Claude is waiting for user permission.
///
/// Writes a heartbeat file that the monitor loop checks to avoid
/// misclassifying permission waits as "stuck" sessions.
/// The monitor should reset its idle timer when this file is fresh.
fn handle_permission_event() -> Result<()> {
    let hook_input = read_hook_input();
    let session_id = resolve_session_id(&hook_input);

    let signal = json!({
        "event": "permission_request",
        "session_id": session_id,
        "timestamp": Utc::now().to_rfc3339(),
    });

    let signal_path = PathBuf::from(SIGNAL_DIR).join("permission-pending.json");
    std::fs::write(&signal_path, serde_json::to_string_pretty(&signal)?)
        .wrap_err("Failed to write permission signal")?;

    Ok(())
}

// ─── Signal File Reader (for monitor loop) ──────────────────────────

/// Check if a stop signal file exists.
///
/// Used by the monitor loop to detect clean session exit.
/// If `expected_session_id` is Some, validates the signal belongs to this session.
pub fn read_stop_signal() -> Option<Value> {
    read_signal_file("session-stop.json")
}

/// Check if a session-end signal file exists.
pub fn read_session_end_signal() -> Option<Value> {
    read_signal_file("session-end.json")
}

/// Check if an API error signal exists (from StopFailure hook).
pub fn read_stop_failure_signal() -> Option<Value> {
    read_signal_file("stop-failure.json")
}

/// Read a signal file and validate it belongs to the expected session.
///
/// Returns None if the signal is for a different session.
/// This prevents cross-session signal contamination when multiple
/// gw instances run in the same project directory.
pub fn read_signal_for_session(filename: &str, expected_session_id: &str) -> Option<Value> {
    let signal = read_signal_file(filename)?;
    let signal_session = signal.get("session_id").and_then(|v| v.as_str()).unwrap_or("");

    // Accept if session matches or if either side is "unknown"
    if signal_session == expected_session_id
        || signal_session == "unknown"
        || expected_session_id == "unknown"
    {
        Some(signal)
    } else {
        None
    }
}

/// Check if a permission-pending signal exists and is recent.
///
/// Returns true if Claude is waiting for permission (within last 60s).
/// Monitor should NOT count this as idle time.
pub fn is_permission_pending() -> bool {
    let signal_path = PathBuf::from(SIGNAL_DIR).join("permission-pending.json");
    match std::fs::metadata(&signal_path) {
        Ok(meta) => {
            // Check if file was written in the last 60 seconds
            meta.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age.as_secs() < 60)
                .unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Clean up signal files (called before starting a new session).
pub fn clear_signals() {
    let signal_dir = PathBuf::from(SIGNAL_DIR);
    if signal_dir.exists() {
        let _ = std::fs::remove_file(signal_dir.join("session-stop.json"));
        let _ = std::fs::remove_file(signal_dir.join("stop-failure.json"));
        let _ = std::fs::remove_file(signal_dir.join("session-end.json"));
        let _ = std::fs::remove_file(signal_dir.join("permission-pending.json"));
    }
}

/// Read and parse a signal file.
fn read_signal_file(filename: &str) -> Option<Value> {
    let signal_path = PathBuf::from(SIGNAL_DIR).join(filename);
    let content = std::fs::read_to_string(&signal_path).ok()?;
    serde_json::from_str(&content).ok()
}

// ─── Hook Installation ───────────────────────────────────────────────

/// Path to Claude Code settings file (project-local).
const SETTINGS_PATH: &str = ".claude/settings.json";

/// The hook command we register.
const HOOK_COMMAND: &str = "gw elden";

/// Marker to identify our hooks in the settings file.
const HOOK_MARKER: &str = "gw elden";

/// Install gw elden hooks into `.claude/settings.json`.
///
/// Creates the file if missing. Merges with existing hooks — never
/// overwrites user's other hooks. Safe to run multiple times (idempotent).
pub fn install() -> Result<()> {
    let settings_path = PathBuf::from(SETTINGS_PATH);

    // Ensure .claude/ directory exists
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("Failed to create {}", parent.display()))?;
    }

    // Load existing settings or start fresh
    let mut settings = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .wrap_err("Failed to read .claude/settings.json")?;
        serde_json::from_str::<Value>(&content)
            .wrap_err("Failed to parse .claude/settings.json")?
    } else {
        json!({})
    };

    // All hooks gw needs, with their commands
    let required_hooks: &[(&str, &str)] = &[
        ("SessionStart", HOOK_COMMAND),
        ("PreCompact", HOOK_COMMAND),
        ("Stop", "gw elden --event stop"),
        ("StopFailure", "gw elden --event stop-failure"),
        ("SessionEnd", "gw elden --event session-end"),
        ("PermissionRequest", "gw elden --event permission"),
    ];

    // Pre-compute which hooks are missing (before taking mutable borrow)
    let missing: Vec<(String, String)> = required_hooks
        .iter()
        .filter(|(event, _)| !has_gw_hook(&settings, event))
        .map(|(e, c)| (e.to_string(), c.to_string()))
        .collect();

    if missing.is_empty() {
        println!("All gw elden hooks already installed in {}", SETTINGS_PATH);
        println!();
        print_hook_summary(&settings);
        return Ok(());
    }

    // Now take mutable borrow for installation
    let obj = settings
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("settings.json root is not an object"))?;

    if !obj.contains_key("hooks") {
        obj.insert("hooks".to_string(), json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .ok_or_else(|| eyre::eyre!("hooks key missing after insert"))?
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("\"hooks\" is not an object"))?;

    for (event, command) in &missing {
        install_hook(hooks, event, command);
    }

    // Write back
    let output = serde_json::to_string_pretty(&settings)
        .wrap_err("Failed to serialize settings")?;
    std::fs::write(&settings_path, output)
        .wrap_err("Failed to write .claude/settings.json")?;

    println!("Installed gw elden hooks into {}", SETTINGS_PATH);
    println!();
    print_hook_summary(&settings);

    Ok(())
}

/// Remove gw elden hooks from `.claude/settings.json`.
pub fn uninstall() -> Result<()> {
    let settings_path = PathBuf::from(SETTINGS_PATH);

    if !settings_path.exists() {
        println!("No {} found — nothing to uninstall.", SETTINGS_PATH);
        return Ok(());
    }

    let content = std::fs::read_to_string(&settings_path)
        .wrap_err("Failed to read .claude/settings.json")?;
    let mut settings: Value = serde_json::from_str(&content)
        .wrap_err("Failed to parse .claude/settings.json")?;

    let mut removed = false;

    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for event in &["SessionStart", "PreCompact", "Stop", "StopFailure", "SessionEnd", "PermissionRequest"] {
            if remove_gw_hooks(hooks, event) {
                removed = true;
            }
        }
    }

    if removed {
        let output = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, output)?;
        println!("Removed gw elden hooks from {}", SETTINGS_PATH);
    } else {
        println!("No gw elden hooks found in {}", SETTINGS_PATH);
    }

    Ok(())
}

/// Show current hook registration status.
pub fn hook_status() -> Result<()> {
    let settings_path = PathBuf::from(SETTINGS_PATH);

    if !settings_path.exists() {
        println!("No {} found.", SETTINGS_PATH);
        println!();
        println!("Run `gw elden --install` to register hooks.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&settings_path)
        .wrap_err("Failed to read .claude/settings.json")?;
    let settings: Value = serde_json::from_str(&content)
        .wrap_err("Failed to parse .claude/settings.json")?;

    print_hook_summary(&settings);

    if !has_gw_hook(&settings, "SessionStart") {
        println!();
        println!("Run `gw elden --install` to register hooks.");
    }

    Ok(())
}

/// Install a single hook entry for a given event, preserving existing hooks.
fn install_hook(hooks: &mut serde_json::Map<String, Value>, event: &str, command: &str) {
    let new_entry = json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": command
        }]
    });

    match hooks.get_mut(event) {
        Some(existing) if existing.is_array() => {
            // Append to existing array — is_array() guard ensures as_array_mut() succeeds
            if let Some(arr) = existing.as_array_mut() {
                arr.push(new_entry);
            }
        }
        _ => {
            // Create new array with our entry
            hooks.insert(event.to_string(), json!([new_entry]));
        }
    }
}

/// Check if a gw elden hook is already registered for an event.
fn has_gw_hook(settings: &Value, event: &str) -> bool {
    settings
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|arr| arr.as_array())
        .map(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains(HOOK_MARKER))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Remove gw elden hooks from a specific event. Returns true if any were removed.
fn remove_gw_hooks(hooks: &mut serde_json::Map<String, Value>, event: &str) -> bool {
    let Some(entries) = hooks.get_mut(event).and_then(|v| v.as_array_mut()) else {
        return false;
    };

    let before = entries.len();
    entries.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|inner| {
                inner.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(HOOK_MARKER))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    let removed = entries.len() < before;

    // Clean up empty arrays
    if entries.is_empty() {
        hooks.remove(event);
    }

    removed
}

/// Print a summary of registered hooks.
fn print_hook_summary(settings: &Value) {
    println!("Hook Status:");

    for event in &["SessionStart", "PreCompact", "Stop", "StopFailure", "SessionEnd", "PermissionRequest"] {
        let installed = has_gw_hook(settings, event);
        let icon = if installed { "OK" } else { "--" };
        println!("  [{}] {}", icon, event);
    }
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
    let input;

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

/// Detect the active arc checkpoint via arc-phase-loop.local.md — no directory scanning.
fn detect_arc_checkpoint() -> Option<ArcCheckpointInfo> {
    let cwd = std::env::current_dir().ok()?;
    let loop_state = crate::monitor::loop_state::read_arc_loop_state(&cwd)?;
    let cp_path = loop_state.resolve_checkpoint_path(&cwd);
    if !cp_path.exists() {
        return None;
    }

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

    // Find first pending/in_progress phase using canonical PHASE_ORDER
    // (serde_json::Map iteration order is not guaranteed to match execution order)
    let current_phase = PHASE_ORDER
        .iter()
        .find(|&phase_name| {
            phases.get(*phase_name).map_or(false, |v| {
                let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                status != "completed" && status != "skipped"
            })
        })
        .map(|s| s.to_string())
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

}
