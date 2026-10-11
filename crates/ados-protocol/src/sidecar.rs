//! On-disk state sidecars: the soft schema-version check, the atomic writers,
//! and the one freshness-gated reader.
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
//!
//! Freshness: a producer rewrites its sidecar whole on each tick, so the file's
//! mtime is the age of the reading inside it, and a producer that died leaves
//! its last file behind until reboot. [`read_fresh`] reads a registered sidecar
//! and classifies it against the registry's `max_age_ms` window; a future mtime
//! (a clock that stepped backwards) makes the age unprovable and reads as
//! [`Reading::Stale`], never as a fresh measurement.

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
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

/// The outcome of a freshness-gated sidecar read.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading<T> {
    /// Parsed, and written within the sidecar's freshness window.
    Fresh(T),
    /// Parsed, but older than the window or carrying a future or unreadable
    /// mtime: the last thing a producer wrote, not a current measurement.
    Stale(T),
    /// No file.
    Absent,
    /// The file exists but could not be read or parsed as `T`.
    Malformed(String),
}

impl<T> Reading<T> {
    /// The value when fresh, else `None`.
    pub fn fresh(self) -> Option<T> {
        match self {
            Reading::Fresh(v) => Some(v),
            _ => None,
        }
    }

    /// Whether the reading is fresh.
    pub fn is_fresh(&self) -> bool {
        matches!(self, Reading::Fresh(_))
    }
}

/// A file's mtime age relative to `now`, or `None` when the file is absent, its
/// mtime is unreadable, or its mtime is after `now` (an unprovable age).
pub fn file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    now.duration_since(modified).ok()
}

/// Read the registered sidecar `id` from its resolved path
/// ([`crate::paths::run_path`]) and classify it against its registered
/// freshness window. An unregistered id reads as [`Reading::Malformed`].
pub fn read_fresh<T: DeserializeOwned>(id: &str, now: SystemTime) -> Reading<T> {
    let Some(row) = crate::paths::sidecar(id) else {
        return Reading::Malformed(format!("sidecar {id:?} is not registered"));
    };
    read_fresh_at(&crate::paths::run_path(id), row.max_age(), now)
}

/// The path-injectable core of [`read_fresh`]: read `path` as `T` and classify
/// it against `max_age` (`None` = no window, every parsed read is fresh). A
/// future or unreadable mtime is [`Reading::Stale`].
pub fn read_fresh_at<T: DeserializeOwned>(
    path: &Path,
    max_age: Option<Duration>,
    now: SystemTime,
) -> Reading<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Reading::Absent,
        Err(e) => return Reading::Malformed(e.to_string()),
    };
    let value: T = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return Reading::Malformed(e.to_string()),
    };
    let Some(window) = max_age else {
        return Reading::Fresh(value);
    };
    match file_age(path, now) {
        Some(age) if age <= window => Reading::Fresh(value),
        _ => Reading::Stale(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_aged(path: &Path, body: &str) -> SystemTime {
        std::fs::write(path, body).unwrap();
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn read_fresh_within_and_past_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let written = write_aged(&p, r#"{"a":1}"#);
        let win = Some(Duration::from_secs(10));
        let at = |s: u64| -> Reading<serde_json::Value> {
            read_fresh_at(&p, win, written + Duration::from_secs(s))
        };
        assert_eq!(at(1), Reading::Fresh(serde_json::json!({"a": 1})));
        assert_eq!(at(11), Reading::Stale(serde_json::json!({"a": 1})));
        // No window: every parsed read is fresh.
        let r: Reading<serde_json::Value> =
            read_fresh_at(&p, None, written + Duration::from_secs(3600));
        assert!(r.is_fresh());
    }

    #[test]
    fn read_fresh_future_mtime_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let written = write_aged(&p, r#"{"a":1}"#);
        // "now" before the write: the clock stepped backwards.
        let earlier = written - Duration::from_secs(60);
        let r: Reading<serde_json::Value> =
            read_fresh_at(&p, Some(Duration::from_secs(10)), earlier);
        assert_eq!(r, Reading::Stale(serde_json::json!({"a": 1})));
        assert!(file_age(&p, earlier).is_none());
    }

    #[test]
    fn absent() {
        let dir = tempfile::tempdir().unwrap();
        let r: Reading<serde_json::Value> =
            read_fresh_at(&dir.path().join("none.json"), None, SystemTime::now());
        assert_eq!(r, Reading::Absent);
        assert!(file_age(&dir.path().join("none.json"), SystemTime::now()).is_none());
    }

    #[test]
    fn malformed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, b"{not json").unwrap();
        let r: Reading<serde_json::Value> = read_fresh_at(&p, None, SystemTime::now());
        assert!(matches!(r, Reading::Malformed(_)));
        // A well-formed body of the wrong shape is malformed for that type.
        std::fs::write(&p, b"[1,2]").unwrap();
        let r: Reading<serde_json::Map<String, serde_json::Value>> =
            read_fresh_at(&p, None, SystemTime::now());
        assert!(matches!(r, Reading::Malformed(_)));
    }

    #[test]
    fn read_fresh_rejects_an_unregistered_id() {
        let r: Reading<serde_json::Value> = read_fresh("no-such-sidecar", SystemTime::now());
        assert!(matches!(r, Reading::Malformed(_)));
    }

    #[test]
    fn registry_resolves_sidecar_and_socket_ids() {
        assert!(crate::paths::run_path("wfb-stats").ends_with(crate::paths::WFB_STATS_JSON));
        assert!(crate::paths::run_path("hid-cmd").ends_with(crate::paths::HID_CMD_SOCK));
        assert_eq!(
            crate::paths::max_age("wfb-stats"),
            Some(Duration::from_millis(crate::paths::WFB_STATS_MAX_AGE_MS))
        );
        // A path outside the run dir resolves as registered.
        assert_eq!(
            crate::paths::run_path("ground-station-input"),
            std::path::PathBuf::from("/etc/ados/ground-station-input.json")
        );
    }

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
