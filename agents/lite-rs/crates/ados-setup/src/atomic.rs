//! Atomic-write helpers for the setup crate.
//!
//! The implementation lives in `ados_core::atomic`; this module is a
//! thin compatibility shim so existing callsites within the setup
//! crate (`crate::atomic::atomic_write`, `crate::atomic::ensure_secret_dir`)
//! keep working unchanged. New callers should prefer the helpers in
//! `ados_core::atomic` directly.

use std::path::Path;

pub use ados_core::atomic::{ensure_secret_dir as core_ensure_secret_dir, AtomicWriteError};

/// Atomically write `bytes` to `path` with the given mode. Returns
/// `std::io::Result` to keep the existing call shape; the underlying
/// helper's typed error variants flatten into `io::Error` with the
/// `Other` kind for the `InvalidMode` case (which the in-tree callers
/// never trigger because they all pass octal constants).
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    match ados_core::atomic::write_atomic(path, bytes, mode) {
        Ok(()) => Ok(()),
        Err(AtomicWriteError::Io(e)) => Err(e),
        Err(AtomicWriteError::InvalidMode(m)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid mode 0o{m:o}"),
        )),
    }
}

/// Ensure a directory exists with mode 0o700. Wraps the canonical
/// helper so the existing `crate::atomic::ensure_secret_dir` call
/// signature (`std::io::Result`) is preserved.
pub fn ensure_secret_dir(parent: &Path) -> std::io::Result<()> {
    match core_ensure_secret_dir(parent) {
        Ok(()) => Ok(()),
        Err(AtomicWriteError::Io(e)) => Err(e),
        Err(AtomicWriteError::InvalidMode(m)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid mode 0o{m:o}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests confirming the shim still upholds the contract that
    //! the setup-crate callsites depend on. The full property suite
    //! lives in `ados-core/tests/atomic.rs`.
    use super::*;

    #[test]
    fn writes_through_to_canonical_helper() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir/state.json");
        atomic_write(&path, b"hello world", 0o644).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn replaces_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        atomic_write(&path, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn applies_mode_at_create() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        atomic_write(&path, b"top-secret", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got 0o{:o}", mode);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_secret_dir_creates_with_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("secrets");
        ensure_secret_dir(&secret).unwrap();
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "expected 0700, got 0o{:o}", mode);
    }
}
