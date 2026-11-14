//! What the engine knows of the camera roster: the cameras it is configured
//! to capture, and each camera's `cameras[]` mount, stamped on its frames and
//! detection batches.

use std::collections::HashMap;

use ados_protocol::framebus::{
    Detection, DetectionBatch, FrameDescriptor, TIMEBASE_SIM, VISION_DETECTION_VERSION,
};

use super::VisionEngine;
use crate::depth::geometry::CameraMount;

impl VisionEngine {
    /// Record the cameras this engine is configured to capture, so a lease on
    /// one is taken before its source delivers (after a restart, or while its
    /// stream is down and reopening).
    pub fn set_capture_cameras(&self, ids: &[String]) {
        *self
            .capture_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ids.to_vec();
    }

    /// Whether the engine captures `camera_id`: it has a ring, or it is
    /// configured and has not delivered a frame yet.
    pub async fn captures(&self, camera_id: &str) -> bool {
        let configured = self
            .capture_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|id| id == camera_id);
        configured || self.has_ring(camera_id).await
    }

    /// Set each camera's `cameras[]` mount (once, at start). A camera absent
    /// from the map has no mount to stamp.
    pub fn configure_camera_mounts(&self, mounts: HashMap<String, CameraMount>) {
        *self
            .camera_mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mounts;
    }

    /// `camera_id`'s configured mount; empty for an unconfigured camera.
    pub(super) fn camera_mount(&self, camera_id: &str) -> CameraMount {
        self.camera_mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(camera_id)
            .copied()
            .unwrap_or_default()
    }

    /// The `vision.detection` batch of `model_id`'s `detections` on the frame
    /// `desc` names: the frame's capture times (host monotonic, and the
    /// simulation clock for a simulated frame) and its camera's configured
    /// mount.
    pub fn detection_batch(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        detections: Vec<Detection>,
    ) -> DetectionBatch {
        let mount = self.camera_mount(&desc.camera_id);
        DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: model_id.to_string(),
            camera_id: desc.camera_id.clone(),
            frame_id: desc.frame_id,
            ts_ms: (desc.ts_ns / 1_000_000) as i64,
            ts_sensor_ms: desc
                .ts_sensor_ns
                .filter(|_| desc.timebase == TIMEBASE_SIM)
                .map(|ns| (ns / 1_000_000) as i64),
            hfov_deg: mount.fov_deg,
            mount_pitch_deg: mount.mount_pitch_deg,
            // The vision frame's pixel size the boxes are in, so the overlay
            // scales to it.
            frame_width: desc.width,
            frame_height: desc.height,
            detections,
        }
    }
}
