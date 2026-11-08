//! The camera's own turn, taken out of the tracker's motion.
//!
//! A camera fixed to the vehicle turns with it. Between two detector batches
//! (about 3 Hz) a vehicle that pitches to fly, or yaws to keep a subject in
//! view, moves everything in the image by up to a few hundred pixels, far
//! more than the subject itself moves. To a box tracker that looks like the
//! subject jumping: the box lands outside the gate, the lock coasts and has
//! to re-confirm. Before each batch the engine asks the flight controller's
//! attitude history how the vehicle was turned when this frame and the
//! previous batch's frame were exposed, and moves the camera's track to
//! where that turn put its line of sight
//! ([`ados_vision::tracker::SingleObjectTracker::move_with_camera`]). The
//! tracker then predicts, gates and crops around where the subject is now.
//!
//! Only a camera with a `cameras[]` entry (its field of view and mount
//! pitch) is moved; one without, or a frame the attitude history does not
//! cover, is tracked as before.

use std::collections::HashMap;

use ados_protocol::framebus::{FrameDescriptor, TIMEBASE_SIM};

use super::VisionEngine;
use crate::depth::feed::{Attitude, FrameTime};
use crate::depth::geometry::CameraGeometry;

/// What moving tracks with the camera needs, set once at start.
#[derive(Debug, Clone, Default)]
pub struct CameraMotionSetup {
    /// Each configured camera's mount and field of view.
    pub geometries: HashMap<String, CameraGeometry>,
    /// The flight controller runs on the simulation clock, the one a
    /// simulated camera stamps its frames' exposure with (a software-in-the-
    /// loop autopilot driven by the simulator). A frame with a simulation
    /// timestamp then reads the attitude at its exposure directly.
    pub fc_on_sim_clock: bool,
}

/// Per camera: the attitude at the last batch's frame, and the batches the
/// track was moved on.
#[derive(Debug, Default)]
pub(super) struct CameraMotion {
    setup: CameraMotionSetup,
    last: HashMap<String, Attitude>,
    moves: HashMap<String, u64>,
}

/// When `desc` was exposed, on the clock the attitude history reads.
pub(super) fn frame_time(desc: &FrameDescriptor, fc_on_sim_clock: bool) -> FrameTime {
    match desc.ts_sensor_ns {
        Some(ns) if fc_on_sim_clock && desc.timebase == TIMEBASE_SIM => {
            FrameTime::FlightController(ns as f64 / 1e6)
        }
        _ => FrameTime::Host(desc.ts_ns as f64 / 1e6),
    }
}

impl VisionEngine {
    fn camera_motion(&self) -> std::sync::MutexGuard<'_, CameraMotion> {
        self.camera_motion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Configure moving tracks with the camera (once, at start).
    pub fn configure_camera_motion(&self, setup: CameraMotionSetup) {
        let mut m = self.camera_motion();
        m.setup = setup;
        m.last.clear();
    }

    /// Batches on which `camera_id`'s track was moved with the camera.
    pub(super) fn turn_moves(&self, camera_id: &str) -> u64 {
        self.camera_motion()
            .moves
            .get(camera_id)
            .copied()
            .unwrap_or(0)
    }

    /// Before the batch on `desc`: move the camera's track by the vehicle's
    /// turn since the previous batch's frame. Returns the shift applied, in
    /// pixels; `None` when there was nothing to move (no tracker, no geometry
    /// for the camera, no attitude for this frame or the last, no track).
    pub(super) async fn move_track_with_camera(
        &self,
        desc: &FrameDescriptor,
    ) -> Option<(f32, f32)> {
        if !self.tracker_enabled || desc.width == 0 || desc.height == 0 {
            return None;
        }
        let (geometry, from, to) = {
            let mut m = self.camera_motion();
            let geometry = *m.setup.geometries.get(&desc.camera_id)?;
            let at = frame_time(desc, m.setup.fc_on_sim_clock);
            let to = self.range_feed.attitude_at(at);
            let from = match to {
                Some(a) => m.last.insert(desc.camera_id.clone(), a),
                None => m.last.remove(&desc.camera_id),
            };
            (geometry, from?, to?)
        };
        let (w, h) = (desc.width as f32, desc.height as f32);
        let proj = geometry.projector(w / h);
        let shift = self
            .trackers
            .lock()
            .await
            .get_mut(&desc.camera_id)?
            .move_with_camera(|x, y| {
                proj.turned(x / w, y / h, &from, &to)
                    .map(|(u, v)| (u * w, v * h))
            })?;
        *self
            .camera_motion()
            .moves
            .entry(desc.camera_id.clone())
            .or_default() += 1;
        Some(shift)
    }
}
