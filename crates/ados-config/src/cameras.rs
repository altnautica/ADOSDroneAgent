//! The top-level `cameras[]` list: one entry per physical camera on the node.
//!
//! One list feeds both camera consumers. The video pipeline (`ados-video`)
//! reads every entry as a stream leg; the vision engine (`ados-vision`) owns
//! the entries marked `capture: true` and is then the only process that opens
//! their device or URL. A captured camera is still previewable: the video
//! pipeline reads it back off the shared-memory frame bus through the
//! `framebus://<camera_id>` source instead of opening the device a second time.
//!
//! `role` is the rig role from the capture contract (`front_left`,
//! `front_right`, `left`, `right`, `down`, `up`, `back`, `nav`, `texture`,
//! `primary`) or a stream label (`eo`, `ir`, ...). The video pipeline's
//! transport primary is the entry whose role is `primary`, else the first.
//!
//! This module holds the shared pieces: the capture enums, the vision engine's
//! view of an entry ([`CaptureCamera`]) and the install-time migration of the
//! retired `video.cameras` / `vision.cameras` lists
//! ([`migrate_legacy_camera_lists`]). The migration runs from the installer's
//! config step only, never on a service's startup path.

use ados_protocol::framebus::FrameFormat;
use serde::{Deserialize, Serialize};
use serde_norway::{Mapping, Value};

/// The source scheme naming a camera the vision engine owns.
pub const FRAMEBUS_SCHEME: &str = "framebus://";

/// The `framebus://<camera_id>` source the video pipeline encodes a captured
/// camera from.
pub fn framebus_source(camera_id: &str) -> String {
    format!("{FRAMEBUS_SCHEME}{camera_id}")
}

/// The camera id of a `framebus://<camera_id>` source, or `None` for any other
/// source (a device path, a hint, a network URL) or an empty id.
pub fn framebus_camera_id(source: &str) -> Option<&str> {
    source
        .trim()
        .strip_prefix(FRAMEBUS_SCHEME)
        .filter(|id| !id.is_empty())
}

/// Where a captured camera's exposure trigger records come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraTrigger {
    /// Free-running; frames carry no trigger sequence.
    #[default]
    None,
    /// The sensor hub's trigger outputs (`trigger` frames, stream `cam<channel>`).
    Hub,
    /// The flight controller's camera trigger (`CAMERA_FEEDBACK`, stream `cam0`).
    Fc,
}

/// The pixel format a captured camera is delivered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureFormat {
    Gray8,
    Gray16,
    #[default]
    Rgb24,
    Nv12,
    Yuv420p,
    #[serde(alias = "bayer")]
    BayerRggb8,
    BayerRggb16,
}

impl CaptureFormat {
    /// The frame-bus format frames of this camera are published in.
    pub fn frame_format(self) -> FrameFormat {
        match self {
            CaptureFormat::Gray8 => FrameFormat::Gray8,
            CaptureFormat::Gray16 => FrameFormat::Gray16,
            CaptureFormat::Rgb24 => FrameFormat::Rgb24,
            CaptureFormat::Nv12 => FrameFormat::Nv12,
            CaptureFormat::Yuv420p => FrameFormat::Yuv420p,
            CaptureFormat::BayerRggb8 => FrameFormat::BayerRggb8,
            CaptureFormat::BayerRggb16 => FrameFormat::BayerRggb16,
        }
    }
}

fn default_id() -> String {
    "main".to_string()
}
fn default_source() -> String {
    "csi".to_string()
}
fn default_true() -> bool {
    true
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

/// The vision engine's view of one `cameras[]` entry. Defaults match the video
/// pipeline's leg defaults, so both consumers read an entry the same way.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CaptureCamera {
    #[serde(default = "default_id")]
    pub id: String,
    /// A V4L2 device path (`/dev/video0`) or an `rtsp://` URL.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The vision engine owns this camera's device or URL.
    #[serde(default)]
    pub capture: bool,
    #[serde(default)]
    pub format: CaptureFormat,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Manual exposure in microseconds (at least 1); absent leaves the
    /// driver's auto exposure in charge.
    #[serde(default, deserialize_with = "positive_exposure")]
    pub exposure_us: Option<u32>,
    /// Sensor gain control value written to the driver; absent leaves it.
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub trigger: CameraTrigger,
    /// The hub trigger output wired to this camera. Absent is the entry's index
    /// in `cameras[]`, the order a simulated rig numbers its cameras in.
    #[serde(default)]
    pub trigger_channel: Option<u8>,
}

/// `exposure_us`, refusing 0 (the Python model's `ge=1`).
fn positive_exposure<'de, D>(d: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<u32>::deserialize(d)? {
        Some(0) => Err(serde::de::Error::custom("exposure_us must be at least 1")),
        v => Ok(v),
    }
}

