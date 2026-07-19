//! Video camera configuration, read from the `video.camera:` block of
//! `/etc/ados/config.yaml`. Field names and defaults mirror the Python
//! `CameraConfig` model (`core/config/video.py`). Only the fields the encoder
//! command builder reads are modelled here.

use serde::{Deserialize, Serialize};

fn default_id() -> String {
    "main".to_string()
}
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
fn default_true() -> bool {
    true
}

/// True when a source string is a network capture URL — a stream mediamtx pulls
/// (or ffmpeg reads) rather than a local V4L2/CSI device. Recognises plain and
/// TLS forms of RTSP and HTTP (`rtsp://`, `rtsps://`, `http://`, `https://`).
pub fn is_network_url(source: &str) -> bool {
    let s = source.trim();
    s.starts_with("rtsp://")
        || s.starts_with("rtsps://")
        || s.starts_with("http://")
        || s.starts_with("https://")
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
    /// An explicit network capture source (`rtsp://…`, `rtsps://…`, `http://…`,
    /// or `https://…`), or `None` for the local-camera discovery path.
    ///
    /// When `source` is a network URL the pipeline streams from it directly
    /// instead of probing for a local V4L2/CSI camera — the "ip camera" mode,
    /// where the operator points the agent at a remote feed (an onboard IP
    /// camera, an encoder box, a network RTSP source). The `"csi"` / `"usb"` /
    /// `"ip"` hint strings and bare device paths return `None` so the existing
    /// discovery path is unchanged (fully backward compatible). The URL is used
    /// verbatim as the ffmpeg input; the `validate_source` allowlist in the
    /// encoder still gates the characters that reach the argv.
    pub fn network_source(&self) -> Option<&str> {
        let s = self.source.trim();
        is_network_url(s).then_some(s)
    }

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
        let raw: RawConfig = ados_config::yaml_or_default(&text, "video");
        raw.video.camera
    }
}

/// A physical fingerprint that re-pins a leg's logical `id` onto its current
/// `source` device across a hot-plug / reboot that renamed the device node.
/// USB cameras get a `vid:pid[:serial]` string; CSI cameras get the sensor name
/// plus the camera port index. Every field is optional — an absent `match` means
/// the leg is pinned only by its `source` locator (a network URL never moves, so
/// a network leg carries no fingerprint). Mirrors the Python `CameraMatch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraMatch {
    /// USB fingerprint, `vid:pid` or `vid:pid:serial` (lowercase hex vid/pid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb: Option<String>,
    /// CSI sensor name (e.g. `imx219`), from the camera enumeration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csi_sensor: Option<String>,
    /// CSI camera port index (the connector the sensor is on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csi_port: Option<u32>,
}

