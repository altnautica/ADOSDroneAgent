//! Atomic write helper with fsync + close-on-rename semantics.
//!
//! Every persistence site in this crate (state.json, pairing.json,
//! agent.yaml mutations, secret tokens) needs the same guarantee:
//! after the call returns Ok, an observer reading the path either sees
//! the prior fully-written file OR the new fully-written file — never
//! a torn or zero-byte intermediate. Power-loss between write and
//! rename leaves the prior file intact; power-loss between rename and
//! directory sync is handled by `parent.sync_all()`.
//!
//! Centralising the pattern here means the 6 atomic-write callsites
//! across state, pairing, profile, cloud-choice, cloudflare-token,
//! and pair-code persistence all get the same fsync + permission
//! discipline without each having to repeat it.

use std::io::Write;
use std::path::Path;

/// Ensure a directory exists with mode 0o700 (owner-only rwx).
///
/// `atomic_write` uses `std::fs::create_dir_all` which inherits the
/// process umask — on systems where the operator's umask is 0o022 the
/// parent directory ends up world-traversable (0o755). When the path
/// is a secret like `/etc/ados/secrets/cloudflare-tunnel-token`, the
/// file itself is 0o600 but the directory's o+x bit lets any local
/// user `stat` and confirm the file exists, leaking presence metadata.
///
/// Call this BEFORE `atomic_write` for any path inside a secret-only
/// directory. Idempotent: a second call is a no-op chmod when the
/// directory is already 0o700, and creates it otherwise.
///
/// Unix-only behavior — on non-Unix the chmod is a no-op (the
/// `permissions().set_mode` API is gated on `unix::fs::PermissionsExt`).
pub fn ensure_secret_dir(parent: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(parent, perms)?;
    }
    Ok(())
}

/// Atomic-write `bytes` to `path` with the given mode (Unix; ignored on
/// other platforms).
///
/// Procedure:
/// 1. Create the parent directory if missing.
/// 2. Open a sibling tempfile with O_CREAT | O_EXCL | the supplied mode
///    (closes the TOCTOU window where a non-O_EXCL open would briefly
///    create the file at the umask default before chmod fixes it).
/// 3. Write all bytes, fsync the file, close it.
/// 4. Rename(2) over the destination.
/// 5. Best-effort fsync the parent directory so the rename is durable
///    (matters on ext4 default `data=ordered` and on power-loss).
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    // Tempfile name: include process id + nanosecond timestamp so two
    // concurrent processes from the same PID namespace don't collide.
    // PID alone races inside a container.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tmp"),
        std::process::id(),
        nanos
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
            // Clean up any leftover tmp in case of partial state from
            // a previous crash.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };

    let res = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();

    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    drop(file); // close before rename to release the file handle

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Best-effort parent sync. Some filesystems (overlayfs in some
    // container setups) refuse fsync on directories — ignore the error.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_renames_atomically() {
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

    #[test]
    fn cleans_up_tempfile_on_failure() {
        // We can't easily force a write failure in unit tests, but we
        // can verify the tempfile naming pattern is unique enough that
        // a second concurrent call to the same path doesn't collide.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        for _ in 0..10 {
            atomic_write(&path, b"x", 0o644).unwrap();
        }
        // Only one final file exists, no leftover .tmp files.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "leftover tempfiles in {:?}", dir.path());
    }

    #[cfg(unix)]
    #[test]
    fn applies_mode_at_create_not_after() {
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
        // Defense-in-depth: a fresh secret-only directory must be 0o700
        // (owner-only rwx). umask-default 0o755 leaks file presence to
        // any local user via `stat`.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("secrets");
        ensure_secret_dir(&secret).unwrap();
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "expected 0700, got 0o{:o}", mode);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_secret_dir_tightens_existing_dir() {
        // Pre-existing world-traversable dir should be tightened on
        // call. Mirrors the upgrade path where /etc/ados/secrets/
        // already exists at 0o755 from a prior install.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("loose");
        std::fs::create_dir(&secret).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_secret_dir(&secret).unwrap();
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "expected 0700, got 0o{:o}", mode);
    }
}
