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
    pub state: CameraState,
    pub primary_path: Option<String>,
    pub primary_name: Option<String>,
    pub total_cameras: u32,
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
            state,
            primary_path,
            primary_name,
            total_cameras,
            updated_at_unix: now_unix(),
        }
    }

    /// A discovery-failure snapshot (`state="error"`, no primary).
    pub fn error(total_cameras: u32) -> Self {
        Self {
            state: CameraState::Error,
            primary_path: None,
            primary_name: None,
            total_cameras,
            updated_at_unix: now_unix(),
        }
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
            "state",
            "primary_path",
            "primary_name",
            "total_cameras",
            "updated_at_unix",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["state"], "ready");
        assert_eq!(v["total_cameras"], 1);
        assert!(v["updated_at_unix"].as_f64().unwrap() > 0.0);
        // error state renders lowercase.
        assert_eq!(serde_json::to_value(CameraState::Error).unwrap(), "error");
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
