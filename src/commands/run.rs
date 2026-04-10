//! Run command implementation.
//!
//! Executes arc phases for one or more plan files, with checkpoint auto-discovery for --resume.

use crate::batch::BatchRunner;
use crate::cleanup::startup_cleanup;
use crate::config::phase_config::{resolve_config, PhaseConfig};
use crate::engine::phase_executor::{ExecutorConfig, PhaseGroupExecutor, PhaseGroupState};
use crate::session::{shell_escape, Tmux};
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Execute the run command.
///
/// # Arguments
///
/// * `plans` - Plan files to execute
/// * `dry_run` - If true, validate but don't execute
/// * `mock` - Optional mock script for testing
/// * `group` - Optional phase group filter (A-G), only for multi-group mode
/// * `config_dir` - Optional custom configuration directory
/// * `resume` - If true, resume a previously interrupted run
/// * `multi_group` - If true, use legacy multi-group mode (7 sessions)
#[allow(clippy::too_many_arguments)] // TODO: extract RunConfig struct
pub fn execute(
    plans: Vec<String>,
    dry_run: bool,
    mock: Option<PathBuf>,
    group: Option<String>,
    config_dir: Option<PathBuf>,
    resume: bool,
    multi_group: bool,
    allow_dirty: bool,
    foreground: bool,
) -> Result<()> {
    let cwd = env::current_dir()?;

    // Resolve config_dir: CLI --config-dir takes priority, then $CLAUDE_CONFIG_DIR env var
    let config_dir = config_dir.or_else(|| {
        env::var("CLAUDE_CONFIG_DIR").ok().and_then(|v| {
            if v.is_empty() {
                None
            } else {
                let path = PathBuf::from(&v);
                tracing::info!(config_dir = %v, "Using CLAUDE_CONFIG_DIR from environment");
                Some(path)
            }
        })
    });

    // Daemon delegation: if the daemon is running and --foreground is not set,
    // submit the plan for background execution instead of running inline.
    // NOTE: This block is placed AFTER config_dir resolution so the daemon
    // receives the resolved config_dir (CLI flag or env var).
    if !foreground && !dry_run && mock.is_none() {
        if crate::client::socket::DaemonClient::is_daemon_running() {
            return delegate_to_daemon(&plans, &cwd, config_dir.as_deref());
        }
    }

    // Pre-flight: check for uncommitted git changes
    // Skip for dry-run, mock, and --allow-dirty
    if !dry_run && mock.is_none() && !allow_dirty {
        preflight_git_clean(&cwd)?;
    }

    // Resolve any GitHub issue URLs into generated plan files
    let plans = crate::github::resolve_github_urls(&plans, &cwd)
        .wrap_err("Failed to resolve GitHub issue URL(s)")?;

    // Expand glob patterns in plans
    let expanded_plans = expand_plan_globs(&plans)?;

    // Resolve config: file on disk → embedded defaults
    let config = match resolve_config(config_dir.as_deref(), &cwd)? {
        Some(path) => PhaseConfig::from_file(&path)
            .wrap_err_with(|| format!("Failed to load config from {}", path.display()))?,
        None => PhaseConfig::from_embedded()
            .wrap_err("Failed to load embedded default config")?,
    };
    config.validate()?;

    // Dry-run mode: print execution plan and exit (works for both modes)
    if dry_run {
        return print_dry_run(&expanded_plans, &config, group.as_ref());
    }

    // Mock mode: always uses multi-group style (spawns per-group tmux sessions)
    if let Some(mock_script) = mock {
        return run_mock(&expanded_plans, &mock_script, &config, group.as_ref());
    }

    // === Route to execution mode ===
    if multi_group {
        // Legacy multi-group mode: 7 sessions (A-G)
        run_multi_group(expanded_plans, config, group, config_dir, resume, &cwd)
    } else {
        // Default: single-session mode
        run_single(expanded_plans, config_dir, resume, &cwd)
    }
}

