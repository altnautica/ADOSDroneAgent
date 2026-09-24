//! `node.info`: the node facts a plugin may read, served from the sources the
//! agent's own services decide on.
//!
//! A sandboxed plugin cannot read these sources itself: the run directory is an
//! empty tmpfs in its mount namespace and the config file is root-only. Each
//! fact is read through the reader its owning crate publishes, so the plugin
//! sees exactly what the agent acts on:
//!
//! * the profile through [`ados_config::node_profile_at`], the resolution every
//!   daemon shares;
//! * the board through the HAL board sidecar's own reader, with NPU presence
//!   decided by `npu_tops > 0` (the rule the offload decision and the status
//!   surfaces key on);
//! * the ground-station role through [`ados_config::ground_station_role`];
//! * camera readiness through the camera-state sidecar's shared
//!   main-stream reader: the pipeline is publishing `main` right now, stamped
//!   within the live window the video orchestrator re-stamps within;
//! * the main stream's geometry through the video config's own primary-leg
//!   resolution, the settings the primary encoder actually runs.
//!
//! An absent source is `null` on the wire, never a guess.

use std::path::Path;

use ados_hal_probe::board_sidecar::{read_sidecar, BoardFingerprint};
use ados_protocol::node_info::{
    BoardInfo, CameraInfo, GroundStationInfo, NodeInfo, StreamGeometry,
};
use ados_video::camera_state::{read_main_stream_live, CAMERA_STATE_LIVE_S};
use ados_video::config::RosterVideoConfig;

use super::*;

/// Where `node.info` reads each fact from.
#[derive(Debug, Clone)]
pub struct NodeInfoSources {
    /// The agent config (`video` block, `agent.profile`).
    pub config_yaml: PathBuf,
    /// The installer's profile sentinel.
    pub profile_conf: PathBuf,
    /// The ground-station role sentinel.
    pub mesh_role: PathBuf,
    /// The HAL board sidecar.
    pub board_sidecar: PathBuf,
    /// The video pipeline's camera-state sidecar.
    pub camera_state: PathBuf,
}

impl NodeInfoSources {
    /// The production sources, honouring the overrides every daemon honours:
    /// `ADOS_CONFIG`, `ADOS_PROFILE_CONF`, `ADOS_MESH_ROLE` and `ADOS_RUN_DIR`.
    pub fn from_env() -> Self {
        let config_yaml = std::env::var_os("ADOS_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(ados_config::log_store::CONFIG_YAML));
        let run_dir = std::env::var_os("ADOS_RUN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/ados"));
        Self {
            config_yaml,
            profile_conf: ados_config::profile_conf_path(),
            mesh_role: ados_config::mesh_role_path(),
            board_sidecar: ados_hal_probe::board_sidecar::sidecar_path(),
            camera_state: run_dir.join("camera-state.json"),
        }
    }

    /// Read every fact, judging the camera sidecar's freshness against
    /// `now_unix` (wall-clock seconds).
    pub fn read(&self, now_unix: f64) -> NodeInfo {
        let profile = ados_config::node_profile_at(&self.config_yaml, &self.profile_conf);
        let ready = read_main_stream_live(&self.camera_state, now_unix, CAMERA_STATE_LIVE_S);
        NodeInfo {
            board: read_sidecar(&self.board_sidecar).map(board_info),
            ground_station: GroundStationInfo {
                role: ados_config::ground_station_role(&profile, &self.mesh_role),
            },
            camera: CameraInfo {
                ready,
                main: main_stream(&self.config_yaml, &profile),
            },
            profile,
        }
    }
}

/// The board facts from the published fingerprint.
fn board_info(fp: BoardFingerprint) -> BoardInfo {
    let has_npu = fp.npu_tops > 0.0;
    let mut accelerators = Vec::new();
    if has_npu {
        accelerators.push("npu".to_string());
    }
    if fp.has_local_inference {
        accelerators.push(format!("cpu-{}", fp.local_inference));
    }
    BoardInfo {
        id: fp.name,
        name: fp.model,
        has_npu,
        accelerators,
    }
}

/// The primary encoder's geometry. `None` without a config file, and on a
/// ground station, which carries no onboard camera and runs no encoder.
fn main_stream(config_yaml: &Path, profile: &str) -> Option<StreamGeometry> {
    if profile == "ground-station" || !config_yaml.is_file() {
        return None;
    }
    let camera = RosterVideoConfig::load_from(config_yaml).primary_camera();
    Some(StreamGeometry {
        width: camera.width,
        height: camera.height,
        fps: camera.fps,
    })
}

impl RealHost {
    /// Read the node facts off disk on the blocking pool and render the reply.
    pub(super) async fn read_node_info(&self) -> Result<HostResult, HostError> {
        let sources = self.node_info_sources.clone();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let info = tokio::task::spawn_blocking(move || sources.read(now))
            .await
            .map_err(|e| HostError::Rpc(format!("node.info read failed: {e}")))?;
        serialize_reply(&info)
    }
}
