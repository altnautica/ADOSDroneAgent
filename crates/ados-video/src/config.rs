//! Video camera configuration, read from the `video.camera:` block of
//! `/etc/ados/config.yaml`. Field names and defaults mirror the Python
//! `CameraConfig` model (`core/config/video.py`). Only the fields the encoder
//! command builder reads are modelled here.

use serde::Deserialize;

fn default_source() -> String {
    "csi".to_string()
}
fn default_codec() -> String {
    "h264".to_string()
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    720
}
fn default_fps() -> u32 {
    30
}
fn default_bitrate_kbps() -> u32 {
    4000
}
fn default_codec_preference() -> String {
    "auto".to_string()
}

/// Camera capture/encode settings. Mirrors the Python `CameraConfig`.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraConfig {
    /// "csi" | "usb" | "ip" device hint, or a device path / RTSP URL.
    #[serde(default = "default_source")]
    pub source: String,
    /// Wire codec: "h264" (default), "h265", "hevc", or "mjpeg".
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_bitrate_kbps")]
    pub bitrate_kbps: u32,
    /// Operator wire-codec preference: "h264" | "h265" | "auto".
    #[serde(default = "default_codec_preference")]
    pub codec_preference: String,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            source: default_source(),
            codec: default_codec(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            bitrate_kbps: default_bitrate_kbps(),
            codec_preference: default_codec_preference(),
        }
    }
}

impl CameraConfig {
    /// Load from the `video.camera:` block in the agent config file. Returns
    /// the defaults when the file is missing or unparseable so config loading
    /// never blocks the pipeline.
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            camera: CameraConfig,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return CameraConfig::default();
        };
        let raw: RawConfig = serde_norway::from_str(&text).unwrap_or_default();
        raw.video.camera
    }
}

// --- agent-level video config (the orchestrator's gates + cloud + GST flags) -

fn default_video_mode() -> String {
    "wfb".to_string()
}
fn default_cloud_rtp_port() -> u16 {
    8000
}

/// The `video.wfb:` sub-block fields the orchestrator reads. Only
/// `sei_latency` matters here; everything else in the WFB block is read by
/// other crates.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WfbVideoConfig {
    /// Inject a wall-clock SEI marker upstream of mediamtx for glass-to-glass
    /// latency measurement (`video.wfb.sei_latency`).
    #[serde(default)]
    pub sei_latency: bool,
}

/// The agent-level video config the orchestrator gates on: the `video:` block
/// (mode / cloud relay / GST flag / wfb sub-block) plus the resolved agent
/// `profile`. Every field is `#[serde(default)]` so a partial / malformed
/// config never blocks the pipeline — a missing `video:` block yields the
/// defaults (mode "wfb", no cloud relay, legacy bash path).
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
    /// UDP port the GST pipeline emits a second RTP copy to when cloud relay is
    /// on (`video.cloud_rtp_port`).
    #[serde(default = "default_cloud_rtp_port")]
    pub cloud_rtp_port: u16,
    /// Opt into the in-process GStreamer air pipeline
    /// (`video.use_gst_air_pipeline`). Deferred in this build: the orchestrator
    /// logs and uses the legacy bash path even when set.
    #[serde(default)]
    pub use_gst_air_pipeline: bool,
    /// The `video.wfb:` sub-block (only `sei_latency` is read here).
    #[serde(default)]
    pub wfb: WfbVideoConfig,
}

