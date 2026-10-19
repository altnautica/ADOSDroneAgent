//! The quiet, single-pass roster slice of the video config.

use serde::Deserialize;

use super::agent::primary_camera_from;
use super::camera::CameraConfig;
use super::legs::CameraLeg;

/// The roster-relevant slice of the video config, loaded in ONE quiet pass.
///
/// The camera-roster route reads config, discovery, and live-stream state and
/// serves them on a pollable HTTP surface. It must NOT re-run the `ados-video`
/// service's broad, status-stamping config load ([`AgentVideoConfig::load_from`](super::AgentVideoConfig::load_from)
/// calls [`ados_config::write_config_status`]) — doing so races the service and
/// flashes spurious config-status faults on the fleet Health view. This loader
/// parses `config.yaml` exactly once with the quiet-default helper and, like
/// [`CameraConfig::load_from`], writes no status sidecar.
///
/// `camera` is `None` when the config declared no `video.camera` block at all, so
/// the roster can tell a real single-camera drone (a declared block) from a
/// camera-less node (no block) and not synthesise a phantom offline `main`.
#[derive(Debug, Clone, Default)]
pub struct RosterVideoConfig {
    /// The resolved node profile (`video.profile` overriding `agent.profile`),
    /// used to gate the roster to companion nodes.
    pub profile: Option<String>,
    /// The legacy single-camera `video.camera` block, or `None` when the config
    /// declared no such block.
    pub camera: Option<CameraConfig>,
    /// The explicit multi-leg top-level `cameras[]` list (empty when absent).
    pub cameras: Vec<CameraLeg>,
}

impl RosterVideoConfig {
    /// Load the roster config quietly (one parse, no config-status side-effect).
    /// Returns the defaults (no profile, no camera, no legs) when the file is
    /// missing or unparseable so the roster read never blocks.
    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::from_text(&text))
            .unwrap_or_default()
    }

    /// [`Self::load_from`] over config text already read.
    pub fn from_text(text: &str) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            agent: AgentSection,
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct AgentSection {
            #[serde(default)]
            profile: Option<String>,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            profile: Option<String>,
            #[serde(default)]
            camera: Option<CameraConfig>,
        }
        let raw: RawConfig = ados_config::yaml_or_default(text, "video");
        Self {
            profile: raw.video.profile.or(raw.agent.profile),
            camera: raw.video.camera,
            cameras: super::legs::legs_from(text),
        }
    }

    /// True when the resolved profile is a ground station — the node carries no
    /// onboard camera, so the roster surface does not apply. The on-disk form is
    /// underscore (`ground_station`); the hyphen wire form is accepted too.
    pub fn is_ground_station(&self) -> bool {
        matches!(
            self.profile.as_deref(),
            Some("ground_station") | Some("ground-station")
        )
    }

    /// The capture/encode settings the primary encoder runs from (the stream
    /// served at `main`), resolved exactly as the video service resolves them:
    /// the primary `cameras` leg, else the `video.camera` block, else that
    /// block's defaults.
    pub fn primary_camera(&self) -> CameraConfig {
        primary_camera_from(&self.cameras, self.camera.clone().unwrap_or_default())
    }
}
