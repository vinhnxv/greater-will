//! Logging module for greater-will.
//!
//! Provides structured logging via tracing-subscriber with stdout output.

pub mod jsonl;
mod structured;

pub use structured::init;