//! The top-level `cameras:` list as stream legs: per-leg declarations and the
//! resolved legs the orchestrator drives.

use ados_config::cameras::{framebus_source, CaptureFormat};
use ados_protocol::framebus::FrameFormat;
use serde::{Deserialize, Serialize};

use super::camera::{
    bounded_keyframe_interval, default_bitrate_kbps, default_codec, default_encoder, default_flip,
    default_fps, default_height, default_keyframe_interval, default_rotation, default_source,
    default_width, is_network_url, CameraConfig, CameraProfile,
};

fn default_id() -> String {
    "main".to_string()
}
fn default_true() -> bool {
    true
}

/// A physical fingerprint that re-pins a leg's logical `id` onto its current
/// `source` device across a hot-plug / reboot that renamed the device node.
/// USB cameras get a `vid:pid[:serial]` string; CSI cameras get the sensor name
/// plus the camera port index. Every field is optional — an absent `match` means
/// the leg is pinned only by its `source` locator (a network URL never moves, so
/// a network leg carries no fingerprint). Matches the Python `CameraMatch`.
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

/// One entry of the top-level `cameras:` list, read as a video leg the node
/// exposes as its own mediamtx path (and `:8889/<id>/whep`). An absent list
/// falls back to the single `video.camera` block verbatim (see
/// [`AgentVideoConfig::resolve_legs`](super::AgentVideoConfig::resolve_legs)).
///
/// `id` is the leg's immutable logical identity; `source` is its current
/// locator (a device path or a network URL); `match` re-pins `source` back to
/// `id` when a hot-plug renames the device node. `role` is the rig role or a
/// stream label; `primary` (else the first leg) is the TRANSPORT primary (the
/// fixed `main` mediamtx path / WFB / cloud); `purpose` is the CONSUMER plane a
/// plugin binds to (feed / detect / navigation / …). The management fields (`name` / `orientation` /
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
    /// Wire codec for this leg. Same single accepted value as
    /// [`CameraConfig::codec`]: `"h264"`.
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
    /// Whether the operator has this leg enabled. A disabled secondary leg is
    /// dropped by [`AgentVideoConfig::resolve_legs`](super::AgentVideoConfig::resolve_legs): no encoder, no mediamtx
    /// path, not advertised. The primary always streams (it carries the radio
    /// leg). Default `true` so existing legs are unchanged.
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
    // --- Encode-plane (image-transform + encoder-selection) keys. Unlike the
    // roster metadata above, these ARE read by the encode pipeline, so they plug
    // straight through to the encoder command builder via [`ResolvedLeg`]. All
    // additive + default-safe (matching the Python `CameraLeg`).
    /// Clockwise image rotation in degrees — 0 | 90 | 180 | 270, default 0
    /// (distinct from the coarse-mount `orientation` metadata above).
    #[serde(default = "default_rotation")]
    pub rotation: u32,
    /// Mirror the image horizontally before encode (default false).
    #[serde(default = "default_flip")]
    pub hflip: bool,
    /// Mirror the image vertically before encode (default false).
    #[serde(default = "default_flip")]
    pub vflip: bool,
    /// Encoder override: "auto" | "omx" | "v4l2m2m" | "software" (default "auto").
    #[serde(default = "default_encoder")]
    pub encoder: String,
    /// Keyframe interval in frames, `0..=30`; 0 ⇒ encoder picks a short
    /// low-latency GOP.
    #[serde(
        default = "default_keyframe_interval",
        deserialize_with = "bounded_keyframe_interval"
    )]
    pub keyframe_interval: u32,
    /// The vision engine owns this camera (`capture: true`): the pipeline never
    /// opens `source` itself and encodes the frames the engine publishes on
    /// the frame bus instead (`framebus://<id>`).
    #[serde(default)]
    pub capture: bool,
    /// The pixel format the vision engine publishes a captured camera in, and
    /// so the raw format the frame-bus encoder input reads.
    #[serde(default)]
    pub format: CaptureFormat,
}

impl CameraLeg {
    /// An explicit network capture source (`rtsp://…` / `rtsps://…` / `http://…`
    /// / `https://…`), or `None` for a local device or a captured camera.
    /// Mirrors [`CameraConfig::network_source`].
    pub fn network_source(&self) -> Option<&str> {
        if self.capture {
            return None;
        }
        let s = self.source.trim();
        is_network_url(s).then_some(s)
    }

    /// The source the pipeline encodes this leg from: `framebus://<id>` for a
    /// camera the vision engine captures, else the declared `source`.
    pub fn pipeline_source(&self) -> String {
        if self.capture {
            framebus_source(&self.id)
        } else {
            self.source.clone()
        }
    }
}

/// The legs of the top-level `cameras[]` list in `text`: every entry that
/// validates for all camera consumers (`ados_config::cameras::camera_entries`),
/// so an entry the vision engine or the supervisor skips is skipped here too
/// and one bad entry never takes the others down.
pub fn legs_from(text: &str) -> Vec<CameraLeg> {
    ados_config::cameras::camera_entries(text)
        .valid
        .into_iter()
        .filter_map(|(_, entry, _)| serde_norway::from_value::<CameraLeg>(entry).ok())
        .collect()
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
    /// Clockwise image rotation in degrees (0 | 90 | 180 | 270), default 0.
    pub rotation: u32,
    pub hflip: bool,
    pub vflip: bool,
    /// Encoder override: "auto" | "omx" | "v4l2m2m" | "software".
    pub encoder: String,
    /// Keyframe interval in frames; 0 ⇒ encoder picks a short low-latency GOP.
    pub keyframe_interval: u32,
    /// The raw frame format of a `framebus://` source (a camera the vision
    /// engine captures); `None` for every other source.
    pub framebus_format: Option<FrameFormat>,
}

impl ResolvedLeg {
    /// The raw frames this leg's `framebus://` source carries (the captured
    /// format at the leg's configured geometry), or `None` for any other source.
    pub fn raw_input(&self) -> Option<crate::encoder::RawInput> {
        self.framebus_format.map(|format| crate::encoder::RawInput {
            format,
            width: self.width,
            height: self.height,
            fps: self.fps,
        })
    }

    /// A [`CameraConfig`] view of this leg, so a secondary local-encode leg can
    /// reuse the same encoder command builder as the primary. The `thumbnail`
    /// attention profile is the default and unread: only the PRIMARY leg rides
    /// the shared radio channel, so only it is attention-switched. The
    /// encode-plane keys (rotation/hflip/vflip/encoder/keyframe_interval) carry
    /// through so a per-leg orientation/encoder override reaches the builder.
    pub fn to_camera_config(&self) -> CameraConfig {
        CameraConfig {
            source: self.source.clone(),
            codec: self.codec.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            thumbnail: CameraProfile::default(),
            rotation: self.rotation,
            hflip: self.hflip,
            vflip: self.vflip,
            encoder: self.encoder.clone(),
            keyframe_interval: self.keyframe_interval,
            passthrough: false,
        }
    }

    /// A leg the orchestrator owns an encoder for (the primary, or a local
    /// secondary) — as opposed to a mediamtx `sourceOnDemand` network pull.
    pub fn is_owned_encoder(&self) -> bool {
        self.is_primary || !self.is_network_pull
    }
}
