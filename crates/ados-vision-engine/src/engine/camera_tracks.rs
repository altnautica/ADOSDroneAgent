//! One camera's trackers: the single-object lock and the multi-object tracker.

use ados_protocol::framebus::{BoundingBox, Detection, TrackState};
use ados_vision::mot::{describe, Features, MotConfig, MultiObjectTracker, FEAT_DIM};
use ados_vision::tracker::{SingleObjectTracker, TrackUpdate, TrackerConfig};

use super::tracking::bbox_iou;

/// One camera's trackers, under one lock: the single-object lock that owns
/// the designated subject's lock state (coast, ambiguity hold, crop pass), and
/// the multi-object tracker that owns every published `track_id`.
///
/// The lock is the authority on which box is the subject: on a measured frame
/// the multi-object tracker is made to agree with it
/// ([`MultiObjectTracker::bind`]), so the subject's box always carries the id
/// [`super::VisionEngine::designate`] returned and no id ever sits on two
/// boxes.
pub(super) struct CameraTracks {
    pub sot: SingleObjectTracker,
    pub mot: MultiObjectTracker,
    /// The lock's single-object id and the published id it maps to.
    bridge: Option<(u64, u32)>,
    /// This frame's appearance descriptors, reused across frames.
    features: Vec<Option<Features>>,
}

impl CameraTracks {
    pub(super) fn new(cfg: TrackerConfig) -> Self {
        Self {
            sot: SingleObjectTracker::new(cfg),
            mot: MultiObjectTracker::new(MotConfig::default()),
            bridge: None,
            features: Vec::new(),
        }
    }

    /// The camera turned since the last frame: move both trackers. Returns the
    /// lock's shift (see [`SingleObjectTracker::move_with_camera`]).
    pub(super) fn move_with_camera(
        &mut self,
        to_new_frame: impl Fn(f32, f32) -> Option<(f32, f32)>,
    ) -> Option<(f32, f32)> {
        self.mot.warp(&to_new_frame);
        self.sot.move_with_camera(&to_new_frame)
    }

    pub(super) fn lock_id(&self) -> Option<u64> {
        let sot = self.sot.current_id()?;
        self.bridge
            .filter(|&(s, _)| s == sot)
            .map(|(_, w)| u64::from(w))
    }

    pub(super) fn designate(&mut self, target: &Detection) -> Option<u64> {
        let bbox = target.bbox.as_ref()?;
        let sot = self.sot.designate(target)?;
        let id = self.mot.designate(bbox, &target.class_label);
        self.bridge = Some((sot, id));
        Some(u64::from(id))
    }

    pub(super) fn reset_lock(&mut self, cfg: TrackerConfig) {
        self.sot = SingleObjectTracker::new(cfg);
        self.mot.clear_designation();
        self.bridge = None;
    }

    /// Advance the multi-object tracker, describing every box from `pixels`
    /// while it holds a designated subject to re-identify.
    pub(super) fn update_mot(
        &mut self,
        detections: &[Detection],
        pixels: Option<(&[u8], u32, u32)>,
        ts_ns: Option<u64>,
    ) {
        self.features.clear();
        if let (Some((frame, w, h)), Some(_)) = (pixels, self.mot.designated_id()) {
            self.features.extend(detections.iter().map(|d| {
                let mut f = [0.0; FEAT_DIM];
                d.bbox
                    .as_ref()
                    .is_some_and(|b| describe(frame, w, h, b, &mut f))
                    .then_some(f)
            }));
        }
        self.mot.update(detections, &self.features, ts_ns);
    }

    /// Stamp this frame's tracking onto its detections. Every detection the
    /// multi-object tracker took carries its `track_state` and, once confirmed,
    /// its `track_id` (and the subject's `appearance_id`); one it did not take
    /// carries none. The lock's object additionally carries `lock_state` and
    /// `assoc_confidence`, on its matching input box (measured frame) or on the
    /// lock's predicted box appended to the batch (coast or ambiguity hold),
    /// which then alone carries the lock's id. Only an operator-designated lock
    /// presents a lock state: an automatically seeded one (the most-confident
    /// auto-pick) is tracked but never presented as locked, so a follow
    /// behavior only ever engages a target the operator actually designated.
    pub(super) fn stamp(
        &mut self,
        mut detections: Vec<Detection>,
        update: TrackUpdate,
    ) -> Vec<Detection> {
        let lock = update.detection.map(|locked| {
            let at = update
                .measured
                .then(|| {
                    locked
                        .bbox
                        .as_ref()
                        .and_then(|b| best_overlap_index(&detections, b))
                })
                .flatten();
            (locked, at)
        });
        // Settle the lock's published id first: binding it may move ids
        // between tracks. The report carries the lock's id even on the frame
        // that declares it lost, when the tracker no longer holds one.
        let mut lock_id = None;
        let sot = lock
            .as_ref()
            .and_then(|(l, _)| l.track_id)
            .or(update.track_id);
        if let (Some((_, at)), Some(sot)) = (&lock, sot) {
            let bridged = self.bridge.filter(|&(s, _)| s == sot).map(|(_, w)| w);
            lock_id = match (*at, bridged) {
                (Some(i), Some(w)) => {
                    self.mot.bind(i, w);
                    Some(w)
                }
                (Some(i), None) => self.mot.confirm_at(i),
                (None, b) => b,
            };
            if let Some(w) = lock_id {
                self.bridge = Some((sot, w));
            }
        }
        for (i, d) in detections.iter_mut().enumerate() {
            let out = self.mot.output(i);
            d.track_id = out.and_then(|o| o.track_id).map(u64::from);
            d.track_state = out.map(|o| o.state);
            d.appearance_id = out.and_then(|o| o.appearance_id);
        }
        let Some((mut locked, at)) = lock else {
            return detections;
        };
        if !update.operator_designated {
            locked.lock_state = None;
        }
        locked.track_id = lock_id.map(u64::from);
        match at {
            Some(i) => {
                let d = &mut detections[i];
                d.track_id = locked.track_id;
                d.lock_state = locked.lock_state;
                d.assoc_confidence = locked.assoc_confidence;
            }
            None => {
                if locked.track_id.is_some() {
                    for d in detections
                        .iter_mut()
                        .filter(|d| d.track_id == locked.track_id)
                    {
                        d.track_id = None;
                    }
                }
                locked.track_state = Some(TrackState::Lost);
                locked.appearance_id = lock_id.and_then(|w| self.mot.appearance_id(w));
                detections.push(locked);
            }
        }
        detections
    }
}

/// The index of the detection with the greatest IoU against `bbox`, or `None`
/// when the list is empty or nothing overlaps at all.
fn best_overlap_index(detections: &[Detection], bbox: &BoundingBox) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, d) in detections.iter().enumerate() {
        // A box-less percept can never overlap the tracked box.
        let Some(db) = d.bbox.as_ref() else {
            continue;
        };
        let i_o_u = bbox_iou(db, bbox);
        if i_o_u > 0.0 && best.is_none_or(|(_, b)| i_o_u > b) {
            best = Some((i, i_o_u));
        }
    }
    best.map(|(i, _)| i)
}