/// Single-session mode (default): one tmux session per plan, Rune drives phases.
fn run_single(
    plans: Vec<String>,
    config_dir: Option<PathBuf>,
    resume: bool,
    cwd: &Path,
) -> Result<()> {
    use crate::engine::single_session::{run_single_session, run_single_session_batch, SingleSessionConfig};

    preflight_tmux()?;

    let mut ss_config = SingleSessionConfig::new(cwd);
    if let Some(dir) = config_dir {
        ss_config = ss_config.with_config_dir(dir);
    }
    if resume {
        ss_config = ss_config.with_resume();
    }

    // --- Resume validation: read checkpoint from loop state and validate ---
    if resume && plans.len() == 1 {
        use crate::checkpoint::reader::{validate_before_resume, read_checkpoint, next_actionable_phase};

        let loop_state = crate::monitor::loop_state::read_arc_loop_state(cwd);
        match loop_state.active() {
            Some(state) => {
                let cp_path = state.resolve_checkpoint_path(cwd);
                if !cp_path.exists() {
                    eyre::bail!(
                        "Loop state exists but checkpoint not written yet: {}\nRetry shortly or start fresh without --resume.",
                        cp_path.display()
                    );
                }
                let cp = read_checkpoint(&cp_path)?;
                match next_actionable_phase(&cp) {
                    Some(next_phase) => {
                        println!("Found checkpoint: {}", cp_path.display());
                        println!("Arc: {} | Next phase: {}", cp.id, next_phase);
                        let arc_dir = cp_path.parent().unwrap();
                        let validation = validate_before_resume(&cp, arc_dir)?;
                        for w in &validation.warnings {
                            println!("  WARN: {}", w);
                        }
                        if !validation.can_resume() {
                            for e in &validation.errors {
                                println!("  ERROR: {}", e);
                            }
                            eyre::bail!(
                                "Pre-resume validation failed. {} critical artifact(s) missing. \
                                 Re-run without --resume to start fresh.",
                                validation.errors.len()
                            );
                        }
                    }
                    None => {
                        eyre::bail!("Arc already completed (all phases done). Start a fresh run without --resume.");
                    }
                }
            }
            None => {
                let plan_hint = plans.first().map(|p| p.as_str()).unwrap_or("<unknown>");
                eyre::bail!(
                    "No active arc state found for plan: {}\nStart a fresh run without --resume.",
                    plan_hint
                );
            }
        }
    }

    if plans.len() == 1 {
        let plan_path = Path::new(&plans[0]);
        let result = run_single_session(plan_path, &ss_config)?;

        println!();
        if result.success {
            println!("=== Pipeline completed ({:.1}s, {} restarts) ===", result.duration.as_secs_f64(), result.crash_restarts);
        } else {
            println!("=== Pipeline failed: {} ===", result.message);
            eyre::bail!("{}", result.message);
        }
    } else {
        let results = run_single_session_batch(&plans, &ss_config)?;

        println!();
        let passed = results.iter().filter(|r| r.success).count();
        let failed = results.len() - passed;
        println!("=== Batch: {}/{} passed ===", passed, results.len());

        if failed > 0 {
            eyre::bail!("{} plan(s) failed", failed);
        }
    }

    Ok(())
}

/// Legacy multi-group mode: 7 sessions (A-G), one per phase group.
fn run_multi_group(
    plans: Vec<String>,
    mut config: PhaseConfig,
    group: Option<String>,
    config_dir: Option<PathBuf>,
    resume: bool,
    cwd: &Path,
) -> Result<()> {
    // Validate group if specified
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

    // Filter config groups if --group specified
    if let Some(ref g) = group {
        config.groups.retain(|grp| grp.name == *g);
        if config.groups.is_empty() {
            eyre::bail!("No groups remaining after filter");
        }
    }

    // Resume mode: reload batch state and continue
    if resume {
        return run_batch_resume(&config, config_dir.as_deref(), cwd);
    }

    // Use batch runner for multiple plans, single executor for one
    if plans.len() > 1 {
        run_batch(&plans, &config, config_dir.as_deref(), cwd)
    } else {
        run_real(&plans, config, group, config_dir.clone(), cwd)
    }
}

