//! Persistent parameter cache.
//!
//! Survives across FC reboots and agent restarts so the GCS does not re-sweep every
//! parameter on each reconnect. The cache belongs to one flight controller: its
//! identity is persisted beside it and a different FC starts it empty, and a
//! complete fetch drops any name that FC no longer reports.

use std::collections::{BTreeMap, HashSet};
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

/// The flight controller a cache's entries came from, as its HEARTBEAT names
/// it. A different system id, autopilot or vehicle type is a different FC whose
/// parameter set shares nothing reliable with the cached one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FcIdentity {
    pub system_id: u8,
    pub autopilot: u8,
    pub vehicle_type: u8,
}

/// The indexed `PARAM_VALUE`s seen against one `param_count`.
#[derive(Debug, Default)]
struct Fetch {
    count: u16,
    names: BTreeMap<u16, String>,
    complete: bool,
}

/// In-memory parameter cache with atomic JSON persistence.
#[derive(Debug)]
pub struct ParamCache {
    path: PathBuf,
    params: BTreeMap<String, Entry>,
    /// Bumped once per [`set`](ParamCache::set) that actually changed an entry.
    /// Deliberately NOT persisted — see [`generation`](ParamCache::generation).
    generation: u64,
    fc: Option<FcIdentity>,
    fetch: Fetch,
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
            fc: None,
            fetch: Fetch::default(),
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

    /// Bind the cache to the flight controller now heartbeating, returning
    /// whether the recorded identity changed (so the caller persists the cache
    /// and then [`serialize_fc`](Self::serialize_fc)).
    ///
    /// A cache recorded for another FC, or of unknown origin (written before
    /// identities were kept), is cleared: serving it would show one vehicle's
    /// parameters on another, and its size would read as an already-complete
    /// download of the new FC's set.
    pub fn adopt_fc(&mut self, id: FcIdentity) -> bool {
        if self.fc == Some(id) {
            return false;
        }
        if !self.params.is_empty() {
            tracing::info!(
                cleared = self.params.len(),
                system_id = id.system_id,
                autopilot = id.autopilot,
                vehicle_type = id.vehicle_type,
                "param_cache_cleared_for_new_fc"
            );
            self.params.clear();
            self.generation += 1;
        }
        self.fetch = Fetch::default();
        self.fc = Some(id);
        true
    }

    /// Note that the FC reported `name` in slot `index` of `count`, returning
    /// how many cached names were pruned.
    ///
    /// Once every slot `0..count` has been seen, the fetch is complete and any
    /// cached name the FC did not report is dropped. A slot outside `0..count`
    /// (the `u16::MAX` a `PARAM_SET` echo carries) never counts, and a change of
    /// `count` starts the tally afresh, so a partial fetch never prunes.
    pub fn note_fetched(&mut self, name: &str, index: u16, count: u16) -> usize {
        if index >= count {
            return 0;
        }
        if self.fetch.count != count {
            self.fetch = Fetch {
                count,
                ..Fetch::default()
            };
        }
        if self.fetch.complete {
            return 0;
        }
        self.fetch.names.insert(index, name.to_string());
        if self.fetch.names.len() < usize::from(count) {
            return 0;
        }
        self.fetch.complete = true;
        let reported: HashSet<String> = std::mem::take(&mut self.fetch.names)
            .into_values()
            .collect();
        let before = self.params.len();
        self.params.retain(|name, _| reported.contains(name));
        let pruned = before - self.params.len();
        if pruned > 0 {
            tracing::info!(pruned, count, "param_cache_pruned_after_complete_fetch");
            self.generation += 1;
        }
        pruned
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

    /// Where the identity of the cache's flight controller persists: beside the
    /// cache, never inside it, since every reader of the cache treats each
    /// top-level key as a parameter name.
    pub fn fc_path(&self) -> PathBuf {
        self.path.with_extension("fc.json")
    }

    /// The recorded FC identity as the JSON bytes [`write_atomic`] persists at
    /// [`fc_path`](Self::fc_path), or `None` before any FC was adopted.
    pub fn serialize_fc(&self) -> Option<Vec<u8>> {
        let id = self.fc?;
        serde_json::to_vec(&json!({
            "system_id": id.system_id,
            "autopilot": id.autopilot,
            "vehicle_type": id.vehicle_type,
        }))
        .ok()
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

    /// Load the cache and its FC identity from disk. A missing file is not an
    /// error (returns an empty cache). Malformed entries are skipped.
    pub fn load(&mut self) -> std::io::Result<()> {
        self.fc = load_fc(&self.fc_path());
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

/// Read a persisted [`FcIdentity`]; absent or malformed reads as unknown.
fn load_fc(path: &Path) -> Option<FcIdentity> {
    let doc: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let field = |key: &str| {
        doc.get(key)
            .and_then(Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
    };
    Some(FcIdentity {
        system_id: field("system_id")?,
        autopilot: field("autopilot")?,
        vehicle_type: field("vehicle_type")?,
    })
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
