//! Which cameras the engine runs and how each one's frames are sourced.
//!
//! The engine owns every enabled `capture: true` entry of the top-level
//! `cameras[]` list: an `rtsp://` source is a simulated rig camera
//! ([`RtspSource`]), anything else a local V4L2 device ([`V4l2Source`]). When
//! the video pipeline's frame tap is on (`video.vision.enabled`), the flight
//! camera it encodes is read off the tap as camera `main`.

use ados_config::cameras::CaptureCamera;

use crate::camera_control::CameraControls;
use crate::config::VisionConfig;
use crate::source::{
    AnySource, ControlQueue, RtspSettings, RtspSource, SimClock, TapSource, V4l2Settings,
    V4l2Source,
};
use crate::trigger::{trigger_stream, CameraTriggers, TriggerFeed, TriggerJoiner};

/// The camera id the video pipeline's tap is read as.
pub const TAP_CAMERA_ID: &str = "main";

/// One camera the engine runs.
#[derive(Debug, Clone, PartialEq)]
pub enum CameraSpec {
    /// The video pipeline's raw frame tap.
    Tap { id: String, socket: String },
    /// A camera this engine owns.
    Owned(CaptureCamera),
}

impl CameraSpec {
    pub fn id(&self) -> &str {
        match self {
            CameraSpec::Tap { id, .. } => id,
            CameraSpec::Owned(c) => &c.id,
        }
    }
}

/// The cameras of `config`: every owned camera, then the tap when it is on and
/// no owned camera already uses its id.
pub fn camera_specs(config: &VisionConfig) -> Vec<CameraSpec> {
    let mut specs: Vec<CameraSpec> = config
        .cameras
        .iter()
        .cloned()
        .map(CameraSpec::Owned)
        .collect();
    if let Some(socket) = &config.tap_sink {
        if !specs.iter().any(|s| s.id() == TAP_CAMERA_ID) {
            specs.push(CameraSpec::Tap {
                id: TAP_CAMERA_ID.to_string(),
                socket: socket.clone(),
            });
        }
    }
    specs
}

/// Whether any owned camera takes trigger records off the sensor bus.
pub fn needs_triggers(specs: &[CameraSpec]) -> bool {
    specs
        .iter()
        .any(|s| matches!(s, CameraSpec::Owned(c) if trigger_stream(c.trigger, 0).is_some()))
}

/// Whether a capture source is a network stream rather than a local device.
pub fn is_rtsp(source: &str) -> bool {
    let s = source.trim();
    s.starts_with("rtsp://") || s.starts_with("rtsps://")
}

/// Builds each attempt's frame source from its spec, and registers where the
/// camera's control requests go.
#[derive(Clone, Default)]
pub struct SourceFactory {
    pub triggers: TriggerFeed,
    pub sim: SimClock,
    pub controls: CameraControls,
}

impl SourceFactory {
    /// The trigger join of an owned camera, when it is triggered.
    fn triggers_for(&self, cam: &CaptureCamera, rtsp: bool) -> Option<CameraTriggers> {
        let stream = trigger_stream(cam.trigger, cam.trigger_channel.unwrap_or(0))?;
        let joiner = if rtsp {
            // A simulated camera's frame sequence is the hub's trigger sequence.
            TriggerJoiner::same_sequence()
        } else {
            TriggerJoiner::learning(1_000_000_000 / u64::from(cam.fps.max(1)) / 2)
        };
        Some(CameraTriggers::new(self.triggers.clone(), stream, joiner))
    }

    /// A fresh source for `spec`. Only a camera configured `capture: true` may
    /// be controlled: a V4L2 one's requests are routed to the new source's
    /// capture thread, a simulated rig has no controls, and the tap (the
    /// video pipeline's camera) or any non-capture entry is refused.
    pub fn open(&self, spec: &CameraSpec) -> AnySource {
        match spec {
            CameraSpec::Tap { id, socket } => {
                self.controls.register_not_capture(id);
                AnySource::Tap(TapSource::new(id.clone(), socket.clone()))
            }
            CameraSpec::Owned(cam) if is_rtsp(&cam.source) => {
                if cam.capture {
                    self.controls.register_not_v4l2(&cam.id);
                } else {
                    self.controls.register_not_capture(&cam.id);
                }
                AnySource::Rtsp(Box::new(RtspSource::new(
                    cam.id.clone(),
                    RtspSettings {
                        url: cam.source.trim().to_string(),
                        width: cam.width,
                        height: cam.height,
                        format: cam.format.frame_format(),
                    },
                    self.triggers_for(cam, true),
                    self.sim.clone(),
                )))
            }
            CameraSpec::Owned(cam) => AnySource::V4l2(Box::new(V4l2Source::new(
                cam.id.clone(),
                V4l2Settings {
                    device: cam.source.trim().to_string(),
                    width: cam.width,
                    height: cam.height,
                    fps: cam.fps,
                    format: cam.format.frame_format(),
                    exposure_us: cam.exposure_us,
                    gain: cam.gain,
                },
                self.triggers_for(cam, false),
                self.control_queue(cam),
            ))),
        }
    }

