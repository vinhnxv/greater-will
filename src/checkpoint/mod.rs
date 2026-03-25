//! Checkpoint module for Rune arc checkpoints.
//!
//! This module provides schema definitions, readers, and writers for
//! the checkpoint.json files that track arc execution progress.
//!
//! # Components
//!
//! - [`schema`] - Serde structs for checkpoint format (forward-compatible)
//! - [`reader`] - Read and validate checkpoint files
//! - [`writer`] - Atomic write operations for checkpoints
//! - [`phase_order`] - Canonical order of 41 arc phases

pub mod phase_order;
pub mod reader;
pub mod schema;
pub mod writer;

