//! Run command implementation.
//!
//! Executes arc phases for one or more plan files.

use crate::config::phase_config::{resolve_config, PhaseConfig};
use crate::session::{shell_escape, Tmux};
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Execute the run command.
///
/// # Arguments
///
/// * `plans` - Plan files to execute
/// * `dry_run` - If true, validate but don't execute
/// * `mock` - Optional mock script for testing
/// * `group` - Optional phase group filter (A-G)
/// * `config_dir` - Optional custom configuration directory
pub fn execute(
    plans: Vec<String>,
    dry_run: bool,
    mock: Option<PathBuf>,
    group: Option<String>,
    config_dir: Option<PathBuf>,
) -> Result<()> {
    // Resolve config path
    let cwd = env::current_dir()?;
    let config_path = resolve_config(config_dir.as_deref(), &cwd)?;

    // Load phase config
    let config = PhaseConfig::from_file(&config_path)
        .wrap_err_with(|| format!("Failed to load config from {}", config_path.display()))?;

    // Validate config
    config.validate()?;

    // Validate group if specified (derived from loaded config, not hardcoded)
    if let Some(ref g) = group {
        let valid_groups: Vec<&str> = config.groups.iter().map(|grp| grp.name.as_str()).collect();
        if !valid_groups.contains(&g.as_str()) {
            eyre::bail!(
                "Unknown group '{}'. Valid groups: {}",
                g,
                valid_groups.join(", ")
            );
        }
    }

    // Expand glob patterns in plans
    let expanded_plans = expand_plan_globs(&plans)?;

    // Dry-run mode: print execution plan and exit
    if dry_run {
        return print_dry_run(&expanded_plans, &config, group.as_ref());
    }

    // Mock mode: run mock script in tmux sessions
    if let Some(mock_script) = mock {
        return run_mock(&expanded_plans, &mock_script, &config, group.as_ref());
    }

    // Real execution not yet implemented
    tracing::info!("Plans to execute: {:?}", expanded_plans);
    if let Some(g) = group {
        tracing::info!("Running group: {}", g);
    }

    println!("run: real execution not yet implemented");
    println!("Use --dry-run to validate or --mock <script> for testing");
    Ok(())
}

/// Expand glob patterns in plan file paths.
///
/// The shell typically expands globs before passing to the CLI,
/// but this handles the case where the shell doesn't or when
/// using single quotes.
fn expand_plan_globs(plans: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::new();

    for pattern in plans {
        // Check if it looks like a glob pattern
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            // Try to expand the glob
            match glob::glob(pattern) {
                Ok(paths) => {
                    let mut found_any = false;
                    for path_result in paths {
                        match path_result {
                            Ok(path) => {
                                expanded.push(path.to_string_lossy().to_string());
                                found_any = true;
                            }
                            Err(e) => {
                                tracing::warn!("Glob error for {}: {}", pattern, e);
                            }
                        }
                    }
                    if !found_any {
                        eyre::bail!("No plan files matched pattern: {}", pattern);
                    }
                }
                Err(e) => {
                    // Invalid glob pattern, treat as literal
                    tracing::warn!("Invalid glob pattern '{}': {}, treating as literal", pattern, e);
                    expanded.push(pattern.clone());
                }
            }
        } else {
            // Not a glob, use as-is
            expanded.push(pattern.clone());
        }
    }

    Ok(expanded)
}

/// Print the dry-run execution plan.
fn print_dry_run(
    plans: &[String],
    config: &PhaseConfig,
    group: Option<&String>,
) -> Result<()> {
    println!("[DRY] === Execution Plan ===");
    println!("[DRY] Plans: {:?}", plans);
    println!(
        "[DRY] Config loaded: {} groups, {} phases",
        config.groups.len(),
        config.total_phases()
    );
    println!("[DRY] Default timeout: {}m", config.settings.default_timeout_min);
    println!();

    println!("[DRY] === Phase Groups ===");
    for g in &config.groups {
        // Filter by group if specified
        if let Some(filter) = group {
            if g.name != *filter {
                continue;
            }
        }

        println!(
            "  Group {}  {}  timeout={}m  ({} phases)",
            g.name,
            g.label,
            g.timeout_min,
            g.phases.len()
        );

        if let Some(skip_if) = &g.skip_if {
            println!("           skip_if: {}", skip_if);
        }

        // List phases (truncate if too many)
        if g.phases.len() <= 5 {
            println!("           phases: {:?}", g.phases);
        } else {
            println!(
                "           phases: {:?} ... (+{} more)",
                &g.phases[..5],
                g.phases.len() - 5
            );
        }
    }

    println!();
    println!("[DRY] Total: ~{}m max timeout", config.total_timeout_min());
    println!("[DRY] No sessions spawned.");
    Ok(())
}

