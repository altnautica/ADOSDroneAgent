//! Persistent parameter cache.
//!
//! Survives across FC reboots and agent restarts so the GCS does not re-sweep every
//! parameter on each reconnect.

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
    /// Bumped once per [`set`](ParamCache::set) that actually changed an entry.
    /// Deliberately NOT persisted — see [`generation`](ParamCache::generation).
    generation: u64,
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
            generation: 0,
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
    ///
    /// Returns whether the entry actually changed — a new name, a different
    /// value, or a different `param_type`. A repeat `PARAM_VALUE` carrying what
    /// is already cached refreshes `last_updated` and returns `false`, leaving
    /// [`generation`](Self::generation) alone: an FC re-announcing the same 700
    /// values must not make every consumer re-download the map. The value
    /// comparison is on the bit pattern so a NaN never reads as "changed
    /// again" on every frame (`NaN != NaN` under `PartialEq`).
    pub fn set(&mut self, name: &str, value: f64, param_type: i64) -> bool {
        let changed = match self.params.get(name) {
            Some(prev) => prev.value.to_bits() != value.to_bits() || prev.param_type != param_type,
            None => true,
        };
        self.params.insert(
            name.to_string(),
            Entry {
                value,
                param_type,
                last_updated: now_unix(),
            },
        );
        if changed {
            self.generation += 1;
        }
        changed
    }

    /// How many times a cached parameter actually changed since this process
    /// started.
    ///
    /// This replaces the whole parameter map in the 10 Hz state snapshot: the
    /// map was ~24 KB of JSON republished ten times a second with no delta and
    /// no cap, which is what made a relayed `/api/telemetry` need ~21 radio
    /// fragments. A consumer instead compares this counter against its own
    /// last-seen value and refetches `GET /api/params` once on a mismatch.
    ///
    /// Deliberately NOT persisted and NOT bumped by [`load`](Self::load): a
    /// restart starting back at 0 reads as a mismatch to every consumer, which
    /// triggers exactly the one refetch a restart warrants.
    pub fn generation(&self) -> u64 {
        self.generation
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
mod tests;
