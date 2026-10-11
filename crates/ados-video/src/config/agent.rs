//! The agent-level video config the orchestrator gates on, and leg resolution.

use serde::Deserialize;

use super::camera::CameraConfig;
use super::legs::{CameraLeg, ResolvedLeg};
use super::sections::{RecordingConfig, VisionTapConfig, WfbVideoConfig};

fn default_video_mode() -> String {
    "wfb".to_string()
}

/// The agent-level video config the orchestrator gates on: the `video:` block
/// (mode / cloud relay / wfb sub-block / vision / cameras / recording) plus the
/// resolved agent `profile`. Every field is `#[serde(default)]` so a partial /
/// malformed config never blocks the pipeline — a missing `video:` block yields
/// the defaults (mode "wfb", no cloud relay).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentVideoConfig {
    /// `video.mode`: "wfb" (default) | "cloud" | "disabled" | ...
    #[serde(default = "default_video_mode")]
    pub mode: String,
    /// Agent profile, resolved from `agent.profile` ("drone" |
    /// "ground_station"). The orchestrator exits early on a ground-station
    /// profile. Defaults to "drone" when unset (the air-side default).
    #[serde(default)]
    pub profile: Option<String>,
    /// Cloud relay RTSP base URL (`video.cloud_relay_url`); empty / absent ⇒
    /// local-only, no cloud push.
    #[serde(default)]
    pub cloud_relay_url: Option<String>,
    /// The `video.wfb:` sub-block (only `sei_latency` is read here).
    #[serde(default)]
    pub wfb: WfbVideoConfig,
    /// The `video.vision:` sub-block (the additive frame tap). Default OFF.
    #[serde(default)]
    pub vision: VisionTapConfig,
    /// The optional `video.cameras:` list — more than one video leg the node
    /// exposes concurrently (a smart pod, a dual-camera rig). Empty ⇒ the single
    /// legacy `video.camera` path (fully backward compatible).
    #[serde(default)]
    pub cameras: Vec<CameraLeg>,
    /// The `video.recording:` sub-block. Drives mediamtx's native fMP4
    /// recorder through the rendered `pathDefaults` block. Default OFF.
    #[serde(default)]
    pub recording: RecordingConfig,
}

impl Default for AgentVideoConfig {
    fn default() -> Self {
        Self {
            mode: default_video_mode(),
            profile: None,
            cloud_relay_url: None,
            wfb: WfbVideoConfig::default(),
            vision: VisionTapConfig::default(),
            cameras: Vec::new(),
            recording: RecordingConfig::default(),
        }
    }
}

impl AgentVideoConfig {
    /// Load from `/etc/ados/config.yaml`. Returns defaults when the file is
    /// missing or unparseable so config loading never blocks the pipeline.
    pub fn load_from(path: &std::path::Path) -> Self {
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
        #[derive(Debug, Deserialize)]
        struct VideoSection {
            #[serde(default = "default_video_mode")]
            mode: String,
            // A defensive `video.profile` override; the canonical profile is
            // `agent.profile`, but reading both lets an operator pin the video
            // gate without touching the agent profile.
            #[serde(default)]
            profile: Option<String>,
            #[serde(default)]
            cloud_relay_url: Option<String>,
            #[serde(default)]
            wfb: WfbVideoConfig,
            #[serde(default)]
            vision: VisionTapConfig,
            #[serde(default)]
            cameras: Vec<CameraLeg>,
            #[serde(default)]
            recording: RecordingConfig,
        }
        impl Default for VideoSection {
            fn default() -> Self {
                Self {
                    mode: default_video_mode(),
                    profile: None,
                    cloud_relay_url: None,
                    wfb: WfbVideoConfig::default(),
                    vision: VisionTapConfig::default(),
                    cameras: Vec::new(),
                    recording: RecordingConfig::default(),
                }
            }
        }

        let Ok(text) = std::fs::read_to_string(path) else {
            return AgentVideoConfig::default();
        };
        let (raw, cfg_err) = ados_config::yaml_reporting::<RawConfig>(&text, "video");
        // Publish the parse result so a malformed config surfaces on the fleet
        // Health view, not just in the log (per-service status sidecar). This is
        // the video service's broad startup config load (agent + video sections);
        // the camera reader goes through the schema slice loader, which writes no
        // status, so the two loads do not clobber each other's "video" status.
        ados_config::write_config_status("video", cfg_err.as_deref());
        // `agent.profile` is canonical; `video.profile` is an optional override.
        let profile = raw.video.profile.or(raw.agent.profile);
        AgentVideoConfig {
            mode: raw.video.mode,
            profile,
            cloud_relay_url: raw.video.cloud_relay_url,
            wfb: raw.video.wfb,
            vision: raw.video.vision,
            cameras: raw.video.cameras,
            recording: raw.video.recording,
        }
    }

    /// The capture/encode settings the PRIMARY encoder runs from. See
    /// [`primary_camera_from`].
    pub fn primary_camera_config(&self, camera: CameraConfig) -> CameraConfig {
        primary_camera_from(&self.cameras, camera)
    }

