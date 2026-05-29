//! On-disk sidecars under `/etc/ados`: the touch calibration file and the
//! ground-station input selection.
//!
//! `touch.calib` is owned by [`crate::affine`] (the matrix blob lives with the
//! math); this module re-exports its canonical path and provides the
//! `ground-station-input.json` `{primary}` persistence. Both writes are atomic
//! (tmp sibling + fsync + rename), modelled on `ados-video`'s
//! `camera_state.rs`, so a power loss mid-save never half-writes the file.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Canonical touch-calibration path (`core/paths.py` `TOUCH_CALIB_PATH`).
pub const TOUCH_CALIB_PATH: &str = "/etc/ados/touch.calib";

/// Canonical ground-station input selection path (`core/paths.py`
/// `GS_INPUT_JSON`).
pub const GS_INPUT_JSON: &str = "/etc/ados/ground-station-input.json";

/// The `ground-station-input.json` payload. A single `primary` field naming
/// the input device id the ground station treats as the primary controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundStationInput {
    /// Device id of the primary input, or `None` when unset.
    #[serde(default)]
    pub primary: Option<String>,
}

impl GroundStationInput {
    /// A selection naming `primary` as the primary input.
    pub fn new(primary: impl Into<String>) -> Self {
        Self {
            primary: Some(primary.into()),
        }
    }

    /// Read the selection from `path`. Returns `None` when the file is missing
    /// or malformed; the caller then has no pinned primary.
    pub fn load(path: &Path) -> Option<GroundStationInput> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Atomically persist the selection to `path` (tmp sibling + fsync +
    /// rename), creating the parent. Compact JSON separators.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        atomic_write(path, &body)
    }
}

/// Atomic tmp-sibling write shared by the sidecar persisters. The tmp name is
/// disambiguated by pid so two writers in the same directory never collide.
fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ados-sidecar");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-input.json");
        let sel = GroundStationInput::new("evdev-0");
        sel.save(&path).unwrap();

        let loaded = GroundStationInput::load(&path).unwrap();
        assert_eq!(loaded, sel);
        assert_eq!(loaded.primary.as_deref(), Some("evdev-0"));

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["primary"], "evdev-0");
        // Compact separators.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(", "));
        assert!(!text.contains(": "));
        // No stray tmp.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn unset_primary_serializes_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-input.json");
        let sel = GroundStationInput { primary: None };
        sel.save(&path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(v["primary"].is_null());
        assert_eq!(GroundStationInput::load(&path).unwrap(), sel);
    }

    #[test]
    fn load_missing_is_none() {
        assert!(
            GroundStationInput::load(Path::new("/nonexistent/ground-station-input.json")).is_none()
        );
    }

    #[test]
    fn load_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-input.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(GroundStationInput::load(&path).is_none());
    }

    #[test]
    fn path_constants_match_paths_py() {
        assert_eq!(TOUCH_CALIB_PATH, "/etc/ados/touch.calib");
        assert_eq!(GS_INPUT_JSON, "/etc/ados/ground-station-input.json");
    }
}
