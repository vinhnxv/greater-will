//! Configuration module for greater-will.
//!
//! This module provides:
//! - `cli_args`: CLI argument definitions using clap derive macros
//! - `phase_config`: TOML-based phase group configuration with serde deserialization

pub mod cli_args;
pub mod phase_config;

