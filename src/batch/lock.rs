//! Instance lock to prevent concurrent gw executions.
//!
//! Uses `fs2` file locking (`flock`) for atomic lock acquisition, with a
//! PID-based stale detection fallback. The lock file lives at `.gw/gw.lock`.

use crate::cleanup::process::is_pid_alive;
use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Instance lock backed by `flock` advisory locking.
#[derive(Debug)]
pub struct InstanceLock {
    lock_path: PathBuf,
    /// Hold the file handle to keep the flock active.
    _lock_file: Option<File>,
    /// Tracks whether release() was explicitly called, to prevent double-cleanup in Drop.
    released: bool,
}

impl InstanceLock {
    /// Acquire the instance lock.
    ///
    /// Uses `flock` (via `fs2`) for atomic acquisition. If the lock file
    /// already exists and is held by a live process, returns an error.
    /// Stale locks (dead PIDs) are automatically reclaimed.
    pub fn acquire() -> Result<Self> {
        let lock_path = PathBuf::from(".gw/gw.lock");

        // Ensure .gw directory exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .wrap_err("Failed to create .gw directory")?;
        }

        // Bounded retry loop for stale lock reclaim (SEC-001: prevents unbounded recursion)
        const MAX_RETRIES: u32 = 3;
        for attempt in 0..=MAX_RETRIES {
            // Open or create the lock file
            let lock_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .wrap_err("Failed to open lock file")?;

            // SEC-004: Restrict lock file permissions to owner-only (0o600)
            #[cfg(unix)]
            {
                let perms = std::fs::Permissions::from_mode(0o600);
                fs::set_permissions(&lock_path, perms)
                    .wrap_err("Failed to set lock file permissions")?;
            }

            // Try non-blocking flock
            match lock_file.try_lock_exclusive() {
                Ok(()) => {
                    // Got the lock — write our PID
                    let mut f = lock_file;
                    f.set_len(0).wrap_err("Failed to truncate lock file")?;
                    write!(f, "{}", std::process::id())
                        .wrap_err("Failed to write PID to lock file")?;
                    f.sync_all().wrap_err("Failed to sync lock file")?;

                    tracing::info!(pid = std::process::id(), "Acquired instance lock");
                    return Ok(Self {
                        lock_path,
                        _lock_file: Some(f),
                        released: false,
                    });
                }
                Err(_) => {
                    // Lock is held — check if the holder is still alive
                    let content = fs::read_to_string(&lock_path).unwrap_or_default();
                    if let Ok(pid) = content.trim().parse::<u32>() {
                        if is_pid_alive(pid) {
                            eyre::bail!(
                                "Another gw instance is running (PID {}). \
                                 If this is stale, remove {}",
                                pid,
                                lock_path.display()
                            );
                        }
                        // Process is dead — stale lock. Force reclaim.
                        tracing::info!(
                            stale_pid = pid,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES,
                            "Reclaiming stale lock from dead process"
                        );

                        if attempt == MAX_RETRIES {
                            eyre::bail!(
                                "Failed to reclaim stale lock after {} attempts. Remove {} manually.",
                                MAX_RETRIES,
                                lock_path.display()
                            );
                        }

                        // SEC-002: TOCTOU note — there is an inherent race window between
                        // remove_file and the next loop iteration's open+flock. The sleep
                        // reduces collision probability with concurrent reclaimers but does
                        // not eliminate it. The flock itself is the authoritative guard.
                        fs::remove_file(&lock_path)
                            .wrap_err("Failed to remove stale lock file")?;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        continue;
                    }
                    eyre::bail!(
                        "Lock file exists but cannot determine holder. Remove {} manually.",
                        lock_path.display()
                    );
                }
            }
        }

        // Unreachable due to loop bounds, but satisfies the compiler
        eyre::bail!("Failed to acquire lock after retries")
    }

    /// Release the instance lock.
    pub fn release(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        if let Some(ref f) = self._lock_file {
            f.unlock().wrap_err("Failed to unlock lock file")?;
        }
        if self.lock_path.exists() {
            fs::remove_file(&self.lock_path)
                .wrap_err("Failed to remove lock file")?;
            tracing::info!("Released instance lock");
        }
        self.released = true;
        Ok(())
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Skip cleanup if release() was already called (BACK-002: prevents double-cleanup)
        if self.released {
            return;
        }
        // Best-effort cleanup on drop
        if let Some(ref f) = self._lock_file {
            let _ = f.unlock();
        }
        if self.lock_path.exists() {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_is_pid_alive_current_process() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn test_is_pid_alive_invalid() {
        assert!(!is_pid_alive(u32::MAX));
    }
}
