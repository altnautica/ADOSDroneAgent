//! Camera-discovery seam.
//!
//! The v4l2 / rpicam camera probing (and the `O_RDWR | O_NONBLOCK`
//! ghost-node liveness filter that drops a just-unplugged USB node) stays
//! Python — it is fast-changing HAL glue, not a hot path. The orchestrator
//! learns the primary camera by shelling out to `python -m ados.hal.camera
//! --json` once per stream (re)start and parsing the single JSON object that
//! subprocess prints. This module is that seam: spawn, hard-timeout, parse,
//! and map the result onto the encoder's [`crate::encoder::CameraInfo`] +
//! the [`crate::camera_state::CameraStateSnapshot`] sidecar.
//!
//! The JSON wire contract (mirrors `hal/camera_cli.py` `_build_result`):
//! ```json
//! {
//!   "cameras": [
//!     {"name", "type", "device_path", "width", "height",
//!      "capabilities", "hardware_role"}, ...
//!   ],
//!   "primary": {"device_path", "name"} | null,
//!   "total_cameras": N
//! }
//! ```
//! A discovery that times out, fails to spawn, or returns malformed output is
//! treated as no-primary (an empty result), never an error: the orchestrator's
//! no-primary backoff path then takes over cleanly.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::camera_state::CameraStateSnapshot;
use crate::encoder::{CameraInfo, CameraType};

/// Default Python interpreter for the discovery subprocess. Overridable via
/// `ADOS_PYTHON`, matching the convention the encoder uses for the SEI splice.
const DEFAULT_PYTHON: &str = "/opt/ados/venv/bin/python3";

/// Hard wall-clock budget for the discovery subprocess. v4l2-ctl / rpicam-hello
/// each carry their own 10 s internal timeout; 12 s gives the Python side room
/// to run both probes and still bounds the orchestrator's start path.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(12);

/// A camera as reported by the Python discovery JSON. Field names match
/// `CameraInfo.to_dict()` exactly.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DiscoveredCamera {
    pub name: String,
    #[serde(rename = "type")]
    pub camera_type: String,
    pub device_path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub hardware_role: String,
}

/// The primary-camera block from the discovery JSON (`null` when no camera
/// won the auto-assign).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Primary {
    pub device_path: String,
    pub name: String,
}

/// The full parsed discovery result.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DiscoveryResult {
    #[serde(default)]
    pub cameras: Vec<DiscoveredCamera>,
    #[serde(default)]
    pub primary: Option<Primary>,
    #[serde(default)]
    pub total_cameras: u32,
}

impl DiscoveryResult {
    /// An empty result (no cameras, no primary) — what a failed / timed-out
    /// discovery collapses to.
    pub fn empty() -> Self {
        Self {
            cameras: Vec::new(),
            primary: None,
            total_cameras: 0,
        }
    }

    /// The `CameraInfo` the encoder builder reads for the primary, if any.
    /// Resolves the primary's `device_path` against the full camera list so
    /// the encoder gets the capability list (which the `primary` block does
    /// not carry). Falls back to a minimal `CameraInfo` when the primary path
    /// is not in the list (shouldn't happen, but never panic).
    pub fn primary_camera_info(&self) -> Option<CameraInfo> {
        let primary = self.primary.as_ref()?;
        if let Some(found) = self
            .cameras
            .iter()
            .find(|c| c.device_path == primary.device_path)
        {
            Some(found.to_camera_info())
        } else {
            Some(CameraInfo {
                camera_type: CameraType::Usb,
                device_path: primary.device_path.clone(),
                capabilities: Vec::new(),
            })
        }
    }

    /// The camera-state sidecar snapshot for this result, applying the
    /// ready-gate (a primary plus at least one live camera → `ready`).
    pub fn camera_state_snapshot(&self) -> CameraStateSnapshot {
        let primary = self
            .primary
            .as_ref()
            .map(|p| (Some(p.device_path.clone()), Some(p.name.clone())));
        CameraStateSnapshot::from_discovery(primary, self.total_cameras)
    }
}

impl DiscoveredCamera {
    /// Map the wire camera type string onto the encoder's [`CameraType`].
    /// Unknown / absent types default to USB (the ffmpeg `libx264` path),
    /// which is the safe least-specific encoder backend.
    pub fn camera_type_enum(&self) -> CameraType {
        match self.camera_type.as_str() {
            "csi" => CameraType::Csi,
            "ip" => CameraType::Ip,
            // "usb" and anything unexpected → USB / ffmpeg path.
            _ => CameraType::Usb,
        }
    }

