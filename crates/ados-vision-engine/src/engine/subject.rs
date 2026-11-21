//! The designated subject's world state (`vision.subject.v1`), estimated
//! once per detector batch from the tracked batch the engine publishes.
//!
//! Only a designation the operator made is located: a plugin's
//! ([`DesignationOrigin::Plugin`]) is tracked and locked, but its subject
//! never gets a world estimate. Per batch, for a camera whose multi-object
//! tracker holds an operator's designation: the lock's box, measured on this
//! frame and locked (not ambiguous), becomes a [`Sighting`]: lines of sight
//! through its top and bottom edges, turned into local NED with the vehicle's
//! attitude at the frame's exposure, from the vehicle's position then
//! (`LOCAL_POSITION_NED`). Its range cues are the box height, the newest
//! metric depth frame of the camera within [`DEPTH_MAX_SKEW_NS`] of the
//! frame, and the ground under the vehicle from the downward rangefinder,
//! taken as the ground under the subject (flat ground). A lock that coasts,
//! holds on ambiguity or lost the subject only moves the estimate on in time.
//! The camera's queued follow-beacon fixes ([`super::beacon`]) no newer than
//! the frame fuse first, in time order. Predictions are walked through the
//! voxel map while it is live ([`super::voxmap_space`]). The estimate is
//! published after every batch while it tracks; when the designation ends,
//! changes, or the estimate loses the subject, one released record follows,
//! and it is what the last-state socket then replays.
//!
//! A camera without a `cameras[]` entry (no mount, no field of view), or a
//! frame the flight controller's attitude and position histories do not
//! cover, publishes nothing: a world position from a guessed geometry would
//! be a wrong one.

use std::collections::{HashMap, VecDeque};

use ados_protocol::framebus::{Detection, FrameDescriptor, LockState, TrackState};
use ados_protocol::vision_depth_frame::DepthFrame;
use ados_protocol::vision_subject::{SubjectRecord, SubjectReleased, SubjectState};
use ados_vision::subject::{
    depth_cue, BeaconFix, GroundCue, Sighting, SubjectConfig, SubjectEstimator,
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

/// Records a slow subscriber may fall behind by before it lags.
const SUBJECT_QUEUE: usize = 32;

/// Beacon fixes held per camera for the next sighting; the oldest goes first.
const MAX_PENDING_BEACONS: usize = 16;

/// Who made a designation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignationOrigin {
    /// The operator, through the control plane's click-to-follow.
    Operator,
    /// A plugin, through `vision.track.designate`.
    Plugin,
}

/// The subject a camera's multi-object tracker holds designated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Designation {
    pub id: u32,
    pub origin: DesignationOrigin,
}

/// One camera's subject estimate, the beacon fixes waiting for it, and
/// whether its subject is on the topic.
struct CameraSubject {
    est: SubjectEstimator,
    beacons: VecDeque<BeaconFix>,
    published: bool,
}

/// The subject estimates and their broadcast.
pub(super) struct SubjectRuntime {
    cameras: HashMap<String, CameraSubject>,
    depth_rx: Option<broadcast::Receiver<DepthFrame>>,
    depth: HashMap<String, DepthFrame>,
    /// The tuning every new estimate is built with.
    cfg: SubjectConfig,
    tx: broadcast::Sender<SubjectRecord>,
}

impl Default for SubjectRuntime {
    fn default() -> Self {
        Self {
            cameras: HashMap::new(),
            depth_rx: None,
            depth: HashMap::new(),
            cfg: SubjectConfig::default(),
            tx: broadcast::channel(SUBJECT_QUEUE).0,
        }
    }
}

impl SubjectRuntime {
    /// Hold a beacon fix for every camera's next sighting not older than it;
    /// each camera's estimate weighs it on its own. With no estimate running
    /// the fix is dropped: a beacon never starts one.
    pub(super) fn queue_beacon(&mut self, fix: BeaconFix) {
        for cam in self.cameras.values_mut() {
            if cam.beacons.len() == MAX_PENDING_BEACONS {
                cam.beacons.pop_front();
            }
            let at = cam.beacons.partition_point(|b| b.stamp_ns <= fix.stamp_ns);
            cam.beacons.insert(at, fix);
        }
    }

