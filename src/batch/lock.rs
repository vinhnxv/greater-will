//! Instance lock to prevent concurrent gw executions.
//!
//! Uses a PID-based lock file at `.gw/gw.lock` with stale detection —
//! if the PID in the lock file is no longer running, the lock is considered
//! stale and can be reclaimed.

use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::fs;
use std::path::PathBuf;

/// Lock file content: the PID of the holding process.
#[derive(Debug)]
pub struct InstanceLock {
    lock_path: PathBuf,
}

impl InstanceLock {
    /// Acquire the instance lock.
    ///
    /// If a lock file exists, checks if the holding PID is still alive.
    /// Stale locks (dead PIDs) are automatically reclaimed.
    pub fn acquire() -> Result<Self> {
        let lock_path = PathBuf::from(".gw/gw.lock");

        // Ensure .gw directory exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .wrap_err("Failed to create .gw directory")?;
        }

        // Check for existing lock
        if lock_path.exists() {
            let content = fs::read_to_string(&lock_path)
                .wrap_err("Failed to read lock file")?;

            if let Ok(pid) = content.trim().parse::<u32>() {
                if is_process_alive(pid) {
                    eyre::bail!(
                        "Another gw instance is running (PID {}). \
                         If this is stale, remove {}",
                        pid,
                        lock_path.display()
                    );
                } else {
                    tracing::info!(
                        stale_pid = pid,
                        "Reclaiming stale lock from dead process"
                    );
                }
            }
            // Lock file exists but is invalid or stale — reclaim it
        }

        // Write our PID
        let our_pid = std::process::id();
        fs::write(&lock_path, our_pid.to_string())
            .wrap_err("Failed to write lock file")?;

        tracing::info!(pid = our_pid, "Acquired instance lock");

        Ok(Self { lock_path })
    }

    /// Release the instance lock.
    pub fn release(&self) -> Result<()> {
        if self.lock_path.exists() {
            fs::remove_file(&self.lock_path)
                .wrap_err("Failed to remove lock file")?;
            tracing::info!("Released instance lock");
        }
        Ok(())
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Best-effort cleanup on drop
        if self.lock_path.exists() {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

/// Check if a process with the given PID is still alive.
fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks if process exists without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
