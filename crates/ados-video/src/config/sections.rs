//! The `video.vision:`, `video.wfb:` and `video.recording:` sub-blocks.

use serde::Deserialize;

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
/// the wfb radio fan-out. The decoder runs only while a vision engine is
/// connected to the tap socket.
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

/// The `video.recording:` sub-block.
///
/// This block existed long before anything read it: its only consumer was a
/// snapshots-subdirectory derivation, while `max_duration_minutes` and the
/// enable flag were read by nobody and no `record:` key reached mediamtx on
/// either profile. The native fMP4 recorder is what makes it live — `enabled`
/// now drives mediamtx's `pathDefaults.record`, and the retention keys drive
/// `recordSegmentDuration` / `recordDeleteAfter`.
///
/// `max_duration_minutes` is deliberately NOT carried over. It described a
/// single whole-stream capture with a hard stop, which is not what continuous
/// segmented recording does: a segment duration plus a retention window
/// expresses the same intent without an arbitrary cliff that ends a capture
/// mid-flight. Segment duration is fixed at one minute (it bounds the blast
/// radius of a corrupt file) and retention is the operator-visible knob.
#[derive(Debug, Clone, Deserialize)]
pub struct RecordingConfig {
    /// Record continuously while the stream is up. With this on, the on-disk
    /// segment set IS the pre-roll ring: an arbitrary window before any event
    /// is extractable after the fact through mediamtx's playback server, so
    /// pre-roll costs disk bounded by retention and zero RAM.
    #[serde(default)]
    pub enabled: bool,
    /// Root of the recording tree.
    #[serde(default = "default_recordings_path")]
    pub path: String,
    /// How long segments are kept before mediamtx deletes them. Time-based
    /// only, so the space-based reclaim is a separate and necessary guard: a
    /// high-bitrate day fills the volume long before this elapses.
    #[serde(default = "default_record_retention")]
    pub retention: String,
}

fn default_recordings_path() -> String {
    crate::mediamtx::RECORDINGS_ROOT.to_string()
}

fn default_record_retention() -> String {
    "24h".to_string()
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_recordings_path(),
            retention: default_record_retention(),
        }
    }
}

impl RecordingConfig {
    /// The renderer inputs this block resolves to.
    ///
    /// `on_segment_complete` is deliberately `None`. mediamtx's
    /// `runOnRecordSegmentComplete` hook would be an attractive place to drive
    /// space-based reclaim off a real segment-close event, but it has to name a
    /// binary that exists on BOTH profiles, and pointing it at a missing one
    /// makes mediamtx exec-fail on every segment boundary. The reclaim owner is
    /// already the supervisor janitor: it runs on both profiles and already does
    /// oldest-first deletion over the recordings tree, so driving a second
    /// reclaimer from this hook would put two processes in a race to delete the
    /// same files. One reclaimer, and it is the janitor.
    pub fn to_params(&self) -> crate::mediamtx::RecordingParams {
        crate::mediamtx::RecordingParams {
            enabled: self.enabled,
            root: self.path.clone(),
            segment_duration: crate::mediamtx::RECORD_SEGMENT_DURATION.to_string(),
            delete_after: self.retention.clone(),
            on_segment_complete: None,
        }
    }
}
