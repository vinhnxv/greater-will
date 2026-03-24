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
    /// This is the primary command that runs the full arc workflow
    /// for each specified plan file in sequence.
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
        /// If not specified, all groups run in sequence.
        #[arg(long, value_name = "GROUP")]
        group: Option<String>,

        /// Custom configuration directory.
        ///
        /// Overrides the default configuration lookup path.
        #[arg(long, value_name = "PATH")]
        config_dir: Option<PathBuf>,
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
    },

    /// Clean up temporary files and tmux sessions.
    ///
    /// Removes:
    /// - Orphaned tmux sessions from interrupted runs
    /// - Temporary checkpoint files
    /// - Log files from previous runs
    Clean,
}