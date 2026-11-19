//! Single-instance guard and cross-process state locks: at most one launcher process holds the
//! data directory, and shared state writes serialize through per-file lock files.
//!
//! The launcher-level lock (`launcher.lock`) makes a second launcher refuse to start while the
//! first is alive. A lock file names the owning process id. A crash leaves the file behind; the
//! next acquirer detects the dead owner (Linux via `/proc`, macOS/BSD via `kill(pid, 0)`,
//! Windows via `OpenProcess`) and reclaims the lock, so there is no permanent deadlock. The owner
//! is the only process allowed to remove its lock file.
//!
//! Shared-state writers use [`StateLock`], a cross-process mutual exclusion on a per-file lock
//! name, for read-modify-write cycles that must not interleave.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The launcher-level lock file's name inside the launcher's data directory.
const LAUNCHER_LOCK: &str = "launcher.lock";

/// A cross-process mutual exclusion lock on a file inside a directory.
///
/// The lock is a file named `name` inside `dir` holding the owning process id. Creating the file
/// with `create_new` is atomic, so exactly one live process can hold it; when the file already
/// exists the owner is checked through its process id and a stale lock is recycled, which keeps
/// the lock crash-safe without permanent deadlocks.
pub struct StateLock {
    lock_path: PathBuf,
}

impl StateLock {
    /// Try to acquire the cross-process lock `name` inside `dir`.
    ///
    /// Fails with a clear message when another live process already holds the lock. A lock naming
    /// a dead process is reclaimed; an unreadable owner fails closed so a partially written lock
    /// can never be mistaken for a stale lock while its creator is still starting.
    pub fn acquire(dir: &Path, name: &str) -> Result<Self, String> {
        let lock_path = dir.join(name);
        let mut attempts = 0;
        loop {
            attempts += 1;
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    if let Err(error) =
                        writeln!(file, "{}", std::process::id()).and_then(|_| file.sync_all())
                    {
                        drop(file);
                        let _ = fs::remove_file(&lock_path);
                        return Err(format!(
                            "Could not initialize lock {}: {error}",
                            lock_path.display()
                        ));
                    }
                    return Ok(Self { lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Never reclaim a lock whose owner cannot be read. A creator may have
                    // created the file but not finished writing its PID yet; deleting it here
                    // would let two live processes believe they own the same lock.
                    let owner_text = match fs::read_to_string(&lock_path) {
                        Ok(text) => text,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            if attempts > 10 {
                                return Err(format!(
                                    "Could not acquire the lock {} after retries: {e}",
                                    lock_path.display()
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            continue;
                        }
                        Err(error) => {
                            return Err(format!(
                                "Could not read lock owner at {}: {error}",
                                lock_path.display()
                            ));
                        }
                    };
                    let owner = owner_text.trim().parse::<u32>().map_err(|_| {
                        format!(
                            "The lock {} exists but its owner PID is unreadable; it was left untouched to avoid starting a second launcher.",
                            lock_path.display()
                        )
                    })?;
                    if process_alive(owner) {
                        return Err(format!(
                            "Another process (PID {owner}) holds the lock {}.",
                            lock_path.display()
                        ));
                    }
                    match fs::remove_file(&lock_path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!(
                                "Could not reclaim stale lock {}: {error}",
                                lock_path.display()
                            ));
                        }
                    }
                    if attempts > 10 {
                        return Err(format!(
                            "Could not acquire the lock {} after retries: {e}",
                            lock_path.display()
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("{lock_path:?}: {e}")),
            }
        }
    }

    /// Release the lock by removing its file.
    ///
    /// The file is removed only when it still names this process as the owner: a lock reclaimed
    /// by another process after a crash is never deleted by a stale owner. The check-and-remove
    /// is best effort (another process may recreate the file between the two); an OS-level file
    /// lock is unavailable without new dependencies.
    pub(crate) fn release(&mut self) {
        release_lock_file(&self.lock_path);
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Remove `lock_path` only when it still names this process as the owner.
fn release_lock_file(lock_path: &Path) {
    let ours = std::process::id().to_string();
    let owner = fs::read_to_string(lock_path)
        .ok()
        .map(|text| text.trim().to_string());
    if owner.as_deref() == Some(ours.as_str()) {
        let _ = fs::remove_file(lock_path);
    }
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

/// Whether a process with `pid` is still alive.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Whether a process with `pid` is still alive, asking the kernel through `kill(pid, 0)`:
/// signal 0 performs error checking without sending a signal.
#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Whether a process with `pid` is still alive: open the process with query limited
/// information and keep the handle only for the check. Access denied means the process exists
/// but cannot be opened, which still answers the question.
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired: u32, inherit: i32, process_id: u32) -> *mut core::ffi::c_void;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_ACCESS_DENIED: u32 = 5;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    unsafe { CloseHandle(handle) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fs::write(&lock, "999999999\n").unwrap(); // a PID that does not exist
        assert!(SingleInstance::acquire(&dir).is_ok());
    }

    #[test]
    fn state_lock_refuses_unreadable_owner_without_removing_it() {
        let dir = scratch("unreadable");
        for (index, contents) in ["", "not-a-pid\\n"].into_iter().enumerate() {
            let name = format!("shared-{index}.lock");
            let lock_path = dir.join(&name);
            fs::write(&lock_path, contents).unwrap();

            let error = match StateLock::acquire(&dir, &name) {
                Ok(_) => panic!("an unreadable lock owner must fail closed"),
                Err(error) => error,
            };
            assert!(error.contains("owner PID is unreadable"), "{error}");
            assert_eq!(fs::read_to_string(&lock_path).unwrap(), contents);
        }
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
    fn state_lock_release_removes_only_the_owners_file() {
        let dir = scratch("owner");
        let lock_path = dir.join("shared.lock");
        // A lock file naming a different process survives a release call.
        fs::write(&lock_path, "424242\n").unwrap();
        let mut not_ours = StateLock {
            lock_path: lock_path.clone(),
        };
        not_ours.release();
        assert!(lock_path.exists());
        // This process's own lock file is removed.
        fs::write(&lock_path, format!("{}\n", std::process::id())).unwrap();
        let mut ours = StateLock {
            lock_path: lock_path.clone(),
        };
        ours.release();
        assert!(!lock_path.exists());
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
        // The child exited without releasing (crash simulation): its stale lock file remains.
        assert!(dir.join("shared.lock").exists());
        // A fresh acquisition reclaims the stale lock instead of deadlocking.
        let _ = StateLock::acquire(&dir, "shared.lock").unwrap();
    }

    #[test]
    fn state_lock_recovers_after_owner_crash() {
        let dir = scratch("crash");
        let out = dir.join("child.txt");
        let ok = run_cross_process_child(&dir, &out);
        assert_eq!(ok.trim(), "ok");
        // The child acquired the lock and exited without releasing: the file remains, naming a
        // process that no longer exists. A fresh acquisition reclaims it.
        let _ = StateLock::acquire(&dir, "shared.lock").unwrap();
    }
}
