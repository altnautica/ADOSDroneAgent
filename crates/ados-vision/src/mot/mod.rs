//! A multi-object tracker: every object the detector reports keeps a stable
//! id from frame to frame.
//!
//! [`crate::tracker`] holds one locked subject. This module tracks everyone,
//! so a consumer can tell people apart, predict where each is going, and
//! re-find the designated subject after it was out of view.
//!
//! ## Per frame
//!
//! 1. Every track's box is predicted with a constant-velocity Kalman filter
//!    over centre, aspect ratio and height (and, when detections carry one, a
//!    second filter over the world position). A camera that turned between
//!    frames first moves every track by that turn ([`MultiObjectTracker::warp`]).
//! 2. Association, in passes, each solved as a minimum-cost assignment:
//!    - confirmed and lost tracks against high-score detections, by IoU of the
//!      predicted box plus a motion-direction consistency term (the direction
//!      from the track's last observation to the detection against its recent
//!      observed motion); a lost track may also take a detection inside its
//!      grown position uncertainty that its drifted box no longer overlaps;
//!    - the confirmed tracks left over against low-score detections (a person
//!      partly hidden scores low but is still there: ByteTrack's second pass);
//!    - tracks still left over against high-score detections by IoU of their
//!      last observed box, which re-finds a track whose prediction drifted
//!      while it was unseen;
//!    - tentative tracks against what remains.
//! 3. A track re-found after a gap is rewound to its last observation and
//!    replayed along a straight path to the new one, so the velocity it
//!    coasted on does not carry over (observation-centric re-update).
//! 4. Lifecycle: a new track is tentative and carries no id until it was
//!    measured on [`MotConfig::birth_hits`] consecutive frames; a confirmed
//!    track that misses a frame turns lost, and is removed after
//!    [`MotConfig::max_age_s`] unseen. Ids are minted in increasing order and
//!    never reused.
//! 5. Re-identification of the designated subject ([`MultiObjectTracker::designate`]):
//!    see `reid`. A subject whose track was removed is re-found by appearance
//!    alone, and never when another visible person scores close to it.
//!
//! The tracker is pure and deterministic: no clock, no camera, no threads, and
//! after warm-up no allocation per frame (buffers are reused; a new class
//! label is the one allocation).

mod appearance;
mod assign;
mod config;
mod identity;
mod kalman;
mod reid;
mod track;
mod warp;

pub use appearance::{describe, similarity, Classifier, Features, FEAT_DIM};
pub use config::MotConfig;
pub use kalman::Box4;
pub use warp::Homography;

use ados_protocol::framebus::{BoundingBox, Detection, TrackState};

use assign::Assigner;
use kalman::WorldKf;
use reid::Subject;
use track::Track;

/// One input detection as association sees it.
#[derive(Debug, Clone, Copy)]
struct DetView {
    bbox: Box4,
    score: f32,
    class: u16,
    world: Option<[f64; 3]>,
    /// False for a box-less percept or a score below [`MotConfig::low_score`].
    active: bool,
    /// Index of the track that took it this frame.
    track: Option<usize>,
}

/// Reused buffers of the association passes.
#[derive(Debug, Default)]
struct Workspace {
    assigner: Assigner,
    rows: Vec<usize>,
    cols: Vec<usize>,
    pairs: Vec<(usize, usize)>,
}

/// What the tracker says about one input detection after an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackOutput {
    /// The track's id; `None` while the track is tentative.
    pub track_id: Option<u32>,
    pub state: TrackState,
    /// The designated subject's appearance identity, when this is its track.
    pub appearance_id: Option<u32>,
}

