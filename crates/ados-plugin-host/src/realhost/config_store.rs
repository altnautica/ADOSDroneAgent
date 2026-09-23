//! The per-scope plugin config store, its caps, and its owner-only on-disk
//! persistence.

use super::*;

// ---------------------------------------------------------------------
// Config store
// ---------------------------------------------------------------------

/// Per-scope config store with optional on-disk persistence. Reads consult drone scope
/// first, then global, then the request default. Absence is expressed as
/// `Option<Value>`: a stored `nil` is `Some(Value::Nil)` (a present value) and is
/// distinct from absent (`None`), so a key explicitly set to nil shadows global and
/// default rather than falling through to them.
///
/// When `persist_path` is set, every `set` hands back a [`ConfigSnapshot`] of the
/// whole store, which the host writes to a 0600 JSON file (atomic
/// temp-then-rename) on the blocking pool, and [`ConfigStore::load`] reads it
/// back at startup so plugin config survives a plugin-host restart. Without a
/// path the store is purely in-memory (the test/default posture).
#[derive(Default)]
pub(super) struct ConfigStore {
    pub(super) drone: BTreeMap<(String, String, String), Value>,
    pub(super) global: BTreeMap<(String, String), Value>,
    pub(super) persist_path: Option<PathBuf>,
    /// Bumped on every accepted write, so the persister can tell a stale
    /// snapshot from a newer one already on disk.
    pub(super) generation: u64,
}

/// The whole store, serialized, as of one accepted write. Written off the
/// runtime by the host; a snapshot older than one already written is skipped.
pub(super) struct ConfigSnapshot {
    pub(super) path: PathBuf,
    pub(super) generation: u64,
    pub(super) json: Vec<u8>,
}

/// One persisted config record. The `value` is the msgpack encoding of the
/// stored [`Value`], base64'd, so any rmpv value (nil, ints, maps, binary)
/// round-trips losslessly through JSON. `agent_id` is `None` for global scope.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct ConfigRecord {
    pub(super) plugin_id: String,
    pub(super) key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(super) agent_id: Option<String>,
    /// base64(msgpack(value)).
    pub(super) value: String,
}

pub(super) fn encode_value(value: &Value) -> Option<String> {
    let bytes = rmp_serde::to_vec(value).ok()?;
    use base64::Engine;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub(super) fn decode_value(encoded: &str) -> Option<Value> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    rmp_serde::from_slice(&bytes).ok()
}

/// Largest single config value a plugin may store (msgpack-encoded bytes). A
/// config value is a setting, not a blob store; every write rewrites the whole
/// persisted file, so an unbounded value is a memory and card-wear lever any
/// plugin holds without a grant.
pub const CONFIG_VALUE_MAX_BYTES: usize = 64 * 1024;

/// Largest total a plugin's config may hold across both scopes: every key plus
/// its encoded value.
pub const CONFIG_PLUGIN_MAX_BYTES: usize = 1024 * 1024;

/// Most keys a plugin's config may hold across both scopes. Bounds the
/// per-write accounting and the persisted record count, which the byte cap
/// alone does not when keys are tiny.
pub const CONFIG_PLUGIN_MAX_KEYS: usize = 256;

/// Largest persisted config file the host will load. A file past it was not
/// written under the caps above; loading it would put an attacker-sized
/// document in memory at every start, so the host starts empty instead.
pub const CONFIG_FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// The msgpack size of `value`. An unencodable value counts as unbounded so the
/// caps refuse it.
pub(super) fn encoded_len(value: &Value) -> usize {
    rmp_serde::to_vec(value).map_or(usize::MAX, |b| b.len())
}