/// Run mock mode with a test script.
fn run_mock(
    plans: &[String],
    mock_script: &PathBuf,
    config: &PhaseConfig,
    group: Option<&String>,
) -> Result<()> {
    // Pre-flight checks
    preflight_mock(mock_script)?;

    println!("[MOCK] === Mock Mode Execution ===");
    println!("[MOCK] Mock script: {}", mock_script.display());
    println!("[MOCK] Plans: {:?}", plans);
    println!();

    for plan in plans {
        println!("[MOCK] Processing plan: {}", plan);

        for g in &config.groups {
            // Filter by group if specified
            if let Some(filter) = group {
                if g.name != *filter {
                    continue;
                }
            }

            // Check skip condition
            if let Some(skip_if) = &g.skip_if {
                println!("[MOCK] Group {} skipped: {}", g.name, skip_if);
                continue;
            }

            // Create unique session name
            let timestamp = chrono::Local::now().format("%Y%m%d%H%M%S");
            let session_name = format!("gw-mock-{}-{}", g.name, timestamp);

            println!("[MOCK] Starting group {} in session: {}", g.name, session_name);

            let tmux = Tmux::new(&session_name)?;

            // Create session
            tmux.create_session()
                .wrap_err_with(|| format!("Failed to create tmux session: {}", session_name))?;

            // Set environment variables for the mock script (shell-escaped for safety)
            tmux.send_command(&format!("export GW_GROUP_NAME={}", shell_escape(&g.name)))?;
            tmux.send_command(&format!("export GW_GROUP_LABEL={}", shell_escape(&g.label)))?;
            tmux.send_command(&format!("export GW_PLAN={}", shell_escape(plan)))?;

            // Run mock script followed by exit to close session when done
            let cmd = format!("{} {} && exit || exit", shell_escape(&mock_script.display().to_string()), shell_escape(plan));
            tmux.send_command(&cmd)?;

            // Wait for completion (simple polling)
            println!("[MOCK] Waiting for completion...");
            wait_for_session_completion(&tmux, g.timeout_min)?;

            // Capture output
            let output = tmux.capture_pane()?;
            println!("[MOCK] Output from {}:", session_name);
            for line in output.lines().take(20) {
                println!("  {}", line);
            }

            // Clean up
            tmux.kill_session()
                .wrap_err_with(|| format!("Failed to kill tmux session: {}", session_name))?;

            println!("[MOCK] Group {} complete", g.name);
            println!();
        }
    }

    println!("[MOCK] All groups complete.");
    Ok(())
}

/// Pre-flight checks for mock mode.
fn preflight_mock(mock_path: &PathBuf) -> Result<()> {
    // Check tmux is installed
    let tmux_check = Command::new("tmux").arg("-V").output();
    match tmux_check {
        Err(_) => {
            eyre::bail!(
                "tmux is required but not found. Install with: brew install tmux (macOS) or apt install tmux (Linux)"
            );
        }
        Ok(out) if !out.status.success() => {
            eyre::bail!("tmux check failed. Please verify tmux is installed correctly.");
        }
        Ok(out) => {
            let version = String::from_utf8_lossy(&out.stdout);
            tracing::info!("tmux version: {}", version.trim());
        }
    }

    // Check mock script exists
    if !mock_path.exists() {
        eyre::bail!("Mock script not found: {}", mock_path.display());
    }

    // Check mock script is executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(mock_path)?;
        let mode = metadata.permissions().mode();
        if (mode & 0o111) == 0 {
            eyre::bail!(
                "Mock script is not executable. Run: chmod +x {}",
                mock_path.display()
            );
        }
    }

    Ok(())
}

/// Wait for a tmux session to complete (process exits).
fn wait_for_session_completion(tmux: &Tmux, timeout_min: u32) -> Result<()> {
    let timeout = Duration::from_secs(timeout_min as u64 * 60);
    let check_interval = Duration::from_secs(2);
    let start = std::time::Instant::now();

    loop {
        // Check if session still exists
        if !Tmux::has_session(tmux.name()) {
            tracing::debug!("Session {} has ended", tmux.name());
            return Ok(());
        }

        // Check for timeout
        if start.elapsed() > timeout {
            tracing::error!("Session {} timed out after {}m", tmux.name(), timeout_min);
            eyre::bail!(
                "Session '{}' timed out after {} minutes. The mock script may be stuck.",
                tmux.name(),
                timeout_min
            );
        }

        // Wait before next check
        std::thread::sleep(check_interval);
    }
}