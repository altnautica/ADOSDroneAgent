//! Detection demand: the detector runs on a camera only while a consumer needs
//! it there.
//!
//! Capture is never gated (the rings, the frame bus, video and every other
//! frame consumer keep running); only inference is. A consumer holds a lease
//! per `(owner, camera)` that expires on its own, so a consumer that goes away
//! without releasing stops costing CPU within one hold. Repeating a request
//! renews the lease; a zero hold releases it. An operator designation grants
//! the [`DESIGNATE_OWNER`] lease on its camera for the configured hold.
//!
//! Mode [`DetectionDemandMode::Always`] runs detection on every camera, as the
//! engine did before the gate (bench and diagnostic use).
//!
//! Times are milliseconds on the engine's monotonic clock
//! ([`ados_protocol::framebus::mono_ns`]), passed in so the logic is testable.

use std::collections::BTreeMap;

use ados_protocol::vision_rpc::{DemandKind, DetectionDemandReply};
use ados_protocol::vision_status::{CameraDemandStatus, DemandLeaseStatus, DetectionDemandStatus};
use serde::Deserialize;

use super::*;

/// The lease owner an operator designation holds on its camera.
pub const DESIGNATE_OWNER: &str = "designate";

/// `vision.detection_demand`: when the detector runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionDemandMode {
    /// Only on a camera some consumer holds a lease on.
    #[default]
    OnDemand,
    /// On every captured camera.
    Always,
}

impl DetectionDemandMode {
    /// The config and status spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnDemand => "on_demand",
            Self::Always => "always",
        }
    }
}

/// How the engine decides where the detector runs and what it keeps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectionPolicy {
    pub mode: DetectionDemandMode,
    /// The lease a designation grants its camera (`vision.designate_hold_s`).
    pub designate_hold_ms: u64,
    /// Detections whose box covers more than this fraction of the frame are
    /// dropped before tracking and publishing (`vision.max_box_area_frac`).
    /// `>= 1` keeps every box.
    pub max_box_area_frac: f32,
}

impl Default for DetectionPolicy {
    /// The shipped defaults.
    fn default() -> Self {
        Self {
            mode: DetectionDemandMode::OnDemand,
            designate_hold_ms: 30_000,
            max_box_area_frac: 0.5,
        }
    }
}

/// The live leases: (kind, camera) → owner → expiry. Detection, depth and
/// surfaces share the map, each kind holding its cameras independently.
#[derive(Debug, Default)]
pub(super) struct DemandLeases {
    by_camera: BTreeMap<(DemandKind, String), BTreeMap<String, u64>>,
}

impl DemandLeases {
    /// Drop every lease that has run out by `now_ms`.
    fn prune(&mut self, now_ms: u64) {
        self.by_camera.retain(|_, owners| {
            owners.retain(|_, expiry| *expiry > now_ms);
            !owners.is_empty()
        });
    }

    /// Hold `kind` on `camera` for `owner` until `now_ms + hold_ms`, or
    /// release that owner's lease with `hold_ms == 0`.
    fn set(&mut self, kind: DemandKind, camera: &str, owner: &str, hold_ms: u64, now_ms: u64) {
        self.prune(now_ms);
        let key = (kind, camera.to_string());
        if hold_ms == 0 {
            if let Some(owners) = self.by_camera.get_mut(&key) {
                owners.remove(owner);
                if owners.is_empty() {
                    self.by_camera.remove(&key);
                }
            }
        } else {
            self.by_camera
                .entry(key)
                .or_default()
                .insert(owner.to_string(), now_ms.saturating_add(hold_ms));
        }
    }

    /// When the last live `kind` lease on `camera` ends, if any is live.
    fn until(&mut self, kind: DemandKind, camera: &str, now_ms: u64) -> Option<u64> {
        self.prune(now_ms);
        self.by_camera
            .get(&(kind, camera.to_string()))
            .and_then(|owners| owners.values().copied().max())
    }

