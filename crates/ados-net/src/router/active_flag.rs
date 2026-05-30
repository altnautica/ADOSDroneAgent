//! The active-uplink sentinel (`/run/ados/uplink-active`) — the headline fix.
//!
//! The mesh gateway-election path reads this file by `.is_file()` to decide
//! whether a node can advertise itself as a cloud gateway (`mesh_manager`'s
//! `has_uplink = UPLINK_ACTIVE_FLAG.is_file()`). In the all-Python agent
//! *nothing wrote it*, so `has_uplink` was always `False` and a perfectly good
//! ground node never offered its uplink to the mesh. This module is the missing
//! writer.
//!
//! Contract:
//!
//! * Presence is the legacy signal: the file exists iff there is an active
//!   uplink the router has selected. When the router has no viable uplink the
//!   file is **unlinked**, so the legacy `.is_file()` reader sees no uplink.
//! * The body is a richer JSON snapshot for consumers that parse it
//!   (`active_uplink`, `internet_reachable`, `timestamp_ms`). It is written
//!   atomically (tmp sibling + rename) so a reader never sees a torn file.
//! * The writer fires on every active-uplink change and on every
//!   internet-reachable transition; the FSM calls [`ActiveFlagWriter::sync`]
//!   with the current state and this module decides write-vs-unlink + dedup.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::{debug, warn};

use crate::paths;
use crate::router::events::DataCapState;
use crate::sidecar;

/// The JSON body written to `/run/ados/uplink-active`.
///
/// The first three fields are the legacy contract that existing readers parse;
/// `data_cap_state` is an additive field, always emitted, so a subscriber can
/// learn the cellular throttle level (`ok`/`warn_80`/`throttle_95`/`blocked_100`)
/// without a separate file. Legacy readers ignore the unknown key, so the body
/// stays compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveUplinkFlag {
    /// The router's currently-selected uplink (`Some` whenever the file
    /// exists; the file is absent when this would be `None`).
    pub active_uplink: String,
    /// Whether the cloud-reachability probe last succeeded on it.
    pub internet_reachable: bool,
    /// Wall-clock write time in milliseconds.
    pub timestamp_ms: u64,
    /// Current cellular data-cap throttle level. `ok` until a data-cap
    /// threshold consumer pushes a higher level.
    pub data_cap_state: String,
}

/// Stateful writer that dedups identical syncs and owns the write-vs-unlink
/// decision. The router holds one of these and calls [`sync`] from the FSM.
///
/// [`sync`]: ActiveFlagWriter::sync
#[derive(Debug)]
pub struct ActiveFlagWriter {
    path: PathBuf,
    /// Last `(active_uplink, internet_reachable, data_cap_state)` we persisted,
    /// to skip a redundant rewrite when nothing changed. `None` means "file
    /// absent".
    last: Option<(String, bool, String)>,
    /// Current cellular data-cap throttle level, mirrored into the body on every
    /// write. Defaults to `ok`; the data-cap throttle consumer updates it.
    cap_state: String,
}

impl ActiveFlagWriter {
    /// Writer targeting the canonical `UPLINK_ACTIVE_FLAG` path.
    pub fn new() -> Self {
        Self::with_path(paths::uplink_active_flag().to_path_buf())
    }