/// A read-only view of one track with an id.
#[derive(Debug, Clone, Copy)]
pub struct TrackView<'a> {
    pub id: u32,
    pub class_label: &'a str,
    pub state: TrackState,
    /// The filter's box this frame: the measurement-corrected estimate, or
    /// the prediction while lost.
    pub bbox: BoundingBox,
    /// Centre velocity, pixels per second.
    pub velocity: (f32, f32),
    /// World position and velocity, when the detections carry a position.
    pub world: Option<([f64; 3], [f64; 3])>,
    pub last_seen_s: f64,
    pub designated: bool,
}

/// The multi-object tracker. See the module docs.
#[derive(Debug)]
pub struct MultiObjectTracker {
    cfg: MotConfig,
    tracks: Vec<Track>,
    labels: Vec<String>,
    dets: Vec<DetView>,
    /// Input detection index to the track index that took it.
    assigned: Vec<Option<usize>>,
    ws: Workspace,
    next_id: u32,
    next_appearance_id: u32,
    subject: Option<Subject>,
    now_s: Option<f64>,
}

impl MultiObjectTracker {
    pub fn new(cfg: MotConfig) -> Self {
        Self {
            cfg,
            tracks: Vec::with_capacity(cfg.max_tracks),
            labels: Vec::new(),
            dets: Vec::new(),
            assigned: Vec::new(),
            ws: Workspace::default(),
            next_id: 1,
            next_appearance_id: 1,
            subject: None,
            now_s: None,
        }
    }

    pub fn config(&self) -> &MotConfig {
        &self.cfg
    }

    /// Advance one frame. `features` holds an appearance descriptor per
    /// detection (same order; shorter or empty when there are none), used
    /// only for the designated subject's re-identification. `ts_ns` is the
    /// frame's capture time; `None` advances by [`MotConfig::nominal_dt_s`].
    pub fn update(
        &mut self,
        detections: &[Detection],
        features: &[Option<Features>],
        ts_ns: Option<u64>,
    ) {
        let t = match (ts_ns, self.now_s) {
            (Some(ns), _) => ns as f64 / 1e9,
            (None, Some(last)) => last + f64::from(self.cfg.nominal_dt_s),
            (None, None) => 0.0,
        };
        let dt = self.now_s.map_or(0.0, |last| (t - last).max(0.0)) as f32;
        self.now_s = Some(t);
        for tr in &mut self.tracks {
            tr.predict(dt);
        }
        self.load(detections);
        self.associate();
        self.apply(features, t);
        self.birth(features, t);
        self.learn(features);
        self.reidentify(t);
        self.finish();
    }

    /// The track the detection at `index` of the last update belongs to.
    pub fn output(&self, index: usize) -> Option<TrackOutput> {
        let tr = &self.tracks[(*self.assigned.get(index)?)?];
        Some(TrackOutput {
            track_id: (tr.id != 0).then_some(tr.id),
            state: tr.state,
            appearance_id: self.appearance_id(tr.id),
        })
    }

