//! Configuration module for greater-will.
//!
//! This module provides:
//! - `cli_args`: CLI argument definitions using clap derive macros
//! - `phase_config`: TOML-based phase group configuration with serde deserialization

pub mod cli_args;
pub mod phase_config;

pub use cli_args::{Cli, Commands};
pub use phase_config::{PhaseConfig, PhaseGroup, Settings, KNOWN_PHASES};