//! At most one daemon per state directory: it holds an exclusive lock on
//! `daemon.lock` for its whole life. The OS drops the lock when the process
//! dies, however it dies, so a stale lock can't block the next daemon.
//! `daemon.pid` is informational only.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

pub const LOCK_FILE_NAME: &str = "daemon.lock";
pub const PID_FILE_NAME: &str = "daemon.pid";

pub struct DaemonLock {
    _file: File,
    pid_path: PathBuf,
}

fn open_lock_file(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE_NAME))
}

/// Take the lock, or None if another daemon has it.
pub fn acquire(dir: &Path) -> io::Result<Option<DaemonLock>> {
    let file = open_lock_file(dir)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    let pid_path = dir.join(PID_FILE_NAME);
    fs::write(&pid_path, format!("{}\n", std::process::id()))?;
    Ok(Some(DaemonLock {
        _file: file,
        pid_path,
    }))
}

/// Whether a daemon holds the lock. Never signals or touches a process.
pub fn daemon_running(dir: &Path) -> bool {
    let Ok(file) = open_lock_file(dir) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false, // released again when `file` drops
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pid_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time() {
        let dir = crate::files::test_dir("lock");
        assert!(!daemon_running(&dir));
        let first = acquire(&dir).unwrap().expect("free lock");
        assert!(daemon_running(&dir));
        assert!(acquire(&dir).unwrap().is_none());
        drop(first);
        assert!(!daemon_running(&dir));
        assert!(acquire(&dir).unwrap().is_some());
    }

    #[test]
    fn probing_leaves_the_lock_free() {
        let dir = crate::files::test_dir("lock-probe");
        assert!(!daemon_running(&dir));
        assert!(!daemon_running(&dir));
        assert!(acquire(&dir).unwrap().is_some());
    }

    #[test]
    fn pid_file_is_written_and_removed() {
        let dir = crate::files::test_dir("lock-pid");
        let lock = acquire(&dir).unwrap().unwrap();
        let pid = fs::read_to_string(dir.join(PID_FILE_NAME)).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
        drop(lock);
        assert!(!dir.join(PID_FILE_NAME).exists());
    }
}
