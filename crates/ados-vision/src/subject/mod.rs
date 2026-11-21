//! The designated subject's state in the world: where the person a follow
//! behaviour tracks stands, how they move, and where they will be.
//!
//! [`crate::mot`] keeps the subject's box and identity in the image. This
//! module lifts the box into local NED and filters it over time:
//!
//! 1. **Range.** Each sighting ([`Sighting`]: the camera's position and the
//!    lines of sight through the box's top and bottom edges, in NED) fuses
//!    the range cues it has: the box height against the subject's height
//!    (prior 1.70 ± 0.12 m, refined whenever another cue measures it), metric
//!    depth inside the box ([`depth_cue`]) and the ground height under the
//!    subject. See `range` for the geometry.
//! 2. **Motion.** An interacting multiple model filter weighs three models of
//!    a walking person ([`Mode`]): constant velocity, a coordinated turn and
//!    standing still, with speed held to [`SubjectConfig::max_speed_mps`] and
//!    turn rate to [`SubjectConfig::max_turn_rate_rps`]. A measurement too far
//!    from the prediction is gated; several gated in a row restart the track
//!    at the measurement (the identity comes from the tracker, so the box is
//!    the subject; the range was wrong, not the person).
//! 3. **Prediction.** Positions and covariances at 0.5 to 3 s, walked through
//!    a [`FreeSpace`] map so a predicted path bends round occupied space.
//! 4. **Beacon.** The subject's own GNSS fix ([`BeaconFix`]) fuses into the
//!    vision track; it never tracks alone unless
//!    [`SubjectConfig::beacon_only`] allows it.
//!
//! Pure and deterministic: time comes in with every call, nothing allocates
//! per update but the prediction list and the depth sample buffers.

mod beacon;
mod depth;
mod imm;
mod mat;
mod models;
mod predict;
mod range;
mod scalar;

pub use beacon::{ned_from_geodetic, BeaconFix, BeaconOutcome, Geodetic};
pub use depth::depth_cue;
pub use models::Mode;
pub use predict::{FreeSpace, OpenSpace};
pub use range::{DepthCue, GroundCue, Sighting};

use ados_protocol::vision_subject::{SubjectSources, SubjectState};

use imm::Imm;
use mat::Mat2;
use scalar::Scalar;

/// The estimator's tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubjectConfig {
    /// The height prior, metres: mean and one sigma.
    pub height_prior_m: f32,
    pub height_sigma_m: f32,
    /// How fast the height estimate may wander (crouching, a raised arm),
    /// one sigma per square-root second, metres.
    pub height_drift_mps: f32,
    /// The fastest a person moves, m/s.
    pub max_speed_mps: f32,
    /// The fastest a person turns, rad/s.
    pub max_turn_rate_rps: f32,
    /// The velocity sigma a new track starts with, m/s.
    pub initial_speed_sigma: f32,
    /// White acceleration of the constant-velocity and the turn model, m/s².
    pub cv_accel_sigma: f32,
    pub ct_accel_sigma: f32,
    /// White turn acceleration of the turn model, rad/s².
    pub turn_accel_sigma: f32,
    /// Position drift of a standing person, m per square-root second.
    pub stop_drift_sigma: f32,
    /// How often a walker switches model, per second.
    pub switch_rate_hz: f32,
    /// How fast the ground under the subject may change height, m per
    /// square-root second.
    pub ground_drift_mps: f32,
    /// The squared Mahalanobis distance past which a measurement is gated
    /// (two degrees of freedom; 13.8 is the 99.9 % bound).
    pub gate_chi2: f32,
    /// Gated measurements in a row that restart the track.
    pub restart_after: u8,
    /// The longest the track coasts without a fix before it is dropped, s.
    pub max_coast_s: f32,
    /// Track on the beacon alone, without a recent vision fix.
    pub beacon_only: bool,
    /// How recent a vision fix must be for a beacon fix to fuse, s.
    pub beacon_vision_window_s: f32,
}

impl Default for SubjectConfig {
    fn default() -> Self {
        Self {
            height_prior_m: 1.70,
            height_sigma_m: 0.12,
            height_drift_mps: 0.02,
            max_speed_mps: 7.0,
            max_turn_rate_rps: 1.5,
            initial_speed_sigma: 2.0,
            cv_accel_sigma: 0.35,
            ct_accel_sigma: 0.35,
            turn_accel_sigma: 0.3,
            stop_drift_sigma: 0.05,
            switch_rate_hz: 0.2,
            ground_drift_mps: 0.3,
            gate_chi2: 13.8,
            restart_after: 3,
            max_coast_s: 5.0,
            beacon_only: false,
            beacon_vision_window_s: 1.5,
        }
    }
}

/// What a sighting did to the estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Started the track.
    Started,
    /// Fused into the track.
    Fused,
    /// Not fused: too far from the prediction.
    Gated,
    /// Too far from the prediction again: the track restarted on it.
    Restarted,
    /// Not fused: the sighting could not place the subject (no range cue and
    /// no track to borrow a range from, or a degenerate line of sight).
    NoFix,
    /// Not fused: older than the estimate.
    Stale,
}

