//! Best-effort schema-version checking for on-disk state sidecars.
//!
//! A state sidecar is a small JSON snapshot a service drops under the run
//! directory (`/run/ados/*.json`) so the REST and heartbeat surfaces can read
//! its state without an IPC round-trip. Unlike the frozen wire contracts, a
//! sidecar is *best-effort*: a reader must tolerate a file written by an older
//! or newer agent rather than reject or crash on it.
//!
//! So the version discipline here is deliberately softer than the wire
//! contracts: when a sidecar's declared `version` does not match the value this
//! build expects, [`check_sidecar_version`] logs a warning and returns `false`
//! but the reader keeps using the data. A stale sidecar written by an older
//! agent (which had no `version` field at all, so it reads back as `0`) must
//! never brick a reader — it warns once and proceeds.
//!
//! Each sidecar's expected version is registered in
//! [`crate::contracts`] and mirrored by a per-file constant on the writer side;
//! a reader passes both to [`check_sidecar_version`] at its deserialize site.

use serde::{Deserialize, Serialize};

/// Compare a sidecar's declared schema version against the version this build
/// expects.
///
/// Returns `true` when they match. On a mismatch it logs a warning naming the
/// sidecar and both versions, then returns `false` — the caller should still
/// use the parsed data (sidecars are best-effort). This function never panics
/// and never rejects: it is a soft, self-describing drift signal, not a gate.
///
/// `got` is the version read off the file (an older file with no `version`
/// field deserializes to `0` via the `#[serde(default)]` mixin), and `ours` is
/// the version constant this build was compiled against for that sidecar.
pub fn check_sidecar_version(file: &str, got: u16, ours: u16) -> bool {
    if got != ours {
        // Fires in every consumer (tracing is a required dependency), so a
        // schema drift is never silent regardless of which crate reads the
        // sidecar; still signal the mismatch via the return value.
        tracing::warn!(
            sidecar = file,
            got,
            ours,
            "sidecar schema version mismatch; reading best-effort"
        );
        return false;
    }
    true
}

/// A minimal mixin for peeking a sidecar's schema version.
///
/// Deserializing a sidecar file into `Versioned` extracts only the `version`
/// field (serde ignores every other field) and defaults it to `0` when absent,
/// so a reader can learn a file's schema version without deserializing the whole
/// snapshot and without failing on an older, version-less file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versioned {
    /// The sidecar schema version. Defaults to `0` for a file written before the
    /// field existed.
    #[serde(default)]
    pub version: u16,
}

/// Replace `path` with `data` so a power cut leaves either the old file or the
/// new one, never a truncated mix. The replacement keeps the target's
/// permissions when the target exists and is created 0644 otherwise. See
/// [`write_atomic`] for the write sequence.
pub fn write_durable(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    write_atomic(path, data, mode)
}

/// Serialize `value` as JSON and write it through [`write_atomic`].
pub fn write_json_atomic<T: Serialize + ?Sized>(
    path: &std::path::Path,
    value: &T,
    mode: u32,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    write_atomic(path, &body, mode)
}

/// Atomically replace `path` with `data` at file mode `mode`.
///
/// Creates the parent directory, then writes a sibling temp file named
/// `<name>.tmp-<8 random hex>` opened `create_new` with `mode` (so two
/// concurrent writers, threads or processes, never share a temp file), sets
/// the exact mode regardless of umask, fsyncs it, renames it over `path`, and
/// fsyncs the directory so the rename itself survives a power cut. On any
/// error the temp file is removed.
pub fn write_atomic(path: &std::path::Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut suffix = [0u8; 4];
    getrandom::fill(&mut suffix).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp-{}", hex::encode(suffix)));
    let tmp = parent.join(tmp_name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let result = (|| {
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    drop(f);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_pass() {
        assert!(check_sidecar_version("example-sidecar", 1, 1));
        assert!(check_sidecar_version("example-sidecar", 0, 0));
    }

    #[test]
    fn mismatched_versions_fail() {
        // Older file (version-less, reads back as 0) against a v1 build.
        assert!(!check_sidecar_version("example-sidecar", 0, 1));
        // Newer file than this build expects.
        assert!(!check_sidecar_version("example-sidecar", 2, 1));
    }

    #[test]
    fn versioned_defaults_to_zero_and_ignores_other_fields() {
        let empty: Versioned = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.version, 0);

        let with_extra: Versioned =
            serde_json::from_str(r#"{"version":3,"link_state":"healthy"}"#).unwrap();
        assert_eq!(with_extra.version, 3);
    }

    #[test]
    fn write_durable_replaces_the_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("extlinux.conf");
        std::fs::write(&p, b"old").unwrap();
        write_durable(&p, b"new contents").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new contents");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn write_atomic_sets_mode_and_creates_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("key.bin");
        write_atomic(&p, &[7u8; 8], 0o600).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // write_durable keeps the existing mode across a rewrite.
        write_durable(&p, b"x").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn concurrent_write_atomic_always_parses_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = std::sync::Arc::new(dir.path().join("state.json"));
        let handles: Vec<_> = (0..8u32)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for j in 0..50u32 {
                        let v =
                            serde_json::json!({ "writer": i, "seq": j, "pad": "x".repeat(512) });
                        write_json_atomic(&p, &v, 0o644).unwrap();
                        let raw = std::fs::read(&*p).unwrap();
                        let _: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
}
