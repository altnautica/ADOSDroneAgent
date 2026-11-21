//! The designated subject's world state (`vision.subject.v1`), estimated
//! once per detector batch from the tracked batch the engine publishes.
//!
//! Per batch, for a camera whose tracker holds an operator-designated lock:
//! the lock's box, measured on this frame and locked (not ambiguous), becomes
//! a [`Sighting`]: lines of sight through its top and bottom edges, turned
//! into local NED with the vehicle's attitude at the frame's exposure, from
//! the vehicle's position then (`LOCAL_POSITION_NED`). Its range cues are the
//! box height, the newest metric depth frame of the camera within
//! [`DEPTH_MAX_SKEW_NS`] of the frame, and the ground under the vehicle from
//! the downward rangefinder, taken as the ground under the subject (flat
//! ground). A lock that coasts or holds on ambiguity only moves the estimate
//! on in time. The estimate is published after every batch while it tracks.
//!
//! A camera without a `cameras[]` entry (no mount, no field of view), or a
//! frame the flight controller's attitude and position histories do not
//! cover, publishes nothing: a world position from a guessed geometry would
//! be a wrong one.

use std::collections::HashMap;

use ados_protocol::framebus::{Detection, FrameDescriptor, LockState, TrackState};
use ados_protocol::vision_depth_frame::DepthFrame;
use ados_protocol::vision_subject::SubjectState;
use ados_vision::subject::{
    depth_cue, GroundCue, OpenSpace, Sighting, SubjectConfig, SubjectEstimator,
};
use tokio::sync::broadcast;

use super::VisionEngine;
use crate::depth::feed::Attitude;
use crate::depth::geometry::Projector;

/// The largest gap between a frame and the depth frame read with it, ns.
const DEPTH_MAX_SKEW_NS: u64 = 250_000_000;

/// One sigma of a detector box edge's jitter, pixels.
const EDGE_JITTER_PX: f32 = 2.0;

/// One sigma of the flat-ground assumption: the ground under the subject
/// against the ground under the vehicle, metres.
const FLAT_GROUND_SIGMA_M: f32 = 0.5;

/// A box edge this close to the frame border, pixels, is taken as cut by it.
const BORDER_PX: f32 = 1.0;

/// States a slow subscriber may fall behind by before it lags.
const SUBJECT_QUEUE: usize = 32;

/// The subject estimates and their broadcast.
pub(super) struct SubjectRuntime {
    estimators: HashMap<String, SubjectEstimator>,
    depth_rx: Option<broadcast::Receiver<DepthFrame>>,
    depth: HashMap<String, DepthFrame>,
    tx: broadcast::Sender<SubjectState>,
}

impl Default for SubjectRuntime {
    fn default() -> Self {
        Self {
            estimators: HashMap::new(),
            depth_rx: None,
            depth: HashMap::new(),
            tx: broadcast::channel(SUBJECT_QUEUE).0,
        }
    }
}

impl SubjectRuntime {
    /// Keep the newest depth frame of every camera.
    fn drain_depth(&mut self) {
        let Some(rx) = &mut self.depth_rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(f) => {
                    self.depth.insert(f.camera_id.clone(), f);
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }
}

/// The designated subject in a tracked batch: its published id, and its box
/// when this frame measured it under a firm lock.
fn designated(detections: &[Detection]) -> Option<(u32, Option<&Detection>)> {
    let d = detections.iter().find(|d| d.lock_state.is_some())?;
    let id = u32::try_from(d.track_id?).ok()?;
    let measured = d.lock_state == Some(LockState::Locked)
        && d.track_state != Some(TrackState::Lost)
        && d.bbox.is_some();
    Some((id, measured.then_some(d)))
}

impl VisionEngine {
    fn subject_runtime(&self) -> std::sync::MutexGuard<'_, SubjectRuntime> {
        self.subject
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Subscribe to the designated subject's state (`vision.subject.v1`).
    pub fn subscribe_subject(&self) -> broadcast::Receiver<SubjectState> {
        self.subject_runtime().tx.subscribe()
    }

