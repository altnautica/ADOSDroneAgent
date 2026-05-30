//! Persistent plugin install state.
//!
//! Records what is installed, what is enabled, and what permissions are granted
//! on this device. The controller reconciles against this state on boot and
//! after any lifecycle transition; the GCS reads it via the plugins REST API.
//!
//! State file at `/var/ados/state/plugin-state.json`, shape:
//!
//! ```json
//! {
//!   "schema": 1,
//!   "installs": [ { "plugin_id": "...", "version": "...", ... } ]
//! }
//! ```
//!
//! The write is atomic (`.tmp` + rename) and serialized under an advisory file
//! lock held on a sidecar `.lock` file so two concurrent install/remove flows
//! on the same host do not corrupt the file. The auto-update fields default
//! safely so older state files load clean.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::errors::LifecycleError;

/// Default state-file path.
pub const PLUGIN_STATE_PATH: &str = "/var/ados/state/plugin-state.json";

/// Plugin lifecycle status. Serializes lowercase to match the Python
/// `PluginStatus` literal set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginStatus {
    Installed,
    Enabled,
    Running,
    Disabled,
    Failed,
    Incompatible,
}

/// Where the plugin came from. Serializes snake_case to match the Python
/// `PluginSource` literal set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    LocalFile,
    GitUrl,
    Registry,
}

/// One permission grant record. `granted_at` / `revoked_at` are epoch
/// milliseconds (matching the Python `_now_ms`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub granted: bool,
    pub granted_at: Option<i64>,
    #[serde(default)]
    pub revoked_at: Option<i64>,
}

/// One install record. The four auto-update fields default safely so older
/// state files load without migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInstall {
    pub plugin_id: String,
    pub version: String,
    pub source: PluginSource,
    pub source_uri: Option<String>,
    pub signer_id: Option<String>,
    pub manifest_hash: String,
    pub status: PluginStatus,
    pub installed_at: i64,
    #[serde(default)]
    pub enabled_at: Option<i64>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub permissions: BTreeMap<String, PermissionGrant>,
    #[serde(default = "default_true")]
    pub auto_update: bool,
    #[serde(default)]
    pub pinned_version: Option<String>,
    #[serde(default)]
    pub last_update_check_at: Option<i64>,
    #[serde(default)]
    pub last_update_attempt: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

/// On-disk wrapper carrying the schema version.
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    schema: u32,
    #[serde(default)]
    installs: Vec<PluginInstall>,
}

/// Current epoch milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn state_path(path: Option<&Path>) -> PathBuf {
    path.map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_STATE_PATH))
}

/// Load the install list. A missing file returns an empty list; a malformed
/// file returns an empty list (mirroring the Python `load_state` tolerance).
/// Individual entries that fail to deserialize are skipped rather than
/// aborting the whole load.
pub fn load_state(path: Option<&Path>) -> Vec<PluginInstall> {
    let target = state_path(path);
    let raw = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // Parse the wrapper loosely so one bad entry does not lose the rest.
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "plugin_state_read_failed");
            return Vec::new();
        }
    };
    let serde_json::Value::Object(map) = &value else {
        return Vec::new();
    };
    let Some(serde_json::Value::Array(installs)) = map.get("installs") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(installs.len());
    for entry in installs {
        match serde_json::from_value::<PluginInstall>(entry.clone()) {
            Ok(install) => out.push(install),
            Err(e) => {
                tracing::warn!(error = %e, "plugin_state_entry_skipped");
            }
        }
    }
    out
}

