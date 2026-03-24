//! Logging module for greater-will.
//!
//! Provides structured logging via tracing-subscriber with stdout output.

mod structured;

pub use structured::init;