/// One entry of the optional `video.cameras:` list — a single video leg the
/// node exposes as its own mediamtx path (and `:8889/<id>/whep`). Present only
/// when the operator (or a driver plugin) declares more than one stream; an
/// absent `video.cameras` falls back to the single legacy `video.camera` block
/// verbatim (see [`AgentVideoConfig::resolve_legs`]).
///
/// `id` is the leg's immutable logical identity; `source` is its current
/// locator (a device path or a network URL); `match` re-pins `source` back to
/// `id` when a hot-plug renames the device node. `role` is the TRANSPORT plane
/// (primary → the fixed `main` mediamtx path / WFB / cloud); `purpose` is the
/// CONSUMER plane a plugin binds to (feed / detect / navigation / …) — they are
/// complementary, not merged. The management fields (`name` / `orientation` /
/// `purpose` / `enabled` / `owner` / `fov_deg` / `mount_pitch_deg` /
/// `calibration` / `match`) are metadata surfaced through the camera roster and
/// consumed by plugins; the encode + radio pipeline reads none of them, so an
/// existing single-`role` config resolves byte-identically.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraLeg {
    /// Stable per-node stream id — the mediamtx path name and the WHEP id. Also
    /// the leg's immutable logical identity in the camera roster. Defaults to
    /// `"main"` (the legacy single-camera identity), matching the Python
    /// `CameraLeg.id` default so a leg written without an explicit id resolves
    /// the same on both halves.
    #[serde(default = "default_id")]
    pub id: String,
    /// Capture source: a device hint / path (local encode) or an `rtsp://` /
    /// `http://` URL (a secondary network leg mediamtx pulls on demand).
    #[serde(default = "default_source")]
    pub source: String,
    /// Logical role: `"primary"` designates the WFB/cloud stream; any other
    /// value (or absent) is a LAN-WHEP-only secondary. Absent on every leg ⇒ the
    /// first leg is the primary.
    #[serde(default)]
    pub role: Option<String>,
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
    /// Operator-facing display name for the roster (e.g. "Belly cam").
    #[serde(default)]
    pub name: Option<String>,
    /// Coarse physical mount orientation: `forward` | `down` | `back` | `left` |
    /// `right` | `up` | `gimbal` | `custom`. Enough for plugin binding (a
    /// down-facing leg is the precision-landing candidate); not full extrinsics.
    #[serde(default)]
    pub orientation: Option<String>,
    /// What the leg is FOR — one or more of `feed` | `detect` | `navigation` |
    /// `precision-landing` | `thermal` | `mapping` | `recording`. Plugins bind to
    /// a purpose; a leg may serve several.
    #[serde(default)]
    pub purpose: Vec<String>,
    /// Whether the operator has this leg enabled. Metadata in v1 (the pipeline
    /// does not gate on it yet); default `true` so existing legs are unchanged.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Who declared this leg: `"operator"` for an operator-managed leg, or a
    /// plugin id (e.g. `com.altnautica.siyi-pod`) for a driver-declared leg. The
    /// merge-by-owner persist keys on this so an operator write preserves a
    /// plugin's legs and vice versa. Absent ⇒ treated as operator-owned.
    #[serde(default)]
    pub owner: Option<String>,
    /// Horizontal field of view in degrees, when known (informational).
    #[serde(default)]
    pub fov_deg: Option<f32>,
    /// Mount pitch offset in degrees (e.g. a 45°-down inspection cam).
    #[serde(default)]
    pub mount_pitch_deg: Option<f32>,
    /// A calibration reference (a profile name or a stored intrinsics id).
    #[serde(default)]
    pub calibration: Option<String>,
    /// Physical fingerprint that re-pins `source` → `id` across a device rename.
    #[serde(default, rename = "match")]
    pub camera_match: Option<CameraMatch>,
}

impl CameraLeg {
    /// An explicit network capture source (`rtsp://…` / `rtsps://…` / `http://…`
    /// / `https://…`), or `None` for a local device. Mirrors
    /// [`CameraConfig::network_source`].
    pub fn network_source(&self) -> Option<&str> {
        let s = self.source.trim();
        is_network_url(s).then_some(s)
    }
}

/// A resolved video leg — what the orchestrator actually drives. Exactly one leg
/// is the primary (the WFB/cloud stream); the rest are LAN-WHEP-only
/// secondaries. A secondary with a network source is a mediamtx `sourceOnDemand`
/// pull (no agent-owned encoder); every other leg (the primary always, or a
/// local secondary) is an agent-owned encoder that publishes into its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLeg {
    pub id: String,
    pub source: String,
    pub role: String,
    pub codec: String,
    /// True for the single designated primary leg (carries WFB + cloud + SEI).
    pub is_primary: bool,
    /// True ⇒ mediamtx pulls the source on demand (no owned encoder process).
    pub is_network_pull: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl ResolvedLeg {
    /// A [`CameraConfig`] view of this leg, so a secondary local-encode leg can
    /// reuse the same encoder command builder as the primary. `codec_preference`
    /// defaults to `"auto"` (the leg carries only the concrete `codec`).
    pub fn to_camera_config(&self) -> CameraConfig {
        CameraConfig {
            source: self.source.clone(),
            codec: self.codec.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            codec_preference: "auto".to_string(),
        }
    }

    /// A leg the orchestrator owns an encoder for (the primary, or a local
    /// secondary) — as opposed to a mediamtx `sourceOnDemand` network pull.
    pub fn is_owned_encoder(&self) -> bool {
        self.is_primary || !self.is_network_pull
    }
}

