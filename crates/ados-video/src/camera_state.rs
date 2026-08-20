//! `camera-state.json` sidecar (Contract E).
//!
//! The heartbeat builder in ados-supervisor / ados-api reads
//! `/run/ados/camera-state.json` to surface a "Camera Missing" pill on the GCS
//! drone card without reaching into the ados-video process. Ports
//! `VideoPipeline._persist_camera_state`: the ready-gate, the exact key set,
//! and the atomic tmp-sibling + rename. Consumers `json.loads` it, so the
//! contract is the field names / types / path (compact vs spaced whitespace is
//! irrelevant), matching how the bind sentinel is written.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Canonical sidecar path (`core/paths.py` `CAMERA_STATE_JSON`).
pub const CAMERA_STATE_JSON: &str = "/run/ados/camera-state.json";

/// Schema version of the `camera-state.json` sidecar. Bump on an incompatible
/// field-set change; a reader compares it best-effort via
/// `ados_protocol::sidecar::check_sidecar_version` and reads anyway on a
/// mismatch. Kept in step with the registry in `contracts.toml`.
///
/// v2 adds `pipeline_state`, `pipeline_reason` and `encoder`: discovery and
/// STREAMING are sequential but independent steps of one `start_stream`, and
/// discovery is persisted BEFORE any of the four `StartError` bails. Without
/// the pipeline outcome in the same sidecar, a node whose encoder is missing
/// still publishes a fresh, confident camera card with a model name, and the
/// GCS renders a detected camera as a streaming one.
pub const CAMERA_STATE_SIDECAR_VERSION: u16 = 2;

/// What the streaming pipeline did with the camera that was discovered.
/// `Unknown` is the honest answer before `start_stream` has resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineOutcome {
    Unknown,
    Starting,
    Streaming,
    Error,
    Stopped,
}

/// Discovery state. `error` is set by the caller on a discovery failure;
/// the ready-gate only ever produces `ready` or `missing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraState {
    Ready,
    Missing,
    Error,
}

/// The exact `camera-state.json` payload (key names + types match the Python
/// `_persist_camera_state` dict).
#[derive(Debug, Clone, Serialize)]
pub struct CameraStateSnapshot {
    /// Sidecar schema version (best-effort drift signal for readers).
    #[serde(default)]
    pub version: u16,
    pub state: CameraState,
    pub primary_path: Option<String>,
    pub primary_name: Option<String>,
    pub total_cameras: u32,
    /// What the STREAMING pipeline did with this camera. A detected camera is
    /// not a streaming camera; keeping the two in one sidecar is what stops a
    /// consumer rendering the first as the second.
    pub pipeline_state: PipelineOutcome,
    /// The `StartError` (or stop reason) behind `pipeline_state == Error`, in
    /// the operator's words. `None` on every other outcome.
    pub pipeline_reason: Option<String>,
    /// The encoder backend actually in use, read off the built command rather
    /// than the backend enum: `rpicam-vid`, or `<family>-<element>` for the
    /// ffmpeg / gstreamer families (`ffmpeg-h264_v4l2m2m`, `ffmpeg-libx264`,
    /// `gstreamer-mpph264enc`, `gstreamer-x264enc`, …), or `<family>-unknown`.
    /// `None` until one is selected. Restores the encoder-identity signal
    /// 0.99.359 dropped.
    pub encoder: Option<String>,
    /// Whether [`Self::encoder`] names a hardware encoder. A field rather than
    /// a string a consumer has to sniff, because "this node is on its software
    /// fallback" is the actual question — a CPU-bound libx264 at 720p30 is a
    /// thermal and latency problem the GCS should be able to show. `None`
    /// whenever `encoder` is `None`.
    pub encoder_hw: Option<bool>,
    pub updated_at_unix: f64,
}

impl CameraStateSnapshot {
    /// Apply the ready-gate: a stale primary can linger while the live camera
    /// count is zero (a just-unplugged node), so never advertise `ready`
    /// without at least one discovered camera. Mirrors the Python gate
    /// (`primary is not None and total > 0`).
    pub fn from_discovery(
        primary: Option<(Option<String>, Option<String>)>,
        total_cameras: u32,
    ) -> Self {
        let (state, primary_path, primary_name) = match primary {
            Some((path, name)) if total_cameras > 0 => (CameraState::Ready, path, name),
            _ => (CameraState::Missing, None, None),
        };
        Self {
            version: CAMERA_STATE_SIDECAR_VERSION,
            state,
            primary_path,
            primary_name,
            total_cameras,
            pipeline_state: PipelineOutcome::Unknown,
            pipeline_reason: None,
            encoder: None,
            encoder_hw: None,
            updated_at_unix: now_unix(),
        }
    }

    /// A discovery-failure snapshot (`state="error"`, no primary).
    pub fn error(total_cameras: u32) -> Self {
        Self {
            version: CAMERA_STATE_SIDECAR_VERSION,
            state: CameraState::Error,
            primary_path: None,
            primary_name: None,
            total_cameras,
            pipeline_state: PipelineOutcome::Error,
            pipeline_reason: Some("camera discovery failed".to_string()),
            encoder: None,
            encoder_hw: None,
            updated_at_unix: now_unix(),
        }
    }

