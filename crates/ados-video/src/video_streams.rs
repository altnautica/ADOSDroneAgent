//! `video-streams.json` sidecar.
//!
//! When a node serves more than one video leg (a smart pod, a dual-camera rig),
//! the orchestrator writes the resolved leg list here so the out-of-process
//! status surfaces (ados-control `/api/status/full`, the ados-cloud heartbeat,
//! and the Python heartbeat) can advertise each leg's `id`/`role`/`codec` — and
//! the GCS can populate its stream switcher + connect to `:8889/<id>/whep` per
//! leg — without reaching into the ados-video process. Mirrors
//! [`crate::camera_state`]: the ready-gate is the caller's, the atomic
//! tmp-sibling + rename is here, consumers `json.loads` it (only the field
//! names / types / path are the contract).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::ResolvedLeg;

/// Canonical sidecar path.
pub const VIDEO_STREAMS_JSON: &str = "/run/ados/video-streams.json";

/// Schema version of the `video-streams.json` sidecar. Kept in step with the
/// registry in `contracts.toml`; readers compare best-effort and read anyway.
pub const VIDEO_STREAMS_SIDECAR_VERSION: u16 = 1;

/// One advertised video leg — the stable identity a viewer connects to at
/// `:8889/<id>/whep`. Only the display/identity fields cross the process
/// boundary (the source URL stays private to the pipeline).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VideoStreamEntry {
    pub id: String,
    pub role: String,
    pub codec: String,
}

/// The exact `video-streams.json` payload.
#[derive(Debug, Clone, Serialize)]
pub struct VideoStreamsSnapshot {
    pub version: u16,
    pub updated_at_unix: f64,
    pub streams: Vec<VideoStreamEntry>,
}

impl VideoStreamsSnapshot {
    /// Build the snapshot from the orchestrator's resolved leg list. The source
    /// URL is intentionally dropped — a viewer keys on `id` (the mediamtx path)
    /// and the surfaces synthesize the WHEP URL from `id`.
    pub fn from_legs(legs: &[ResolvedLeg]) -> Self {
        Self {
            version: VIDEO_STREAMS_SIDECAR_VERSION,
            updated_at_unix: now_unix(),
            streams: legs
                .iter()
                .map(|l| VideoStreamEntry {
                    id: l.id.clone(),
                    role: l.role.clone(),
                    codec: l.codec.clone(),
                })
                .collect(),
        }
    }

    /// Atomically write the snapshot to `path` (tmp sibling + rename), creating
    /// the parent. Best-effort: an I/O error is returned for the caller to log
    /// and discard, never fatal. Mirrors [`crate::camera_state::CameraStateSnapshot::write_to`].
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o644)
                .open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(id: &str, role: &str, codec: &str) -> ResolvedLeg {
        ResolvedLeg {
            id: id.into(),
            source: "rtsp://x/y".into(),
            role: role.into(),
            codec: codec.into(),
            is_primary: id == "main",
            is_network_pull: id != "main",
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
        }
    }

    #[test]
    fn video_streams_sidecar_version_matches_registry() {
        assert_eq!(
            VIDEO_STREAMS_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("video-streams").unwrap()
        );
    }

    #[test]
    fn snapshot_carries_id_role_codec_per_leg_and_drops_source() {
        let s =
            VideoStreamsSnapshot::from_legs(&[leg("main", "eo", "h265"), leg("ir", "ir", "h264")]);
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(v["version"], 1);
        assert!(v["updated_at_unix"].as_f64().unwrap() > 0.0);
        let streams = v["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["id"], "main");
        assert_eq!(streams[0]["role"], "eo");
        assert_eq!(streams[0]["codec"], "h265");
        assert_eq!(streams[1]["id"], "ir");
        // The private source URL never crosses the boundary.
        assert!(streams[0].get("source").is_none());
    }

    #[test]
    fn write_is_atomic_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video-streams.json");
        let s = VideoStreamsSnapshot::from_legs(&[leg("main", "eo", "h264")]);
        s.write_to(&path).unwrap();
        let reloaded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded["streams"][0]["id"], "main");
        assert!(!dir.path().join("video-streams.tmp").exists());
    }
}