// --- agent-level video config (the orchestrator's gates + cloud + GST flags) -

fn default_video_mode() -> String {
    "wfb".to_string()
}
fn default_cloud_rtp_port() -> u16 {
    8000
}

// --- vision frame-tap sub-block ----------------------------------------------

fn default_vision_fps() -> u32 {
    10
}
fn default_vision_width() -> u32 {
    640
}
fn default_vision_height() -> u32 {
    480
}
fn default_vision_format() -> String {
    "rgb24".to_string()
}
fn default_vision_sink() -> String {
    "/run/ados/vision-tap-main.sock".to_string()
}

/// The `video.vision:` sub-block. Configures an additive, optional frame tap
/// that feeds raw decoded frames to the on-box vision engine without touching
/// the encode + radio byte path. Default OFF: an unconfigured rig runs the
/// exact same encoder + wfb_tee commands it always has, byte-for-byte.
///
/// The tap reads the local mediamtx RTSP `/main` stream with a third ffmpeg
/// (`-c:v copy` is not used here — frames are decoded, downscaled, and emitted
/// as `rawvideo`), so a crash or stall on this leg never reaches the encoder or
/// the wfb radio fan-out. When `raw_tap` is set the tap instead rides a
/// pre-encode `-filter_complex` split inside the encoder command itself, gated
/// behind the flag and off by default.
#[derive(Debug, Clone, Deserialize)]
pub struct VisionTapConfig {
    /// Master switch. `false` ⇒ no tap is ever spawned and the encoder command
    /// is unchanged.
    #[serde(default)]
    pub enabled: bool,
    /// Frames per second delivered to the vision engine (the tap throttles the
    /// RTSP stream down to this rate before scaling).
    #[serde(default = "default_vision_fps")]
    pub fps: u32,
    /// Output frame width the vision engine expects.
    #[serde(default = "default_vision_width")]
    pub width: u32,
    /// Output frame height the vision engine expects.
    #[serde(default = "default_vision_height")]
    pub height: u32,
    /// Raw pixel format of the emitted frames: "rgb24" | "nv12" | "yuv420p".
    #[serde(default = "default_vision_format")]
    pub format: String,
    /// When `true`, ride a pre-encode filter split inside the encoder command
    /// (lower latency, single decode) instead of the decoupled third-ffmpeg
    /// tap. Off by default; the decoupled tap is the safe default because it
    /// cannot perturb the encode output.
    #[serde(default)]
    pub raw_tap: bool,
    /// Filesystem path the vision engine reads raw frames from (a unix socket
    /// or fifo). The tap writes `rawvideo` here.
    #[serde(default = "default_vision_sink")]
    pub sink: String,
}

impl Default for VisionTapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fps: default_vision_fps(),
            width: default_vision_width(),
            height: default_vision_height(),
            format: default_vision_format(),
            raw_tap: false,
            sink: default_vision_sink(),
        }
    }
}

impl VisionTapConfig {
    /// Normalise the configured pixel format to one ffmpeg accepts, falling
    /// back to "rgb24" on an unrecognised value so a typo never wedges the tap.
    pub fn pixel_format(&self) -> &str {
        match self.format.as_str() {
            "rgb24" | "nv12" | "yuv420p" => self.format.as_str(),
            _ => "rgb24",
        }
    }
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
}

