//! Persistent parameter cache.
//!
//! Mirrors the Python `ParamCache` (services/mavlink/param_cache.py): an
//! in-memory map of FC parameters persisted to `/var/lib/ados/params.json` as
//! `{ "NAME": { "value": f64, "param_type": i64, "last_updated": f64 } }`,
//! written atomically (temp file + rename) so a crash mid-write never leaves a
//! truncated cache. Survives across FC reboots and agent restarts so the GCS
//! does not re-sweep every parameter on each reconnect.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

/// Default on-disk location. The state dir is created at install time.
pub const DEFAULT_PARAMS_PATH: &str = "/var/lib/ados/params.json";

#[derive(Debug, Clone)]
struct Entry {
    value: f64,
    param_type: i64,
    last_updated: f64,
}

/// In-memory parameter cache with atomic JSON persistence.
#[derive(Debug)]
pub struct ParamCache {
    path: PathBuf,
    params: BTreeMap<String, Entry>,
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl ParamCache {
    /// Create a cache backed by `path` (not yet loaded from disk).
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            params: BTreeMap::new(),
        }
    }

    /// Create a cache at the default path.
    pub fn default_path() -> Self {
        Self::new(DEFAULT_PARAMS_PATH)
    }

    /// Number of cached parameters.
    pub fn count(&self) -> usize {
        self.params.len()
    }

    /// Get a single parameter value.
    pub fn get(&self, name: &str) -> Option<f64> {
        self.params.get(name).map(|e| e.value)
    }

    /// Insert or update a parameter (does not persist; call [`save`](Self::save)).
    pub fn set(&mut self, name: &str, value: f64, param_type: i64) {
        self.params.insert(
            name.to_string(),
            Entry {
                value,
                param_type,
                last_updated: now_unix(),
            },
        );
    }

    /// All parameters as a `{ name: value }` JSON object (the `params` blob the
    /// state snapshot carries).
    pub fn get_all(&self) -> Map<String, Value> {
        self.params
            .iter()
            .map(|(k, e)| (k.clone(), json!(e.value)))
            .collect()
    }

    /// The on-disk path this cache persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialise the cache to the JSON bytes that [`write_atomic`] persists.
    /// Cheap and synchronous; intended to be called under a lock so the bytes
    /// can be snapshotted and the lock released before the (slow) disk write.
    pub fn serialize(&self) -> std::io::Result<Vec<u8>> {
        let obj: Map<String, Value> = self
            .params
            .iter()
            .map(|(k, e)| {
                (
                    k.clone(),
                    json!({
                        "value": e.value,
                        "param_type": e.param_type,
                        "last_updated": e.last_updated,
                    }),
                )
            })
            .collect();
        Ok(serde_json::to_vec(&Value::Object(obj))?)
    }

    /// Persist the cache to disk atomically (temp file + rename). Best-effort:
    /// returns the IO error if the write fails so the caller can log it.
    ///
    /// This performs blocking disk I/O. On the async FC read loop, prefer
    /// snapshotting the bytes with [`serialize`](Self::serialize) under the lock
    /// and writing them off-reactor with [`write_atomic`]; this method remains
    /// for synchronous callers and tests.
    pub fn save(&self) -> std::io::Result<()> {
        let body = self.serialize()?;
        write_atomic(&self.path, &body)
    }

    /// Load the cache from disk. A missing file is not an error (returns an
    /// empty cache). Malformed entries are skipped.
    pub fn load(&mut self) -> std::io::Result<()> {
        let body = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let parsed: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Ok(()), // corrupt cache: start empty rather than fail
        };
        if let Value::Object(obj) = parsed {
            for (name, entry) in obj {
                if let Some(value) = entry.get("value").and_then(Value::as_f64) {
                    let param_type = entry.get("param_type").and_then(Value::as_i64).unwrap_or(0);
                    let last_updated = entry
                        .get("last_updated")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    self.params.insert(
                        name,
                        Entry {
                            value,
                            param_type,
                            last_updated,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}

/// Write `body` to `path` atomically (temp file + rename) creating the parent
/// directory if needed. Blocking disk I/O: call from a synchronous context or
/// from `tokio::task::spawn_blocking`, never directly on the reactor.
pub fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_count_and_get_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ParamCache::new(dir.path().join("params.json"));
        assert_eq!(c.count(), 0);
        c.set("WPNAV_SPEED", 500.0, 9);
        c.set("ATC_RAT_RLL_P", 0.135, 9);
        assert_eq!(c.count(), 2);
        assert_eq!(c.get("WPNAV_SPEED"), Some(500.0));
        assert_eq!(c.get("MISSING"), None);
        let all = c.get_all();
        assert_eq!(all.get("WPNAV_SPEED"), Some(&json!(500.0)));
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        {
            let mut c = ParamCache::new(&path);
            c.set("FOO", 1.5, 9);
            c.set("BAR", -2.0, 6);
            c.save().unwrap();
        }
        let mut c2 = ParamCache::new(&path);
        c2.load().unwrap();
        assert_eq!(c2.count(), 2);
        assert_eq!(c2.get("FOO"), Some(1.5));
        assert_eq!(c2.get("BAR"), Some(-2.0));
    }

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ParamCache::new(dir.path().join("does-not-exist.json"));
        c.load().unwrap();
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn load_corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let mut c = ParamCache::new(&path);
        c.load().unwrap();
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c/params.json");
        let mut c = ParamCache::new(&nested);
        c.set("X", 1.0, 9);
        c.save().unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn serialize_then_write_atomic_matches_save_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/params.json");
        let mut c = ParamCache::new(&path);
        c.set("FOO", 1.5, 9);
        c.set("BAR", -2.0, 6);

        // The off-reactor split (serialize under the lock, write later) must
        // produce a file equivalent to the synchronous save() path.
        let body = c.serialize().unwrap();
        write_atomic(&path, &body).unwrap();
        assert!(path.exists(), "write_atomic creates the parent dir");

        let mut reloaded = ParamCache::new(&path);
        reloaded.load().unwrap();
        assert_eq!(reloaded.count(), 2);
        assert_eq!(reloaded.get("FOO"), Some(1.5));
        assert_eq!(reloaded.get("BAR"), Some(-2.0));
    }

    #[test]
    fn path_accessor_returns_backing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        let c = ParamCache::new(&path);
        assert_eq!(c.path(), path.as_path());
    }
}
