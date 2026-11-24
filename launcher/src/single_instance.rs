//! Single-instance guard and cross-process state locks: at most one launcher process holds the
//! data directory, and shared state writes serialize through per-file lock files.
//!
//! The launcher-level lock (`launcher.lock`) is held by the OS: the first process owns it until it
//! exits or drops the guard, so a lock file left by a crash needs no PID check or cleanup.
//!
//! Shared-state writers use [`StateLock`], a cross-process mutual exclusion on a per-file lock
//! name, for read-modify-write cycles that must not interleave.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

/// The launcher-level lock file's name inside the launcher's data directory.
const LAUNCHER_LOCK: &str = "launcher.lock";

/// A cross-process mutual exclusion lock on a file inside a directory.
///
/// The lock is a file named `name` inside `dir`, held with an OS advisory lock. Creating the file
/// is atomic and the kernel releases the lock when its owner exits, so a leftover file cannot be
/// stolen or recycle a reused process id.
pub struct StateLock {
    _file: File,
}

impl StateLock {
    /// Try to acquire the cross-process lock `name` inside `dir`.
    ///
    /// Fails with a clear message when another live process already holds the lock.
    pub fn acquire(dir: &Path, name: &str) -> Result<Self, String> {
        let lock_path = dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("{lock_path:?}: {e}"))?;
        if !try_lock(&file) {
            return Err(format!(
                "Another process holds the lock {}.",
                lock_path.display()
            ));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn try_lock(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    // The kernel drops flock on exit, including process::exit or a crash.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(windows)]
fn try_lock(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    let mut overlapped = std::mem::zeroed();
    unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        ) != 0
    }
}

/// The application-level single-instance guard: at most one launcher process owns the data
/// directory. Backed by a [`StateLock`] on `launcher.lock`.
pub struct SingleInstance {
    // Kept alive for the OS lock, which is released exactly once when the guard is dropped.
    #[allow(dead_code)]
    inner: StateLock,
}

impl SingleInstance {
    /// Try to acquire the single-instance lock for the launcher's data directory.
    ///
    /// Fails with a clear message when another live launcher instance already holds the lock.
    pub fn acquire(data_dir: &Path) -> Result<Self, String> {
        // A fresh install (and a clean CI runner) may not have created the data directory yet.
        // Create it before opening launcher.lock; otherwise the first launch fails with NotFound.
        fs::create_dir_all(data_dir).map_err(|e| {
            format!(
                "Could not create the launcher's data directory {}: {e}",
                data_dir.display()
            )
        })?;
        Ok(Self {
            inner: StateLock::acquire(data_dir, LAUNCHER_LOCK)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "horizon-single-instance-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn single_instance_creates_a_missing_data_directory() {
        let dir = std::env::temp_dir().join(format!(
            "horizon-single-instance-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        let guard = SingleInstance::acquire(&dir).unwrap();
        assert!(dir.join(LAUNCHER_LOCK).exists());
        drop(guard);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn single_instance_acquire_succeeds_once() {
        let dir = scratch("once");
        let first = SingleInstance::acquire(&dir).unwrap();
        assert!(SingleInstance::acquire(&dir).is_err());
        drop(first);
        assert!(SingleInstance::acquire(&dir).is_ok());
    }

    #[test]
    fn single_instance_reuses_stale_lock_from_dead_process() {
        let dir = scratch("stale");
        let lock = dir.join(LAUNCHER_LOCK);
        fs::write(&lock, "4294967295\n").unwrap(); // an invalid PID on supported platforms
        assert!(SingleInstance::acquire(&dir).is_ok());
    }

    #[test]
    fn stale_lock_cannot_be_stolen_while_another_process_holds_it() {
        let dir = scratch("race");
        fs::write(dir.join(LAUNCHER_LOCK), "4294967295\n").unwrap();
        let ready = dir.join("ready");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "single_instance::tests::hold_lock_for_parent",
                "--nocapture",
            ])
            .env("HORIZON_LOCK_CHILD_DIR", &dir)
            .env("HORIZON_LOCK_READY", &ready)
            .spawn()
            .unwrap();
        let mut acquired = false;
        for _ in 0..100 {
            if ready.exists() {
                acquired = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(acquired, "child did not acquire the lock");
        assert!(
            SingleInstance::acquire(&dir).is_err(),
            "a stale pid let a second process steal a live lock"
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(SingleInstance::acquire(&dir).is_ok());
    }

    #[test]
    fn hold_lock_for_parent() {
        let (Ok(dir), Ok(ready)) = (
            std::env::var("HORIZON_LOCK_CHILD_DIR"),
            std::env::var("HORIZON_LOCK_READY"),
        ) else {
            return;
        };
        let guard = SingleInstance::acquire(Path::new(&dir)).unwrap();
        fs::write(ready, b"ok").unwrap();
        thread::sleep(Duration::from_secs(5));
        drop(guard);
    }

    #[test]
    fn state_lock_excludes_a_second_holder_and_releases() {
        let dir = scratch("statelock");
        let first = StateLock::acquire(&dir, "shared.lock").unwrap();
        assert!(StateLock::acquire(&dir, "shared.lock").is_err());
        drop(first);
        assert!(StateLock::acquire(&dir, "shared.lock").is_ok());
    }
}