/// Run multiple plans using the batch queue manager.
fn run_batch(
    plans: &[String],
    config: &PhaseConfig,
    config_dir: Option<&Path>,
    cwd: &Path,
) -> Result<()> {
    preflight_tmux()?;

    let mut exec_config = ExecutorConfig::new(cwd);
    if let Some(dir) = config_dir {
        exec_config = exec_config.with_config_dir(dir.to_path_buf());
    }

    let mut runner = BatchRunner::new(plans.to_vec())
        .wrap_err("Failed to initialize batch runner")?;

    let summary = runner.run(config, &exec_config)?;

    println!();
    println!("{}", summary);

    if summary.failed > 0 {
        eyre::bail!("{} plan(s) failed in batch", summary.failed);
    }

    Ok(())
}

/// Resume a previously interrupted batch run.
fn run_batch_resume(
    config: &PhaseConfig,
    config_dir: Option<&Path>,
    cwd: &Path,
) -> Result<()> {
    preflight_tmux()?;

    let mut exec_config = ExecutorConfig::new(cwd);
    if let Some(dir) = config_dir {
        exec_config = exec_config.with_config_dir(dir.to_path_buf());
    }

    let mut runner = BatchRunner::resume()
        .wrap_err("Failed to resume batch")?;

    let summary = runner.run(config, &exec_config)?;

    println!();
    println!("{}", summary);

    if summary.failed > 0 {
        eyre::bail!("{} plan(s) failed in resumed batch", summary.failed);
    }

    Ok(())
}

/// Run real execution using the PhaseGroupExecutor.
fn run_real(
    plans: &[String],
    mut config: PhaseConfig,
    group: Option<String>,
    config_dir: Option<PathBuf>,
    cwd: &Path,
) -> Result<()> {
    // Pre-flight: tmux availability check
    preflight_tmux()?;

    // Startup cleanup: kill stale sessions from previous crashed runs
    startup_cleanup()?;

    // Filter config groups if --group specified
    if let Some(ref g) = group {
        config.groups.retain(|grp| grp.name == *g);
        if config.groups.is_empty() {
            eyre::bail!("No groups remaining after filter");
        }
    }

    println!("=== Greater-Will Arc Executor ===");
    println!(
        "Plans: {} | Groups: {} | Phases: {}",
        plans.len(),
        config.groups.len(),
        config.total_phases()
    );
    if let Some(ref dir) = config_dir {
        println!("Config: {} (CLAUDE_CONFIG_DIR)", dir.display());
    }
    println!();

    let mut all_succeeded = true;

    for plan in plans {
        let plan_path = Path::new(plan);

        // Validate plan exists
        if !plan_path.exists() {
            eyre::bail!("Plan file not found: {}", plan);
        }

        println!("--- Executing plan: {} ---", plan);

        // Build executor config
        let mut exec_config = ExecutorConfig::new(cwd);
        if let Some(ref dir) = config_dir {
            exec_config = exec_config.with_config_dir(dir.clone());
        }

        // Create executor and run
        let mut executor = PhaseGroupExecutor::new(config.clone());
        let results = executor.execute_plan(plan_path, &exec_config)?;

        // Print results summary
        println!();
        println!("=== Results for {} ===", plan);
        for result in &results {
            let status_icon = match result.state {
                PhaseGroupState::Succeeded => "✓",
                PhaseGroupState::Skipped => "⊘",
                PhaseGroupState::Failed { .. } => "✗",
                _ => "?",
            };
            println!(
                "  {} Group {}: {:?} ({:.1}s, {} retries)",
                status_icon,
                result.group_name,
                result.state,
                result.duration.as_secs_f64(),
                result.retries,
            );
            if let Some(ref err) = result.error_message {
                println!("    Error: {}", err);
            }
        }

        // Check for failures
        let failed = results.iter().any(|r| matches!(r.state, PhaseGroupState::Failed { .. }));
        if failed {
            all_succeeded = false;
            println!();
            println!("Plan {} had failures. Stopping.", plan);
            break;
        }

        println!("Plan {} completed successfully.", plan);
        println!();
    }

    if all_succeeded {
        println!("=== All plans completed successfully ===");
    } else {
        eyre::bail!("One or more plans failed");
    }

    Ok(())
}