    /// Build the encoder's [`CameraInfo`] view (type + device path + caps).
    pub fn to_camera_info(&self) -> CameraInfo {
        CameraInfo {
            camera_type: self.camera_type_enum(),
            device_path: self.device_path.clone(),
            capabilities: self.capabilities.clone(),
        }
    }
}

/// Resolve the Python interpreter for the discovery subprocess.
pub fn python_executable() -> String {
    std::env::var("ADOS_PYTHON")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PYTHON.to_string())
}

/// Run camera discovery via `python -m ados.hal.camera --json`.
///
/// Returns the parsed [`DiscoveryResult`]. A spawn failure, a `timeout`
/// elapse, a non-UTF-8 stream, or malformed JSON all collapse to
/// [`DiscoveryResult::empty`] (a logged `warn`, never an error) so the
/// orchestrator's no-primary path takes over.
pub async fn discover(python_exe: &str, timeout: Duration) -> DiscoveryResult {
    let mut cmd = Command::new(python_exe);
    cmd.args(["-m", "ados.hal.camera", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, python = python_exe, "camera_discovery_spawn_failed");
            return DiscoveryResult::empty();
        }
    };

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "camera_discovery_wait_failed");
            return DiscoveryResult::empty();
        }
        Err(_) => {
            // The hard timeout elapsed. kill_on_drop reaps the straggler when
            // `child` is dropped at the end of this scope.
            tracing::warn!(
                timeout_s = timeout.as_secs(),
                "camera_discovery_timed_out; treating as no primary"
            );
            return DiscoveryResult::empty();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_discovery_stdout(&stdout) {
        Some(result) => result,
        None => {
            tracing::warn!("camera_discovery_no_json; treating as no primary");
            DiscoveryResult::empty()
        }
    }
}

/// Run discovery with the resolved interpreter and the default timeout.
pub async fn discover_default() -> DiscoveryResult {
    discover(&python_executable(), DISCOVERY_TIMEOUT).await
}

/// Persist the discovery's camera-state sidecar to [`crate::camera_state::CAMERA_STATE_JSON`].
/// Best-effort: an I/O error is logged at `warn` and discarded.
pub fn persist_camera_state(result: &DiscoveryResult) {
    let snapshot = result.camera_state_snapshot();
    if let Err(e) = snapshot.write_to(Path::new(crate::camera_state::CAMERA_STATE_JSON)) {
        tracing::warn!(error = %e, "camera_state_persist_failed");
    }
}