impl ConfigStore {
    /// An in-memory store bound to a persistence path. Loads any existing
    /// records so prior plugin config survives a restart; a missing or
    /// unparseable file starts empty (config is best-effort durable, never a
    /// startup blocker).
    pub(super) fn load(path: PathBuf) -> Self {
        let mut store = ConfigStore {
            persist_path: Some(path.clone()),
            ..ConfigStore::default()
        };
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > CONFIG_FILE_MAX_BYTES => {
                tracing::error!(
                    path = %path.display(),
                    bytes = meta.len(),
                    "plugin config file over the size cap; starting empty"
                );
                return store;
            }
            _ => {}
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(records) = serde_json::from_str::<Vec<ConfigRecord>>(&text) {
                for r in records {
                    let Some(value) = decode_value(&r.value) else {
                        continue;
                    };
                    match r.agent_id {
                        Some(agent) => {
                            store.drone.insert((r.plugin_id, agent, r.key), value);
                        }
                        None => {
                            store.global.insert((r.plugin_id, r.key), value);
                        }
                    }
                }
            }
        }
        store
    }

    pub(super) fn get(&self, plugin_id: &str, key: &str, agent_id: &str, default: Value) -> Value {
        if !agent_id.is_empty() {
            if let Some(v) =
                self.drone
                    .get(&(plugin_id.to_string(), agent_id.to_string(), key.to_string()))
            {
                return v.clone();
            }
        }
        if let Some(v) = self.global.get(&(plugin_id.to_string(), key.to_string())) {
            return v.clone();
        }
        default
    }

    /// Every value `plugin_id` reads on the drone `agent_id`: each global key,
    /// with the drone's own keys over them, the precedence [`Self::get`] applies
    /// to one key.
    pub(super) fn effective(&self, plugin_id: &str, agent_id: &str) -> Vec<(String, Value)> {
        let mut out = std::collections::BTreeMap::new();
        for ((p, k), v) in &self.global {
            if p == plugin_id {
                out.insert(k.clone(), v.clone());
            }
        }
        if !agent_id.is_empty() {
            for ((p, a, k), v) in &self.drone {
                if p == plugin_id && a == agent_id {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        out.into_iter().collect()
    }

    /// Store `value`, refusing a value over [`CONFIG_VALUE_MAX_BYTES`] or a write
    /// that would take the plugin's whole store over [`CONFIG_PLUGIN_MAX_BYTES`].
    /// Sizes are the msgpack encoding, the form every read and persist handles.
    /// Returns the snapshot to persist when the store is bound to a file.
    pub(super) fn set(
        &mut self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
        agent_id: &str,
    ) -> Result<Option<ConfigSnapshot>, String> {
        // drone scope with no bound agent degrades to global, matching the
        // Python store. With a real agent-id lookup wired (build_host reads the
        // paired device id), a drone-scoped write now isolates per drone.
        let effective_scope = if scope == "drone" && agent_id.is_empty() {
            "global"
        } else {
            scope
        };
        let size = encoded_len(&value);
        if size > CONFIG_VALUE_MAX_BYTES {
            return Err(format!(
                "value for '{key}' is {size} bytes, over the {CONFIG_VALUE_MAX_BYTES}-byte limit"
            ));
        }
        let drone_key = (plugin_id.to_string(), agent_id.to_string(), key.to_string());
        let global_key = (plugin_id.to_string(), key.to_string());
        let replaced = if effective_scope == "drone" {
            self.drone.get(&drone_key)
        } else {
            self.global.get(&global_key)
        }
        .map(|old| key.len() + encoded_len(old));
        if replaced.is_none() && self.plugin_keys(plugin_id) >= CONFIG_PLUGIN_MAX_KEYS {
            return Err(format!(
                "config for {plugin_id} already holds {CONFIG_PLUGIN_MAX_KEYS} keys"
            ));
        }
        let replaced = replaced.unwrap_or(0);
        let total = self.plugin_bytes(plugin_id) - replaced + key.len() + size;
        if total > CONFIG_PLUGIN_MAX_BYTES {
            return Err(format!(
                "config for {plugin_id} would hold {total} bytes, over the \
                 {CONFIG_PLUGIN_MAX_BYTES}-byte limit"
            ));
        }
        if effective_scope == "drone" {
            self.drone.insert(drone_key, value);
        } else {
            self.global.insert(global_key, value);
        }
        Ok(self.snapshot())
    }

    /// Keys `plugin_id` holds across both scopes.
    pub(super) fn plugin_keys(&self, plugin_id: &str) -> usize {
        self.drone.keys().filter(|(p, _, _)| p == plugin_id).count()
            + self.global.keys().filter(|(p, _)| p == plugin_id).count()
    }

    /// Bytes `plugin_id` holds across both scopes: every key plus its encoded
    /// value.
    pub(super) fn plugin_bytes(&self, plugin_id: &str) -> usize {
        let drone = self
            .drone
            .iter()
            .filter(|((p, _, _), _)| p == plugin_id)
            .map(|((_, _, k), v)| k.len() + encoded_len(v));
        let global = self
            .global
            .iter()
            .filter(|((p, _), _)| p == plugin_id)
            .map(|((_, k), v)| k.len() + encoded_len(v));
        drone.chain(global).sum()
    }

    /// The whole store serialized for the persistence path, stamped with a fresh
    /// generation. `None` when no path is bound, or when serialization fails
    /// (logged; durability is best-effort and never fails a plugin's
    /// config.set).
    pub(super) fn snapshot(&mut self) -> Option<ConfigSnapshot> {
        let path = self.persist_path.clone()?;
        let mut records: Vec<ConfigRecord> = Vec::new();
        for ((plugin_id, agent_id, key), value) in &self.drone {
            if let Some(encoded) = encode_value(value) {
                records.push(ConfigRecord {
                    plugin_id: plugin_id.clone(),
                    key: key.clone(),
                    agent_id: Some(agent_id.clone()),
                    value: encoded,
                });
            }
        }
        for ((plugin_id, key), value) in &self.global {
            if let Some(encoded) = encode_value(value) {
                records.push(ConfigRecord {
                    plugin_id: plugin_id.clone(),
                    key: key.clone(),
                    agent_id: None,
                    value: encoded,
                });
            }
        }
        let json = match serde_json::to_vec(&records) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "plugin config serialize failed");
                return None;
            }
        };
        self.generation += 1;
        Some(ConfigSnapshot {
            path,
            generation: self.generation,
            json,
        })
    }
}

/// Write `json` to `path` owner-only (0600) via an atomic temp-then-rename,
/// enforcing the mode on every write (the open-time mode flag only applies on
/// creation, so a reused looser-perm inode would otherwise keep its mode).
/// Blocking; the host runs it on the blocking pool.
pub(super) fn write_json_owner_only(path: &std::path::Path, json: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    write_bytes_owner_only(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn write_bytes_owner_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn write_bytes_owner_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}
