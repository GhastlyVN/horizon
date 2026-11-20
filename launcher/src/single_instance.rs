//! Single-instance guard and cross-process state locks: at most one launcher process holds the
//! data directory, and shared state writes serialize through per-file lock files.
//!
//! The launcher-level lock (`launcher.lock`) makes a second launcher refuse to start while the
//! first is alive. A kernel-managed advisory file lock is authoritative; the PID in the file is
//! diagnostic only. The kernel releases the lock when a process exits or crashes, so a persistent
//! lock file can be safely reused without a read/check/unlink race.
//!
//! Shared-state writers use [`StateLock`], a cross-process mutual exclusion on a per-file lock
//! name, for read-modify-write cycles that must not interleave.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// The launcher-level lock file's name inside the launcher's data directory.
const LAUNCHER_LOCK: &str = "launcher.lock";

/// Where the Windows byte-range lock sits. Windows file locks are mandatory: another handle
/// cannot read the locked bytes, so the lock stays clear of the diagnostic PID text at the
/// start of the file and a second process can still read it for the error message.
#[cfg(windows)]
const LOCK_REGION_OFFSET: u64 = 1 << 20;

/// A cross-process mutual exclusion lock on a file inside a directory.
///
/// The file is persistent; the OS-level exclusive lock, not its diagnostic PID text, determines
/// ownership. Kernel locks are released automatically when a process exits or crashes.
pub struct StateLock {
    file: Option<File>,
}

impl StateLock {
    /// Try to acquire the cross-process lock `name` inside `dir`.
    ///
    /// The file is opened or created, then locked by the kernel. Its PID text is written only
    /// after the lock is held and is used for diagnostics, never to decide whether it is stale.
    pub fn acquire(dir: &Path, name: &str) -> Result<Self, String> {
        let lock_path = dir.join(name);
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&lock_path)
            .map_err(|error| format!("Could not open lock {}: {error}", lock_path.display()))?;

        if let Err(error) = lock_file(&file) {
            if lock_is_held(&error) {
                let owner = fs::read_to_string(&lock_path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                return Err(match owner {
                    Some(owner) => format!(
                        "Another process (PID {owner}) holds the lock {}.",
                        lock_path.display()
                    ),
                    None => format!(
                        "Another process holds the lock {}; its diagnostic PID is unavailable.",
                        lock_path.display()
                    ),
                });
            }
            return Err(format!(
                "Could not acquire lock {}: {error}",
                lock_path.display()
            ));
        }

        let initialized = file
            .set_len(0)
            .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|_| writeln!(file, "{}", std::process::id()))
            .and_then(|_| file.sync_all());
        if let Err(error) = initialized {
            unlock_file(&file);
            return Err(format!(
                "Could not initialize lock {}: {error}",
                lock_path.display()
            ));
        }

