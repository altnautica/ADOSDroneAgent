use std::fs::{File, OpenOptions};
use std::path::Path;

/// A held advisory lock. Drop releases it.
pub struct Wlan0Lock {
    #[cfg(target_os = "linux")]
    _flock: nix::fcntl::Flock<File>,
    #[cfg(not(target_os = "linux"))]
    _file: File,
}

impl Wlan0Lock {
    /// Try to take the exclusive non-blocking lock on `path`. Errs when the
    /// lock is already held by another owner or the file cannot be opened.
    pub fn try_acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        #[cfg(target_os = "linux")]
        {
            use nix::fcntl::{Flock, FlockArg};
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(flock) => Ok(Self { _flock: flock }),
                Err((_file, errno)) => Err(std::io::Error::from_raw_os_error(errno as i32)),
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            use std::os::unix::io::AsRawFd;
            // LOCK_EX = 2, LOCK_NB = 4 on BSD/macOS and Linux alike.
            let rc = unsafe { flock_ffi(file.as_raw_fd(), 2 | 4) };
            if rc == 0 {
                Ok(Self { _file: file })
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for Wlan0Lock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // LOCK_UN = 8. Best-effort; the fd close also drops the lock.
        unsafe {
            let _ = flock_ffi(self._file.as_raw_fd(), 8);
        }
    }
}

// On Linux nix::Flock's own Drop releases (LOCK_UN) and closes the fd.

#[cfg(not(target_os = "linux"))]
extern "C" {
    #[link_name = "flock"]
    fn flock_ffi(fd: i32, operation: i32) -> i32;
}