    /// Writer targeting an explicit path (tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            last: None,
            cap_state: DataCapState::Ok.as_str().to_string(),
        }
    }

    /// Update the data-cap throttle level reflected in the body. Returns `true`
    /// when the level changed (the caller can then re-`sync` to persist it).
    /// The change does not write on its own; the next `sync` carries it.
    pub fn set_data_cap_state(&mut self, state: DataCapState) -> bool {
        let next = state.as_str().to_string();
        if self.cap_state == next {
            return false;
        }
        self.cap_state = next;
        true
    }

    /// The current data-cap throttle level string.
    pub fn data_cap_state(&self) -> &str {
        &self.cap_state
    }

    /// Reconcile the on-disk flag to the router's current state.
    ///
    /// * `active_uplink = Some(name)` → ensure the file exists with the current
    ///   body. Dedups when `(name, internet_reachable)` is unchanged.
    /// * `active_uplink = None` → unlink the file so the legacy `.is_file()`
    ///   reader sees no uplink.
    ///
    /// Returns `true` if a write or unlink actually touched the disk.
    pub fn sync(&mut self, active_uplink: Option<&str>, internet_reachable: bool) -> bool {
        match active_uplink {
            Some(name) => {
                let key = (name.to_string(), internet_reachable, self.cap_state.clone());
                if self.last.as_ref() == Some(&key) && self.path.is_file() {
                    return false;
                }
                let body = ActiveUplinkFlag {
                    active_uplink: name.to_string(),
                    internet_reachable,
                    timestamp_ms: now_ms(),
                    data_cap_state: self.cap_state.clone(),
                };
                match serde_json::to_vec(&body) {
                    Ok(bytes) => match sidecar::write_atomic(&self.path, &bytes) {
                        Ok(()) => {
                            self.last = Some(key);
                            true
                        }
                        Err(exc) => {
                            warn!(error = %exc, "uplink.active_flag_write_failed");
                            false
                        }
                    },
                    Err(exc) => {
                        warn!(error = %exc, "uplink.active_flag_encode_failed");
                        false
                    }
                }
            }
            None => {
                let was_present = self.last.is_some();
                self.last = None;
                match std::fs::remove_file(&self.path) {
                    Ok(()) => true,
                    Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => was_present,
                    Err(exc) => {
                        debug!(error = %exc, "uplink.active_flag_unlink_failed");
                        false
                    }
                }
            }
        }
    }

    /// The path this writer targets.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for ActiveFlagWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_exists_iff_active_uplink_is_some() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        let mut w = ActiveFlagWriter::with_path(path.clone());

        // No uplink → no file (unlink of an absent file is a no-op write here).
        assert!(!w.sync(None, false));
        assert!(!path.is_file());

        // Active uplink → file present, body parses, presence is the signal.
        assert!(w.sync(Some("eth0"), true));
        assert!(path.is_file());
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["active_uplink"], "eth0");
        assert_eq!(body["internet_reachable"], true);
        assert!(body["timestamp_ms"].as_u64().unwrap() > 0);

        // Same state again → dedup, no disk touch, file still there.
        assert!(!w.sync(Some("eth0"), true));
        assert!(path.is_file());

        // internet_reachable transition → rewrite.
        assert!(w.sync(Some("eth0"), false));
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["internet_reachable"], false);

        // active-uplink change → rewrite with new name.
        assert!(w.sync(Some("wlan0_client"), true));
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["active_uplink"], "wlan0_client");

        // Drop to no-uplink → file unlinked, legacy reader sees no uplink.
        assert!(w.sync(None, false));
        assert!(!path.is_file());
    }

    #[test]
    fn data_cap_state_is_additive_and_defaults_to_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        let mut w = ActiveFlagWriter::with_path(path.clone());

        // Default body carries data_cap_state: "ok".
        assert!(w.sync(Some("eth0"), true));
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["data_cap_state"], "ok");
        // Legacy keys still present and parseable by an old reader.
        assert_eq!(body["active_uplink"], "eth0");
        assert_eq!(body["internet_reachable"], true);
        assert!(body["timestamp_ms"].as_u64().unwrap() > 0);

        // A cap-state change is staged, then carried by the next sync.
        assert!(w.set_data_cap_state(DataCapState::Throttle95));
        assert!(!w.set_data_cap_state(DataCapState::Throttle95)); // no-op repeat.
        assert!(w.sync(Some("eth0"), true)); // same uplink+reachable, but cap changed → rewrite.
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["data_cap_state"], "throttle_95");

        // No change → dedup, no rewrite.
        assert!(!w.sync(Some("eth0"), true));

        // Escalate to blocked_100.
        assert!(w.set_data_cap_state(DataCapState::Blocked100));
        assert!(w.sync(Some("eth0"), true));
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["data_cap_state"], "blocked_100");
    }

    #[test]
    fn no_torn_tmp_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        let mut w = ActiveFlagWriter::with_path(path.clone());
        w.sync(Some("eth0"), true);
        assert!(!dir.path().join("uplink-active.tmp").exists());
    }
}
