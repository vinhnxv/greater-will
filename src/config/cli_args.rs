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
        /// Plan files to execute (required, one or more).
        ///
        /// Each plan file should be a markdown file with YAML frontmatter.
        /// Plans are executed in the order specified.
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

    /// Clean up temporary files and tmux sessions.
    ///
    /// Removes:
    /// - Orphaned tmux sessions from interrupted runs
    /// - Temporary checkpoint files
    /// - Log files from previous runs
    Clean,
}