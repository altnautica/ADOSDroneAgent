//! The engine's socket paths, each under the configured socket directory.

use super::VisionConfig;

impl VisionConfig {
    /// The `vision.sock` path under the configured socket directory.
    pub fn vision_socket_path(&self) -> String {
        format!("{}/vision.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The `vision-detections.sock` path: a last-state broadcast socket the
    /// API process subscribes to so the browser can receive live detection
    /// batches over a WebSocket. Distinct from `vision.sock` (the request /
    /// response plugin bridge) — this one is broadcast-only and carries
    /// length-prefixed msgpack [`ados_protocol::framebus::DetectionBatch`].
    pub fn detections_socket_path(&self) -> String {
        format!(
            "{}/vision-detections.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The `vision-frames.sock` path: a last-state broadcast socket that
    /// re-publishes every frame descriptor so an on-box service (the world-model
    /// capture service) can subscribe and map the ring the descriptor names.
    /// Carries length-prefixed msgpack [`ados_protocol::framebus::FrameDescriptor`].
    pub fn frames_socket_path(&self) -> String {
        format!(
            "{}/vision-frames.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The `vision-depth.sock` path: the broadcast of every depth grid and
    /// landing-zone frame ([`ados_protocol::vision_depth::DepthRecord`]).
    pub fn depth_socket_path(&self) -> String {
        format!(
            "{}/vision-depth.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The MAVLink broadcast socket the depth fit reads attitude and the
    /// downward rangefinder from.
    pub fn mavlink_socket_path(&self) -> String {
        format!(
            "{}/{}",
            self.socket_dir.trim_end_matches('/'),
            ados_protocol::paths::MAVLINK_SOCK
        )
    }

    /// The accelerator sidecar socket path the RKNN backend talks to.
    pub fn rknn_socket_path(&self) -> String {
        format!("{}/vision-rknn.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The accelerator sidecar socket path the Hailo backend talks to (Pi + AI HAT).
    pub fn hailo_socket_path(&self) -> String {
        format!(
            "{}/vision-hailo.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }
}