    /// The control queue of a V4L2 camera, when it is a capture camera.
    fn control_queue(&self, cam: &CaptureCamera) -> Option<ControlQueue> {
        if cam.capture {
            Some(self.controls.register_capture(&cam.id))
        } else {
            self.controls.register_not_capture(&cam.id);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FrameSource;

    fn config(text: &str) -> VisionConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, text).unwrap();
        VisionConfig::load_from(&path)
    }

    const RIG: &str = "video:\n  vision:\n    enabled: true\n    sink: /run/ados/vision-tap-main.sock\ncameras:\n  - id: main\n    source: /dev/video0\n  - id: cam_fl\n    source: rtsp://192.168.1.50:8554/we_v0_cam_fl\n    capture: true\n    trigger: hub\n  - id: nav\n    source: /dev/video2\n    format: gray8\n    capture: true\n    trigger: fc\n  - id: free\n    source: /dev/video4\n    capture: true\n";

    #[test]
    fn owned_cameras_and_the_tap_are_run() {
        let specs = camera_specs(&config(RIG));
        let ids: Vec<&str> = specs.iter().map(CameraSpec::id).collect();
        assert_eq!(ids, ["cam_fl", "nav", "free", "main"]);
        assert!(
            matches!(&specs[3], CameraSpec::Tap { socket, .. } if socket == "/run/ados/vision-tap-main.sock")
        );
        assert!(needs_triggers(&specs));
        // No tap and no captured camera: nothing to run.
        assert!(camera_specs(&config("agent:\n  profile: drone\n")).is_empty());
    }

    #[test]
    fn each_source_kind_is_chosen_from_the_entry() {
        let specs = camera_specs(&config(RIG));
        let factory = SourceFactory::default();
        let kinds: Vec<&str> = specs
            .iter()
            .map(|s| match factory.open(s) {
                AnySource::Rtsp(src) => {
                    assert_eq!(src.camera_id(), "cam_fl");
                    "rtsp"
                }
                AnySource::V4l2(_) => "v4l2",
                AnySource::Tap(_) => "tap",
            })
            .collect();
        assert_eq!(kinds, ["rtsp", "v4l2", "v4l2", "tap"]);
        let CameraSpec::Owned(nav) = &specs[1] else {
            panic!()
        };
        assert!(factory.triggers_for(nav, false).is_some());
        let CameraSpec::Owned(free) = &specs[2] else {
            panic!()
        };
        assert!(factory.triggers_for(free, false).is_none());
    }

    #[tokio::test]
    async fn each_opened_source_registers_its_control_route() {
        use ados_protocol::camera_control::{reason, CameraControlRequest};
        let factory = SourceFactory::default();
        let sources: Vec<AnySource> = camera_specs(&config(RIG))
            .iter()
            .map(|s| factory.open(s))
            .collect();
        let why = |id: &str| {
            let req = CameraControlRequest {
                camera_id: id.into(),
                gain: Some(2.0),
                ..Default::default()
            };
            let controls = factory.controls.clone();
            async move { controls.set(&req).await.reason }
        };
        // The simulated rig camera has no device controls; the tap is the
        // video pipeline's camera, not a capture camera.
        assert_eq!(why("cam_fl").await.as_deref(), Some(reason::NOT_V4L2));
        assert_eq!(
            why("main").await.as_deref(),
            Some(reason::NOT_CAPTURE_CAMERA)
        );
        assert_eq!(why("other").await.as_deref(), Some(reason::UNKNOWN_CAMERA));
        // An owned entry without `capture: true` is refused too.
        let CameraSpec::Owned(mut free) = camera_specs(&config(RIG)).remove(2) else {
            panic!()
        };
        free.capture = false;
        let _uncaptured = factory.open(&CameraSpec::Owned(free));
        assert_eq!(
            why("free").await.as_deref(),
            Some(reason::NOT_CAPTURE_CAMERA)
        );
        // The config keeps the ids of the entries the engine does not own.
        assert_eq!(config(RIG).other_camera_ids, ["main"]);
        // A V4L2 source routes to its capture thread; once the source is gone
        // there is no open device to apply on.
        drop(sources);
        assert_eq!(why("nav").await.as_deref(), Some(reason::NOT_STREAMING));
    }
}