    /// Stamp the pipeline's outcome onto this snapshot. The caller re-persists
    /// the sidecar at every `start_stream` exit — each `StartError` bail and the
    /// success path — and on every health tick, so the sidecar can never say
    /// "camera ready" about a pipeline that is in `Error`.
    pub fn with_pipeline(
        mut self,
        outcome: PipelineOutcome,
        reason: Option<String>,
        encoder: Option<String>,
        encoder_hw: Option<bool>,
    ) -> Self {
        self.pipeline_state = outcome;
        self.pipeline_reason = reason;
        // No encoder identity means no answer to the hardware question either;
        // a bare `false` there would read as "running the software fallback".
        self.encoder_hw = if encoder.is_some() { encoder_hw } else { None };
        self.encoder = encoder;
        self.updated_at_unix = now_unix();
        self
    }

    /// Atomically write the snapshot to `path` (tmp sibling + rename), creating
    /// the parent. Best-effort: an I/O error is returned for the caller to log
    /// and discard, never fatal.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Python uses `path.with_suffix(".tmp")` → replaces `.json` with `.tmp`.
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

    #[test]
    fn camera_state_sidecar_version_matches_registry() {
        // The per-file const and the sidecar registry are the two sources of
        // truth for this sidecar's schema version; a drift is caught here.
        assert_eq!(
            CAMERA_STATE_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("camera-state").unwrap()
        );
    }

    #[test]
    fn ready_gate_requires_a_primary_and_a_live_camera() {
        // Primary present + cameras → ready, carries path/name.
        let s = CameraStateSnapshot::from_discovery(
            Some((Some("/dev/video0".into()), Some("HZ USB Camera".into()))),
            1,
        );
        assert_eq!(s.state, CameraState::Ready);
        assert_eq!(s.primary_path.as_deref(), Some("/dev/video0"));
        assert_eq!(s.total_cameras, 1);

        // Stale primary but zero live cameras → missing, nulled out.
        let s = CameraStateSnapshot::from_discovery(
            Some((Some("/dev/video0".into()), Some("ghost".into()))),
            0,
        );
        assert_eq!(s.state, CameraState::Missing);
        assert!(s.primary_path.is_none());
        assert!(s.primary_name.is_none());

        // No primary → missing.
        let s = CameraStateSnapshot::from_discovery(None, 2);
        assert_eq!(s.state, CameraState::Missing);
    }

    #[test]
    fn json_shape_matches_python_keys() {
        let s = CameraStateSnapshot::from_discovery(Some((Some("/dev/video0".into()), None)), 1);
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        for k in [
            "version",
            "state",
            "primary_path",
            "primary_name",
            "total_cameras",
            "pipeline_state",
            "pipeline_reason",
            "encoder",
            "encoder_hw",
            "updated_at_unix",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        // The v2 key set is exactly those ten; a consumer keying off the
        // sidecar is entitled to the drift signal an extra key would be.
        assert_eq!(v.as_object().unwrap().len(), 10);
        assert_eq!(v["state"], "ready");
        assert_eq!(v["total_cameras"], 1);
        assert!(v["updated_at_unix"].as_f64().unwrap() > 0.0);
        // error state renders lowercase.
        assert_eq!(serde_json::to_value(CameraState::Error).unwrap(), "error");
    }

    #[test]
    fn encoder_hw_is_absent_whenever_the_encoder_identity_is() {
        // The hardware flag is only meaningful next to a label. A node that
        // never selected an encoder must not publish `encoder_hw: false`, which
        // reads as "running the software fallback" rather than "not streaming".
        let s = CameraStateSnapshot::from_discovery(Some((Some("/dev/video0".into()), None)), 1)
            .with_pipeline(
                PipelineOutcome::Error,
                Some("boom".into()),
                None,
                Some(true),
            );
        assert!(s.encoder.is_none());
        assert!(s.encoder_hw.is_none());

        let s = CameraStateSnapshot::from_discovery(Some((Some("/dev/video0".into()), None)), 1)
            .with_pipeline(
                PipelineOutcome::Streaming,
                None,
                Some("ffmpeg-libx264".into()),
                Some(false),
            );
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(v["pipeline_state"], "streaming");
        assert_eq!(v["encoder"], "ffmpeg-libx264");
        assert_eq!(v["encoder_hw"], false);
    }

    #[test]
    fn write_is_atomic_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera-state.json");
        let s = CameraStateSnapshot::from_discovery(Some((Some("/dev/video0".into()), None)), 1);
        s.write_to(&path).unwrap();
        let reloaded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded["state"], "ready");
        assert_eq!(reloaded["primary_path"], "/dev/video0");
        // No leftover tmp sibling (camera-state.tmp, matching with_suffix).
        assert!(!dir.path().join("camera-state.tmp").exists());
    }
}