/// A `cameras[]` entry that does not validate, skipped by every consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCamera {
    /// Position in the list.
    pub index: usize,
    /// The entry's `id` when it has a string one, else `cameras[<index>]`.
    pub id: String,
    pub error: String,
}

/// The video-plane keys of an entry, typed exactly as the video pipeline
/// reads them, so an entry is valid for every consumer or for none.
#[derive(Deserialize)]
#[allow(dead_code)]
struct VideoPlane {
    codec: Option<String>,
    bitrate_kbps: Option<u32>,
    name: Option<String>,
    orientation: Option<String>,
    purpose: Option<Vec<String>>,
    owner: Option<String>,
    fov_deg: Option<f32>,
    mount_pitch_deg: Option<f32>,
    calibration: Option<String>,
    #[serde(rename = "match")]
    camera_match: Option<MatchShape>,
    rotation: Option<u32>,
    hflip: Option<bool>,
    vflip: Option<bool>,
    encoder: Option<EncoderChoice>,
    /// Keyframe interval in frames, `0..=30` (the encoder's GOP ceiling and
    /// the Python model's bound).
    #[serde(default, deserialize_with = "bounded_keyframe_interval")]
    keyframe_interval: Option<u32>,
}

/// The largest `keyframe_interval` an entry may declare.
pub const MAX_KEYFRAME_INTERVAL: u32 = 30;

fn bounded_keyframe_interval<'de, D>(d: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<u32>::deserialize(d)? {
        Some(v) if v > MAX_KEYFRAME_INTERVAL => Err(serde::de::Error::custom(format!(
            "keyframe_interval must be at most {MAX_KEYFRAME_INTERVAL}"
        ))),
        v => Ok(v),
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct MatchShape {
    usb: Option<String>,
    csi_sensor: Option<String>,
    csi_port: Option<u32>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "lowercase")]
enum EncoderChoice {
    Auto,
    Omx,
    V4l2m2m,
    Software,
}

/// The validated `cameras[]` list.
#[derive(Debug, Clone, Default)]
pub struct CameraEntries {
    /// Each valid entry with its position in the list, as the raw mapping
    /// (every consumer deserializes its own view from it).
    pub valid: Vec<(usize, Value, CaptureCamera)>,
    pub invalid: Vec<InvalidCamera>,
}

/// Invalid entries already warned about, so a config re-read on every
/// request logs each problem once.
static WARNED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Validate the `cameras[]` list in `text` entry by entry. One bad entry is
/// skipped (and warned about once) without taking the others down; a
/// missing list, or a document that does not parse, has no entries.
pub fn camera_entries(text: &str) -> CameraEntries {
    #[derive(Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        cameras: Option<Value>,
    }
    let raw: Raw = crate::yaml_or_default(text, "cameras");
    let mut out = CameraEntries::default();
    let list = match raw.cameras {
        None | Some(Value::Null) => return out,
        Some(Value::Sequence(list)) => list,
        Some(_) => {
            warn_once("cameras: not a list; no camera is configured".to_string());
            return out;
        }
    };
    for (index, entry) in list.into_iter().enumerate() {
        match check_entry(&entry) {
            Ok(mut c) => {
                c.trigger_channel = c.trigger_channel.or_else(|| u8::try_from(index).ok());
                out.valid.push((index, entry, c));
            }
            Err(error) => {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("cameras[{index}]"));
                warn_once(format!("cameras[{index}] ({id}) skipped: {error}"));
                out.invalid.push(InvalidCamera { index, id, error });
            }
        }
    }
    out
}

/// Check one `cameras[]` entry against every consumer's view of it (the vision
/// engine's [`CaptureCamera`] and the video pipeline's keys). The entry's
/// `trigger_channel` is left as written (not resolved from its position).
pub fn check_entry(entry: &Value) -> Result<CaptureCamera, String> {
    serde_norway::from_value::<CaptureCamera>(entry.clone())
        .and_then(|c| serde_norway::from_value::<VideoPlane>(entry.clone()).map(|_| c))
        .map_err(|e| e.to_string())
}

/// [`check_entry`] for an entry held as JSON (an HTTP or socket body). JSON
/// `null` members are treated as absent, the way a cleared key is persisted.
pub fn check_entry_json(entry: &serde_json::Value) -> Result<CaptureCamera, String> {
    let mut entry = entry.clone();
    if let Some(obj) = entry.as_object_mut() {
        obj.retain(|_, v| !v.is_null());
    }
    let yaml = serde_norway::to_value(&entry).map_err(|e| e.to_string())?;
    check_entry(&yaml)
}

