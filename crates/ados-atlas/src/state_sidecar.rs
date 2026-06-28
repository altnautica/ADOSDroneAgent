//! The Atlas plugin-state sidecar.
//!
//! On every capture-state change, `ados-atlas` writes its own telemetry slice to
//! `/run/ados/plugins/atlas-state.json` (the universal plugin-state dir). The
//! cloud heartbeat producer then ferries it opaquely under `pluginState.atlas`,
//! and the on-box `/api/plugins/atlas/state` serves it locally — both from this
//! one file. Atlas owns this slice's shape end-to-end; the core never inspects
//! it. This is the plugin-owned replacement for the old per-feature heartbeat
//! columns.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ados_protocol::atlas::{CaptureState, CaptureStatus, VioHealth};
use serde::Serialize;

/// Where the Atlas capture slice is written.
pub const ATLAS_STATE_SIDECAR: &str = "/run/ados/plugins/atlas-state.json";

/// The Atlas telemetry slice — the camelCase shape the GCS Atlas plugin reads
/// (the drone-side capture fields; reconstruction fields like the gaussian count
/// come from the compute node's own slice).
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
}

fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Map the capture status to the slice and write the sidecar (atomic .tmp +
/// rename). Best-effort: a write error is logged, never fatal.
pub fn write_atlas_state_sidecar(status: &CaptureStatus) {
    write_atlas_state_sidecar_to(Path::new(ATLAS_STATE_SIDECAR), status);
}

/// Write to an explicit path (for tests).
pub fn write_atlas_state_sidecar_to(path: &Path, status: &CaptureStatus) {
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
        write_atlas_state_sidecar_to(&path, &status());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["state"], "capturing");
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["keyframesIngested"], 42);
        assert_eq!(v["ingestRateHz"], 9.5);
        assert_eq!(v["cameraCount"], 3);
        assert_eq!(v["vioHealth"], "good");
        assert!(v["generatedAtMs"].as_i64().is_some());
        assert!(!dir.join("atlas-state.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