        Ok(Self { file: Some(file) })
    }

    /// Release the kernel lock while leaving the reusable lock file in place.
    pub(crate) fn release(&mut self) {
        if let Some(file) = self.file.take() {
            unlock_file(&file);
            drop(file);
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Try to lock one byte without waiting. The OS releases this lock on process exit.
#[cfg(unix)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Release the Unix advisory lock. Closing the file also releases it.
#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

/// Try to lock the sentinel byte without waiting on Windows.
#[cfg(windows)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // windows-sys exposes the byte range as a union member: set it through the named structure.
    unsafe {
        overlapped.Anonymous.Anonymous.Offset = LOCK_REGION_OFFSET as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (LOCK_REGION_OFFSET >> 32) as u32;
    }
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Release the sentinel byte. Closing the file also releases the Windows lock.
#[cfg(windows)]
fn unlock_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // windows-sys exposes the byte range as a union member: set it through the named structure.
    unsafe {
        overlapped.Anonymous.Anonymous.Offset = LOCK_REGION_OFFSET as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (LOCK_REGION_OFFSET >> 32) as u32;
    }
    let _ = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
}

#[cfg(unix)]
fn lock_is_held(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
}

#[cfg(windows)]
fn lock_is_held(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

/// The application-level single-instance guard: at most one launcher process owns the data
/// directory. Backed by a [`StateLock`] on `launcher.lock`.
pub struct SingleInstance {
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

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // Release the single-instance lock.
        self.inner.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        fs::write(&lock, "999999999\n").unwrap(); // stale diagnostic PID; no kernel lock is held
        let guard = SingleInstance::acquire(&dir).unwrap();
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(guard);
    }

    #[test]
    fn state_lock_uses_the_kernel_lock_even_when_pid_text_is_unreadable() {
        let dir = scratch("unreadable");
        let lock_path = dir.join("shared.lock");
        fs::write(&lock_path, "not-a-pid\n").unwrap();

        let first = StateLock::acquire(&dir, "shared.lock").unwrap();
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            format!("{}\n", std::process::id())
        );
        let second = StateLock::acquire(&dir, "shared.lock");
        assert!(
            second.is_err(),
            "a live kernel lock must exclude a second holder"
        );
        drop(first);

        let next = StateLock::acquire(&dir, "shared.lock").unwrap();
        drop(next);
        assert!(
            lock_path.exists(),
            "the reusable lock file remains in place"
        );
    }

    #[test]
    fn state_lock_excludes_a_second_holder_and_releases() {
        let dir = scratch("statelock");
        let first = StateLock::acquire(&dir, "shared.lock").unwrap();
        assert!(StateLock::acquire(&dir, "shared.lock").is_err());
        drop(first);
        assert!(StateLock::acquire(&dir, "shared.lock").is_ok());
    }

    #[test]
    fn state_lock_release_unlocks_without_deleting_the_reusable_file() {
        let dir = scratch("owner");
        let lock_path = dir.join("shared.lock");
        let guard = StateLock::acquire(&dir, "shared.lock").unwrap();
        assert!(lock_path.exists());
        drop(guard);

        assert!(lock_path.exists());
        let next = StateLock::acquire(&dir, "shared.lock").unwrap();
        drop(next);
    }

    /// Child half of the cross-process tests: runs when HORIZON_TEST_LOCK_DIR is set, reports
    /// the outcome through HORIZON_TEST_OUTCOME, and exits without dropping its guard, so the
    /// parent can observe a crash.
    #[test]
    fn state_lock_cross_process_child() {
        let Some(dir) = std::env::var_os("HORIZON_TEST_LOCK_DIR") else {
            return; // not a child invocation: pass trivially
        };
        let Some(out) = std::env::var_os("HORIZON_TEST_OUTCOME") else {
            return;
        };
        let dir = PathBuf::from(dir);
        match StateLock::acquire(&dir, "shared.lock") {
            Ok(guard) => {
                let _ = fs::write(&out, "ok");
                // Deliberately do not drop the guard: simulate a crash.
                std::mem::forget(guard);
            }
            Err(error) => {
                let _ = fs::write(&out, format!("held:{error}"));
            }
        }
        std::process::exit(0);
    }

    fn run_cross_process_child(dir: &Path, out: &Path) -> String {
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(exe)
            .args([
                "single_instance::tests::state_lock_cross_process_child",
                "--exact",
                "--nocapture",
            ])
            .env("HORIZON_TEST_LOCK_DIR", dir)
            .env("HORIZON_TEST_OUTCOME", out)
            .status()
            .unwrap();
        assert!(status.success());
        fs::read_to_string(out).unwrap()
    }

    #[test]
    fn state_lock_excludes_another_process_then_releases() {
        let dir = scratch("xproc");
        let guard = StateLock::acquire(&dir, "shared.lock").unwrap();
        let out1 = dir.join("child-1.txt");
        let held = run_cross_process_child(&dir, &out1);
        assert!(
            held.trim().starts_with("held:"),
            "a live owner must refuse a second process: {held}"
        );
        drop(guard);
        let out2 = dir.join("child-2.txt");
        let ok = run_cross_process_child(&dir, &out2);
        assert_eq!(ok.trim(), "ok");
        // The child exited without releasing (crash simulation): the kernel released its lock.
        // The persistent file is reused by the next owner.
        assert!(dir.join("shared.lock").exists());
        let _ = StateLock::acquire(&dir, "shared.lock").unwrap();
    }

    #[test]
    fn state_lock_recovers_after_owner_crash() {
        let dir = scratch("crash");
        let out = dir.join("child.txt");
        let ok = run_cross_process_child(&dir, &out);
        assert_eq!(ok.trim(), "ok");
        // The child acquired the lock and exited without releasing: the kernel freed it.
        let _ = StateLock::acquire(&dir, "shared.lock").unwrap();
    }
}