fn warn_once(msg: String) {
    if WARNED.lock().insert(msg.clone()) {
        tracing::warn!(problem = %msg, "camera_config_invalid");
    }
}

/// Every valid entry of the `cameras[]` list in `text`, trigger channels
/// resolved.
fn camera_list(text: &str) -> Vec<CaptureCamera> {
    camera_entries(text)
        .valid
        .into_iter()
        .map(|(_, _, c)| c)
        .collect()
}

/// The enabled `capture: true` entries of the `cameras[]` list in `text`, each
/// with its trigger channel resolved. A missing or malformed list is empty.
pub fn capture_cameras_from(text: &str) -> Vec<CaptureCamera> {
    camera_list(text)
        .into_iter()
        .filter(|c| c.capture && c.enabled)
        .collect()
}

/// What the `cameras[]` list asks the node to run, for the service gates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeclaredCameras {
    /// Enabled `capture: true` entries: the vision engine must run, and the
    /// video pipeline encodes them from the frame bus.
    pub captured: usize,
    /// An enabled, not captured entry with a network URL source: the video
    /// pipeline pulls it with no local camera node present.
    pub network: bool,
}

/// Summarise the `cameras[]` list in `text`.
pub fn declared_cameras_from(text: &str) -> DeclaredCameras {
    let mut out = DeclaredCameras::default();
    for c in camera_list(text).into_iter().filter(|c| c.enabled) {
        if c.capture {
            out.captured += 1;
        } else {
            let s = c.source.trim();
            out.network |= ["rtsp://", "rtsps://", "http://", "https://"]
                .iter()
                .any(|p| s.starts_with(p));
        }
    }
    out
}

fn key(name: &str) -> Value {
    Value::String(name.to_string())
}

/// Remove `section.cameras` from the root mapping, returning the list when it
/// was a sequence. The section itself stays, even when it is left empty.
fn take_legacy_list(root: &mut Mapping, section: &str) -> Option<Option<Vec<Value>>> {
    let map = root.get_mut(section)?.as_mapping_mut()?;
    let old = map.remove("cameras")?;
    Some(match old {
        Value::Sequence(seq) => Some(seq),
        _ => None,
    })
}

/// A legacy `vision.cameras` entry folded into the unified list. A `capture`
/// entry marks the same-id leg `capture: true` (taking its device path when
/// the leg has no source) or becomes a new captured entry; a `tap` entry is
/// dropped, because the frame tap is configured by `video.vision` and was the
/// only thing such an entry could name.
fn fold_vision_entry(list: &mut Vec<Value>, entry: &Value) {
    let Some(entry) = entry.as_mapping() else {
        return;
    };
    let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("tap");
    if kind != "capture" {
        return;
    }
    let Some(id) = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let device = entry.get("device_path").and_then(Value::as_str);
    let existing = list
        .iter_mut()
        .filter_map(Value::as_mapping_mut)
        .find(|leg| leg.get("id").and_then(Value::as_str).unwrap_or("main") == id);
    if let Some(leg) = existing {
        leg.insert(key("capture"), Value::Bool(true));
        if let (None, Some(d)) = (leg.get("source"), device) {
            leg.insert(key("source"), key(d));
        }
        return;
    }
    let mut leg = Mapping::new();
    leg.insert(key("id"), key(id));
    if let Some(d) = device {
        leg.insert(key("source"), key(d));
    }
    leg.insert(key("capture"), Value::Bool(true));
    list.push(Value::Mapping(leg));
}

/// Rewrite the retired `video.cameras` and `vision.cameras` lists into the
/// top-level `cameras[]` (pure, in memory). Returns whether `doc` changed.
///
/// A config that already carries a top-level `cameras` keeps it as is and only
/// loses the retired keys. Otherwise the `video.cameras` legs move verbatim and
/// the `vision.cameras` entries fold in per [`fold_vision_entry`]. Running it
/// again on its own output changes nothing.
pub fn migrate_legacy_camera_lists(doc: &mut Value) -> bool {
    let Some(root) = doc.as_mapping_mut() else {
        return false;
    };
    let video = take_legacy_list(root, "video");
    let vision = take_legacy_list(root, "vision");
    if video.is_none() && vision.is_none() {
        return false;
    }
    if root.get("cameras").is_some() {
        return true;
    }
    let mut list = video.flatten().unwrap_or_default();
    for entry in vision.flatten().unwrap_or_default() {
        fold_vision_entry(&mut list, &entry);
    }
    if !list.is_empty() {
        root.insert(key("cameras"), Value::Sequence(list));
    }
    true
}

#[cfg(test)]
#[path = "cameras_tests.rs"]
mod tests;
