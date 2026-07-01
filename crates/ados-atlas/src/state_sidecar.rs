//! The Atlas plugin-state sidecar.
//!
//! On every capture-state change, `ados-atlas` writes its own telemetry slice to
//! `/run/ados/plugins/atlas-state.json` (the universal plugin-state dir). The
//! cloud heartbeat producer then ferries it opaquely under `pluginState.atlas`,
//! and the on-box `/api/plugins/atlas/state` serves it locally — both from this
//! one file. Atlas owns this slice's shape end-to-end; the core never inspects
//! it. This is the plugin-owned replacement for the old per-feature heartbeat
//! columns.
//!
//! The capture fields are the capture service's own (state / session / keyframes
//! / cameras / VIO health). The three *transport* fields — the compute node, the
//! active bearer, and the last-forwarded-keyframe time — are known only by the
//! egress forwarder (`ados-cloud`), which writes them to the
//! [`ATLAS_FORWARD_SIDECAR`] handoff file; this writer folds a *fresh* handoff in
//! (a stale one, from a dead forwarder, is dropped so the Stream card never shows
//! a compute node that is no longer there — operating rule 44).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_protocol::atlas::{
    AtlasForwardStatus, CaptureState, CaptureStatus, VioHealth, ATLAS_FORWARD_SIDECAR,
};
use serde::Serialize;

/// Where the Atlas capture slice is written.
pub const ATLAS_STATE_SIDECAR: &str = "/run/ados/plugins/atlas-state.json";

/// A forwarder handoff not re-written within this window is treated as absent, so
/// a dead forwarder never keeps a stale compute node / bearer on the Stream card
/// (operating rule 44). Comfortably larger than the forwarder's refresh cadence.
const FORWARD_STALE: Duration = Duration::from_secs(15);

/// The Atlas telemetry slice — the camelCase shape the GCS Atlas plugin reads.
/// The capture fields are the capture service's own; the three transport fields
/// (compute node / bearer / last keyframe) are folded from the forwarder handoff
/// and omitted when there is no fresh handoff.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AtlasStateSlice<'a> {
    /// Local write time; the producer also mtime-gates the file, this is for the
    /// consumer's own freshness reasoning.
    generated_at_ms: i64,
    /// `CaptureState` serializes snake_case (idle / capturing / paused / …).
    state: &'a CaptureState,
    session_id: &'a str,
    keyframes_ingested: u64,
    ingest_rate_hz: f32,
    camera_count: u32,
    vio_health: &'a VioHealth,
    /// The paired compute node (mDNS `deviceId`), from the forwarder handoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_node_id: Option<&'a str>,
    /// The active transport bearer (`direct-lan` / `wfb-relay` / `cloud`), from
    /// the forwarder handoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer: Option<&'a str>,
    /// Epoch ms a keyframe was last forwarded, from the forwarder handoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_kf_at: Option<i64>,
}

fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Read the forwarder handoff at `path` if it exists AND was written within
/// [`FORWARD_STALE`] of `now`. A stale file (a dead forwarder whose tmpfs file
/// persists) is treated as absent so the Stream card never shows a compute node
/// that is gone. A future/unreadable mtime counts as fresh. Best-effort: any I/O
/// or parse error yields `None` (the transport fields are simply omitted).
fn read_fresh_forward_status(path: &Path, now: SystemTime) -> Option<AtlasForwardStatus> {
    let meta = std::fs::metadata(path).ok()?;
    if let Ok(mtime) = meta.modified() {
        if let Ok(age) = now.duration_since(mtime) {
            if age > FORWARD_STALE {
                return None;
            }
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<AtlasForwardStatus>(&text).ok()
}

/// Map the capture status to the slice and write the sidecar (atomic .tmp +
/// rename), folding in a fresh forwarder handoff. Best-effort: a write error is
/// logged, never fatal.
pub fn write_atlas_state_sidecar(status: &CaptureStatus) {
    let forward = read_fresh_forward_status(Path::new(ATLAS_FORWARD_SIDECAR), SystemTime::now());
    write_atlas_state_sidecar_to(Path::new(ATLAS_STATE_SIDECAR), status, forward.as_ref());
}

/// Write to an explicit path with an explicit (optional) forwarder handoff (for
/// tests and the production entry point above).
pub fn write_atlas_state_sidecar_to(
    path: &Path,
    status: &CaptureStatus,
    forward: Option<&AtlasForwardStatus>,
) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let slice = AtlasStateSlice {
        generated_at_ms: now_ms,
        state: &status.state,
        session_id: &status.session_id,
        keyframes_ingested: status.keyframes,
        ingest_rate_hz: status.ingest_rate_hz,
        camera_count: status.camera_count,
        vio_health: &status.vio_health,
        compute_node_id: forward.and_then(|f| f.compute_node_id.as_deref()),
        bearer: forward.and_then(|f| f.bearer.as_deref()),
        last_kf_at: forward.and_then(|f| f.last_kf_at_ms),
    };
    match serde_json::to_vec(&slice) {
        Ok(body) => {
            if let Err(e) = write_atomic(path, &body) {
                tracing::warn!(error = %e, "atlas_state_sidecar_write_failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "atlas_state_sidecar_encode_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> CaptureStatus {
        CaptureStatus {
            session_id: "sess-1".into(),
            state: CaptureState::Capturing,
            keyframes: 42,
            vio_health: VioHealth::Good,
            camera_count: 3,
            ingest_rate_hz: 9.5,
        }
    }

    #[test]
    fn the_slice_uses_the_camelcase_gcs_field_names() {
        let dir = std::env::temp_dir().join(format!("ados-atlas-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("atlas-state.json");
        write_atlas_state_sidecar_to(&path, &status(), None);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["state"], "capturing");
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["keyframesIngested"], 42);
        assert_eq!(v["ingestRateHz"], 9.5);
        assert_eq!(v["cameraCount"], 3);
        assert_eq!(v["vioHealth"], "good");
        assert!(v["generatedAtMs"].as_i64().is_some());
        // With no forwarder handoff, the transport fields are omitted (not null).
        assert!(v.get("computeNodeId").is_none());
        assert!(v.get("bearer").is_none());
        assert!(v.get("lastKfAt").is_none());
        assert!(!dir.join("atlas-state.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_forwarder_handoff_folds_the_transport_fields_into_the_slice() {
        let dir = std::env::temp_dir().join(format!("ados-atlas-fwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("atlas-state.json");
        let forward = AtlasForwardStatus {
            compute_node_id: Some("rtx-box".into()),
            bearer: Some("direct-lan".into()),
            last_kf_at_ms: Some(1_700),
            generated_at_ms: 1_699,
        };
        write_atlas_state_sidecar_to(&path, &status(), Some(&forward));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The camelCase keys the GCS mapAtlasSlice reads for the Stream card.
        assert_eq!(v["computeNodeId"], "rtx-box");
        assert_eq!(v["bearer"], "direct-lan");
        assert_eq!(v["lastKfAt"], 1_700);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_handoff_is_dropped_but_a_fresh_one_is_read() {
        let dir = std::env::temp_dir().join(format!("ados-atlas-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fwd_path = dir.join("atlas-forward.json");
        let forward = AtlasForwardStatus {
            compute_node_id: Some("rtx-box".into()),
            bearer: Some("wfb-relay".into()),
            last_kf_at_ms: Some(9),
            generated_at_ms: 9,
        };
        std::fs::write(&fwd_path, serde_json::to_vec(&forward).unwrap()).unwrap();

        // Fresh now → read back.
        let fresh = read_fresh_forward_status(&fwd_path, SystemTime::now());
        assert_eq!(
            fresh.as_ref().and_then(|f| f.bearer.as_deref()),
            Some("wfb-relay")
        );

        // A "now" far in the future makes the file older than the window → dropped.
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(read_fresh_forward_status(&fwd_path, future).is_none());

        // A missing file → None (no handoff yet).
        assert!(read_fresh_forward_status(&dir.join("nope.json"), SystemTime::now()).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
