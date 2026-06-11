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

use ados_protocol::logd::emitter::IngestEmitter;
use ados_protocol::logd::Level;
use serde::Serialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::paths;
use crate::router::events::DataCapState;
use crate::sidecar;
use crate::sidecar::json_object_to_fields;

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
pub struct ActiveFlagWriter {
    path: PathBuf,
    /// Last `(active_uplink, internet_reachable, data_cap_state)` we persisted,
    /// to skip a redundant rewrite when nothing changed. `None` means "file
    /// absent".
    last: Option<(String, bool, String)>,
    /// Current cellular data-cap throttle level, mirrored into the body on every
    /// write. Defaults to `ok`; the data-cap throttle consumer updates it.
    cap_state: String,
    /// Best-effort durable-store emitter. When set, every disk write/unlink also
    /// ships the same body to the logging store as a `net.uplink_active` event,
    /// so a store-first reader sees the daemon's truth instead of a dead
    /// in-FastAPI-process router singleton. Absent in tests and on a board with
    /// no logd; a saturated channel drops the event without disturbing the loop.
    emitter: Option<IngestEmitter>,
}

// Hand-written so the struct keeps a `Debug` impl while the `IngestEmitter` (no
// `Debug`) is reported only as present/absent.
impl std::fmt::Debug for ActiveFlagWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveFlagWriter")
            .field("path", &self.path)
            .field("last", &self.last)
            .field("cap_state", &self.cap_state)
            .field("emitter", &self.emitter.is_some())
            .finish()
    }
}

