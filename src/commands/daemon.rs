//! Daemon management subcommands.
//!
//! Provides start/stop/restart/status/install/uninstall for the gw daemon.
//! The daemon runs as a background process and can optionally be managed
//! via macOS launchd for auto-start on login.

use crate::client::socket::DaemonClient;
use crate::config::cli_args::DaemonAction;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{ensure_gw_home, gw_home, GlobalConfig};
use crate::output::tags::tag;
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::warn;

/// Dispatch a daemon action.
pub fn execute(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start {
            foreground,
            verbose,
        } => start(foreground, verbose),
        DaemonAction::Stop => stop(),
        DaemonAction::Status => cmd_status(),
        DaemonAction::Restart => restart(),
        DaemonAction::Install => install(),
        DaemonAction::Uninstall => uninstall(),
    }
}

/// Start the daemon process.
fn start(foreground: bool, verbose: u8) -> Result<()> {
    // Ensure state directories exist
    ensure_gw_home()?;

    if DaemonClient::is_daemon_running() {
        println!("{} Daemon is already running.", tag("WARN"));
        return Ok(());
    }

    if foreground {
        println!("{} Starting daemon in foreground...", tag("RUN"));
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(crate::daemon::start_daemon(verbose))?;
    } else {
        println!("{} Starting daemon in background...", tag("RUN"));

        // Re-exec ourselves with `daemon start --foreground` as a detached process,
        // forwarding verbosity flags so the daemon subprocess logs at the same level.
        let exe = std::env::current_exe().wrap_err("failed to determine executable path")?;
        let mut args = vec!["daemon", "start", "--foreground"];
        let v_flag = match verbose {
            1 => Some("-v"),
            2 => Some("-vv"),
            v if v >= 3 => Some("-vvv"),
            _ => None,
        };
        if let Some(flag) = v_flag {
            args.push(flag);
        }

        let child = Command::new(&exe)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .wrap_err("failed to spawn daemon process")?;

        let level_label = match verbose {
            0 => "info (default)",
            1 => "info",
            2 => "debug",
            _ => "trace",
        };
        println!(
            "{} Daemon started (pid: {}, log level: {})",
            tag("OK"),
            child.id(),
            level_label
        );
        println!(
            "  Socket: {}",
            GlobalConfig::load()?.socket_path().display()
        );

        // Write PID file for tracking
        let pid_path = gw_home().join("daemon.pid");
        fs::write(&pid_path, child.id().to_string()).wrap_err("failed to write PID file")?;
    }

    Ok(())
}

/// Stop the running daemon via IPC.
///
/// If a launchd service is installed (`KeepAlive: true`), the daemon would
/// respawn immediately after shutdown.  We send the graceful IPC shutdown
/// first, then unload the launchd plist to prevent respawn.
fn stop() -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        return Ok(());
    }

    let service_is_loaded = is_launchd_service_loaded();

    // 1. Send graceful IPC shutdown while socket is still alive.
    let client = DaemonClient::new()?;
    match client.send(Request::Shutdown)? {
        Response::Ok { message } => {
            println!("{} {}", tag("OK"), message);

            // Clean up PID file
            let pid_path = gw_home().join("daemon.pid");
            if pid_path.exists() {
                let _ = fs::remove_file(&pid_path);
            }
        }
        Response::Error { message, .. } => {
            println!("{} {}", tag("FAIL"), message);
        }
        _ => {
            println!("{} Unexpected response from daemon", tag("WARN"));
        }
    }

    // 2. Unload the launchd service *after* IPC shutdown so KeepAlive
    //    does not respawn the process.  If launchd already restarted it
    //    in the brief window, unload will stop the new instance too.
    if service_is_loaded {
        unload_launchd_service();
        println!("  (launchd service was unloaded — run `gw daemon install` to re-enable)");
    }

    Ok(())
}

