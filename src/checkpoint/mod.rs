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

pub use phase_order::{PHASE_ORDER, PHASE_COUNT, PHASE_GROUPS, phase_index, phase_at, group_for_phase};
pub use reader::{
    read_checkpoint, try_read_checkpoint, validate_artifact_hash,
    compute_file_hash, validate_all_artifacts, mark_phases_completed_before,
    checkpoint_summary, next_pending_phase,
};
pub use schema::{
    Checkpoint, PhaseStatus, SchemaCompat,
    SCHEMA_VERSION_MIN, SCHEMA_VERSION_MAX,
};
pub use writer::{
    write_checkpoint, create_fresh_checkpoint, backup_checkpoint, mark_phase_completed,
};