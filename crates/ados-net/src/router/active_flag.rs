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
use crate::sidecar;

/// The JSON body written to `/run/ados/uplink-active`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveUplinkFlag {
    /// The router's currently-selected uplink (`Some` whenever the file
    /// exists; the file is absent when this would be `None`).
    pub active_uplink: String,
    /// Whether the cloud-reachability probe last succeeded on it.
    pub internet_reachable: bool,
    /// Wall-clock write time in milliseconds.
    pub timestamp_ms: u64,
}

/// Stateful writer that dedups identical syncs and owns the write-vs-unlink
/// decision. The router holds one of these and calls [`sync`] from the FSM.
///
/// [`sync`]: ActiveFlagWriter::sync
#[derive(Debug)]
pub struct ActiveFlagWriter {
    path: PathBuf,
    /// Last `(active_uplink, internet_reachable)` we persisted, to skip a
    /// redundant rewrite when neither changed. `None` means "file absent".
    last: Option<(String, bool)>,
}

impl ActiveFlagWriter {
    /// Writer targeting the canonical `UPLINK_ACTIVE_FLAG` path.
    pub fn new() -> Self {
        Self::with_path(paths::uplink_active_flag().to_path_buf())
    }

    /// Writer targeting an explicit path (tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self { path, last: None }
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
                let key = (name.to_string(), internet_reachable);
                if self.last.as_ref() == Some(&key) && self.path.is_file() {
                    return false;
                }
                let body = ActiveUplinkFlag {
                    active_uplink: name.to_string(),
                    internet_reachable,
                    timestamp_ms: now_ms(),
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
    fn no_torn_tmp_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        let mut w = ActiveFlagWriter::with_path(path.clone());
        w.sync(Some("eth0"), true);
        assert!(!dir.path().join("uplink-active.tmp").exists());
    }
}