    /// End `camera_id`'s estimate: when its subject was on the topic, say so.
    fn release(&mut self, camera_id: &str, stamp_ns: u64) {
        if let Some(cam) = self.cameras.remove(camera_id) {
            self.released(camera_id, &cam, stamp_ns);
        }
    }

    fn released(&self, camera_id: &str, cam: &CameraSubject, stamp_ns: u64) {
        if cam.published {
            let _ = self.tx.send(SubjectRecord::Released(SubjectReleased {
                camera_id: camera_id.to_owned(),
                id: cam.est.id(),
                stamp_ns,
            }));
        }
    }

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

/// The lock's box in a tracked batch when it is the designated subject's and
/// this frame measured it under a firm lock.
fn measured(detections: &[Detection], id: u32) -> Option<&Detection> {
    detections.iter().find(|d| {
        d.lock_state == Some(LockState::Locked)
            && d.track_id == Some(u64::from(id))
            && d.track_state != Some(TrackState::Lost)
            && d.bbox.is_some()
    })
}

impl VisionEngine {
    pub(super) fn subject_runtime(&self) -> std::sync::MutexGuard<'_, SubjectRuntime> {
        self.subject
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Subscribe to the designated subject's records (`vision.subject.v1`).
    pub fn subscribe_subject(&self) -> broadcast::Receiver<SubjectRecord> {
        self.subject_runtime().tx.subscribe()
    }

    /// Set the subject estimate's tuning (`vision.subject`, once, at start);
    /// an estimate already running keeps its own.
    pub fn configure_subject(&self, cfg: SubjectConfig) {
        self.subject_runtime().cfg = cfg;
    }

    /// Advance `desc`'s camera's subject estimate on its tracked batch
    /// `detections` (stamped `stamp_ns` on the tracker's frame clock) and
    /// broadcast the state. `designation` is the subject the camera's
    /// multi-object tracker holds designated: an operator's is estimated for
    /// as long as it holds, through vision losing the subject (when the beacon
    /// may carry it); with none, or a plugin's, the estimate is released.
    /// Returns the state published, if any.
    pub(super) fn update_subject(
        &self,
        desc: &FrameDescriptor,
        detections: &[Detection],
        designation: Option<Designation>,
        stamp_ns: u64,
    ) -> Option<SubjectState> {
        let Some(id) = designation
            .filter(|d| d.origin == DesignationOrigin::Operator)
            .map(|d| d.id)
        else {
            self.subject_runtime().release(&desc.camera_id, stamp_ns);
            return None;
        };
        let depth_rx = self
            .subject_runtime()
            .depth_rx
            .is_none()
            .then(|| self.subscribe_depth_frames());
        let seen = measured(detections, id).and_then(|d| self.seen(desc, d, stamp_ns));
        let now_ms = ados_protocol::framebus::mono_ns() / 1_000_000;
        // The map is refreshed before the subject lock is taken: its file I/O
        // never holds up the estimate or the beacon.
        let space = self.voxmap_view(now_ms);
        let mut guard = self.subject_runtime();
        let rt = &mut *guard;
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
        if rt
            .cameras
            .get(&desc.camera_id)
            .is_some_and(|c| c.est.id() != id)
        {
            // A new designation: the old subject and its beacon evidence go.
            rt.release(&desc.camera_id, stamp_ns);
        }
        let cfg = rt.cfg;
        let cam = rt
            .cameras
            .entry(desc.camera_id.clone())
            .or_insert_with(|| CameraSubject {
                est: SubjectEstimator::new(id, cfg),
                beacons: VecDeque::new(),
                published: false,
            });
        while let Some(fix) = cam.beacons.front().filter(|f| f.stamp_ns <= stamp_ns) {
            cam.est.fuse_beacon(fix);
            cam.beacons.pop_front();
        }
        match &sighting {
            Some(s) => {
                cam.est.observe(s);
            }
            None => cam.est.coast(stamp_ns),
        }
        let Some(state) = cam.est.state(&desc.camera_id, &space) else {
            // Lost past the coast limit: released, and estimated afresh
            // should vision find the subject again.
            rt.release(&desc.camera_id, stamp_ns);
            return None;
        };
        cam.published = true;
        let _ = rt.tx.send(SubjectRecord::State(state.clone()));
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