/// A fix this much older than the estimate is still fused, at the
/// estimate's time.
const STALE_TOLERANCE_NS: u64 = 50_000_000;

/// A source is reported for this long after it last fused, ns.
const SOURCE_WINDOW_NS: u64 = 1_000_000_000;

/// The subject's height is held to a person's.
const HEIGHT_RANGE_M: (f64, f64) = (1.0, 2.2);

/// The vertical sigma a beacon-started track takes, metres.
const BEACON_DOWN_SIGMA_M: f64 = 5.0;

#[derive(Debug, Clone)]
struct Track {
    imm: Imm,
    /// The feet's down coordinate.
    down: Scalar,
    last_fix_ns: u64,
    last_vision_ns: Option<u64>,
    /// When each source (box, depth, ground, beacon) last fused.
    source_ns: [Option<u64>; 4],
    /// The range bias of the last vision fix, as a horizontal covariance.
    bias: Mat2,
    gated: u8,
}

/// One designated subject's estimator.
#[derive(Debug, Clone)]
pub struct SubjectEstimator {
    id: u32,
    cfg: SubjectConfig,
    height: Scalar,
    track: Option<Track>,
    t_ns: Option<u64>,
}

impl SubjectEstimator {
    /// An estimator for the subject published as track `id`.
    pub fn new(id: u32, cfg: SubjectConfig) -> Self {
        Self {
            id,
            cfg,
            height: Scalar::new(f64::from(cfg.height_prior_m), f64::from(cfg.height_sigma_m)),
            track: None,
            t_ns: None,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// The estimate's time, ns.
    pub fn stamp_ns(&self) -> Option<u64> {
        self.t_ns
    }

    /// The subject's estimated height and its sigma, metres.
    pub fn height_m(&self) -> (f32, f32) {
        (self.height.value as f32, self.height.sigma() as f32)
    }

    /// Whether the estimator holds a track it has not coasted out.
    pub fn is_tracking(&self) -> bool {
        match (&self.track, self.t_ns) {
            (Some(tr), Some(t)) => {
                (t.saturating_sub(tr.last_fix_ns) as f64 / 1e9) <= f64::from(self.cfg.max_coast_s)
            }
            _ => false,
        }
    }

    /// Whether `stamp_ns` is too old to fuse.
    fn stale(&self, stamp_ns: u64) -> bool {
        self.t_ns
            .is_some_and(|t| stamp_ns.saturating_add(STALE_TOLERANCE_NS) < t)
    }

    /// Move the estimate on to `stamp_ns` (never back).
    fn advance(&mut self, stamp_ns: u64) {
        let t = self.t_ns.map_or(stamp_ns, |t| t.max(stamp_ns));
        let dt = self.t_ns.map_or(0.0, |prev| (t - prev) as f64 / 1e9);
        self.t_ns = Some(t);
        let prior_var = f64::from(self.cfg.height_sigma_m).powi(2);
        self.height.drift(f64::from(self.cfg.height_drift_mps), dt);
        self.height.var = self.height.var.min(prior_var);
        if let Some(tr) = &mut self.track {
            tr.imm.predict(dt, &self.cfg);
            tr.down.drift(f64::from(self.cfg.ground_drift_mps), dt);
        }
    }

    /// The subject was not seen in the frame at `stamp_ns`: coast on.
    pub fn coast(&mut self, stamp_ns: u64) {
        if !self.stale(stamp_ns) {
            self.advance(stamp_ns);
        }
    }

    /// Fuse one sighting.
    pub fn observe(&mut self, s: &Sighting) -> Outcome {
        if self.stale(s.stamp_ns) {
            return Outcome::Stale;
        }
        self.advance(s.stamp_ns);
        let t = self.t_ns.unwrap_or(s.stamp_ns);
        let prior_range = self.track.as_ref().map(|tr| {
            let (x, _) = tr.imm.combined();
            (x[0] - f64::from(s.camera_ned[0])).hypot(x[1] - f64::from(s.camera_ned[1]))
        });
        let Some(fix) = range::fix(s, &self.height, prior_range) else {
            return Outcome::NoFix;
        };
        let outcome = match &mut self.track {
            Some(tr) if tr.imm.distance2(fix.ne, &fix.r) > f64::from(self.cfg.gate_chi2) => {
                tr.gated = tr.gated.saturating_add(1);
                if tr.gated < self.cfg.restart_after {
                    return Outcome::Gated;
                }
                self.track = None;
                Outcome::Restarted
            }
            Some(tr) => {
                tr.gated = 0;
                tr.imm.update(fix.ne, &fix.r, &self.cfg);
                tr.down.update(fix.down, fix.down_var);
                Outcome::Fused
            }
            None => Outcome::Started,
        };
        let tr = self.track.get_or_insert_with(|| Track {
            imm: Imm::new(fix.ne, &fix.r, &self.cfg),
            down: Scalar::new(fix.down, fix.down_var.sqrt()),
            last_fix_ns: t,
            last_vision_ns: None,
            source_ns: [None; 4],
            bias: [[0.0; 2]; 2],
            gated: 0,
        });
        tr.last_fix_ns = t;
        tr.last_vision_ns = Some(t);
        let flags = [
            fix.sources.box_height,
            fix.sources.depth,
            fix.sources.ground,
        ];
        for (slot, on) in tr.source_ns.iter_mut().zip(flags) {
            if on {
                *slot = Some(t);
            }
        }
        let (u, b2) = (fix.bearing, fix.bias_sigma * fix.bias_sigma);
        tr.bias = [
            [b2 * u[0] * u[0], b2 * u[0] * u[1]],
            [b2 * u[0] * u[1], b2 * u[1] * u[1]],
        ];
        if let Some((h, var)) = fix.height_obs {
            self.height.update(h, var);
            self.height.value = self.height.value.clamp(HEIGHT_RANGE_M.0, HEIGHT_RANGE_M.1);
        }
        outcome
    }

    /// Fuse the subject's own GNSS fix. It fuses only while a vision fix is
    /// at most [`SubjectConfig::beacon_vision_window_s`] old, unless
    /// [`SubjectConfig::beacon_only`] allows tracking on the beacon alone.
    pub fn fuse_beacon(&mut self, fix: &BeaconFix) -> BeaconOutcome {
        if !fix
            .ned
            .iter()
            .chain([&fix.horiz_acc_m])
            .all(|v| v.is_finite())
            || self.stale(fix.stamp_ns)
        {
            return BeaconOutcome::Stale;
        }
        let window = (f64::from(self.cfg.beacon_vision_window_s) * 1e9) as u64;
        let seen = self
            .track
            .as_ref()
            .and_then(|tr| tr.last_vision_ns)
            .is_some_and(|v| fix.stamp_ns.saturating_sub(v) <= window);
        if !seen && !self.cfg.beacon_only {
            return BeaconOutcome::NoVision;
        }
        self.advance(fix.stamp_ns);
        let t = self.t_ns.unwrap_or(fix.stamp_ns);
        let z = [f64::from(fix.ned[0]), f64::from(fix.ned[1])];
        let s2 = f64::from(fix.sigma_m()).powi(2);
        let r = [[s2, 0.0], [0.0, s2]];
        let outcome = match &mut self.track {
            Some(tr) if tr.imm.distance2(z, &r) > f64::from(self.cfg.gate_chi2) => {
                return BeaconOutcome::Gated;
            }
            Some(tr) => {
                tr.imm.update(z, &r, &self.cfg);
                BeaconOutcome::Fused
            }
            None => BeaconOutcome::Started,
        };
        let tr = self.track.get_or_insert_with(|| Track {
            imm: Imm::new(z, &r, &self.cfg),
            down: Scalar::new(f64::from(fix.ned[2]), BEACON_DOWN_SIGMA_M),
            last_fix_ns: t,
            last_vision_ns: None,
            source_ns: [None; 4],
            bias: [[0.0; 2]; 2],
            gated: 0,
        });
        tr.last_fix_ns = t;
        tr.source_ns[3] = Some(t);
        outcome
    }

    /// The published state, with predictions walked through `free`; `None`
    /// without a track, or once it coasted out.
    pub fn state(&self, camera_id: &str, free: &impl FreeSpace) -> Option<SubjectState> {
        if !self.is_tracking() {
            return None;
        }
        let tr = self.track.as_ref()?;
        let t = self.t_ns?;
        let (x, p) = tr.imm.combined();
        let mu = tr.imm.mode_probs();
        let recent = |s: Option<u64>| s.is_some_and(|s| t.saturating_sub(s) < SOURCE_WINDOW_NS);
        Some(SubjectState {
            camera_id: camera_id.to_owned(),
            id: self.id,
            stamp_ns: t,
            fix_age_ms: u32::try_from(t.saturating_sub(tr.last_fix_ns) / 1_000_000)
                .unwrap_or(u32::MAX),
            pos_ned: [x[0] as f32, x[1] as f32, tr.down.value as f32],
            cov: predict::cov3(&p, &tr.bias, tr.down.var),
            vel_ned: [x[2] as f32, x[3] as f32, 0.0],
            mode_probs: mu.map(|m| m as f32),
            height_m: self.height.value as f32,
            predictions: predict::predictions(
                &tr.imm,
                &tr.down,
                self.height.value,
                &tr.bias,
                &self.cfg,
                free,
            ),
            sources: SubjectSources {
                box_height: recent(tr.source_ns[0]),
                depth: recent(tr.source_ns[1]),
                ground: recent(tr.source_ns[2]),
                beacon: recent(tr.source_ns[3]),
            },
        })
    }
}

#[cfg(test)]
mod scenario_tests;
#[cfg(test)]
mod tests;
