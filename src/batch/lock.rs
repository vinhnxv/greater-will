//! Instance lock to prevent concurrent gw executions.
//!
//! Uses an advisory file lock via `fs2` at `.gw/gw.lock` — the OS kernel
//! guarantees mutual exclusion, eliminating TOCTOU races that a
//! check-then-write PID scheme would have.

use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use fs2::FileExt;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

/// Advisory file lock guard for single-instance enforcement.
///
/// The lock is held for the lifetime of this struct. Dropping it
/// releases the advisory lock and removes the lock file.
#[derive(Debug)]
pub struct InstanceLock {
    lock_path: PathBuf,
    #[allow(dead_code)]
    lock_file: File,
}

impl InstanceLock {
    /// Acquire the instance lock.
    ///
    /// Uses `fs2::try_lock_exclusive()` for atomic, race-free locking.
    /// The PID is written to the file for debugging purposes only.
    pub fn acquire() -> Result<Self> {
        let lock_path = PathBuf::from(".gw/gw.lock");

        // Ensure .gw directory exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .wrap_err("Failed to create .gw directory")?;
        }

        // Open/create the lock file and attempt an exclusive advisory lock
        let mut lock_file = File::create(&lock_path)
            .wrap_err("Failed to create lock file")?;

        lock_file.try_lock_exclusive().map_err(|_| {
            // Read existing PID for a helpful error message
            let pid_info = fs::read_to_string(&lock_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|pid| format!(" (PID {pid})"))
                .unwrap_or_default();
            eyre::eyre!(
                "Another gw instance is running{pid_info}. \
                 If this is stale, remove {}",
                lock_path.display()
            )
        })?;

        // Write our PID for debugging (lock is already held)
        let our_pid = std::process::id();
        write!(lock_file, "{our_pid}")
            .wrap_err("Failed to write PID to lock file")?;

        tracing::info!(pid = our_pid, "Acquired instance lock");

        Ok(Self {
            lock_path,
            lock_file,
        })
    }

    /// Release the instance lock.
    pub fn release(&self) -> Result<()> {
        if self.lock_path.exists() {
            if let Err(e) = fs::remove_file(&self.lock_path) {
                eprintln!("Warning: failed to remove lock file: {e}");
            }
            tracing::info!("Released instance lock");
        }
        Ok(())
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Best-effort cleanup on drop — advisory lock auto-releases
        // when File is dropped, but remove the file for tidiness.
        if self.lock_path.exists() {
            if let Err(e) = fs::remove_file(&self.lock_path) {
                eprintln!("Warning: failed to remove lock file on drop: {e}");
            }
        }
    }
}