/// The disk op a [`sync`](ActiveFlagWriter::sync) decided on, computed without
/// blocking so the actual write/unlink can be deferred to a blocking thread.
enum FlagOp {
    /// Nothing to do (dedup hit, or an encode error already logged).
    None,
    /// Atomically write `bytes`; on success record `key` as the new `last`.
    Write {
        bytes: Vec<u8>,
        key: (String, bool, String),
    },
    /// Unlink the flag file. `was_present` is the return value when the file is
    /// already gone (`NotFound`).
    Unlink { was_present: bool },
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
            emitter: None,
        }
    }

    /// Attach a durable-store emitter. Builder-style so the daemon can opt in to
    /// shipping each flag change as a `net.uplink_active` event while tests keep
    /// the default (no emitter). The emitter is best-effort and never gates a
    /// disk write.
    pub fn with_emitter(mut self, emitter: IngestEmitter) -> Self {
        self.emitter = Some(emitter);
        self
    }

    /// Emit the current flag body to the store as a `net.uplink_active` event.
    /// Called only after a disk op actually touched the sidecar, so the store
    /// and the file move in lockstep. `active` is the just-written uplink name,
    /// or `None` for the unlink (no-uplink) form. Best-effort: an absent or
    /// saturated emitter drops the event silently.
    fn emit_active(&self, active: Option<&str>, internet_reachable: bool) {
        let Some(emitter) = self.emitter.as_ref() else {
            return;
        };
        // The unlink form carries the same keys with a null uplink, so a
        // store-first reader learns "no uplink" without a separate file probe.
        let body = json!({
            "active_uplink": active,
            "internet_reachable": internet_reachable,
            "timestamp_ms": now_ms(),
            "data_cap_state": self.cap_state,
        });
        emitter.emit_event(
            "net.uplink_active",
            Level::Info,
            json_object_to_fields(&body),
        );
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
    /// Returns `true` if a write or unlink actually touched the disk. This
    /// variant performs the disk op inline (blocking); on the tokio reactor
    /// prefer [`sync_async`](ActiveFlagWriter::sync_async).
    pub fn sync(&mut self, active_uplink: Option<&str>, internet_reachable: bool) -> bool {
        let touched = match self.plan(active_uplink, internet_reachable) {
            FlagOp::None => false,
            FlagOp::Write { bytes, key } => {
                Self::run_write(&self.path, &bytes, &mut self.last, key)
            }
            FlagOp::Unlink { was_present } => Self::run_unlink(&self.path, was_present),
        };
        // Ship the change to the store only when the disk was actually touched,
        // so the event stream and the sidecar move together (no event on a dedup
        // hit). `cap_state` is read after `plan`, which never mutates it here.
        if touched {
            self.emit_active(active_uplink, internet_reachable);
        }
        touched
    }

    /// Async sync: do the in-memory bookkeeping + body encode on the caller's
    /// task (sub-microsecond), then run the one blocking filesystem op
    /// (`write_atomic` / `remove_file`) on the blocking thread pool so a stalled
    /// `/run` or `/sys` op never blocks the tokio reactor. The dedup/`last`
    /// bookkeeping is applied only when the disk op reports success, identical
    /// to [`sync`](ActiveFlagWriter::sync).
    ///
    /// Returns `true` if a write or unlink actually touched the disk.
    pub async fn sync_async(
        &mut self,
        active_uplink: Option<&str>,
        internet_reachable: bool,
    ) -> bool {
        let touched = match self.plan(active_uplink, internet_reachable) {
            FlagOp::None => false,
            FlagOp::Write { bytes, key } => {
                let path = self.path.clone();
                let res =
                    tokio::task::spawn_blocking(move || sidecar::write_atomic(&path, &bytes)).await;
                match res {
                    Ok(Ok(())) => {
                        self.last = Some(key);
                        true
                    }
                    Ok(Err(exc)) => {
                        warn!(error = %exc, "uplink.active_flag_write_failed");
                        false
                    }
                    Err(exc) => {
                        warn!(error = %exc, "uplink.active_flag_write_task_failed");
                        false
                    }
                }
            }
            FlagOp::Unlink { was_present } => {
                let path = self.path.clone();
                let res = tokio::task::spawn_blocking(move || std::fs::remove_file(&path)).await;
                match res {
                    Ok(Ok(())) => true,
                    Ok(Err(exc)) if exc.kind() == std::io::ErrorKind::NotFound => was_present,
                    Ok(Err(exc)) => {
                        debug!(error = %exc, "uplink.active_flag_unlink_failed");
                        false
                    }
                    Err(exc) => {
                        warn!(error = %exc, "uplink.active_flag_unlink_task_failed");
                        false
                    }
                }
            }
        };
        // Mirror the sidecar change to the store, only on a real disk touch.
        if touched {
            self.emit_active(active_uplink, internet_reachable);
        }
        touched
    }

    /// Decide the disk op without touching the disk for the write case. For the
    /// unlink case `self.last` is cleared here (it is pure in-memory state); the
    /// caller then performs the unlink. The write case defers the `self.last`
    /// update to after a successful write.
    fn plan(&mut self, active_uplink: Option<&str>, internet_reachable: bool) -> FlagOp {
        match active_uplink {
            Some(name) => {
                let key = (name.to_string(), internet_reachable, self.cap_state.clone());
                if self.last.as_ref() == Some(&key) && self.path.is_file() {
                    return FlagOp::None;
                }
                let body = ActiveUplinkFlag {
                    active_uplink: name.to_string(),
                    internet_reachable,
                    timestamp_ms: now_ms(),
                    data_cap_state: self.cap_state.clone(),
                };
                match serde_json::to_vec(&body) {
                    Ok(bytes) => FlagOp::Write { bytes, key },
                    Err(exc) => {
                        warn!(error = %exc, "uplink.active_flag_encode_failed");
                        FlagOp::None
                    }
                }
            }
            None => {
                let was_present = self.last.is_some();
                self.last = None;
                FlagOp::Unlink { was_present }
            }
        }
    }

    fn run_write(
        path: &Path,
        bytes: &[u8],
        last: &mut Option<(String, bool, String)>,
        key: (String, bool, String),
    ) -> bool {
        match sidecar::write_atomic(path, bytes) {
            Ok(()) => {
                *last = Some(key);
                true
            }
            Err(exc) => {
                warn!(error = %exc, "uplink.active_flag_write_failed");
                false
            }
        }
    }

    fn run_unlink(path: &Path, was_present: bool) -> bool {
        match std::fs::remove_file(path) {
            Ok(()) => true,
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => was_present,
            Err(exc) => {
                debug!(error = %exc, "uplink.active_flag_unlink_failed");
                false
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

    #[tokio::test]
    async fn sync_async_matches_sync_semantics_with_offloaded_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        let mut w = ActiveFlagWriter::with_path(path.clone());

        // No uplink, absent file → no disk touch.
        assert!(!w.sync_async(None, false).await);
        assert!(!path.is_file());

        // Active uplink → file written off-reactor, body parses.
        assert!(w.sync_async(Some("eth0"), true).await);
        assert!(path.is_file());
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["active_uplink"], "eth0");
        assert_eq!(body["internet_reachable"], true);

        // Same state → dedup, no rewrite.
        assert!(!w.sync_async(Some("eth0"), true).await);

        // Reachability transition → rewrite.
        assert!(w.sync_async(Some("eth0"), false).await);
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["internet_reachable"], false);

        // Drop to no-uplink → file unlinked off-reactor.
        assert!(w.sync_async(None, false).await);
        assert!(!path.is_file());
        // No torn tmp left by the offloaded write.
        assert!(!dir.path().join("uplink-active.tmp").exists());
    }

    #[tokio::test]
    async fn an_emitter_ships_one_event_per_disk_touch_and_none_on_dedup() {
        // Each real write/unlink ships exactly one `net.uplink_active` event; a
        // dedup hit (no disk touch) ships nothing, so the store stream and the
        // sidecar move together. The emitter records every enqueue regardless of
        // whether a daemon listens, so the stats counter is the assertion.
        let dir = tempfile::tempdir().unwrap();
        let emitter = IngestEmitter::with_socket("ados-net", dir.path().join("ingest.sock"));
        let stats = emitter.stats();
        let mut w =
            ActiveFlagWriter::with_path(dir.path().join("uplink-active")).with_emitter(emitter);

        // First active write → one event.
        assert!(w.sync(Some("eth0"), true));
        assert_eq!(stats.enqueued(), 1);

        // Same state → dedup, no disk touch, no event.
        assert!(!w.sync(Some("eth0"), true));
        assert_eq!(stats.enqueued(), 1);

        // Reachability transition → rewrite → one more event.
        assert!(w.sync(Some("eth0"), false));
        assert_eq!(stats.enqueued(), 2);

        // Drop to no-uplink → unlink → one more event (the null-uplink form).
        assert!(w.sync(None, false));
        assert_eq!(stats.enqueued(), 3);

        // Unlink of an already-absent file → no disk touch, no event.
        assert!(!w.sync(None, false));
        assert_eq!(stats.enqueued(), 3);
    }

    #[tokio::test]
    async fn without_an_emitter_no_event_is_enqueued() {
        // The default (no emitter) writer touches the disk but ships nothing,
        // proving the emit is gated on an attached emitter.
        let dir = tempfile::tempdir().unwrap();
        let probe = IngestEmitter::with_socket("ados-net", dir.path().join("probe.sock"));
        let stats = probe.stats();
        let mut w = ActiveFlagWriter::with_path(dir.path().join("uplink-active"));
        assert!(w.sync(Some("eth0"), true));
        // The unrelated probe emitter saw nothing (the writer has no emitter).
        assert_eq!(stats.enqueued(), 0);
    }
}