    /// The live `kind` leases on `camera`, by owner, with the time left on
    /// each.
    fn leases(&mut self, kind: DemandKind, camera: &str, now_ms: u64) -> Vec<DemandLeaseStatus> {
        self.prune(now_ms);
        self.by_camera
            .get(&(kind, camera.to_string()))
            .map(|owners| {
                owners
                    .iter()
                    .map(|(owner, expiry)| DemandLeaseStatus {
                        owner: owner.clone(),
                        remaining_ms: expiry - now_ms,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every camera holding a live lease of one of `kinds`.
    fn cameras(&mut self, kinds: &[DemandKind], now_ms: u64) -> Vec<String> {
        self.prune(now_ms);
        let mut ids: Vec<String> = self
            .by_camera
            .keys()
            .filter(|(k, _)| kinds.contains(k))
            .map(|(_, c)| c.clone())
            .collect();
        ids.dedup();
        ids
    }
}

impl VisionEngine {
    /// The configured demand mode.
    pub fn detection_mode(&self) -> DetectionDemandMode {
        self.policy.mode
    }

    pub(super) fn leases(&self) -> std::sync::MutexGuard<'_, DemandLeases> {
        // A panic while holding this lock leaves the map consistent (every
        // mutation is a single insert/remove), so a poisoned lock is reused.
        self.demand
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the detector should run on `camera_id` now. The capture loop
    /// asks this before scheduling inference on a frame.
    pub fn detection_wanted(&self, camera_id: &str, now_ms: u64) -> bool {
        self.policy.mode == DetectionDemandMode::Always
            || self
                .leases()
                .until(DemandKind::Detection, camera_id, now_ms)
                .is_some()
    }

    /// Whether a depth grid and landing zones are wanted on `camera_id` now:
    /// `(depth, surfaces)`.
    pub fn depth_wanted(&self, camera_id: &str, now_ms: u64) -> (bool, bool) {
        let mut l = self.leases();
        (
            l.until(DemandKind::Depth, camera_id, now_ms).is_some(),
            l.until(DemandKind::Surfaces, camera_id, now_ms).is_some(),
        )
    }

    /// `camera_id`'s detection demand state now.
    pub fn detection_demand(&self, camera_id: &str, now_ms: u64) -> DetectionDemandReply {
        self.demand_state(DemandKind::Detection, camera_id, now_ms)
    }

    /// `camera_id`'s demand state for `kind` now. Depth and surfaces run only
    /// while a depth model is loaded, so a lease without one is not running.
    pub fn demand_state(
        &self,
        kind: DemandKind,
        camera_id: &str,
        now_ms: u64,
    ) -> DetectionDemandReply {
        let until_ms = self.leases().until(kind, camera_id, now_ms);
        let running = match kind {
            DemandKind::Detection => {
                self.policy.mode == DetectionDemandMode::Always || until_ms.is_some()
            }
            DemandKind::Depth | DemandKind::Surfaces => {
                until_ms.is_some() && self.depth_model_ready()
            }
        };
        DetectionDemandReply {
            kind,
            camera_id: camera_id.to_string(),
            detecting: running,
            until_ms,
        }
    }

    /// Take, renew (`hold_ms > 0`) or release (`hold_ms == 0`) `owner`'s
    /// detection lease on `camera_id`, and return the camera's demand state
    /// after it. The caller has checked the camera is one the engine captures.
    pub fn request_detection(
        &self,
        camera_id: &str,
        owner: &str,
        hold_ms: u64,
        now_ms: u64,
    ) -> DetectionDemandReply {
        self.request_demand(DemandKind::Detection, camera_id, owner, hold_ms, now_ms)
    }

    /// [`Self::request_detection`] for a lease of any kind.
    pub fn request_demand(
        &self,
        kind: DemandKind,
        camera_id: &str,
        owner: &str,
        hold_ms: u64,
        now_ms: u64,
    ) -> DetectionDemandReply {
        self.leases().set(kind, camera_id, owner, hold_ms, now_ms);
        self.demand_state(kind, camera_id, now_ms)
    }

    /// An operator designated a target on `camera_id`: hold detection on it for
    /// the configured designation hold.
    pub(super) fn grant_designation(&self, camera_id: &str, now_ms: u64) {
        self.leases().set(
            DemandKind::Detection,
            camera_id,
            DESIGNATE_OWNER,
            self.policy.designate_hold_ms,
            now_ms,
        );
    }

    /// The status sidecar's `detection` block: the mode and, for each of
    /// `cameras` plus any other leased camera, whether detection runs on it,
    /// its live leases and its designated-track crop counters.
    pub fn detection_status(&self, cameras: &[String], now_ms: u64) -> DetectionDemandStatus {
        let mut leases = self.leases();
        let mut ids: Vec<String> = cameras.to_vec();
        for id in leases.cameras(&[DemandKind::Detection], now_ms) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let always = self.policy.mode == DetectionDemandMode::Always;
        let cameras = ids
            .into_iter()
            .map(|id| {
                let held = leases.leases(DemandKind::Detection, &id, now_ms);
                let crop = self.crop_counts(&id);
                let status = CameraDemandStatus {
                    detecting: always || !held.is_empty(),
                    leases: held,
                    crop_runs: crop.runs,
                    crop_hits: crop.hits,
                    turn_moves: self.turn_moves(&id),
                };
                (id, status)
            })
            .collect();
        DetectionDemandStatus {
            mode: self.policy.mode.as_str().to_string(),
            cameras,
        }
    }

    /// Each of `cameras` plus any camera with a depth or surfaces lease, with
    /// its live depth and surfaces leases (owners prefixed with the kind).
    pub(super) fn depth_leases(
        &self,
        cameras: &[String],
        now_ms: u64,
    ) -> Vec<(String, Vec<DemandLeaseStatus>, bool, bool)> {
        let mut leases = self.leases();
        let mut ids: Vec<String> = cameras.to_vec();
        for id in leases.cameras(&[DemandKind::Depth, DemandKind::Surfaces], now_ms) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids.into_iter()
            .map(|id| {
                let mut held = Vec::new();
                let mut on = [false; 2];
                for (slot, kind) in [DemandKind::Depth, DemandKind::Surfaces]
                    .into_iter()
                    .enumerate()
                {
                    for l in leases.leases(kind, &id, now_ms) {
                        on[slot] = true;
                        held.push(DemandLeaseStatus {
                            owner: format!("{}:{}", kind.as_str(), l.owner),
                            remaining_ms: l.remaining_ms,
                        });
                    }
                }
                (id, held, on[0], on[1])
            })
            .collect()
    }
}

/// Drop every detection whose box covers more than `max_frac` of the
/// `width` x `height` frame (the box clipped to the frame first). A detection
/// without a box, a frame of unknown size, or `max_frac >= 1` keeps everything.
pub(super) fn drop_oversized_boxes(
    detections: &mut Vec<Detection>,
    width: u32,
    height: u32,
    max_frac: f32,
) {
    if width == 0 || height == 0 || max_frac.is_nan() || max_frac >= 1.0 {
        return;
    }
    let (fw, fh) = (width as f32, height as f32);
    let frame_area = fw * fh;
    detections.retain(|d| {
        let Some(b) = d.bbox else {
            return true;
        };
        let w = ((b.x + b.width).min(fw) - b.x.max(0.0)).max(0.0);
        let h = ((b.y + b.height).min(fh) - b.y.max(0.0)).max(0.0);
        w * h <= max_frac * frame_area
    });
}
