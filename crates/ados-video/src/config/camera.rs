//! The `video.camera:` block: capture/encode settings and the thumbnail
//! attention profile.

use serde::{Deserialize, Serialize};

pub(super) fn default_source() -> String {
    "csi".to_string()
}
pub(super) fn default_codec() -> String {
    "h264".to_string()
}
pub(super) fn default_width() -> u32 {
    1280
}
pub(super) fn default_height() -> u32 {
    720
}
pub(super) fn default_fps() -> u32 {
    30
}
pub(super) fn default_bitrate_kbps() -> u32 {
    4000
}
pub(super) fn default_rotation() -> u32 {
    0
}
pub(super) fn default_flip() -> bool {
    false
}
pub(super) fn default_encoder() -> String {
    "auto".to_string()
}
pub(super) fn default_keyframe_interval() -> u32 {
    0
}

/// The largest `keyframe_interval` (frames) the config accepts; matches the
/// encoder's GOP ceiling so a configured value can never stretch the radio
/// leg's keyframe spacing past one second at the default 30 fps.
pub const MAX_KEYFRAME_INTERVAL: u32 = crate::encoder::MAX_GOP_FRAMES;

/// Deserialize `keyframe_interval`, holding it to `0..=MAX_KEYFRAME_INTERVAL`
/// (0 = the encoder's half-second default). A larger value is pulled down to
/// the ceiling and logged rather than handed to the encoder.
pub(super) fn bounded_keyframe_interval<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = u32::deserialize(deserializer)?;
    if v > MAX_KEYFRAME_INTERVAL {
        tracing::warn!(
            keyframe_interval = v,
            max = MAX_KEYFRAME_INTERVAL,
            "keyframe_interval_out_of_range: held at the ceiling"
        );
        return Ok(MAX_KEYFRAME_INTERVAL);
    }
    Ok(v)
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

fn default_thumbnail_width() -> u32 {
    320
}
fn default_thumbnail_height() -> u32 {
    180
}
fn default_thumbnail_fps() -> u32 {
    1
}
fn default_thumbnail_bitrate_kbps() -> u32 {
    50
}

/// The `video.camera.thumbnail:` sub-block — the non-hero encoder profile, what
/// a drone streams while some OTHER drone holds the operator's attention. The
/// hero profile is the top-level [`CameraConfig`] `width`/`height`/`fps`/
/// `bitrate_kbps` (unchanged defaults). Mirrors the Python
/// `CameraThumbnailProfile`; the airtime arithmetic that fixes these numbers is
/// documented on [`crate::profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct CameraProfile {
    #[serde(default = "default_thumbnail_width")]
    pub width: u32,
    #[serde(default = "default_thumbnail_height")]
    pub height: u32,
    #[serde(default = "default_thumbnail_fps")]
    pub fps: u32,
    #[serde(default = "default_thumbnail_bitrate_kbps")]
    pub bitrate_kbps: u32,
}

impl Default for CameraProfile {
    fn default() -> Self {
        Self {
            width: default_thumbnail_width(),
            height: default_thumbnail_height(),
            fps: default_thumbnail_fps(),
            bitrate_kbps: default_thumbnail_bitrate_kbps(),
        }
    }
}

/// Camera capture/encode settings. Mirrors the Python `CameraConfig`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CameraConfig {
    /// "csi" | "usb" | "ip" device hint, or a device path / RTSP URL.
    #[serde(default = "default_source")]
    pub source: String,
    /// Wire codec. `"h264"` is the only value the encoder builder accepts —
    /// `crate::encoder::validate_codec` refuses anything else and the pipeline
    /// reports `UnsupportedCodec` rather than publishing an untuned stream onto
    /// the H.264-framed radio leg.
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
    /// The non-hero encoder profile (`video.camera.thumbnail:`). The fields
    /// above ARE the hero profile; this is what the drone falls back to while
    /// the operator's attention is on a different aircraft.
    #[serde(default)]
    pub thumbnail: CameraProfile,
    /// Clockwise image rotation in degrees — 0 | 90 | 180 | 270, default 0.
    /// This is an IMAGE transform applied before encode, distinct from the
    /// coarse physical-mount `orientation` metadata (which the pipeline ignores).
    #[serde(default = "default_rotation")]
    pub rotation: u32,
    /// Mirror the image horizontally before encode (default false).
    #[serde(default = "default_flip")]
    pub hflip: bool,
    /// Mirror the image vertically before encode (default false).
    #[serde(default = "default_flip")]
    pub vflip: bool,
    /// Encoder selection override: "auto" (default, probe the board) |
    /// "omx" (Allwinner OMX hardware) | "v4l2m2m" (ffmpeg V4L2-M2M hardware) |
    /// "software" (libx264). "auto" routes a vendor/Allwinner board to the OMX
    /// GStreamer hardware encoder when `omxh264videoenc` is present, else falls
    /// back to the probed ffmpeg/gstreamer path.
    #[serde(default = "default_encoder")]
    pub encoder: String,
    /// Keyframe (GOP) interval in frames, `0..=30`; 0 (default) lets the encoder
    /// pick a short low-latency GOP (0.5 s at the configured fps) so radio FEC
    /// recovers fast. An explicit value is honoured as `-g` / `key-int-max`.
    #[serde(
        default = "default_keyframe_interval",
        deserialize_with = "bounded_keyframe_interval"
    )]
    pub keyframe_interval: u32,
    /// `video.camera.passthrough`: for a network source that already publishes
    /// H.264, copy it (`-c:v copy`) instead of decoding and re-encoding. The
    /// camera's own GOP and bitrate then reach the radio leg unchanged, so it
    /// is opt-in. Ignored for local cameras. Default false.
    #[serde(default)]
    pub passthrough: bool,
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
            thumbnail: CameraProfile::default(),
            rotation: default_rotation(),
            hflip: default_flip(),
            vflip: default_flip(),
            encoder: default_encoder(),
            keyframe_interval: default_keyframe_interval(),
            passthrough: false,
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

    /// The camera id of a `framebus://<id>` source — a camera the vision
    /// engine captures, encoded from the frame bus — or `None`.
    pub fn framebus_camera(&self) -> Option<&str> {
        ados_config::cameras::framebus_camera_id(&self.source)
    }

    /// Load the `/video/camera` slice of the effective agent config (schema
    /// defaults overlaid with the file). A malformed file or slice is logged by
    /// the slice loader and yields the defaults, so config loading never blocks
    /// the pipeline.
    pub fn load_from(path: &std::path::Path) -> Self {
        ados_config::schema::load_slice(path, "/video/camera").unwrap_or_default()
    }
}