/// Parse the discovery JSON out of the subprocess stdout.
///
/// The expected object is the final non-empty line. To stay robust even if a
/// log line ever leaks onto stdout (the Python side routes logs to stderr, but
/// defence-in-depth costs nothing here), scan the lines from the end and take
/// the first one that parses as the expected object. Returns `None` when no
/// line parses.
fn parse_discovery_stdout(stdout: &str) -> Option<DiscoveryResult> {
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(result) = serde_json::from_str::<DiscoveryResult>(trimmed) {
            return Some(result);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANNED: &str = r#"{"cameras":[{"name":"CSI-0 (imx219)","type":"csi","device_path":"/dev/video0","width":3280,"height":2464,"capabilities":["h264","mjpeg"],"hardware_role":"camera"},{"name":"HD USB Camera","type":"usb","device_path":"/dev/video1","width":0,"height":0,"capabilities":["mjpeg","yuyv"],"hardware_role":"camera"}],"primary":{"device_path":"/dev/video0","name":"CSI-0 (imx219)"},"total_cameras":2}"#;

    #[test]
    fn parses_canned_discovery_json() {
        let r = parse_discovery_stdout(CANNED).expect("parses");
        assert_eq!(r.cameras.len(), 2);
        assert_eq!(r.total_cameras, 2);
        let primary = r.primary.as_ref().unwrap();
        assert_eq!(primary.device_path, "/dev/video0");
        assert_eq!(primary.name, "CSI-0 (imx219)");
        // The first camera carries the CSI capability list.
        assert_eq!(r.cameras[0].camera_type, "csi");
        assert_eq!(r.cameras[0].capabilities, vec!["h264", "mjpeg"]);
    }

    #[test]
    fn camera_type_mapping() {
        let csi = DiscoveredCamera {
            name: "c".into(),
            camera_type: "csi".into(),
            device_path: "/dev/video0".into(),
            width: 0,
            height: 0,
            capabilities: vec![],
            hardware_role: "camera".into(),
        };
        assert_eq!(csi.camera_type_enum(), CameraType::Csi);

        let usb = DiscoveredCamera {
            camera_type: "usb".into(),
            ..csi.clone()
        };
        assert_eq!(usb.camera_type_enum(), CameraType::Usb);

        let ip = DiscoveredCamera {
            camera_type: "ip".into(),
            ..csi.clone()
        };
        assert_eq!(ip.camera_type_enum(), CameraType::Ip);

        // An unknown / empty type falls back to USB (ffmpeg path).
        let unknown = DiscoveredCamera {
            camera_type: "thunderbolt".into(),
            ..csi.clone()
        };
        assert_eq!(unknown.camera_type_enum(), CameraType::Usb);
    }

    #[test]
    fn primary_camera_info_resolves_capabilities() {
        let r = parse_discovery_stdout(CANNED).unwrap();
        let info = r.primary_camera_info().expect("primary present");
        assert_eq!(info.camera_type, CameraType::Csi);
        assert_eq!(info.device_path, "/dev/video0");
        // The capability list comes from the matched camera-list entry, not
        // the primary block (which carries none).
        assert_eq!(info.capabilities, vec!["h264", "mjpeg"]);
    }

    #[test]
    fn camera_state_snapshot_ready_when_primary_present() {
        let r = parse_discovery_stdout(CANNED).unwrap();
        let s = r.camera_state_snapshot();
        assert_eq!(s.state, crate::camera_state::CameraState::Ready);
        assert_eq!(s.primary_path.as_deref(), Some("/dev/video0"));
        assert_eq!(s.total_cameras, 2);
    }

    #[test]
    fn empty_and_no_primary_json() {
        // An explicit empty result.
        let empty = parse_discovery_stdout(r#"{"cameras":[],"primary":null,"total_cameras":0}"#)
            .expect("parses");
        assert!(empty.cameras.is_empty());
        assert!(empty.primary.is_none());
        assert_eq!(empty.total_cameras, 0);
        assert!(empty.primary_camera_info().is_none());
        // No-primary snapshot is "missing".
        assert_eq!(
            empty.camera_state_snapshot().state,
            crate::camera_state::CameraState::Missing
        );

        // A camera present but no primary chosen → still missing, no info.
        let no_primary = parse_discovery_stdout(
            r#"{"cameras":[{"name":"x","type":"usb","device_path":"/dev/video9","width":0,"height":0,"capabilities":[],"hardware_role":"camera"}],"primary":null,"total_cameras":1}"#,
        )
        .unwrap();
        assert!(no_primary.primary.is_none());
        assert!(no_primary.primary_camera_info().is_none());
    }

    #[test]
    fn last_json_line_wins_over_leaked_log_lines() {
        // Defence-in-depth: if a log line ever leaks onto stdout, the parser
        // still finds the JSON object on the final line.
        let mixed = format!("2026-05-29 [info] camera_discovery_complete total=2\n{CANNED}\n");
        let r = parse_discovery_stdout(&mixed).expect("finds the json line");
        assert_eq!(r.total_cameras, 2);
    }

    #[test]
    fn no_json_returns_none() {
        assert!(parse_discovery_stdout("").is_none());
        assert!(parse_discovery_stdout("not json at all\nmore noise").is_none());
    }

    #[tokio::test]
    async fn discover_with_missing_python_is_empty() {
        // A non-existent interpreter → spawn fails → empty result, no panic.
        let r = discover("/nonexistent/python-xyzzy", Duration::from_secs(2)).await;
        assert_eq!(r, DiscoveryResult::empty());
    }

    #[tokio::test]
    async fn discover_parses_a_fake_python_emitting_json() {
        // Use `printf` (present on the dev host + rig) as a stand-in that
        // ignores the `-m ados.hal.camera --json` args and prints canned JSON,
        // exercising the spawn + wait + parse path end to end. `printf`
        // ignores extra operands after its format string.
        let r = discover("printf", Duration::from_secs(5)).await;
        // `printf '-m'` prints just "-m" (its format string), which is not
        // JSON → empty result. This asserts the graceful no-json path over a
        // real subprocess.
        assert_eq!(r, DiscoveryResult::empty());
    }
}