impl Default for AgentVideoConfig {
    fn default() -> Self {
        Self {
            mode: default_video_mode(),
            profile: None,
            cloud_relay_url: None,
            cloud_rtp_port: default_cloud_rtp_port(),
            wfb: WfbVideoConfig::default(),
            vision: VisionTapConfig::default(),
            cameras: Vec::new(),
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
            wfb: WfbVideoConfig,
            #[serde(default)]
            vision: VisionTapConfig,
            #[serde(default)]
            cameras: Vec<CameraLeg>,
        }
        impl Default for VideoSection {
            fn default() -> Self {
                Self {
                    mode: default_video_mode(),
                    profile: None,
                    cloud_relay_url: None,
                    cloud_rtp_port: default_cloud_rtp_port(),
                    wfb: WfbVideoConfig::default(),
                    vision: VisionTapConfig::default(),
                    cameras: Vec::new(),
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
        // the camera reader stays on the quiet-default helper so the two loads do
        // not clobber each other's "video" status.
        ados_config::write_config_status("video", cfg_err.as_deref());
        // `agent.profile` is canonical; `video.profile` is an optional override.
        let profile = raw.video.profile.or(raw.agent.profile);
        AgentVideoConfig {
            mode: raw.video.mode,
            profile,
            cloud_relay_url: raw.video.cloud_relay_url,
            cloud_rtp_port: raw.video.cloud_rtp_port,
            wfb: raw.video.wfb,
            vision: raw.video.vision,
            cameras: raw.video.cameras,
        }
    }

    /// Resolve the effective video legs the orchestrator drives.
    ///
    /// Back-compat: an empty `video.cameras` yields a single `main` primary leg
    /// built from the legacy `video.camera` block, so the pipeline is
    /// byte-identical to the single-stream path. Otherwise the leg whose role is
    /// `"primary"` (else the first) is the primary — always an owned encoder, so
    /// a network-primary keeps its ffmpeg bridge — and every other leg with a
    /// network source becomes a mediamtx `sourceOnDemand` pull.
    ///
    /// The primary leg is always served at the fixed path/id `"main"` — the WFB
    /// radio, cloud relay, and vision tap all key on `main`. Secondary legs keep
    /// their declared ids (their own mediamtx path + `:8889/<id>/whep`). Roles
    /// (`eo` / `eo_wide` / `ir`) carry the labels, so a primary named `main`
    /// still reads as "EO Zoom" on the GCS.
    pub fn resolve_legs(&self, camera: &CameraConfig) -> Vec<ResolvedLeg> {
        if self.cameras.is_empty() {
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
            }];
        }
        let primary_idx = self
            .cameras
            .iter()
            .position(|c| c.role.as_deref() == Some("primary"))
            .unwrap_or(0);
        self.cameras
            .iter()
            .enumerate()
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

/// The roster-relevant slice of the video config, loaded in ONE quiet pass.
///
/// The camera-roster route reads config, discovery, and live-stream state and
/// serves them on a pollable HTTP surface. It must NOT re-run the `ados-video`
/// service's broad, status-stamping config load ([`AgentVideoConfig::load_from`]
/// calls [`ados_config::write_config_status`]) — doing so races the service and
/// flashes spurious config-status faults on the fleet Health view. This loader
/// parses `config.yaml` exactly once with the quiet-default helper (the same
/// posture as [`CameraConfig::load_from`]) and writes no status sidecar.
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
    /// The explicit multi-leg `video.cameras[]` list (empty when absent).
    pub cameras: Vec<CameraLeg>,
}

impl RosterVideoConfig {
    /// Load the roster config quietly (one parse, no config-status side-effect).
    /// Returns the defaults (no profile, no camera, no legs) when the file is
    /// missing or unparseable so the roster read never blocks.
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
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            profile: Option<String>,
            #[serde(default)]
            camera: Option<CameraConfig>,
            #[serde(default)]
            cameras: Vec<CameraLeg>,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let raw: RawConfig = ados_config::yaml_or_default(&text, "video");
        Self {
            profile: raw.video.profile.or(raw.agent.profile),
            camera: raw.video.camera,
            cameras: raw.video.cameras,
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

    #[test]
    fn network_source_detects_rtsp_and_http() {
        let mut c = CameraConfig::default();
        // The default hint is a local camera → discovery path.
        assert_eq!(c.network_source(), None);
        for hint in ["csi", "usb", "ip", "/dev/video0"] {
            c.source = hint.to_string();
            assert_eq!(c.network_source(), None, "{hint} is not a network source");
        }
        c.source = "rtsp://10.0.0.9:554/live".to_string();
        assert_eq!(c.network_source(), Some("rtsp://10.0.0.9:554/live"));
        c.source = "http://cam.local:8080/stream".to_string();
        assert_eq!(c.network_source(), Some("http://cam.local:8080/stream"));
        // The TLS forms of RTSP and HTTP are network sources too.
        c.source = "rtsps://10.0.0.9:322/live".to_string();
        assert_eq!(c.network_source(), Some("rtsps://10.0.0.9:322/live"));
        c.source = "https://cam.local/stream".to_string();
        assert_eq!(c.network_source(), Some("https://cam.local/stream"));
        // Surrounding whitespace is trimmed.
        c.source = "  rtsp://host/main  ".to_string();
        assert_eq!(c.network_source(), Some("rtsp://host/main"));
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
        assert!(!c.wfb.sei_latency);
        assert!(!c.is_ground_station());
        assert!(!c.is_disabled());
        assert!(!c.cloud_enabled());
        // The vision tap is off by default with the documented defaults.
        assert!(!c.vision.enabled);
        assert_eq!(c.vision.fps, 10);
        assert_eq!(c.vision.width, 640);
        assert_eq!(c.vision.height, 480);
        assert_eq!(c.vision.format, "rgb24");
        assert!(!c.vision.raw_tap);
        assert_eq!(c.vision.sink, "/run/ados/vision-tap-main.sock");
    }

    #[test]
    fn vision_tap_loads_full_block() {
        let yaml = "\
video:
  vision:
    enabled: true
    fps: 5
    width: 1280
    height: 720
    format: nv12
    raw_tap: true
    sink: /run/ados/custom.sock
";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert!(c.vision.enabled);
        assert_eq!(c.vision.fps, 5);
        assert_eq!(c.vision.width, 1280);
        assert_eq!(c.vision.height, 720);
        assert_eq!(c.vision.format, "nv12");
        assert!(c.vision.raw_tap);
        assert_eq!(c.vision.sink, "/run/ados/custom.sock");
        assert_eq!(c.vision.pixel_format(), "nv12");
    }

    #[test]
    fn vision_tap_partial_block_fills_defaults() {
        // Only enabled flipped on; everything else must take the defaults.
        let yaml = "video:\n  vision:\n    enabled: true\n";
        let (_dir, path) = write_tmp(yaml);
        let c = AgentVideoConfig::load_from(&path);
        assert!(c.vision.enabled);
        assert_eq!(c.vision.fps, 10);
        assert_eq!(c.vision.width, 640);
        assert_eq!(c.vision.height, 480);
        assert_eq!(c.vision.format, "rgb24");
        assert!(!c.vision.raw_tap);
        assert_eq!(c.vision.sink, "/run/ados/vision-tap-main.sock");
    }

    #[test]
    fn vision_pixel_format_falls_back_on_garbage() {
        let mut v = VisionTapConfig::default();
        assert_eq!(v.pixel_format(), "rgb24");
        v.format = "yuv420p".to_string();
        assert_eq!(v.pixel_format(), "yuv420p");
        v.format = "bogus".to_string();
        assert_eq!(v.pixel_format(), "rgb24");
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

    // --- video.cameras[] / resolve_legs -------------------------------

    #[test]
    fn cameras_absent_synthesizes_single_main_leg() {
        // No `video.cameras` ⇒ one primary "main" leg from the legacy block,
        // byte-identical to the single-stream path.
        let cfg = AgentVideoConfig::default();
        let cam = CameraConfig {
            source: "usb".to_string(),
            codec: "h265".to_string(),
            ..CameraConfig::default()
        };
        let legs = cfg.resolve_legs(&cam);
        assert_eq!(legs.len(), 1);
        let leg = &legs[0];
        assert_eq!(leg.id, "main");
        assert_eq!(leg.role, "primary");
        assert_eq!(leg.source, "usb");
        assert_eq!(leg.codec, "h265");
        assert!(leg.is_primary);
        assert!(!leg.is_network_pull);
    }

    #[test]
    fn cameras_parse_multi_leg_and_resolve() {
        let yaml = "\
video:
  cameras:
    - { id: eo-zoom, source: rtsp://192.168.144.25:8554/main, role: eo, codec: h265 }
    - { id: eo-wide, source: rtsp://192.168.144.25:8554/sub, role: eo_wide, codec: h264 }
    - { id: ir, source: rtsp://192.168.144.25:8554/ir, role: ir }
";
        let (_dir, path) = write_tmp(yaml);
        let cfg = AgentVideoConfig::load_from(&path);
        assert_eq!(cfg.cameras.len(), 3);
        let legs = cfg.resolve_legs(&CameraConfig::default());
        assert_eq!(legs.len(), 3);
        // No leg declared role "primary" → the first leg is the primary, served
        // at the fixed "main" path as an owned encoder; the two secondary RTSP
        // legs are network pulls that keep their declared ids.
        assert_eq!(legs[0].id, "main");
        assert!(legs[0].is_primary);
        assert!(!legs[0].is_network_pull);
        assert_eq!(legs[0].role, "eo"); // A6: keeps its declared role, not "primary"
        assert_eq!(legs[1].id, "eo-wide");
        assert!(!legs[1].is_primary);
        assert!(legs[1].is_network_pull);
        assert_eq!(legs[1].role, "eo_wide");
        assert!(legs[2].is_network_pull);
        assert_eq!(legs[2].codec, "h264"); // per-leg codec default
    }

    #[test]
    fn resolve_legs_honours_explicit_primary_role() {
        let yaml = "\
video:
  cameras:
    - { id: ir, source: rtsp://10.0.0.9/ir, role: ir }
    - { id: main-eo, source: /dev/video0, role: primary }
";
        let (_dir, path) = write_tmp(yaml);
        let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
        // The second leg is the declared primary (owned encoder, served at the
        // fixed "main" path); the first, though listed first, is a secondary
        // network pull that keeps its declared id.
        let primary: Vec<_> = legs.iter().filter(|l| l.is_primary).collect();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].id, "main");
        assert!(!primary[0].is_network_pull);
        assert!(legs.iter().find(|l| l.id == "ir").unwrap().is_network_pull);
    }

    #[test]
    fn resolve_legs_keeps_the_primary_declared_role_label() {
        // A6: the primary leg keeps its declared role (e.g. "eo") so the GCS
        // label map resolves — it is NOT clobbered to "primary". (The SIYI ZT30
        // shape: first leg EO-zoom on main, second IR on sub.)
        let yaml = "\
video:
  cameras:
    - { id: eo-zoom, source: rtsp://192.168.144.25:8554/main, role: eo }
    - { id: sub,     source: rtsp://192.168.144.25:8554/sub,  role: ir }
";
        let (_dir, path) = write_tmp(yaml);
        let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
        let primary = legs.iter().find(|l| l.is_primary).unwrap();
        assert_eq!(primary.id, "main"); // served at the fixed main path
        assert_eq!(primary.role, "eo"); // but keeps the EO label
    }

    #[test]
    fn camera_leg_partial_fields_fill_defaults() {
        let yaml = "\
video:
  cameras:
    - { id: solo, source: /dev/video0 }
";
        let (_dir, path) = write_tmp(yaml);
        let cfg = AgentVideoConfig::load_from(&path);
        let leg = &cfg.cameras[0];
        assert_eq!(leg.codec, "h264");
        assert_eq!(leg.width, 1280);
        assert_eq!(leg.height, 720);
        assert_eq!(leg.fps, 30);
        assert_eq!(leg.bitrate_kbps, 4000);
        assert!(leg.role.is_none());
        // The management fields default to the same values the Python `CameraLeg`
        // model does, so a leg declared before the roster fields existed reads the
        // same on both halves (name/orientation/owner/fov/mount/calibration/match
        // absent, purpose empty, enabled true).
        assert!(leg.name.is_none());
        assert!(leg.orientation.is_none());
        assert!(leg.purpose.is_empty());
        assert!(leg.enabled);
        assert!(leg.owner.is_none());
        assert!(leg.fov_deg.is_none());
        assert!(leg.mount_pitch_deg.is_none());
        assert!(leg.calibration.is_none());
        assert!(leg.camera_match.is_none());
        // A single declared leg (no role) is the primary owned encoder.
        let legs = cfg.resolve_legs(&CameraConfig::default());
        assert_eq!(legs.len(), 1);
        assert!(legs[0].is_primary);
        assert!(!legs[0].is_network_pull);
    }

    #[test]
    fn camera_leg_management_fields_parse_and_resolve_unchanged() {
        // A leg carrying the full management field set parses every field, and
        // the resolved leg is byte-identical to one without them (the pipeline
        // reads none of the metadata — it stays on id/source/role/codec/geometry).
        let yaml = "\
video:
  cameras:
    - id: belly
      source: /dev/video2
      role: primary
      codec: h265
      name: Belly cam
      orientation: down
      purpose: [detect, precision-landing]
      enabled: false
      owner: operator
      fov_deg: 82.5
      mount_pitch_deg: -45.0
      calibration: belly-v1
      match: { usb: \"046d:0825:ABC123\" }
";
        let (_dir, path) = write_tmp(yaml);
        let cfg = AgentVideoConfig::load_from(&path);
        assert_eq!(cfg.cameras.len(), 1);
        let leg = &cfg.cameras[0];
        assert_eq!(leg.name.as_deref(), Some("Belly cam"));
        assert_eq!(leg.orientation.as_deref(), Some("down"));
        assert_eq!(leg.purpose, vec!["detect", "precision-landing"]);
        assert!(!leg.enabled);
        assert_eq!(leg.owner.as_deref(), Some("operator"));
        assert_eq!(leg.fov_deg, Some(82.5));
        assert_eq!(leg.mount_pitch_deg, Some(-45.0));
        assert_eq!(leg.calibration.as_deref(), Some("belly-v1"));
        assert_eq!(
            leg.camera_match.as_ref().and_then(|m| m.usb.as_deref()),
            Some("046d:0825:ABC123")
        );
        // The resolved leg carries only the transport-plane fields; the metadata
        // never reaches the encoder, so an `enabled: false` leg still resolves the
        // same way (enable-gating is a later polish item, not this schema pass).
        let legs = cfg.resolve_legs(&CameraConfig::default());
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].id, "main"); // primary → the fixed main path
        assert_eq!(legs[0].role, "primary");
        assert_eq!(legs[0].codec, "h265");
        assert!(legs[0].is_primary);
    }

    #[test]
    fn camera_leg_id_defaults_to_main_matching_python() {
        // A leg written without an explicit `id` resolves to "main" (the legacy
        // single-camera identity), matching the Python CameraLeg.id default so the
        // two halves parse an id-less leg the same way.
        let yaml = "video:\n  cameras:\n    - { source: /dev/video0 }\n";
        let (_dir, path) = write_tmp(yaml);
        let cfg = AgentVideoConfig::load_from(&path);
        assert_eq!(cfg.cameras.len(), 1);
        assert_eq!(cfg.cameras[0].id, "main");
    }

    #[test]
    fn camera_match_csi_fingerprint_parses() {
        let yaml = "\
video:
  cameras:
    - id: nadir
      source: /dev/video0
      match: { csi_sensor: imx219, csi_port: 1 }
";
        let (_dir, path) = write_tmp(yaml);
        let cfg = AgentVideoConfig::load_from(&path);
        let m = cfg.cameras[0].camera_match.as_ref().unwrap();
        assert_eq!(m.csi_sensor.as_deref(), Some("imx219"));
        assert_eq!(m.csi_port, Some(1));
        assert!(m.usb.is_none());
    }

    // --- RosterVideoConfig (quiet single-pass roster loader) ----------

    #[test]
    fn roster_config_loads_profile_camera_presence_and_legs_in_one_pass() {
        // The camera block present ⇒ Some (a real single-camera drone); the
        // cameras[] list and the resolved profile parse in the same pass. This is
        // the quiet loader the roster route uses (no config-status side-effect —
        // it calls the quiet yaml_or_default, never write_config_status).
        let yaml = "\
agent:
  profile: drone
video:
  camera: { source: usb, codec: h265 }
  cameras:
    - { id: eo, source: /dev/video0, role: primary }
    - { id: ir, source: rtsp://pod/ir, role: ir }
";
        let (_dir, path) = write_tmp(yaml);
        let cfg = RosterVideoConfig::load_from(&path);
        assert_eq!(cfg.profile.as_deref(), Some("drone"));
        assert!(!cfg.is_ground_station());
        let camera = cfg.camera.expect("a declared video.camera block is Some");
        assert_eq!(camera.source, "usb");
        assert_eq!(camera.codec, "h265");
        assert_eq!(cfg.cameras.len(), 2);
        assert_eq!(cfg.cameras[0].id, "eo");
    }

    #[test]
    fn roster_config_camera_absent_is_none() {
        // No `video.camera` block ⇒ None, so the roster can distinguish a
        // camera-less node from a real single-camera drone.
        let yaml = "agent:\n  profile: ground_station\nvideo:\n  mode: wfb\n";
        let (_dir, path) = write_tmp(yaml);
        let cfg = RosterVideoConfig::load_from(&path);
        assert!(cfg.camera.is_none());
        assert!(cfg.cameras.is_empty());
        assert!(cfg.is_ground_station());
    }

    #[test]
    fn roster_config_profile_override_and_defaults() {
        // video.profile overrides agent.profile; a missing/malformed file yields
        // the empty default (no profile, no camera, no legs) and never blocks.
        let yaml = "agent:\n  profile: drone\nvideo:\n  profile: ground-station\n";
        let (_dir, path) = write_tmp(yaml);
        assert!(RosterVideoConfig::load_from(&path).is_ground_station());
        let missing = RosterVideoConfig::load_from(std::path::Path::new("/nope/config.yaml"));
        assert!(missing.profile.is_none());
        assert!(missing.camera.is_none());
        assert!(missing.cameras.is_empty());
        let (_bad_dir, bad) = write_tmp("this: : not [valid yaml }}}");
        let malformed = RosterVideoConfig::load_from(&bad);
        assert!(malformed.camera.is_none());
        assert!(malformed.cameras.is_empty());
    }

    #[test]
    fn resolved_leg_ownership_and_camera_config() {
        let yaml = "\
video:
  cameras:
    - { id: main, source: /dev/video0, role: eo, codec: h264 }
    - { id: belly, source: /dev/video1, role: eo_wide, codec: h265, width: 640, height: 480, fps: 15 }
    - { id: ir, source: rtsp://pod/ir, role: ir }
";
        let (_dir, path) = write_tmp(yaml);
        let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
        // Primary + local secondary own an encoder; the network secondary is a pull.
        assert!(legs[0].is_owned_encoder()); // main (primary)
        assert!(legs[1].is_owned_encoder()); // belly (local secondary)
        assert!(!legs[2].is_owned_encoder()); // ir (network pull)
                                              // The local secondary's CameraConfig view carries its own geometry.
        let cam = legs[1].to_camera_config();
        assert_eq!(cam.source, "/dev/video1");
        assert_eq!(cam.codec, "h265");
        assert_eq!(cam.width, 640);
        assert_eq!(cam.fps, 15);
    }
}
