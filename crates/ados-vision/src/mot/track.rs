//! One tracked object: its filters, its recent observations and its running
//! appearance.

use ados_protocol::framebus::TrackState;

use super::appearance::{blend, Features, FEAT_DIM};
use super::kalman::{Box4, ImageKf, WorldKf};
use super::MotConfig;

/// Observations kept for the motion-direction term.
const HISTORY: usize = 5;
/// How many observations back the motion direction is measured over.
const MOMENTUM_SPAN: usize = 3;
/// Most interpolated steps an observation-centric re-update replays.
const MAX_REPLAY_STEPS: f32 = 30.0;

#[derive(Debug, Clone)]
pub(super) struct Track {
    /// Published id; 0 while tentative (an id is minted at confirmation).
    pub id: u32,
    pub class: u16,
    pub state: TrackState,
    pub kf: ImageKf,
    /// This frame's predicted box.
    pub pred: Box4,
    /// Consecutive frames with a measurement.
    pub hits: u32,
    pub born_s: f64,
    pub last_seen_s: f64,
    /// The last measured box and the filter as it stood right after it.
    pub last_obs: Box4,
    kf_at_obs: ImageKf,
    /// Recent measured centres `(x, y)`, oldest first once full.
    history: [(f32, f32); HISTORY],
    history_len: usize,
    pub world: Option<WorldKf>,
    /// Running mean of the descriptors seen on this track.
    pub feat: Features,
    pub feat_n: u32,
    /// The detection index this track took on the current frame.
    pub det: Option<usize>,
    /// Marked for removal at the end of the frame.
    pub removed: bool,
}

impl Track {
    pub fn new(b: Box4, class: u16, t: f64, cfg: &MotConfig) -> Self {
        let kf = ImageKf::new(&b, cfg.accel_noise_h, cfg.meas_noise_h);
        let (cx, cy) = b.center();
        let mut history = [(0.0, 0.0); HISTORY];
        history[0] = (cx, cy);
        Self {
            id: 0,
            class,
            state: TrackState::Tentative,
            kf,
            pred: b,
            hits: 1,
            born_s: t,
            last_seen_s: t,
            last_obs: b,
            kf_at_obs: kf,
            history,
            history_len: 1,
            world: None,
            feat: [0.0; FEAT_DIM],
            feat_n: 0,
            det: None,
            removed: false,
        }
    }

    pub fn predict(&mut self, dt: f32) {
        self.kf.predict(dt);
        self.pred = self.kf.bbox();
        if let Some(w) = &mut self.world {
            w.predict(f64::from(dt));
        }
        self.det = None;
    }

    /// Fuse the measurement `b` taken at `t`. A track coming back from `Lost`
    /// is rewound to its last observation and replayed along a straight
    /// virtual path to `b` (observation-centric re-update), so the velocity it
    /// drifted on while unseen does not survive the re-find.
    pub fn observe(&mut self, b: Box4, t: f64, world: Option<[f64; 3]>, cfg: &MotConfig) {
        let gap = (t - self.last_seen_s) as f32;
        let nominal = cfg.nominal_dt_s.max(1e-3);
        if self.state == TrackState::Lost && gap > 0.0 {
            let steps = (gap / nominal).round().clamp(1.0, MAX_REPLAY_STEPS);
            let dt = gap / steps;
            let mut kf = self.kf_at_obs;
            let n = steps as usize;
            for k in 1..n {
                kf.predict(dt);
                kf.update(&self.last_obs.lerp(&b, k as f32 / steps));
            }
            kf.predict(dt);
            self.kf = kf;
        }
        self.kf.update(&b);
        self.kf_at_obs = self.kf;
        self.last_obs = b;
        self.last_seen_s = t;
        self.push_history(b.center());
        if let Some(p) = world {
            match &mut self.world {
                Some(w) => w.update(p),
                None => self.world = Some(WorldKf::new(p)),
            }
        }
    }

    fn push_history(&mut self, c: (f32, f32)) {
        if self.history_len < HISTORY {
            self.history[self.history_len] = c;
            self.history_len += 1;
        } else {
            self.history.rotate_left(1);
            self.history[HISTORY - 1] = c;
        }
    }

    /// Unit direction of the track's recent observed motion, `None` before
    /// it has moved measurably.
    pub fn momentum(&self) -> Option<(f32, f32)> {
        if self.history_len < 2 {
            return None;
        }
        let last = self.history[self.history_len - 1];
        let from = self.history[self.history_len.saturating_sub(1 + MOMENTUM_SPAN)];
        unit(last.0 - from.0, last.1 - from.1)
    }

    /// Motion-direction inconsistency of a detection centre `(x, y)` with this
    /// track, in `0..=1` (0 = straight on, 1 = straight back).
    pub fn direction_cost(&self, x: f32, y: f32) -> f32 {
        let Some((mx, my)) = self.momentum() else {
            return 0.0;
        };
        let (ox, oy) = self.last_obs.center();
        let Some((dx, dy)) = unit(x - ox, y - oy) else {
            return 0.0;
        };
        (mx * dx + my * dy).clamp(-1.0, 1.0).acos() / std::f32::consts::PI
    }

    /// Fold a descriptor into the running mean.
    pub fn add_features(&mut self, f: &Features) {
        if self.feat_n == 0 {
            self.feat = *f;
        } else {
            blend(
                &mut self.feat,
                f,
                (1.0 / (self.feat_n as f32 + 1.0)).max(0.2),
            );
        }
        self.feat_n += 1;
    }

    /// Re-express the track in the current camera frame: `map` takes an old
    /// pixel position to the new one.
    pub fn warp(&mut self, map: &impl Fn(f32, f32) -> Option<(f32, f32)>) {
        if let Some(b) = warp_box(&self.kf.bbox(), map) {
            self.kf.move_to(&b);
            self.pred = b;
        }
        if let Some(b) = warp_box(&self.last_obs, map) {
            self.kf_at_obs.move_to(&b);
            self.last_obs = b;
        }
        for p in &mut self.history[..self.history_len] {
            if let Some(q) = map(p.0, p.1).filter(|q| q.0.is_finite() && q.1.is_finite()) {
                *p = q;
            }
        }
    }
}

fn unit(dx: f32, dy: f32) -> Option<(f32, f32)> {
    let n = (dx * dx + dy * dy).sqrt();
    (n > 1.0).then(|| (dx / n, dy / n))
}

/// Map a box through `map`: its centre moves, and its width and height follow
/// the mapped edge midpoints (a zoom or a perspective change rescales it).
fn warp_box(b: &Box4, map: &impl Fn(f32, f32) -> Option<(f32, f32)>) -> Option<Box4> {
    let (cx, cy) = b.center();
    let finite = |p: Option<(f32, f32)>| p.filter(|q| q.0.is_finite() && q.1.is_finite());
    let (nx, ny) = finite(map(cx, cy))?;
    let span = |a: Option<(f32, f32)>, z: Option<(f32, f32)>, fallback: f32| match (a, z) {
        (Some(a), Some(z)) => ((z.0 - a.0).powi(2) + (z.1 - a.1).powi(2)).sqrt().max(1.0),
        _ => fallback,
    };
    let w = span(finite(map(b.x1, cy)), finite(map(b.x2, cy)), b.width());
    let h = span(finite(map(cx, b.y1)), finite(map(cx, b.y2)), b.height());
    Some(Box4::from_center(nx, ny, w, h))
}