/// Atomically persist the install list. Writes a sibling `.tmp` and renames it
/// over the target so a crash mid-write never leaves a truncated file.
pub fn save_state(installs: &[PluginInstall], path: Option<&Path>) -> Result<(), LifecycleError> {
    let target = state_path(path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = StateFile {
        schema: 1,
        installs: installs.to_vec(),
    };
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|e| LifecycleError::Io(std::io::Error::other(e)))?;
    let tmp = with_added_suffix(&target, ".tmp");
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

/// Append `.<suffix>` to a path's file name (so `state.json` -> `state.json.tmp`
/// and `state.json.lock`), matching Python's `Path.with_suffix(suffix + extra)`
/// behavior of *adding* a suffix to the full name rather than replacing it.
fn with_added_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Block-mode advisory file lock around a state read-modify-write.
///
/// The lock is held on the sibling `.lock` file (not on `state.json` itself) so
/// the atomic rename in [`save_state`] does not invalidate the lock fd. On
/// Linux this is a real `flock(LOCK_EX)` held by an owning [`nix::fcntl::Flock`]
/// guard that releases on drop; on a non-Linux dev host the guard holds nothing
/// (the controller logic still serializes within a single process via the
/// `&mut self` borrow), which keeps the pure-logic core testable off-target.
pub struct StateLock {
    #[cfg(target_os = "linux")]
    _flock: nix::fcntl::Flock<std::fs::File>,
}

impl StateLock {
    /// Acquire the lock for the state file at `path` (or the default path).
    pub fn acquire(path: Option<&Path>) -> Result<StateLock, LifecycleError> {
        let target = state_path(path);
        let lock_path = with_added_suffix(&target, ".lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // The lock file is a flock target only; never truncate it.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        #[cfg(target_os = "linux")]
        {
            let flock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
                .map_err(|(_, e)| LifecycleError::Io(std::io::Error::other(e)))?;
            Ok(StateLock { _flock: flock })
        }
        #[cfg(not(target_os = "linux"))]
        {
            // The lock file path now exists (matching Linux semantics); the
            // file handle drops here and the guard holds nothing.
            drop(file);
            Ok(StateLock {})
        }
    }
}

/// Find an install by id.
pub fn find_install<'a>(
    installs: &'a [PluginInstall],
    plugin_id: &str,
) -> Option<&'a PluginInstall> {
    installs.iter().find(|i| i.plugin_id == plugin_id)
}

/// Find a mutable install by id.
pub fn find_install_mut<'a>(
    installs: &'a mut [PluginInstall],
    plugin_id: &str,
) -> Option<&'a mut PluginInstall> {
    installs.iter_mut().find(|i| i.plugin_id == plugin_id)
}

/// Insert or replace an install by id, returning the new list.
pub fn upsert_install(
    mut installs: Vec<PluginInstall>,
    install: PluginInstall,
) -> Vec<PluginInstall> {
    installs.retain(|i| i.plugin_id != install.plugin_id);
    installs.push(install);
    installs
}

/// Remove an install by id, returning the new list.
pub fn remove_install(installs: Vec<PluginInstall>, plugin_id: &str) -> Vec<PluginInstall> {
    installs
        .into_iter()
        .filter(|i| i.plugin_id != plugin_id)
        .collect()
}

/// Grant a permission on an install (epoch-ms timestamp).
pub fn grant_permission(install: &mut PluginInstall, permission_id: &str) {
    install.permissions.insert(
        permission_id.to_string(),
        PermissionGrant {
            granted: true,
            granted_at: Some(now_ms()),
            revoked_at: None,
        },
    );
}

/// Revoke a granted permission. A permission that was never granted is left
/// untouched (matching the Python early-return).
pub fn revoke_permission(install: &mut PluginInstall, permission_id: &str) {
    if let Some(grant) = install.permissions.get_mut(permission_id) {
        grant.granted = false;
        grant.revoked_at = Some(now_ms());
    }
}

/// Whether a permission is currently granted.
pub fn is_permission_granted(install: &PluginInstall, permission_id: &str) -> bool {
    install
        .permissions
        .get(permission_id)
        .map(|g| g.granted)
        .unwrap_or(false)
}

/// The set of capability ids currently granted to an install. Capability ids
/// are the granted permission ids (the gate checks `token.granted_caps`
/// against the method's required capability). Mirrors the Python
/// `get_granted_caps`: the granted permission keys of the install record.
pub fn granted_caps(install: &PluginInstall) -> std::collections::BTreeSet<String> {
    install
        .permissions
        .iter()
        .filter(|(_, g)| g.granted)
        .map(|(k, _)| k.clone())
        .collect()
}

