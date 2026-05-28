use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct ProfileLock {
    path: PathBuf,
    _file: File,
}

#[derive(Debug)]
pub enum ProfileLockError {
    Open { path: PathBuf, source: std::io::Error },
    Held { path: PathBuf },
    Flock { path: PathBuf, source: std::io::Error },
}

impl std::fmt::Display for ProfileLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileLockError::Open { path, source } => {
                write!(f, "failed to open profile lock file {}: {source}", path.display())
            }
            ProfileLockError::Held { path } => {
                write!(
                    f,
                    "Profile lock {} is held by another unidrive process; refusing mount.",
                    path.display()
                )
            }
            ProfileLockError::Flock { path, source } => {
                write!(f, "flock({}) failed: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ProfileLockError {}

impl ProfileLock {
    /// Open `path` (creating it if missing — matching the JVM-side
    /// `ProcessLock` `StandardOpenOption.CREATE + WRITE`) and acquire an
    /// exclusive BSD advisory lock via `flock(LOCK_EX | LOCK_NB)`. If the
    /// file is already locked, return `ProfileLockError::Held` immediately
    /// (no blocking — the JVM-side `tryLock(timeout)` is the human-friendly
    /// waiter; the co-daemon fails fast).
    ///
    /// The lock is held for the lifetime of the returned `ProfileLock`. The
    /// kernel releases it on FD close — i.e. on `Drop`, or on process exit
    /// (orderly or via SIGKILL). That kernel-on-exit release is the load
    /// bearing property: spec §4 R4 closes the `kill -9 <jvm-pid>` window
    /// by giving the co-daemon its own lock that outlives the JVM parent.
    pub fn acquire(path: &Path) -> Result<Self, ProfileLockError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| ProfileLockError::Open {
                path: path.to_path_buf(),
                source: e,
            })?;

        let fd = file.as_raw_fd();
        // SAFETY: `flock(2)` FFI — fd is owned by `file` and stays valid for
        // the whole call. We pass a constant flags integer; no aliasing.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(ProfileLock {
                path: path.to_path_buf(),
                _file: file,
            });
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Err(ProfileLockError::Held {
                path: path.to_path_buf(),
            }),
            _ => Err(ProfileLockError::Flock {
                path: path.to_path_buf(),
                source: err,
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn acquire_creates_missing_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".lock");
        assert!(!lock_path.exists());
        let _guard = ProfileLock::acquire(&lock_path).expect("acquire on fresh path");
        assert!(lock_path.exists());
    }

    #[test]
    fn acquire_succeeds_on_unheld_file() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".lock");
        std::fs::write(&lock_path, b"").unwrap();
        let _guard = ProfileLock::acquire(&lock_path).expect("first acquire");
    }

    #[test]
    fn second_acquire_returns_held_when_already_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".lock");
        // Pre-acquire from an independent FD in the same process. flock(2)
        // semantics: BSD locks are per-FD (not per-inode-per-process), so
        // a second FD opening the same file sees LOCK_EX held by the first.
        let other = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let rc = unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "pre-acquire must succeed");

        let err = ProfileLock::acquire(&lock_path).unwrap_err();
        assert!(matches!(err, ProfileLockError::Held { .. }), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("refusing mount"), "message: {msg}");
    }

    #[test]
    fn drop_releases_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".lock");
        {
            let _guard = ProfileLock::acquire(&lock_path).expect("first acquire");
        }
        // After drop, a fresh acquire must succeed.
        let _guard2 = ProfileLock::acquire(&lock_path).expect("second acquire after drop");
    }
}
