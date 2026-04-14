//! CLI argument definitions using clap derive macros.
//!
//! This module defines the complete CLI interface for the `gw` binary:
//! - Global flags: `-v`/`--verbose` for logging control
//! - Subcommands: `run`, `status`, `replay`, `clean`

use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

/// External Arc Controller for Rune.
///
/// Runs each Rune arc phase group in a fresh Claude Code session.
#[derive(Parser)]
#[command(name = "gw", version, about, long_about = None)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Commands,

    /// Increase logging verbosity (-v, -vv, -vvv).
    ///
    /// Each `-v` increases the log level:
    /// - `-v`: INFO level
    /// - `-vv`: DEBUG level
    /// - `-vvv`: TRACE level
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
}

/// Available subcommands for the gw CLI.
#[derive(Subcommand)]
pub enum Commands {
    /// Execute arc phases for one or more plan files.
    ///
    /// By default, runs in single-session mode: one tmux session per plan,
    /// with Rune's own stop hook driving phase iteration. Greater-will acts
    /// as a watchdog with crash recovery.
    ///
    /// Use `--multi-group` for the legacy mode that splits phases into
    /// groups (A-G), each in a separate tmux session.
    Run {
        /// Plan files or GitHub issue URLs to execute (one or more).
        ///
        /// Accepts plan file paths, glob patterns, or GitHub issue URLs.
        /// GitHub URLs (e.g. `https://github.com/owner/repo/issues/123`
        /// or shorthand `owner/repo#123`) are automatically fetched via
        /// `gh` CLI and converted into plan files under `plans/`.
        #[arg(required = true)]
        plans: Vec<String>,

        /// Perform a dry run without executing phases.
        ///
        /// Validates plans and prints what would be executed.
        #[arg(long)]
        dry_run: bool,

        /// Use mock mode with a custom script.
        ///
        /// Instead of running real Claude Code sessions, execute
        /// the provided script for each phase. Useful for testing.
        #[arg(long, value_name = "SCRIPT")]
        mock: Option<PathBuf>,

        /// Run only a specific phase group (A-G).
        ///
        /// Only valid with `--multi-group`. If not specified, all groups run.
        #[arg(long, value_name = "GROUP")]
        group: Option<String>,

        /// Custom configuration directory (CLAUDE_CONFIG_DIR).
        ///
        /// Overrides the default configuration lookup path.
        #[arg(long, value_name = "PATH")]
        config_dir: Option<PathBuf>,

        /// Resume a previously interrupted run.
        ///
        /// In single-session mode: passes `--resume` to `/rune:arc`.
        /// In multi-group mode: reads batch state from `.gw/batch-state.json`.
        #[arg(long)]
        resume: bool,

        /// Allow running with uncommitted git changes.
        ///
        /// By default, gw refuses to run when the git working tree has
        /// uncommitted changes (staged or unstaged) to prevent accidental
        /// data loss. Use this flag to bypass the check.
        #[arg(long)]
        allow_dirty: bool,

        /// Use multi-group execution mode (legacy).
        ///
        /// Splits arc phases into 7 groups (A-G), each running in a
        /// separate tmux session. Useful when Rune plugin supports
        /// per-phase-group execution.
        #[arg(long)]
        multi_group: bool,

        /// Force inline execution, bypassing the daemon even if one is running.
        ///
        /// By default, `gw run` delegates to the daemon if one is active.
        /// This flag forces the run to execute in the current process instead.
        #[arg(long)]
        foreground: bool,
    },

    /// Show status of active or recent arc runs.
    ///
    /// Displays information about currently running or recently
    /// completed arc sessions.
    Status,

    /// Resume from a checkpoint file.
    ///
    /// Restarts the arc workflow from a previously saved checkpoint,
    /// skipping phases that already completed successfully.
    Replay {
        /// Path to the checkpoint file.
        checkpoint: PathBuf,

        /// Actually resume execution (not just display status).
        #[arg(long)]
        resume: bool,

        /// Skip pre-resume artifact validation.
        #[arg(long)]
        force: bool,
    },

    /// Inject workspace context into Claude Code session (hook command).
    ///
    /// Without flags: called by Claude Code's `SessionStart` hook.
    /// Reads hook JSON from stdin, prints context to stdout.
    ///
    /// With `--install`: registers gw elden hooks in `.claude/settings.json`.
    /// With `--uninstall`: removes gw elden hooks from `.claude/settings.json`.
    /// With `--status`: shows current hook registration status.
    Elden {
        /// Install gw elden hooks into `.claude/settings.json`.
        ///
        /// Creates the file if it doesn't exist. Merges hooks if the file
        /// already has other hooks configured. Safe to run multiple times.
        #[arg(long)]
        install: bool,

        /// Remove gw elden hooks from `.claude/settings.json`.
        #[arg(long)]
        uninstall: bool,

        /// Show current hook registration status.
        #[arg(long)]
        status: bool,

        /// Hook event type override. When set, routes to the specific event
        /// handler instead of auto-detecting from stdin JSON.
        ///
        /// Used when registering separate hooks per event:
        ///   Stop → `gw elden --event stop`
        ///   SessionEnd → `gw elden --event session-end`
        ///   PermissionRequest → `gw elden --event permission`
        #[arg(long)]
        event: Option<String>,
    },

    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// List all runs across repositories.
    Ps {
        /// Show all runs including completed ones.
        #[arg(short, long)]
        all: bool,

        /// Output as JSON for scripting.
        #[arg(long)]
        json: bool,

        /// Show only running runs.
        #[arg(long)]
        running: bool,

        /// Show only failed runs.
        #[arg(long)]
        failed: bool,
    },

