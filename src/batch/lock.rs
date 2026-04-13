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
    #[allow(dead_code)] // Held for struct lifetime; dropping releases the advisory lock
    lock_file: File,
}

impl InstanceLock {
    /// Acquire the instance lock.
    ///
    /// Uses `fs2::try_lock_exclusive()` for atomic, race-free locking.
    /// The PID is written to the file for debugging purposes only.
    pub fn acquire() -> Result<Self> {
        let lock_path = std::env::current_dir()
            .wrap_err("Failed to determine current directory for lock path")?
            .join(".gw/gw.lock");

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
                .map(|pid| {
                    let alive = crate::cleanup::process::is_pid_alive(pid);
                    if alive {
                        format!(" (PID {pid}, running)")
                    } else {
                        format!(" (PID {pid}, not running — lock may be stale)")
                    }
                })
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

    /// Acquire the instance lock, blocking until available.
    ///
    /// Unlike [`acquire`], this uses `fs2::lock_exclusive()` which waits
    /// in the kernel until the lock is free — providing natural FIFO
    /// queueing across concurrent `gw run` invocations on the same repo.
    ///
    /// Ctrl+C is still responsive because `flock` is interruptible by
    /// signals on POSIX. Callers should print a "waiting/queued" message
    /// before calling this so users know why the CLI is paused.
    pub fn acquire_blocking() -> Result<Self> {
        let base = std::env::current_dir()
            .wrap_err("Failed to determine current directory for lock path")?;
        Self::acquire_blocking_at(&base)
    }

    /// Blocking-acquire variant that takes an explicit base directory.
    ///
    /// The lock path is `{base}/.gw/gw.lock`. Exists primarily so tests can
    /// exercise the lock without mutating process-wide `current_dir`, but is
    /// also safe for production callers that want to lock a specific repo
    /// directory rather than whatever cwd happens to be at the moment.
    pub fn acquire_blocking_at(base: &std::path::Path) -> Result<Self> {
        let lock_path = base.join(".gw/gw.lock");

        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .wrap_err("Failed to create .gw directory")?;
        }

        let mut lock_file = File::create(&lock_path)
            .wrap_err("Failed to create lock file")?;

        // Blocking acquire — kernel queues us behind any existing holders.
        // If no process holds the lock (empty dir, or a previous gw crashed
        // without running Drop), this succeeds instantly — the kernel
        // auto-released the flock when the previous holder's fd closed.
        lock_file.lock_exclusive()
            .wrap_err("Failed to acquire exclusive lock (blocking)")?;

        let our_pid = std::process::id();
        write!(lock_file, "{our_pid}")
            .wrap_err("Failed to write PID to lock file")?;

        tracing::info!(pid = our_pid, path = %lock_path.display(), "Acquired instance lock (blocking)");

        Ok(Self {
            lock_path,
            lock_file,
        })
    }

    /// Release the instance lock.
    pub fn release(&self) -> Result<()> {
        match fs::remove_file(&self.lock_path) {
            Ok(()) => tracing::info!("Released instance lock"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "Failed to remove lock file"),
        }
        Ok(())
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Best-effort cleanup on drop — advisory lock auto-releases
        // when File is dropped, but remove the file for tidiness.
        match fs::remove_file(&self.lock_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "Failed to remove lock file on drop"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Guard: when no other gw holds the lock, a blocking acquire must
    /// return instantly. This is the "daemon-down + foreground" guarantee —
    /// an empty repo with no active gw must never hang or error.
    #[test]
    fn blocking_acquire_on_empty_dir_is_instant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let start = Instant::now();
        let _lock = InstanceLock::acquire_blocking_at(tmp.path())
            .expect("blocking acquire on empty dir must succeed");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "blocking acquire on empty dir should be near-instant, took {:?}",
            start.elapsed()
        );
    }

    /// Guard: after a lock is dropped, a fresh blocking acquire must succeed
    /// instantly — covers the case where a previous gw cleanly exited.
    #[test]
    fn blocking_acquire_after_drop_is_instant() {
        let tmp = tempfile::TempDir::new().unwrap();
        {
            let _lock = InstanceLock::acquire_blocking_at(tmp.path()).unwrap();
        } // drop releases
        let start = Instant::now();
        let _lock2 = InstanceLock::acquire_blocking_at(tmp.path())
            .expect("re-acquire after drop must succeed");
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    /// Guard: a STALE lock file (exists on disk but no process holds the
    /// flock, e.g., previous gw was SIGKILL'd) must not block the next
    /// acquire. The kernel releases flocks when the holding fd closes,
    /// even on crash — stale file content is cosmetic only.
    #[test]
    fn blocking_acquire_over_stale_lock_file_is_instant() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Simulate stale lock file: write a fake PID, but don't flock it.
        let gw_dir = tmp.path().join(".gw");
        std::fs::create_dir_all(&gw_dir).unwrap();
        std::fs::write(gw_dir.join("gw.lock"), "99999").unwrap();

        let start = Instant::now();
        let _lock = InstanceLock::acquire_blocking_at(tmp.path())
            .expect("acquire over stale lock file must succeed");
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    /// Guard: concurrent acquire attempts serialize correctly. Second
    /// acquire blocks until the first drops, then proceeds. This is the
    /// "different plan → queue" behavior at the primitive level.
    #[test]
    fn concurrent_acquires_serialize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();

        let first = InstanceLock::acquire_blocking_at(&base).unwrap();

        let base_clone = base.clone();
        let waiter = std::thread::spawn(move || {
            let t0 = Instant::now();
            let _second = InstanceLock::acquire_blocking_at(&base_clone).unwrap();
            t0.elapsed()
        });

        // Let the waiter actually block on the lock for a measurable window
        std::thread::sleep(Duration::from_millis(300));
        drop(first);

        let waited = waiter.join().expect("waiter thread panicked");
        assert!(
            waited >= Duration::from_millis(200),
            "second acquire should have blocked at least ~300ms, only waited {:?}",
            waited
        );
    }
}