    /// Every track that carries an id (confirmed or lost).
    pub fn tracks(&self) -> impl Iterator<Item = TrackView<'_>> {
        self.tracks.iter().filter(|t| t.id != 0).map(|t| {
            let b = t.kf.bbox();
            TrackView {
                id: t.id,
                class_label: &self.labels[usize::from(t.class)],
                state: t.state,
                bbox: BoundingBox {
                    x: b.x1,
                    y: b.y1,
                    width: b.width(),
                    height: b.height(),
                },
                velocity: t.kf.velocity(),
                world: t.world.map(|w| (w.position(), w.velocity())),
                last_seen_s: t.last_seen_s,
                designated: self.subject.as_ref().is_some_and(|s| s.id == t.id),
            }
        })
    }

    fn class_index(&mut self, label: &str) -> u16 {
        match self.labels.iter().position(|l| l == label) {
            Some(i) => i as u16,
            None => {
                self.labels.push(label.to_string());
                (self.labels.len() - 1) as u16
            }
        }
    }

    fn load(&mut self, detections: &[Detection]) {
        self.dets.clear();
        for d in detections {
            let class = self.class_index(&d.class_label);
            let (bbox, active) = match &d.bbox {
                Some(b) if b.width > 0.0 && b.height > 0.0 => (
                    Box4 {
                        x1: b.x,
                        y1: b.y,
                        x2: b.x + b.width,
                        y2: b.y + b.height,
                    },
                    d.confidence >= self.cfg.low_score,
                ),
                _ => (Box4::default(), false),
            };
            self.dets.push(DetView {
                bbox,
                score: d.confidence,
                class,
                world: d.world_pos,
                active,
                track: None,
            });
        }
    }

    fn associate(&mut self) {
        let cfg = self.cfg;
        let (tracks, dets, ws) = (&mut self.tracks, &mut self.dets, &mut self.ws);
        let lost = TrackState::Lost;
        pass(
            tracks,
            dets,
            ws,
            |t| t.state != TrackState::Tentative,
            |d| d.score >= cfg.high_score,
            |t, d| {
                let iou = t.pred.iou(&d.bbox);
                let (x, y) = d.bbox.center();
                let ocm = cfg.momentum_weight * t.direction_cost(x, y) * d.score;
                if iou >= cfg.match_iou {
                    return Some(1.0 - iou + ocm);
                }
                if t.state != lost || cfg.lost_gate <= 0.0 {
                    return None;
                }
                let ratio = t.pred.height() / d.bbox.height().max(1.0);
                let similar = ratio <= cfg.max_height_ratio && ratio * cfg.max_height_ratio >= 1.0;
                let (px, py) = t.pred.center();
                let near = (x - px).hypot(y - py) <= cfg.lost_max_dist_h * t.pred.height();
                let d2 = t.kf.center_distance2(x, y);
                (similar && near && d2 <= cfg.lost_gate)
                    .then(|| 1.0 + 0.5 * d2 / cfg.lost_gate + ocm)
            },
        );
        pass(
            tracks,
            dets,
            ws,
            |t| t.state != TrackState::Tentative,
            |d| d.score < cfg.high_score,
            |t, d| iou_cost(&t.pred, &d.bbox, cfg.low_match_iou),
        );
        pass(
            tracks,
            dets,
            ws,
            |t| t.state != TrackState::Tentative,
            |d| d.score >= cfg.high_score,
            |t, d| iou_cost(&t.last_obs, &d.bbox, cfg.recovery_iou),
        );
        pass(
            tracks,
            dets,
            ws,
            |t| t.state == TrackState::Tentative,
            |d| d.score >= cfg.high_score,
            |t, d| iou_cost(&t.pred, &d.bbox, cfg.tentative_iou),
        );
    }

    fn apply(&mut self, features: &[Option<Features>], t: f64) {
        let cfg = self.cfg;
        for tr in &mut self.tracks {
            match tr.det {
                Some(di) => {
                    let d = self.dets[di];
                    // A low score is usually an object partly hidden, whose box
                    // covers only its visible part: keep the height the track
                    // knows rather than learn a shrink (and a centre jump) that
                    // is not motion.
                    let meas = if d.score < cfg.high_score {
                        truncated(&tr.pred, &d.bbox)
                    } else {
                        d.bbox
                    };
                    tr.observe(meas, t, d.world, &cfg);
                    if let (true, Some(Some(f))) = (d.score >= cfg.high_score, features.get(di)) {
                        tr.add_features(f);
                    }
                    tr.hits += 1;
                    match tr.state {
                        TrackState::Tentative if tr.hits >= cfg.birth_hits => {
                            tr.id = self.next_id;
                            self.next_id += 1;
                            tr.state = TrackState::Confirmed;
                        }
                        TrackState::Lost => tr.state = TrackState::Confirmed,
                        _ => {}
                    }
                }
                None => match tr.state {
                    TrackState::Tentative => tr.removed = true,
                    TrackState::Confirmed => {
                        tr.state = TrackState::Lost;
                        tr.hits = 0;
                    }
                    TrackState::Lost => {
                        tr.removed = t - tr.last_seen_s > f64::from(cfg.max_age_s);
                    }
                },
            }
        }
    }

    fn birth(&mut self, features: &[Option<Features>], t: f64) {
        let cfg = self.cfg;
        for di in 0..self.dets.len() {
            let d = self.dets[di];
            if !d.active || d.track.is_some() || d.score < cfg.new_track_score {
                continue;
            }
            if self.tracks.len() >= cfg.max_tracks {
                break;
            }
            let mut tr = Track::new(d.bbox, d.class, t, &cfg);
            tr.world = d.world.map(WorldKf::new);
            tr.det = Some(di);
            if let Some(Some(f)) = features.get(di) {
                tr.add_features(f);
            }
            if cfg.birth_hits <= 1 {
                tr.id = self.mint_id();
                tr.state = TrackState::Confirmed;
            }
            self.dets[di].track = Some(self.tracks.len());
            self.tracks.push(tr);
        }
    }

    fn finish(&mut self) {
        self.tracks.retain(|t| !t.removed);
        self.assigned.clear();
        self.assigned.resize(self.dets.len(), None);
        for (ti, tr) in self.tracks.iter().enumerate() {
            if let Some(di) = tr.det {
                self.assigned[di] = Some(ti);
            }
        }
    }

    fn mint_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// The full box of an object whose detected box `b` is cut short by an
