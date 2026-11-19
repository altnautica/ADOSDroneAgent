//! Refuse the uplink while this ground station stands by for a phone.
//!
//! While a phone holds the fleet's uplink, this box's two ground transmitters
//! are stopped (`ados-groundlink` owns that state and writes it into the radio
//! sidecar's `uplink` field). A GCS connected to this box could still send a
//! command; without this gate the bytes would be written to the stopped
//! transmitter's ingress and vanish, and the client would read a dead link as
//! a slow one. The gate refuses them instead, counted as `uplink_standby`, so
//! the box surface says why and never appears to command a drone it cannot
//! reach.
//!
//! A stale sidecar (the radio service stopped) blocks nothing: with no
//! current state the router keeps today's behaviour.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

/// How often the sidecar is re-read.
const REFRESH: Duration = Duration::from_secs(1);

/// A radio sidecar older than this is from a stopped service.
const STALE_AFTER: Duration = Duration::from_secs(10);

/// Whether the radio sidecar at `path` reports the box standing by.
pub(super) fn reads_standby(path: &Path, now: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let fresh = meta
        .modified()
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|age| age <= STALE_AFTER);
    if !fresh {
        return false;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|doc| {
            doc.get("uplink")
                .and_then(|u| u.get("state"))
                .and_then(Value::as_str)
                .map(|s| s == "standby")
        })
        .unwrap_or(false)
}

/// The gate: re-reads the sidecar at most once per [`REFRESH`].
pub(super) struct StandbyGate {
    path: PathBuf,
    standby: bool,
    read_at: Option<Instant>,
    refused: Arc<AtomicU64>,
}

impl StandbyGate {
    pub(super) fn new(path: PathBuf, refused: Arc<AtomicU64>) -> Self {
        Self {
            path,
            standby: false,
            read_at: None,
            refused,
        }
    }

    fn refresh(&mut self) {
        let now = Instant::now();
        if self
            .read_at
            .is_some_and(|at| now.duration_since(at) < REFRESH)
        {
            return;
        }
        self.read_at = Some(now);
        let next = reads_standby(&self.path, SystemTime::now());
        if next != self.standby {
            if next {
                tracing::warn!(
                    reason = "uplink_standby",
                    "aux_uplink_refusing: a phone holds the uplink"
                );
            } else {
                tracing::info!("aux_uplink_resumed: this ground station transmits again");
            }
            self.standby = next;
        }
    }

    /// True when `chunk` may go on the uplink; otherwise it is refused and its
    /// frames counted.
    pub(super) fn admit(&mut self, chunk: &[u8]) -> bool {
        self.refresh();
        if !self.standby {
            return true;
        }
        let frames = ados_protocol::aux_mux::split_frames(chunk).len().max(1);
        self.refused.fetch_add(frames as u64, Ordering::Relaxed);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, state: &str) -> PathBuf {
        let path = dir.join("wfb-stats.json");
        std::fs::write(
            &path,
            serde_json::json!({"state": "active", "uplink": {"state": state}}).to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn a_standby_box_refuses_and_counts_every_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "standby");
        let refused = Arc::new(AtomicU64::new(0));
        let mut gate = StandbyGate::new(path, refused.clone());
        assert!(!gate.admit(b"\xfd\x00"));
        assert!(!gate.admit(b"\xfd\x00"));
        assert_eq!(refused.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_transmitting_box_or_a_silent_radio_service_blocks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let refused = Arc::new(AtomicU64::new(0));
        let path = write(dir.path(), "transmitting");
        assert!(StandbyGate::new(path.clone(), refused.clone()).admit(b"x"));
        assert!(StandbyGate::new(dir.path().join("absent.json"), refused.clone()).admit(b"x"));
        // A standby sidecar left behind by a stopped service is not current.
        let stale = write(dir.path(), "standby");
        let later = SystemTime::now() + Duration::from_secs(30);
        assert!(!reads_standby(&stale, later));
        assert_eq!(refused.load(Ordering::Relaxed), 0);
    }
}