    /// Advance `desc`'s camera's subject estimate on its tracked batch
    /// `detections` (stamped `stamp_ns` on the tracker's frame clock) and
    /// broadcast the state. Returns the state published, if any.
    pub(super) fn update_subject(
        &self,
        desc: &FrameDescriptor,
        detections: &[Detection],
        stamp_ns: u64,
    ) -> Option<SubjectState> {
        let Some((id, seen)) = designated(detections) else {
            self.subject_runtime().estimators.remove(&desc.camera_id);
            return None;
        };
        let depth_rx = self
            .subject_runtime()
            .depth_rx
            .is_none()
            .then(|| self.subscribe_depth_frames());
        let seen = seen.and_then(|d| self.seen(desc, d, stamp_ns));
        let mut rt = self.subject_runtime();
        if depth_rx.is_some() {
            rt.depth_rx = depth_rx;
        }
        rt.drain_depth();
        let sighting = seen.map(|s| {
            s.with_depth(
                rt.depth
                    .get(&desc.camera_id)
                    .filter(|f| f.stamp_ns.abs_diff(desc.ts_ns) <= DEPTH_MAX_SKEW_NS),
            )
        });
        let est = rt
            .estimators
            .entry(desc.camera_id.clone())
            .or_insert_with(|| SubjectEstimator::new(id, SubjectConfig::default()));
        if est.id() != id {
            *est = SubjectEstimator::new(id, SubjectConfig::default());
        }
        match &sighting {
            Some(s) => {
                est.observe(s);
            }
            None => est.coast(stamp_ns),
        }
        let state = est.state(&desc.camera_id, &OpenSpace)?;
        let _ = rt.tx.send(state.clone());
        Some(state)
    }

    /// The subject boxed by `d` in `desc`'s frame. `None` without the
    /// camera's geometry or the vehicle's pose at the frame.
    fn seen(&self, desc: &FrameDescriptor, d: &Detection, stamp_ns: u64) -> Option<Seen> {
        let b = d.bbox.as_ref()?;
        let (w, h) = (desc.width as f32, desc.height as f32);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let (geometry, at) = self.camera_frame(desc)?;
        let attitude = self.range_feed.attitude_at(at)?;
        let camera = self.range_feed.position_at(at)?;
        let proj = geometry.projector(w / h);
        let u = (b.x + 0.5 * b.width) / w;
        let ground = self
            .range_feed
            .snapshot(desc.ts_ns / 1_000_000)
            .down_m
            .map(|down| GroundCue {
                down_m: camera[2] + down * attitude.roll.cos() * attitude.pitch.cos(),
                sigma_m: FLAT_GROUND_SIGMA_M,
            });
        Some(Seen {
            sighting: Sighting {
                stamp_ns,
                camera_ned: camera,
                top_ray: proj.ray_ned(u, b.y / h, &attitude),
                foot_ray: proj.ray_ned(u, (b.y + b.height) / h, &attitude),
                full_height: b.y > BORDER_PX && b.y + b.height < h - BORDER_PX,
                edge_sigma_rad: EDGE_JITTER_PX * geometry.hfov_deg.to_radians() / w,
                depth: None,
                ground,
            },
            bbox: [b.x / w, b.y / h, (b.x + b.width) / w, (b.y + b.height) / h],
            proj,
            attitude,
        })
    }
}

/// A sighting before its depth cue, with what reads the cue.
struct Seen {
    sighting: Sighting,
    /// The box, normalised `[x0, y0, x1, y1]`.
    bbox: [f32; 4],
    proj: Projector,
    attitude: Attitude,
}

impl Seen {
    /// The sighting, with the depth cue `frame` holds on the subject.
    fn with_depth(self, frame: Option<&DepthFrame>) -> Sighting {
        let Self {
            mut sighting,
            bbox,
            proj,
            attitude,
        } = self;
        sighting.depth =
            frame.and_then(|f| depth_cue(f, bbox, |u, v| Some(proj.ray_ned(u, v, &attitude))));
        sighting
    }
}

#[cfg(test)]
#[path = "subject_tests.rs"]
mod tests;