    /// Resolve the effective video legs the orchestrator drives.
    ///
    /// Back-compat: an empty `video.cameras` yields a single `main` primary leg
    /// built from the legacy `video.camera` block, so the pipeline is
    /// byte-identical to the single-stream path. Otherwise the leg whose role is
    /// `"primary"` (else the first) is the primary — always an owned encoder, so
    /// a network-primary keeps its ffmpeg bridge — and every other leg with a
    /// network source becomes a mediamtx `sourceOnDemand` pull. A secondary leg
    /// the operator disabled is dropped entirely; the primary always streams.
    ///
    /// The primary leg is always served at the fixed path/id `"main"` — the WFB
    /// radio, cloud relay, and vision tap all key on `main`. Secondary legs keep
    /// their declared ids (their own mediamtx path + `:8889/<id>/whep`). Roles
    /// (`eo` / `eo_wide` / `ir`) carry the labels, so a primary named `main`
    /// still reads as "EO Zoom" on the GCS.
    pub fn resolve_legs(&self, camera: &CameraConfig) -> Vec<ResolvedLeg> {
        let Some(primary_idx) = primary_leg_index(&self.cameras) else {
            return vec![ResolvedLeg {
                id: "main".to_string(),
                source: camera.source.clone(),
                role: "primary".to_string(),
                codec: camera.codec.clone(),
                is_primary: true,
                is_network_pull: false,
                width: camera.width,
                height: camera.height,
                fps: camera.fps,
                bitrate_kbps: camera.bitrate_kbps,
                rotation: camera.rotation,
                hflip: camera.hflip,
                vflip: camera.vflip,
                encoder: camera.encoder.clone(),
                keyframe_interval: camera.keyframe_interval,
            }];
        };
        self.cameras
            .iter()
            .enumerate()
            .filter(|(i, c)| *i == primary_idx || c.enabled)
            .map(|(i, c)| {
                let is_primary = i == primary_idx;
                let is_network_pull = !is_primary && c.network_source().is_some();
                // Keep the leg's DECLARED role (e.g. "eo") so the GCS label map
                // resolves; a primary named "main" still reads as "EO Zoom". Fall
                // back to "primary"/"secondary" only when the leg declared no role.
                let role = c.role.clone().unwrap_or_else(|| {
                    if is_primary {
                        "primary".to_string()
                    } else {
                        "secondary".to_string()
                    }
                });
                ResolvedLeg {
                    // The primary is always served at the fixed "main" path.
                    id: if is_primary {
                        "main".to_string()
                    } else {
                        c.id.clone()
                    },
                    source: c.source.clone(),
                    role,
                    codec: c.codec.clone(),
                    is_primary,
                    is_network_pull,
                    width: c.width,
                    height: c.height,
                    fps: c.fps,
                    bitrate_kbps: c.bitrate_kbps,
                    rotation: c.rotation,
                    hflip: c.hflip,
                    vflip: c.vflip,
                    encoder: c.encoder.clone(),
                    keyframe_interval: c.keyframe_interval,
                }
            })
            .collect()
    }

    /// True when the resolved profile is the ground station (the orchestrator
    /// exits early). The on-disk form is underscore (`ground_station`); accept
    /// the hyphen wire form too for robustness.
    pub fn is_ground_station(&self) -> bool {
        matches!(
            self.profile.as_deref(),
            Some("ground_station") | Some("ground-station")
        )
    }

    /// True when `video.mode` is `"disabled"` (the orchestrator exits early).
    pub fn is_disabled(&self) -> bool {
        self.mode == "disabled"
    }

    /// True when cloud relay is configured (a non-empty `cloud_relay_url`).
    pub fn cloud_enabled(&self) -> bool {
        self.cloud_relay_url
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

/// The index of the primary leg in a `video.cameras` list: the leg whose role is
/// `"primary"`, else the first. `None` when no legs are declared.
fn primary_leg_index(cameras: &[CameraLeg]) -> Option<usize> {
    if cameras.is_empty() {
        return None;
    }
    Some(
        cameras
            .iter()
            .position(|c| c.role.as_deref() == Some("primary"))
            .unwrap_or(0),
    )
}

/// The capture/encode settings the PRIMARY encoder runs from.
///
/// With no `video.cameras` declared that is the legacy `video.camera` block,
/// unchanged. With legs declared, the primary leg's source and encode-plane
/// keys (codec, geometry, fps, bitrate, orientation, encoder override,
/// keyframe interval) replace the block's, so an edit to the primary leg
/// reaches the primary encoder. The block's `thumbnail` profile stays: the
/// non-hero attention profile is a node setting, not a per-leg one.
pub(super) fn primary_camera_from(cameras: &[CameraLeg], camera: CameraConfig) -> CameraConfig {
    let Some(leg) = primary_leg_index(cameras).map(|i| &cameras[i]) else {
        return camera;
    };
    CameraConfig {
        source: leg.source.clone(),
        codec: leg.codec.clone(),
        width: leg.width,
        height: leg.height,
        fps: leg.fps,
        bitrate_kbps: leg.bitrate_kbps,
        thumbnail: camera.thumbnail,
        rotation: leg.rotation,
        hflip: leg.hflip,
        vflip: leg.vflip,
        encoder: leg.encoder.clone(),
        keyframe_interval: leg.keyframe_interval,
        passthrough: camera.passthrough,
    }
}
