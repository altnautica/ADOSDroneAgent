//! The ground station's upstream key pair across a bind: saved before
//! `wfb_keygen` overwrites it, put back when the bind never reaches its key
//! apply.
//!
//! `wfb_keygen` writes the new pair straight over `/etc/gs.key` and
//! `/etc/drone.key`, because the upstream bind client sends the drone
//! `/etc/drone.key` from that fixed path. The station's radio key
//! (`rx.key`) only changes at the key apply, but `/etc/drone.key` is also the
//! shared secret the hop, presence and swarm keys derive from on both rigs. A
//! bind that minted a pair and then timed out, was cancelled or hit the
//! watchdog used to leave the fresh, never-delivered `drone.key` behind: the
//! station kept decoding its drone's video on the old `rx.key` while every
//! authenticated control frame between the two failed, and nothing short of a
//! re-pair recovered it. So the pair the station had is saved first, in
//! memory for this process and on disk for a supervisor killed or a rig
//! powered off mid-bind, and is put back on every exit that did not make the
//! new pair live.
//!
//! "Live" is read off the files, not the session's progress: the apply
//! writes the keygen's `gs.key` as the radio key, so a radio key equal to
//! `/etc/gs.key` means the new pair is in use and stays. Any other state, a
//! bind cut off before its apply or an apply that never landed, keeps the
//! station on the pair its radio key belongs to.

use serde_json::{json, Value};

use super::{BindError, BindOrchestrator};
use crate::bind::{BindRole, UPSTREAM_DRONE_KEY, UPSTREAM_GS_KEY, UPSTREAM_PAIR_BACKUP};

/// The two upstream key files as they stood before a bind's keygen; `None`
/// for a file that did not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpstreamPair {
    gs_key: Option<Vec<u8>>,
    drone_key: Option<Vec<u8>>,
}

impl UpstreamPair {
    /// Each upstream path with the content it held.
    fn files(&self) -> [(&'static str, Option<&[u8]>); 2] {
        [
            (UPSTREAM_GS_KEY, self.gs_key.as_deref()),
            (UPSTREAM_DRONE_KEY, self.drone_key.as_deref()),
        ]
    }

    /// The on-disk record.
    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let hex = |b: &Option<Vec<u8>>| b.as_ref().map(hex::encode);
        serde_json::to_vec(&json!({
            "gs_key": hex(&self.gs_key),
            "drone_key": hex(&self.drone_key),
        }))
        .unwrap_or_default()
    }

    /// Parse the on-disk record. `None` for anything that is not one: a
    /// record that cannot be read must never be written over real keys.
    pub(super) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let field = |name: &str| -> Option<Option<Vec<u8>>> {
            match v.get(name)? {
                Value::Null => Some(None),
                Value::String(s) => hex::decode(s).ok().map(Some),
                _ => None,
            }
        };
        Some(Self {
            gs_key: field("gs_key")?,
            drone_key: field("drone_key")?,
        })
    }
}

impl BindOrchestrator {
    /// Read one upstream file: `None` when absent, an error for anything else
    /// (an unreadable key cannot be saved, so the bind must not replace it).
    fn read_upstream(&self, path: &str) -> Result<Option<Vec<u8>>, BindError> {
        match self.host.read_file(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(BindError::new(format!(
                "cannot read {path} to save it before the bind: {e}"
            ))),
        }
    }

    /// Save the station's upstream pair before the keygen replaces it. A
    /// failure aborts the bind before anything is touched.
    pub(super) fn save_upstream_pair(&self) -> Result<(), BindError> {
        let pair = UpstreamPair {
            gs_key: self.read_upstream(UPSTREAM_GS_KEY)?,
            drone_key: self.read_upstream(UPSTREAM_DRONE_KEY)?,
        };
        self.host
            .write_key(UPSTREAM_PAIR_BACKUP, &pair.to_bytes())
            .map_err(|e| {
                BindError::new(format!(
                    "cannot save the upstream key pair before the bind: {e}"
                ))
            })?;
        *self.upstream_pair_slot() = Some(pair);
        Ok(())
    }

    /// Whether the keygen's pair is the one in use: the station's radio key
    /// is the `gs.key` on disk.
    fn upstream_pair_live(&self) -> bool {
        matches!(
            (
                self.host.read_file(BindRole::Gs.key_path()),
                self.host.read_file(UPSTREAM_GS_KEY),
            ),
            (Ok(radio), Ok(gs)) if radio == gs
        )
    }

    /// Close out this session's saved pair: put it back unless the new pair
    /// went live, then drop the record. A no-op for a session that saved
    /// nothing (a drone, or a station that failed before its keygen).
    pub(super) fn settle_upstream_pair(&self) {
        let Some(pair) = self.upstream_pair_slot().take() else {
            return;
        };
        if !self.upstream_pair_live() {
            self.restore_upstream_pair(&pair);
        }
        self.discard_upstream_backup();
    }

    /// At startup, after any committed apply is finished: settle the pair a
    /// ground-station bind saved on disk. Every session that ends drops the
    /// record, so one still there means the previous process died mid-bind
    /// (a kill, or a power loss that also took the `/run` sentinel with it).
    pub(super) fn recover_upstream_pair(&self) {
        let Ok(bytes) = self.host.read_file(UPSTREAM_PAIR_BACKUP) else {
            return;
        };
        if !self.upstream_pair_live() {
            match UpstreamPair::from_bytes(&bytes) {
                Some(pair) => self.restore_upstream_pair(&pair),
                None => tracing::warn!(
                    path = UPSTREAM_PAIR_BACKUP,
                    "bind_upstream_pair_unreadable: the minted pair stays"
                ),
            }
        }
        self.discard_upstream_backup();
    }

    /// Write back every upstream file that differs from the saved pair.
    fn restore_upstream_pair(&self, pair: &UpstreamPair) {
        let mut restored = Vec::new();
        for (path, saved) in pair.files() {
            if self.host.read_file(path).ok().as_deref() == saved {
                continue;
            }
            let result = match saved {
                Some(blob) => self.host.write_key(path, blob),
                None => self.host.remove_file(path),
            };
            match result {
                Ok(()) => restored.push(path),
                Err(e) => tracing::warn!(path, error = %e, "bind_upstream_key_restore_failed"),
            }
        }
        if !restored.is_empty() {
            tracing::info!(files = ?restored, "bind_upstream_pair_restored");
        }
    }

    fn discard_upstream_backup(&self) {
        if let Err(e) = self.host.remove_file(UPSTREAM_PAIR_BACKUP) {
            tracing::warn!(path = UPSTREAM_PAIR_BACKUP, error = %e, "bind_upstream_backup_discard_failed");
        }
    }

    fn upstream_pair_slot(&self) -> std::sync::MutexGuard<'_, Option<UpstreamPair>> {
        self.upstream_pair.lock().unwrap_or_else(|e| e.into_inner())
    }
}