impl Default for AgentVideoConfig {
    fn default() -> Self {
        Self {
            mode: default_video_mode(),
            profile: None,
            cloud_relay_url: None,
            cloud_rtp_port: default_cloud_rtp_port(),
            use_gst_air_pipeline: false,
            wfb: WfbVideoConfig::default(),
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
            #[serde(default = "default_cloud_rtp_port")]
            cloud_rtp_port: u16,
            #[serde(default)]
            use_gst_air_pipeline: bool,
            #[serde(default)]
            wfb: WfbVideoConfig,
        }
        impl Default for VideoSection {
            fn default() -> Self {
                Self {
                    mode: default_video_mode(),
                    profile: None,
                    cloud_relay_url: None,
                    cloud_rtp_port: default_cloud_rtp_port(),
                    use_gst_air_pipeline: false,
                    wfb: WfbVideoConfig::default(),
                }
            }
        }

        let Ok(text) = std::fs::read_to_string(path) else {
            return AgentVideoConfig::default();
        };
        let raw: RawConfig = serde_norway::from_str(&text).unwrap_or_default();
        // `agent.profile` is canonical; `video.profile` is an optional override.
        let profile = raw.video.profile.or(raw.agent.profile);
        AgentVideoConfig {
            mode: raw.video.mode,
            profile,
            cloud_relay_url: raw.video.cloud_relay_url,
            cloud_rtp_port: raw.video.cloud_rtp_port,
            use_gst_air_pipeline: raw.video.use_gst_air_pipeline,
            wfb: raw.video.wfb,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let c = CameraConfig::default();
        assert_eq!(c.source, "csi");
        assert_eq!(c.codec, "h264");
        assert_eq!(c.width, 1280);
        assert_eq!(c.height, 720);
        assert_eq!(c.fps, 30);
        assert_eq!(c.bitrate_kbps, 4000);
        assert_eq!(c.codec_preference, "auto");
    }

    #[test]
    fn partial_yaml_fills_defaults() {
        let yaml = "video:\n  camera:\n    width: 1920\n    height: 1080\n    fps: 60\n";
        let raw: serde_norway::Value = serde_norway::from_str(yaml).unwrap();
        // Round-trip through the same nested shape the loader uses.
        let cfg: CameraConfig = {
            #[derive(serde::Deserialize, Default)]
            struct R {
                #[serde(default)]
                video: V,
            }
            #[derive(serde::Deserialize, Default)]
            struct V {
                #[serde(default)]
                camera: CameraConfig,
            }
            let r: R = serde_norway::from_value(raw).unwrap();
            r.video.camera
        };
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.fps, 60);
        // Untouched fields fall back to defaults.
        assert_eq!(cfg.codec, "h264");
        assert_eq!(cfg.bitrate_kbps, 4000);
    }

    // --- AgentVideoConfig ---------------------------------------------

    fn write_tmp(yaml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        (dir, path)
    }

    #[test]
    fn agent_video_defaults_on_missing_file() {
        let c = AgentVideoConfig::load_from(std::path::Path::new("/nonexistent/config.yaml"));
        assert_eq!(c.mode, "wfb");
        assert!(c.profile.is_none());
        assert!(c.cloud_relay_url.is_none());
        assert_eq!(c.cloud_rtp_port, 8000);
        assert!(!c.use_gst_air_pipeline);
        assert!(!c.wfb.sei_latency);
        assert!(!c.is_ground_station());
        assert!(!c.is_disabled());
        assert!(!c.cloud_enabled());
    }

    #[test]
    fn agent_video_loads_extra_fields() {
        let yaml = "\
agent:
  profile: drone
video:
  mode: cloud
  cloud_relay_url: rtsp://relay.example.com:8554
  cloud_rtp_port: 8100
  use_gst_air_pipeline: true
  wfb:
    sei_latency: true
";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert_eq!(c.mode, "cloud");
        assert_eq!(c.profile.as_deref(), Some("drone"));
        assert_eq!(
            c.cloud_relay_url.as_deref(),
            Some("rtsp://relay.example.com:8554")
        );
        assert_eq!(c.cloud_rtp_port, 8100);
        assert!(c.use_gst_air_pipeline);
        assert!(c.wfb.sei_latency);
        assert!(c.cloud_enabled());
        assert!(!c.is_disabled());
    }

    #[test]
    fn agent_video_partial_config_fills_defaults() {
        // Only a width override under camera + a bare video.mode; everything
        // else must default (never block on a partial config).
        let yaml = "video:\n  mode: disabled\n";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert_eq!(c.mode, "disabled");
        assert!(c.is_disabled());
        assert_eq!(c.cloud_rtp_port, 8000);
        assert!(!c.use_gst_air_pipeline);
        assert!(c.profile.is_none());
    }

    #[test]
    fn agent_video_ground_station_gate() {
        let yaml = "agent:\n  profile: ground_station\n";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert!(c.is_ground_station());
        // Hyphen wire form is accepted too.
        let yaml2 = "agent:\n  profile: ground-station\n";
        let (_dir2, path2) = write_tmp(yaml2);
        assert!(AgentVideoConfig::load_from(&path2).is_ground_station());
    }

    #[test]
    fn agent_video_profile_override_precedence() {
        // video.profile overrides agent.profile when both are set.
        let yaml = "\
agent:
  profile: drone
video:
  profile: ground_station
";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert!(c.is_ground_station());
    }

    #[test]
    fn agent_video_empty_cloud_url_is_not_enabled() {
        let yaml = "video:\n  cloud_relay_url: \"\"\n";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert!(!c.cloud_enabled());
    }

    #[test]
    fn agent_video_malformed_yaml_falls_back_to_default() {
        let yaml = "this: : is not [valid yaml at all }}}";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        // Unparseable → defaults, never a panic / block.
        assert_eq!(c.mode, "wfb");
        assert!(c.profile.is_none());
    }
}