/// occluder: when `b` is clearly shorter than the prediction `pred`, extend
/// it to the predicted height from whichever edge agrees with the prediction.
fn truncated(pred: &Box4, b: &Box4) -> Box4 {
    let h = pred.height();
    if b.height() >= 0.85 * h {
        return *b;
    }
    if (b.y1 - pred.y1).abs() <= (b.y2 - pred.y2).abs() {
        Box4 { y2: b.y1 + h, ..*b }
    } else {
        Box4 { y1: b.y2 - h, ..*b }
    }
}

fn iou_cost(a: &Box4, b: &Box4, min_iou: f32) -> Option<f32> {
    let iou = a.iou(b);
    (iou >= min_iou).then_some(1.0 - iou)
}

/// One association pass: every live, unmatched track that `row_ok` admits
/// against every active, unmatched detection that `col_ok` admits, with the
/// pairs `cost` allows, solved for the least total cost. Same class only.
fn pass(
    tracks: &mut [Track],
    dets: &mut [DetView],
    ws: &mut Workspace,
    row_ok: impl Fn(&Track) -> bool,
    col_ok: impl Fn(&DetView) -> bool,
    cost: impl Fn(&Track, &DetView) -> Option<f32>,
) {
    let Workspace {
        assigner,
        rows,
        cols,
        pairs,
    } = ws;
    rows.clear();
    rows.extend(
        (0..tracks.len())
            .filter(|&i| tracks[i].det.is_none() && !tracks[i].removed && row_ok(&tracks[i])),
    );
    cols.clear();
    cols.extend(
        (0..dets.len()).filter(|&i| dets[i].active && dets[i].track.is_none() && col_ok(&dets[i])),
    );
    if rows.is_empty() || cols.is_empty() {
        return;
    }
    assigner.reset(rows.len(), cols.len());
    for (r, &ti) in rows.iter().enumerate() {
        for (c, &di) in cols.iter().enumerate() {
            if tracks[ti].class != dets[di].class {
                continue;
            }
            if let Some(x) = cost(&tracks[ti], &dets[di]) {
                assigner.set(r, c, x);
            }
        }
    }
    assigner.solve(pairs);
    for &(r, c) in pairs.iter() {
        tracks[rows[r]].det = Some(cols[c]);
        dets[cols[c]].track = Some(rows[r]);
    }
}

#[cfg(test)]
mod bench_tests;
#[cfg(test)]
mod reid_tests;
#[cfg(test)]
mod scene_tests;
#[cfg(test)]
mod tests;