/// Check that the git working tree is clean (no uncommitted changes).
///
/// The arc pipeline creates commits and modifies files. Running with
/// uncommitted changes risks:
/// - Unintended changes getting swept into arc-generated commits
/// - Merge conflicts when Rune tries to commit
/// - Lost work if a crash triggers cleanup
///
/// Users can bypass with `--allow-dirty`.
fn preflight_git_clean(cwd: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output();

    match output {
        Err(_) => {
            // git not available or not a git repo — skip the check
            tracing::debug!("git not available, skipping dirty check");
            Ok(())
        }
        Ok(out) if !out.status.success() => {
            // Not a git repo or other git error — skip silently
            tracing::debug!("git status failed, skipping dirty check");
            Ok(())
        }
        Ok(out) => {
            let status = String::from_utf8_lossy(&out.stdout);
            let dirty_files: Vec<&str> = status
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect();

            if dirty_files.is_empty() {
                tracing::info!("Git working tree is clean");
                return Ok(());
            }

            // Show what's dirty
            let preview_count = dirty_files.len().min(10);
            let mut msg = format!(
                "Git working tree has {} uncommitted change(s):\n",
                dirty_files.len()
            );
            for line in &dirty_files[..preview_count] {
                msg.push_str(&format!("  {}\n", line));
            }
            if dirty_files.len() > preview_count {
                msg.push_str(&format!("  ... and {} more\n", dirty_files.len() - preview_count));
            }
            msg.push_str("\nCommit or stash your changes before running, or use --allow-dirty to bypass.");

            eyre::bail!("{}", msg);
        }
    }
}

/// Check that tmux is available.
fn preflight_tmux() -> Result<()> {
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
            Ok(())
        }
    }
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

/// Delegate plan execution to the background daemon.
///
/// Resolves plan paths to absolute, submits each to the daemon, and prints
/// the run ID so the user can monitor with `gw ps` / `gw logs`.
fn delegate_to_daemon(plans: &[String], cwd: &Path, config_dir: Option<&Path>) -> Result<()> {
    use crate::client::socket::DaemonClient;
    use crate::daemon::protocol::{Request, Response};
    use crate::output::tags::tag;

    println!("{} Daemon detected — submitting run(s) for background execution", tag("RUN"));
    println!();

    for plan in plans {
        // Resolve to absolute path so the daemon can find it from any cwd
        let plan_path = if Path::new(plan).is_absolute() {
            PathBuf::from(plan)
        } else {
            cwd.join(plan)
        };

        if !plan_path.exists() {
            eyre::bail!("Plan file not found: {}", plan_path.display());
        }

        let client = DaemonClient::new()?;
        let request = Request::SubmitRun {
            plan_path: plan_path.clone(),
            repo_dir: cwd.to_path_buf(),
            session_name: None,
            config_dir: config_dir.map(|p| p.to_path_buf()),
        };

        match client.send(request)? {
            Response::RunSubmitted { run_id } => {
                let short = crate::commands::util::short_id(&run_id);
                println!("{} Submitted: {} -> run {}", tag("OK"), plan, short);
                println!("  Monitor: gw ps");
                println!("  Logs:    gw logs {}", short);
                println!("  Stop:    gw stop {}", short);
                println!();
            }
            Response::Error { message, .. } => {
                println!("{} Failed to submit {}: {}", tag("FAIL"), plan, message);
            }
            _ => {
                println!("{} Unexpected response for {}", tag("WARN"), plan);
            }
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