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
    /// Manual exposure in microseconds; absent leaves the driver's auto
    /// exposure in charge.
    #[serde(default)]
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

/// The enabled `capture: true` entries of the `cameras[]` list in `text`, each
/// with its trigger channel resolved. A missing or malformed list is empty.
pub fn capture_cameras_from(text: &str) -> Vec<CaptureCamera> {
    #[derive(Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        cameras: Vec<CaptureCamera>,
    }
    let raw: Raw = crate::yaml_or_default(text, "cameras");
    raw.cameras
        .into_iter()
        .enumerate()
        .filter(|(_, c)| c.capture && c.enabled)
        .map(|(i, mut c)| {
            c.trigger_channel = c
                .trigger_channel
                .or_else(|| u8::try_from(i).ok());
            c
        })
        .collect()
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
    let Some(id) = entry.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return;
    };
    let device = entry.get("device_path").and_then(Value::as_str);
    let existing = list.iter_mut().filter_map(Value::as_mapping_mut).find(|leg| {
        leg.get("id").and_then(Value::as_str).unwrap_or("main") == id
    });
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
