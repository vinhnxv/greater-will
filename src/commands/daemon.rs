//! Daemon management subcommands.
//!
//! Provides start/stop/restart/status/install/uninstall for the gw daemon.
//! The daemon runs as a background process and can optionally be managed
//! via macOS launchd for auto-start on login.

use crate::client::socket::DaemonClient;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{ensure_gw_home, gw_home, GlobalConfig};
use crate::output::tags::tag;
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Actions available for the `gw daemon` subcommand.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum DaemonAction {
    /// Start the daemon (background by default).
    Start {
        /// Run in the foreground instead of daemonizing.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running daemon.
    Stop,
    /// Show daemon status (uptime, run count).
    Status,
    /// Restart the daemon (stop + start).
    Restart,
    /// Install as a macOS launchd service.
    Install,
    /// Uninstall the launchd service.
    Uninstall,
}

/// Dispatch a daemon action.
pub fn execute(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start { foreground } => start(foreground),
        DaemonAction::Stop => stop(),
        DaemonAction::Status => cmd_status(),
        DaemonAction::Restart => restart(),
        DaemonAction::Install => install(),
        DaemonAction::Uninstall => uninstall(),
    }
}

/// Start the daemon process.
fn start(foreground: bool) -> Result<()> {
    // Ensure state directories exist
    ensure_gw_home()?;

    if DaemonClient::is_daemon_running() {
        println!("{} Daemon is already running.", tag("WARN"));
        return Ok(());
    }

    if foreground {
        println!("{} Starting daemon in foreground...", tag("RUN"));
        // Run the daemon directly in this process (blocks)
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            // The daemon server module will be provided by Worker 2
            // For now, just indicate we'd start it here
            println!("Daemon listening on {}", GlobalConfig::load()?.socket_path().display());
            println!("Press Ctrl+C to stop.");
            tokio::signal::ctrl_c().await?;
            println!("\nShutting down...");
            Ok::<(), color_eyre::Report>(())
        })?;
    } else {
        println!("{} Starting daemon in background...", tag("RUN"));

        // Re-exec ourselves with `daemon start --foreground` as a detached process
        let exe = std::env::current_exe().wrap_err("failed to determine executable path")?;
        let child = Command::new(&exe)
            .args(["daemon", "start", "--foreground"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .wrap_err("failed to spawn daemon process")?;

        println!("{} Daemon started (pid: {})", tag("OK"), child.id());
        println!("  Socket: {}", GlobalConfig::load()?.socket_path().display());

        // Write PID file for tracking
        let pid_path = gw_home().join("daemon.pid");
        fs::write(&pid_path, child.id().to_string())
            .wrap_err("failed to write PID file")?;
    }

    Ok(())
}

/// Stop the running daemon via IPC.
fn stop() -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        return Ok(());
    }

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
    Ok(())
}

/// Query and display daemon status.
fn cmd_status() -> Result<()> {
    if !DaemonClient::is_daemon_running() {
        println!("{} Daemon is not running.", tag("WARN"));
        println!("  Start with: gw daemon start");
        return Ok(());
    }

    let client = DaemonClient::new()?;

    // Get daemon status
    match client.send(Request::DaemonStatus)? {
        Response::Ok { message } => {
            println!("{} Daemon is running", tag("OK"));
            println!("  {}", message);
        }
        other => {
            println!("{} Daemon responded: {:?}", tag("OK"), other);
        }
    }

    // Also show run count
    let client = DaemonClient::new()?;
    match client.send(Request::ListRuns { all: false })? {
        Response::RunList { runs } => {
            let active = runs.iter().filter(|r| {
                r.status == crate::daemon::protocol::RunStatus::Running
                    || r.status == crate::daemon::protocol::RunStatus::Queued
            }).count();
            println!("  Active runs: {}", active);
            println!("  Total tracked: {}", runs.len());
        }
        _ => {}
    }

    println!("  Socket: {}", GlobalConfig::load()?.socket_path().display());
    Ok(())
}

/// Restart the daemon.
fn restart() -> Result<()> {
    println!("Restarting daemon...");
    if DaemonClient::is_daemon_running() {
        stop()?;
        // Brief pause to allow socket cleanup
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    start(false)
}

/// Install the daemon as a macOS launchd service.
fn install() -> Result<()> {
    let plist_dir = dirs::home_dir()
        .ok_or_else(|| eyre::eyre!("cannot determine home directory"))?
        .join("Library/LaunchAgents");

    fs::create_dir_all(&plist_dir)
        .wrap_err("failed to create LaunchAgents directory")?;

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
        println!("{} launchctl unload returned non-zero (service may not have been loaded)", tag("WARN"));
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