/// Check whether the launchd service is currently loaded.
fn is_launchd_service_loaded() -> bool {
    let plist = match plist_path() {
        Ok(p) if p.exists() => p,
        _ => return false,
    };
    let _ = plist; // plist existence is a prerequisite

    Command::new("launchctl")
        .args(["list", "com.greater-will.daemon"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Unload the launchd plist so KeepAlive stops respawning the daemon.
fn unload_launchd_service() {
    let plist = match plist_path() {
        Ok(p) if p.exists() => p,
        _ => return,
    };

    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Query and display daemon status.
fn cmd_status() -> Result<()> {
    let home = gw_home();

    // Check for a crash loop flag first — it may be the *reason* the
    // daemon is not running, so surface it prominently before anything
    // else.  `read_crashloop_flag` returns None for both missing and
    // malformed files, so this is a safe no-op when there is no flag.
    //
    // The flag path must come from `crashloop_flag_path` — hardcoding
    // `~/.gw/crashloop.flag` would mislead operators running with a
    // non-default `$GW_HOME`, since the writer in `daemon/mod.rs` honors
    // that environment variable.
    let flag_path = crate::daemon::crashloop_flag_path(&home);
    if let Some(flag) = crate::daemon::read_crashloop_flag(&home) {
        print_crashloop_banner(&flag, &flag_path);
    }

    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let config = GlobalConfig::load()?;

    // Read PID and compute uptime from PID file modification time
    let pid_path = home.join("daemon.pid");
    let pid = fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let uptime = fs::metadata(&pid_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok());

    // Header
    println!("{} Daemon is running", tag("OK"));

    // PID + uptime
    if let Some(p) = pid {
        let uptime_str = uptime
            .map(format_duration)
            .unwrap_or_else(|| "unknown".into());
        println!("  PID:        {}", p);
        println!("  Uptime:     {}", uptime_str);
    }

    // Paths
    println!("  GW_HOME:    {}", home.display());
    println!("  Socket:     {}", config.socket_path().display());
    println!("  Log file:   {}", home.join("daemon.log").display());

    // Run counts by status
    let client = DaemonClient::new()?;
    if let Response::RunList { runs } = client.send(Request::ListRuns { all: true })? {
        use crate::daemon::protocol::RunStatus;
        let running = runs
            .iter()
            .filter(|r| r.status == RunStatus::Running)
            .count();
        let queued = runs
            .iter()
            .filter(|r| r.status == RunStatus::Queued)
            .count();
        let succeeded = runs
            .iter()
            .filter(|r| r.status == RunStatus::Succeeded)
            .count();
        let failed = runs
            .iter()
            .filter(|r| r.status == RunStatus::Failed)
            .count();
        let stopped = runs
            .iter()
            .filter(|r| r.status == RunStatus::Stopped)
            .count();

        println!();
        println!("  Runs:");
        println!("    Running:   {}", running);
        if queued > 0 {
            println!("    Queued:    {}", queued);
        }
        println!("    Succeeded: {}", succeeded);
        println!("    Failed:    {}", failed);
        if stopped > 0 {
            println!("    Stopped:   {}", stopped);
        }
        println!("    Total:     {}", runs.len());
    }

    // Last heartbeat — check pane.log mtime as a proxy
    let runs_dir = home.join("runs");
    if let Some(last_hb) = latest_heartbeat_time(&runs_dir) {
        let ago = last_hb
            .elapsed()
            .ok()
            .map(format_duration)
            .unwrap_or_else(|| "unknown".into());
        println!();
        println!("  Last heartbeat: {} ago", ago);
    }

    Ok(())
}

/// Print a prominent banner warning that the daemon is halted by the
/// crash-loop detector.
///
/// Called from `cmd_status` when the crash-loop flag exists.  The banner
/// must be visually loud because its presence means launchd respawn has
/// stopped and the operator needs to intervene before the daemon will
/// run again.
///
/// `flag_path` is the actual filesystem path of the sentinel flag (from
/// [`crate::daemon::crashloop_flag_path`]) — it is NOT hardcoded to
/// `~/.gw/crashloop.flag` because the writer honors `$GW_HOME`, and the
/// recovery instructions must reference the same path the writer used.
fn print_crashloop_banner(flag: &crate::daemon::CrashLoopFlag, flag_path: &Path) {
    // Emit a structured tracing event FIRST so launchd-captured stderr
    // (daemon.stderr.log) and any other tracing sinks record the alert
    // even when stdout is not being watched.  `println!` below is for
    // interactive `gw daemon status` operators; this `warn!` is for log
    // scrapers and on-call tooling.
    warn!(
        target: "daemon::crashloop",
        crash_count = flag.crash_count,
        flag_path = %flag_path.display(),
        last_error = %flag.last_error.as_deref().unwrap_or("unknown"),
        "crash-loop detector tripped — daemon halted, manual recovery required"
    );

    let bar = "━".repeat(60);
    let log_path = flag_path
        .parent()
        .map(|p| p.join("daemon.log"))
        .unwrap_or_else(|| PathBuf::from("daemon.log"));
    println!("{}", bar);
    println!(
        "{} CRASHLOOP DETECTED — daemon halted by launchd backoff",
        tag("FAIL")
    );
    println!("{}", bar);
    println!("  Crashes in window : {}", flag.crash_count);
    println!("  Flag timestamp    : {} (unix)", flag.timestamp);
    if let Some(err) = &flag.last_error {
        println!("  Last error        : {err}");
    }
    println!("  Details           : {}", flag.message);
    println!(
        "  Recovery          : inspect {}, fix the root cause,",
        log_path.display()
    );
    println!(
        "                      then `rm {} && gw daemon start`",
        flag_path.display()
    );
    println!("{}", bar);
    println!();
}

/// Format a duration as a human-readable string (e.g., "2h 15m 30s").
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;

    if days > 0 {
        format!("{}d {}h {}m", days, hours, mins)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, mins, s)
    } else if mins > 0 {
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", s)
    }
}

/// Find the most recent pane.log modification time across all runs.
///
/// The heartbeat monitor appends to `pane.log` on each tick, so its mtime
/// serves as a proxy for the last heartbeat capture.
fn latest_heartbeat_time(runs_dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let entries = fs::read_dir(runs_dir).ok()?;
    entries
        .flatten()
        .filter_map(|e| {
            let log_path = e.path().join("logs").join("pane.log");
            fs::metadata(&log_path).ok()?.modified().ok()
        })
        .max()
}

/// Restart the daemon.
fn restart() -> Result<()> {
    println!("Restarting daemon...");
    if DaemonClient::is_daemon_running() {
        stop()?;
        // Brief pause to allow socket cleanup
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    start(false, 0)
}

/// Install the daemon as a macOS launchd service.
fn install() -> Result<()> {
    let plist_dir = dirs::home_dir()
        .ok_or_else(|| eyre::eyre!("cannot determine home directory"))?
        .join("Library/LaunchAgents");

    fs::create_dir_all(&plist_dir).wrap_err("failed to create LaunchAgents directory")?;

    let plist_path = plist_dir.join("com.greater-will.daemon.plist");
    let exe = std::env::current_exe().wrap_err("failed to determine executable path")?;
    let log_dir = gw_home().join("logs");
    fs::create_dir_all(&log_dir).wrap_err("failed to create log directory")?;

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.greater-will.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>AbandonProcessGroup</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>"#,
        exe = exe.display(),
        stdout = log_dir.join("daemon.stdout.log").display(),
        stderr = log_dir.join("daemon.stderr.log").display(),
    );

    fs::write(&plist_path, plist_content)
        .wrap_err_with(|| format!("failed to write plist: {}", plist_path.display()))?;

    // Load the service
    let status = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path)
        .status()
        .wrap_err("failed to run launchctl load")?;

    if status.success() {
        println!("{} Installed and loaded launchd service", tag("OK"));
        println!("  Plist: {}", plist_path.display());
    } else {
        eyre::bail!("launchctl load failed (exit code: {:?})", status.code());
    }

    Ok(())
}

/// Uninstall the launchd service.
fn uninstall() -> Result<()> {
    let plist_path = plist_path()?;

    if !plist_path.exists() {
        println!("{} Service not installed (plist not found)", tag("WARN"));
        return Ok(());
    }

    // Unload the service
    let status = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status()
        .wrap_err("failed to run launchctl unload")?;

    if !status.success() {
        println!(
            "{} launchctl unload returned non-zero (service may not have been loaded)",
            tag("WARN")
        );
    }

    // Remove the plist file
    fs::remove_file(&plist_path)
        .wrap_err_with(|| format!("failed to remove plist: {}", plist_path.display()))?;

    println!("{} Uninstalled launchd service", tag("OK"));
    Ok(())
}

/// Resolve the standard plist path.
fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
    Ok(home.join("Library/LaunchAgents/com.greater-will.daemon.plist"))
}