/// Drop any granted permission the manifest no longer declares.
///
/// **Security** — defends against a tampered state file granting permissions
/// the plugin never asked for. Called on every load so the in-memory
/// representation is always a subset of what the manifest authorizes.
pub fn filter_permissions_against_manifest(
    install: &mut PluginInstall,
    declared: &std::collections::BTreeSet<String>,
) {
    let bad: Vec<String> = install
        .permissions
        .keys()
        .filter(|k| !declared.contains(*k))
        .cloned()
        .collect();
    if bad.is_empty() {
        return;
    }
    tracing::warn!(
        plugin_id = %install.plugin_id,
        dropped = ?bad,
        "plugin_state_permission_filtered"
    );
    install.permissions.retain(|k, _| declared.contains(k));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn sample() -> PluginInstall {
        PluginInstall {
            plugin_id: "com.example.thermal".into(),
            version: "1.0.0".into(),
            source: PluginSource::LocalFile,
            source_uri: Some("/tmp/x.adosplug".into()),
            signer_id: Some("altnautica-2026-A".into()),
            manifest_hash: "abc".into(),
            status: PluginStatus::Installed,
            installed_at: 1_700_000_000_000,
            enabled_at: None,
            failure_reason: None,
            permissions: BTreeMap::new(),
            auto_update: true,
            pinned_version: None,
            last_update_check_at: None,
            last_update_attempt: None,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-state.json");
        let mut inst = sample();
        grant_permission(&mut inst, "hardware.spi");
        save_state(&[inst.clone()], Some(&path)).unwrap();

        let loaded = load_state(Some(&path));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].plugin_id, "com.example.thermal");
        assert!(is_permission_granted(&loaded[0], "hardware.spi"));
        // The on-disk file carries the schema wrapper.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"schema\": 1"), "{raw}");
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        assert!(load_state(Some(&path)).is_empty());
    }

    #[test]
    fn lock_acquires_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-state.json");
        // Acquire, drop, re-acquire: must not deadlock on a single thread.
        {
            let _g = StateLock::acquire(Some(&path)).unwrap();
        }
        let _g2 = StateLock::acquire(Some(&path)).unwrap();
        // The sidecar .lock file is created next to the state path.
        assert!(dir.path().join("plugin-state.json.lock").exists());
    }

    #[test]
    fn filter_drops_undeclared_permissions() {
        let mut inst = sample();
        grant_permission(&mut inst, "hardware.spi");
        grant_permission(&mut inst, "vehicle.command");
        let mut declared = BTreeSet::new();
        declared.insert("hardware.spi".to_string());
        filter_permissions_against_manifest(&mut inst, &declared);
        assert!(inst.permissions.contains_key("hardware.spi"));
        assert!(!inst.permissions.contains_key("vehicle.command"));
    }

    #[test]
    fn upsert_and_remove() {
        let installs = vec![sample()];
        let mut other = sample();
        other.plugin_id = "com.example.other".into();
        let installs = upsert_install(installs, other);
        assert_eq!(installs.len(), 2);
        // Upsert with the same id replaces rather than duplicates.
        let installs = upsert_install(installs, sample());
        assert_eq!(installs.len(), 2);
        let installs = remove_install(installs, "com.example.thermal");
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].plugin_id, "com.example.other");
    }

    #[test]
    fn revoke_keeps_record_but_flips_granted() {
        let mut inst = sample();
        grant_permission(&mut inst, "hardware.spi");
        revoke_permission(&mut inst, "hardware.spi");
        assert!(!is_permission_granted(&inst, "hardware.spi"));
        assert!(inst.permissions["hardware.spi"].revoked_at.is_some());
        // Revoking an ungranted permission is a no-op.
        revoke_permission(&mut inst, "never.granted");
        assert!(!inst.permissions.contains_key("never.granted"));
    }

    #[test]
    fn older_state_without_autoupdate_fields_loads() {
        // A v1-era entry that lacks the four auto-update fields must load with
        // safe defaults (auto_update true, the rest null).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-state.json");
        let legacy = r#"{
          "schema": 1,
          "installs": [
            {
              "plugin_id": "com.example.old",
              "version": "1.0.0",
              "source": "local_file",
              "source_uri": null,
              "signer_id": null,
              "manifest_hash": "h",
              "status": "installed",
              "installed_at": 1700000000000
            }
          ]
        }"#;
        std::fs::write(&path, legacy).unwrap();
        let loaded = load_state(Some(&path));
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].auto_update);
        assert!(loaded[0].pinned_version.is_none());
    }
}