    /// View logs for a specific run.
    ///
    /// By default shows structured event logs (phase transitions, status
    /// changes, errors). Use --pane to see the raw tmux pane capture instead.
    Logs {
        /// The run ID to view logs for.
        run_id: String,

        /// Follow log output in real time.
        #[arg(short, long)]
        follow: bool,

        /// Show only the last N lines.
        #[arg(long, value_name = "N")]
        tail: Option<usize>,

        /// Show raw tmux pane capture instead of structured events.
        #[arg(long)]
        pane: bool,
    },

    /// Stop a running arc.
    ///
    /// By default, prompts for confirmation and kills the tmux session.
    /// Use `--force` to skip confirmation.
    /// Use `--detach` to stop tracking but keep the tmux session alive.
    /// Use `--all` to stop every running and queued run.
    Stop {
        /// The run ID to stop. Omit when using `--all`.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        run_id: Option<String>,
        /// Skip confirmation prompt.
        #[arg(long, short)]
        force: bool,
        /// Stop GW tracking but keep the tmux session alive.
        /// The session can be re-adopted on next daemon restart.
        #[arg(long, short)]
        detach: bool,
        /// Stop every running and queued run.
        #[arg(long, short = 'A')]
        all: bool,
    },

    /// Generate shell completions.
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Clean up temporary files and tmux sessions.
    ///
    /// Removes:
    /// - Orphaned tmux sessions from interrupted runs
    /// - Temporary checkpoint files
    /// - Log files from previous runs
    Clean,

    /// Manage scheduled plan executions.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },

    /// Manage the pending run queue.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },

    /// Browse history of past arc runs.
    ///
    /// Reads run metadata and logs directly from disk (~/.gw/runs/).
    /// Works without the daemon running.
    History {
        /// Maximum number of runs to display.
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,

        /// Filter by status: succeeded, failed, stopped.
        #[arg(short, long)]
        status: Option<String>,

        /// Filter by repository path (substring match).
        #[arg(short, long)]
        repo: Option<String>,

        /// Show detailed info for a specific run ID (prefix match).
        #[arg(long)]
        detail: Option<String>,

        /// Show event logs for a specific run ID (prefix match).
        ///
        /// Reads from ~/.gw/runs/<id>/logs/events.jsonl directly.
        #[arg(long)]
        logs: Option<String>,

        /// When used with --logs, show raw pane capture instead of events.
        #[arg(long)]
        pane: bool,

        /// When used with --logs, show only the last N lines.
        #[arg(long, value_name = "N")]
        tail: Option<usize>,

        /// Output as JSON for scripting.
        #[arg(long)]
        json: bool,
    },
}

/// Subcommands for `gw daemon`.
#[derive(Subcommand, Debug, Clone)]
pub enum DaemonAction {
    /// Start the daemon process.
    Start {
        /// Run in the foreground instead of daemonizing.
        #[arg(long)]
        foreground: bool,

        /// Increase daemon logging verbosity (-v, -vv, -vvv).
        ///
        /// Overrides the daemon's default log level (INFO):
        /// - `-v`: INFO level
        /// - `-vv`: DEBUG level
        /// - `-vvv`: TRACE level
        #[arg(short, long, action = ArgAction::Count)]
        verbose: u8,
    },
    /// Stop the running daemon.
    Stop,
    /// Show daemon status.
    Status,
    /// Install as a system service (launchd/systemd).
    Install,
    /// Uninstall the system service.
    Uninstall,
    /// Restart the daemon.
    Restart,
}

/// Subcommands for `gw schedule`.
#[derive(Subcommand, Debug, Clone)]
pub enum ScheduleAction {
    /// Add a new scheduled plan execution.
    Add {
        /// Plan file path to schedule.
        plan: String,

        /// Cron expression (e.g. "*/5 * * * *").
        #[arg(long, conflicts_with_all = ["at", "after"])]
        cron: Option<String>,

        /// Fire once at a specific time (ISO 8601 or "HH:MM").
        #[arg(long, conflicts_with_all = ["cron", "after"])]
        at: Option<String>,

        /// Fire once after a delay (e.g. "30m", "2h", "1d").
        #[arg(long, conflicts_with_all = ["cron", "at"])]
        after: Option<String>,

        /// Optional human-readable label.
        #[arg(long)]
        label: Option<String>,

        /// Custom configuration directory (CLAUDE_CONFIG_DIR).
        #[arg(long)]
        config_dir: Option<PathBuf>,
    },
    /// List all schedules.
    List {
        /// Output as JSON for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Remove a schedule by ID.
    Remove {
        /// Schedule ID (prefix match).
        id: String,
    },
    /// Pause a schedule.
    Pause {
        /// Schedule ID (prefix match).
        id: String,
    },
    /// Resume a paused schedule.
    Resume {
        /// Schedule ID (prefix match).
        id: String,
    },
}

/// Subcommands for `gw queue`.
#[derive(Subcommand, Debug, Clone)]
pub enum QueueAction {
    /// List queued plans.
    List {
        /// Filter by repository path.
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Output as JSON for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Remove a queued plan by ID.
    Remove {
        /// Run ID (prefix match).
        id: String,
    },
    /// Clear all queued plans.
    Clear {
        /// Clear queues for all repos (default: current repo only).
        #[arg(long)]
        all: bool,
    },
}