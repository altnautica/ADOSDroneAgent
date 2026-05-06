//! Canonical atomic-write helper.
//!
//! Every persistence site in the agent — state.json, pairing.json,
//! agent.yaml mutations, secret tokens, signing-key material, downloaded
//! binaries — needs the same crash-safe contract:
//!
//! After [`write_atomic`] returns Ok, an observer reading `path` either
//! sees the prior fully-written file OR the new fully-written file —
//! never a torn or zero-byte intermediate. Power loss between the write
//! and the rename leaves the prior file intact. Power loss between the
//! rename and the directory sync is handled by a best-effort parent
//! `sync_all` (matters on ext4 default `data=ordered` and on power-loss).
//!
//! Procedure on Unix:
//! 1. Create the parent directory if missing.
//! 2. Open a sibling tempfile with `O_CREAT | O_EXCL | mode` — closes
//!    the TOCTOU window where a non-`O_EXCL` open would briefly create
//!    the file at the umask default before chmod fixes it.
//! 3. Write all bytes, fsync the file, close it.
//! 4. `rename(2)` over the destination.
//! 5. Best-effort `fsync` of the parent directory.
//!
//! On error before the rename, the tempfile is removed so a partial
//! state never leaks onto disk.
//!
//! ext4 ordered-mode default is sufficient durability for v1. If a
//! future use case needs sync-on-rename across all filesystems we add a
//! `*_synced` variant rather than retrofit this one.

use std::io::Write;
use std::path::Path;

/// Errors returned by the atomic-write helpers. Wraps `io::Error` for
/// any underlying filesystem failure plus an `InvalidMode` variant for
/// the rare case where a caller hands a mode outside the 0o0..=0o7777
/// envelope. The mode check is paranoia: callers in this workspace pass
/// constants like `0o600` and `0o644`, but a fuzzed input that came in
/// over a REST handler should not silently widen permissions.
#[derive(Debug, thiserror::Error)]
pub enum AtomicWriteError {
    /// Underlying filesystem failure during create / write / rename /
    /// permission update.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Mode value is outside the 12-bit Unix permission space. Callers
    /// should pass octal constants like `0o600` or `0o644`.
    #[error("invalid mode: {0:o}")]
    InvalidMode(u32),
}

/// Atomically write `bytes` to `path` with file mode `mode`.
///
/// The caller picks the mode explicitly. For secrets pass `0o600`; for
/// public configs pass `0o644`. The convenience wrappers
/// [`write_atomic_secret`] and [`write_atomic_config`] cover the common
/// cases.
///
/// Uses tempfile-in-same-directory + `rename(2)` so a crash mid-write
/// leaves the previous file intact. The tempfile is created with the
/// requested mode at open time via `O_CREAT | O_EXCL` so the file is
/// never briefly readable at the process umask default.
///
/// See module-level docs for the full procedure and durability notes.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), AtomicWriteError> {
    if mode & !0o7777 != 0 {
        return Err(AtomicWriteError::InvalidMode(mode));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    // Tempfile name: include process id + nanosecond timestamp + a u64
    // counter sourced from a thread-local atomic so two concurrent
    // writers within the same process and same nanosecond resolution
    // do not collide. The leading dot keeps the tempfile out of casual
    // `ls` output.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = next_counter();
    let tmp = parent.join(format!(
        ".{}.{}.{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tmp"),
        std::process::id(),
        nanos,
        counter,
    ));

    // O_CREAT | O_EXCL — fail if the tempfile already exists rather
    // than risk a symlink-following clobber. Mode is set at open time
    // so the file never briefly exists at the umask default.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let mut file = match opts.open(&tmp) {
        Ok(f) => f,
        Err(e) => {
            // Clean up any leftover tmp in case of partial state from a
            // previous crash.
            let _ = std::fs::remove_file(&tmp);
            return Err(AtomicWriteError::Io(e));
        }
    };

    // Defence in depth: chmod after open in case the platform ignored
    // the OpenOptionsExt mode. Cheap and idempotent on Unix; a no-op on
    // non-Unix where the OpenOptionsExt path is compiled out.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(AtomicWriteError::Io(e));
        }
    }

    let res = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();

    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::Io(e));
    }

    drop(file); // close before rename so the file handle is released

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::Io(e));
    }

    // Best-effort parent sync. Some filesystems (overlayfs in some
    // container setups) refuse fsync on directories — ignore the error.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// Atomically write a secret with mode `0o600` (owner read+write).
///
/// Use for keypair files, pairing codes, API tokens, anything whose
/// presence or contents should not be visible to other local users.
/// Pair with [`ensure_secret_dir`] on the parent directory so the o+x
/// bit on the directory does not leak file existence via `stat`.
pub fn write_atomic_secret(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    write_atomic(path, bytes, 0o600)
}

/// Atomically write a public-readable config file with mode `0o644`.
///
/// Use for state.json, agent.yaml, board.yaml, any file an operator may
/// reasonably want to read with a non-root account.
pub fn write_atomic_config(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    write_atomic(path, bytes, 0o644)
}

/// Ensure a directory exists with mode `0o700` (owner-only rwx).
///
/// `write_atomic` calls `create_dir_all` which inherits the process
/// umask. On systems where the operator's umask is `0o022` the parent
/// directory ends up world-traversable (`0o755`). When the path is a
/// secret like `/etc/ados/secrets/cloudflare-tunnel-token`, the file
/// itself is `0o600` but the directory's `o+x` bit lets any local user
/// `stat` and confirm the file exists, leaking presence metadata.
///
/// Call this BEFORE `write_atomic_secret` for any path inside a
/// secret-only directory. Idempotent: a second call is a no-op chmod
/// when the directory is already `0o700`, and creates it otherwise.
///
/// Unix-only behaviour — on non-Unix the chmod is a no-op (the
/// `permissions().set_mode` API is gated on
/// `unix::fs::PermissionsExt`).
pub fn ensure_secret_dir(parent: &Path) -> Result<(), AtomicWriteError> {
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(parent, perms)?;
    }
    Ok(())
}

/// Process-local monotonic counter for tempfile name disambiguation.
fn next_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic_secret(&path, b"top-secret").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"top-secret");
    }

    #[test]
    fn round_trip_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir/config.yaml");
        write_atomic_config(&path, b"channel: 161\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "channel: 161\n"
        );
    }

    #[test]
    fn rejects_invalid_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let err = write_atomic(&path, b"x", 0o10000).unwrap_err();
        assert!(matches!(err, AtomicWriteError::InvalidMode(0o10000)));
    }
}
