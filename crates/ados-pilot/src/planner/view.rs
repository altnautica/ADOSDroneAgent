//! The view planner for a `follow_intent` goal: where to fly so the subject
//! stays in sight.
//!
//! Every [`VIEW_PERIOD_MS`] it samples viewpoints round the subject on the
//! skill's standoff band (every [`RADIUS_STEP_M`]), at every
//! [`BEARING_STEP_DEG`] of bearing relative to the subject's heading (or to
//! the bearing over the ground the skill prefers, an orbit's phase) and
//! every [`HEIGHT_STEP_M`] of the height band, and scores each one:
//!
//! - visibility: a viewpoint keeps its place relative to the subject, so at
//!   each predicted position it sits that far off it; the line of sight
//!   from there to the subject's torso is tested against the obstacle map
//!   ([`Sight`]), each prediction weighing less the further ahead it lies;
//!   a viewpoint past the edge of the space the sensors have seen is
//!   credited with only [`UNSEEN_VIS`] of that (a wall the scans have not
//!   reached yet may stand there) and costs as unreachable;
//! - the standoff's distance from the band's middle, the bearing's from the
//!   skill's preference (none for auto: the best view wins), the height's
//!   from the skill's, the distance from the vehicle and the clearance
//!   along the viewpoint's way;
//! - the viewpoint held last is kept unless another is clearly better, and
//!   one whose planned trajectory lost the subject costs that loss on the
//!   next choice ([`ViewPlanner::note_plan_visibility`]).
//!
//! A viewpoint inside the margin of an obstacle now is never picked. The
//! subject's heading follows its velocity, turned at most
//! [`HEADING_RATE_DPS`] (a subject turning back swings the viewpoint round
//! it, never across it), and looks along the predicted turn while the
//! coordinated-turn mode is likely. With no fix for [`LOS_LOST_S`] the
//! line of sight is lost: the planner then picks the viewpoint that sees
//! where the subject most probably is now, until the skill's search takes
//! over with a vantage goal.

use ados_protocol::pilot::FOLLOW_HEIGHT_FLOOR_M;

use crate::geo::{self, V2};

/// How often a viewpoint is picked.
pub const VIEW_PERIOD_MS: u64 = 100;
/// Bearing samples round the subject, degrees apart.
pub const BEARING_STEP_DEG: f32 = 15.0;
/// Height samples in the band, metres apart.
pub const HEIGHT_STEP_M: f32 = 0.5;
/// Standoff samples in the band, at most this far apart, metres.
pub const RADIUS_STEP_M: f32 = 1.0;
/// No fix of the subject for this long, s: its line of sight is lost.
pub const LOS_LOST_S: f32 = 0.5;
/// The subject heading turns at most this fast, degrees per second.
pub const HEADING_RATE_DPS: f32 = 30.0;
/// A prediction this far ahead weighs 1/e of the subject now, s.
const HORIZON_TAU_S: f32 = 1.5;
/// A subject slower than this keeps the heading it had, m/s.
const HEADING_MIN_MPS: f32 = 0.5;
/// The coordinated-turn mode's weight from which the heading looks along
/// the predicted turn.
const TURN_PROB: f32 = 0.3;
/// The predicted turn is looked along between these offsets ahead, s.
const TURN_LOOK_S: [f32; 2] = [0.5, 1.5];
/// The sighted point's height over the feet, as a share of the subject's
/// height.
const TORSO_SHARE: f32 = 0.6;
/// A line of sight ends this short of the subject (its own body is in the
/// map), metres.
const BODY_M: f32 = 0.5;
/// Clearance beyond the margin wanted along a viewpoint's way, metres.
const CLEAR_WANT_M: f32 = 1.0;
/// The subject's torso height over the vehicle's flight down position when
/// no ground is known, metres.
const TORSO_UNDER_M: f32 = 2.5;

/// Cost weights: a viewpoint that loses the whole predicted view costs
/// [`W_VIS`] (twice that when the skill requires the line of sight); one
/// at the band's edge [`W_STANDOFF`]; one opposite the preferred bearing
/// [`W_BEARING`] ([`W_BEARING_WORLD`] for a bearing over the ground); one
/// a standoff away from the vehicle [`W_MOVE`]; one with no clearance
/// beyond the margin on its way [`W_CLEAR`]; one a metre off the skill's
/// height [`W_HEIGHT`]; one the vehicle cannot fly to in a straight line
/// past what is seen, keeping its margin and [`REACH_ROOM_M`] beyond it,
/// [`W_REACH`] (a viewpoint beyond a wall or round a corner the vehicle
/// would go round). The viewpoint held is [`HOLD_BONUS`] cheaper.
const W_VIS: f32 = 4.0;
const W_STANDOFF: f32 = 0.6;
const W_BEARING: f32 = 1.5;
/// A bearing over the ground (an orbit's phase) is the skill's own motion:
/// its bearings are sampled from it, so the slot held turns with it, and
/// it weighs well above what holding a slot and staying near the vehicle
/// save ([`HOLD_BONUS`] + [`W_MOVE`]), so the viewpoint keeps to the phase
/// rather than wait for the vehicle; a viewpoint that loses the subject
/// still costs more than a few steps off it.
const W_BEARING_WORLD: f32 = 4.0;
const W_MOVE: f32 = 0.4;
const W_CLEAR: f32 = 0.5;
const W_HEIGHT: f32 = 0.2;
const W_REACH: f32 = 1.5;
const HOLD_BONUS: f32 = 0.3;
/// A planned trajectory that sees less than this share of the subject
/// marks its viewpoint ([`ViewPlanner::note_plan_visibility`]).
const PLAN_VIS_OK: f32 = 0.7;
/// The share of its visibility a viewpoint past the seen space's edge is
/// credited with: what an unseen place sees is unknown.
const UNSEEN_VIS: f32 = 0.5;
/// Clearance beyond the margin a straight way to a viewpoint keeps to count
/// as reached straight, metres: the route planner's own room.
const REACH_ROOM_M: f32 = 0.25;
/// The straight way to a viewpoint is checked this often, metres.
const REACH_STEP_M: f32 = 0.25;

/// One predicted subject position, `dt_s` after now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prediction {
    pub dt_s: f32,
    pub ne: V2,
}

/// The subject as the view planner sees it now.
#[derive(Debug, Clone, PartialEq)]
pub struct SubjectView {
    /// Where it most probably is now.
    pub ne: V2,
    pub vel: V2,
    /// The ground under it, local down (m), when known.
    pub ground_d: Option<f32>,
    pub height_m: f32,
    /// Ahead of now, in time order.
    pub preds: Vec<Prediction>,
    /// The coordinated-turn mode's weight.
    pub turn_prob: f32,
    /// Since the last fix, s.
    pub unseen_s: f32,
}

impl SubjectView {
    /// Where the subject is predicted `t` s from now: along the
    /// predictions, on at the last segment's velocity past them.
    pub fn at(&self, t: f32) -> V2 {
        let mut prev = (0.0f32, self.ne);
        for p in &self.preds {
            if p.dt_s >= t {
                let span = (p.dt_s - prev.0).max(1e-3);
                let k = ((t - prev.0) / span).clamp(0.0, 1.0);
                return geo::add(prev.1, geo::scale(geo::sub(p.ne, prev.1), k));
            }
            prev = (p.dt_s, p.ne);
        }
        let vel = match self.preds.len() {
            0 | 1 => self.vel,
            n => {
                let (a, b) = (self.preds[n - 2], self.preds[n - 1]);
                geo::scale(geo::sub(b.ne, a.ne), 1.0 / (b.dt_s - a.dt_s).max(1e-3))
            }
        };
        geo::add(prev.1, geo::scale(vel, t - prev.0))
    }
}

/// What the skill asks of the view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewAsk {
    /// Horizontal distance band to the subject, metres.
    pub band: [f32; 2],
    /// Preferred bearing, rad: from the subject's heading, or from north
    /// with [`ViewAsk::bearing_world`]; `None` for the best view.
    pub bearing_pref: Option<f32>,
    /// `bearing_pref` is a bearing over the ground (an orbit's phase).
    pub bearing_world: bool,
    /// Height band above the ground, metres.
    pub heights: [f32; 2],
    /// The skill's own down position.
    pub d_pref: f32,
    /// The ground under the vehicle, local down (m), when known.
    pub ground_d: Option<f32>,
    /// The obstacle margin.
    pub margin: f32,
    pub require_los: bool,
}

/// What the view planner asks of the world model.
pub trait Sight {
    /// Clearance at `ne` at down position `d`, metres (infinite where
    /// nothing is known).
    fn clearance(&self, ne: V2, d: f32) -> f32;
    /// Nothing observed blocks the segment `from` → `to` (local NED).
    fn sees(&self, from: [f32; 3], to: [f32; 3]) -> bool;
    /// The sensors have seen `to` free from the vehicle at `from` (local
    /// NED): it lies inside the seen space, not beyond its edge.
    fn seen(&self, from: [f32; 3], to: [f32; 3]) -> bool;
}

/// The viewpoint picked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewChoice {
    /// Where to fly now, and at what down position.
    pub ne: V2,
    pub d: f32,
    /// Its bearing from the subject (absolute, rad) and distance.
    pub bearing: f32,
    pub radius: f32,
    /// The weighted share of the subject's predicted positions it sees.
    pub visibility: f32,
    /// Picked with the line of sight lost.
    pub lost: bool,
    /// Its cost when picked (the least of every viewpoint's).
    pub cost: f32,
}

/// A viewpoint's place relative to the subject: bearing sample (from the
/// heading, or from a bearing over the ground the skill prefers: then
/// `world`), standoff sample, height sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    world: bool,
    bearing: i32,
    radius: i32,
    height: i32,
}

/// The view planner's memory between choices.
#[derive(Debug, Clone, Default)]
pub struct ViewPlanner {
    heading: Option<f32>,
    /// The way a turn back swings the heading (±1), latched until the
    /// heading is within the turn-back angle of the subject's way.
    turn_back: Option<f32>,
    held: Option<Slot>,
    /// A slot whose planned trajectory lost the subject, and how much.
    penalty: Option<(Slot, f32)>,
    last_ms: Option<u64>,
}

/// Evenly spaced samples from `lo` to `hi`, at most `step` apart.
fn samples(lo: f32, hi: f32, step: f32) -> Vec<f32> {
    let hi = hi.max(lo);
    let n = ((hi - lo) / step).ceil().max(0.0) as usize;
    if n == 0 {
        return vec![(lo + hi) / 2.0];
    }
    (0..=n)
        .map(|i| lo + (hi - lo) * i as f32 / n as f32)
        .collect()
}

/// The straight way `a` → `b` at down position `d` keeps `keep` clear of
/// every obstacle past the first [`REACH_STEP_M`] (the vehicle leaves where
/// it is, however near a wall that is).
fn straight_clear(sight: &dyn Sight, a: V2, b: V2, d: f32, keep: f32) -> bool {
    let n = (geo::dist(a, b) / REACH_STEP_M).ceil() as usize;
    (1..=n).all(|k| {
        let p = geo::add(a, geo::scale(geo::sub(b, a), k as f32 / n as f32));
        sight.clearance(p, d) >= keep
    })
}

/// Whether `sight` sees the subject at `ne` from `from` (local NED): the
/// line to its torso, ended [`BODY_M`] short of its centre. Its torso
/// stands on its own ground, else on `ground_d` (under the vehicle), else
/// [`TORSO_UNDER_M`] under `from`.
pub fn sees_subject(
    sight: &dyn Sight,
    s: &SubjectView,
    ne: V2,
    from: [f32; 3],
    ground_d: Option<f32>,
) -> bool {
    let at = [from[0], from[1]];
    let to = geo::sub(ne, at);
    let len = geo::norm(to);
    let end = if len > BODY_M {
        geo::add(at, geo::scale(to, (len - BODY_M) / len))
    } else {
        at
    };
    let to_d = match s.ground_d.or(ground_d) {
        Some(g) => g - TORSO_SHARE * s.height_m,
        None => from[2] + TORSO_UNDER_M,
    };
    sight.sees(from, [end[0], end[1], to_d])
}

impl ViewPlanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the subject (another goal, or none).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The subject heading used for bearings, once picked.
    pub fn heading(&self) -> Option<f32> {
        self.heading
    }

    /// The trajectory flown to the last viewpoint saw `vis` of the
    /// subject's predicted positions: under [`PLAN_VIS_OK`] that viewpoint
    /// costs the loss on the next choice.
    pub fn note_plan_visibility(&mut self, vis: f32) {
        self.penalty = match self.held {
            Some(s) if vis < PLAN_VIS_OK => Some((s, 1.0 - vis.clamp(0.0, 1.0))),
            _ => None,
        };
    }

    /// Turn the heading toward the subject's way (or its predicted turn)
    /// over `dt` s: at most [`HEADING_RATE_DPS`], round the side the
    /// viewpoint held lies on when the way turns back.
    fn turn(&mut self, s: &SubjectView, vehicle: V2, pref: f32, dt: f32) -> f32 {
        let speed = geo::norm(s.vel);
        let way = if s.turn_prob >= TURN_PROB {
            let d = geo::sub(s.at(TURN_LOOK_S[1]), s.at(TURN_LOOK_S[0]));
            (geo::norm(d) > 0.3).then(|| geo::bearing_of(d))
        } else {
            None
        }
        .or_else(|| (speed >= HEADING_MIN_MPS).then(|| geo::bearing_of(s.vel)));
        let h = match (self.heading, way) {
            (None, Some(w)) => w,
            // First pick of a standing subject: the vehicle is behind it.
            (None, None) => geo::bearing_of(geo::sub(s.ne, vehicle)),
            (Some(h), None) => h,
            (Some(h), Some(w)) => {
                let err = geo::wrap_pi(w - h);
                let step = HEADING_RATE_DPS.to_radians() * dt;
                let dir = if err.abs() > 150f32.to_radians() {
                    // Turning back: swing the viewpoint toward the
                    // vehicle's side of the subject first, and on round
                    // that way.
                    *self.turn_back.get_or_insert_with(|| {
                        let side = geo::bearing_of(geo::sub(vehicle, s.ne));
                        let slot = h + pref;
                        geo::wrap_pi(side - slot).signum()
                    })
                } else {
                    self.turn_back = None;
                    err.signum()
                };
                if err.abs() <= step {
                    w
                } else {
                    geo::wrap_pi(h + dir * step)
                }
            }
        };
        self.heading = Some(h);
        h
    }

    /// Pick the viewpoint for `s` as `ask` asks, the vehicle at
    /// `vehicle`; `None` when every viewpoint is inside an obstacle's
    /// margin.
    pub fn choose(
        &mut self,
        s: &SubjectView,
        ask: &ViewAsk,
        vehicle: [f32; 3],
        sight: &dyn Sight,
        now_ms: u64,
    ) -> Option<ViewChoice> {
        let dt = self
            .last_ms
            .map_or(0.0, |t| now_ms.saturating_sub(t) as f32 / 1000.0)
            .min(0.5);
        self.last_ms = Some(now_ms);
        let here = [vehicle[0], vehicle[1]];
        let rel_pref = ask.bearing_pref.filter(|_| !ask.bearing_world);
        let world_pref = ask.bearing_pref.filter(|_| ask.bearing_world);
        let pref = rel_pref.unwrap_or(std::f32::consts::PI);
        let heading = self.turn(s, here, pref, dt);
        // Bearings are sampled from the subject's heading, or from a bearing
        // over the ground (an orbit's phase): the slots turn with it.
        let base = world_pref.unwrap_or(heading);
        let lost = s.unseen_s >= LOS_LOST_S;
        // What is looked at: the subject now and its predictions, or, lost,
        // where it most probably is now.
        let mut looks = vec![(0.0f32, s.ne, 1.0f32)];
        if !lost {
            looks.extend(
                s.preds
                    .iter()
                    .map(|p| (p.dt_s, p.ne, (-p.dt_s / HORIZON_TAU_S).exp())),
            );
        }
        let w_sum: f32 = looks.iter().map(|l| l.2).sum();
        let band = [ask.band[0].min(ask.band[1]), ask.band[0].max(ask.band[1])];
        let r_mid = (band[0] + band[1]) / 2.0;
        let r_half = ((band[1] - band[0]) / 2.0).max(0.5);
        let radii = samples(band[0], band[1], RADIUS_STEP_M);
        let (heights, h_pref) = match ask.ground_d {
            Some(g) => {
                let lo = ask.heights[0].max(FOLLOW_HEIGHT_FLOOR_M);
                let hi = ask.heights[1].max(lo);
                let pref = (g - ask.d_pref).clamp(lo, hi);
                (
                    samples(lo, hi, HEIGHT_STEP_M)
                        .into_iter()
                        .map(|h| g - h)
                        .collect(),
                    g - pref,
                )
            }
            None => (vec![ask.d_pref], ask.d_pref),
        };
        let w_vis = if ask.require_los { 2.0 * W_VIS } else { W_VIS };
        let n_b = (360.0 / BEARING_STEP_DEG).round() as i32;
        let mut best: Option<(f32, Slot, ViewChoice)> = None;
        for k in 0..n_b {
            let rel = geo::wrap_pi((k as f32 * BEARING_STEP_DEG).to_radians());
            let b = base + rel;
            let u = geo::bearing_unit(b);
            let bearing_cost = match (world_pref, rel_pref) {
                (Some(_), _) => W_BEARING_WORLD * rel.abs() / std::f32::consts::PI,
                (None, Some(p)) => W_BEARING * geo::wrap_pi(rel - p).abs() / std::f32::consts::PI,
                (None, None) => 0.0,
            };
            for (i, &r) in radii.iter().enumerate() {
                let p0 = geo::add(s.ne, geo::scale(u, r));
                let move_cost = geo::dist(here, p0) / r_mid.max(1.0);
                for (j, &d) in heights.iter().enumerate() {
                    let c0 = sight.clearance(p0, d);
                    if c0 < ask.margin {
                        continue;
                    }
                    let mut clear = c0;
                    let mut seen = 0.0;
                    let at0 = [p0[0], p0[1], d];
                    // A viewpoint past the seen space's edge or behind what
                    // the vehicle sees is not known to be reachable, nor what
                    // it sees; one the vehicle cannot fly to straight,
                    // keeping its margin, is reached round.
                    let known = sight.seen(vehicle, at0) && sight.sees(vehicle, at0);
                    let reach =
                        known && straight_clear(sight, here, p0, d, ask.margin + REACH_ROOM_M);
                    let ground = ask.ground_d;
                    for &(t, ne, w) in &looks {
                        let at = if t > 0.0 {
                            let p = geo::add(ne, geo::scale(u, r));
                            clear = clear.min(sight.clearance(p, d));
                            p
                        } else {
                            p0
                        };
                        if sees_subject(sight, s, ne, [at[0], at[1], d], ground) {
                            seen += w;
                        }
                    }
                    let vis = seen / w_sum.max(1e-6) * if known { 1.0 } else { UNSEEN_VIS };
                    let slot = Slot {
                        world: world_pref.is_some(),
                        bearing: k,
                        radius: i as i32,
                        height: j as i32,
                    };
                    let room = ((clear - ask.margin) / CLEAR_WANT_M).clamp(0.0, 1.0);
                    let mut cost = w_vis * (1.0 - vis)
                        + W_STANDOFF * (r - r_mid).abs() / r_half
                        + bearing_cost
                        + W_MOVE * move_cost
                        + W_CLEAR * (1.0 - room)
                        + W_HEIGHT * (d - h_pref).abs()
                        + if reach { 0.0 } else { W_REACH };
                    if self.held == Some(slot) {
                        cost -= HOLD_BONUS;
                    }
                    if let Some((p, loss)) = self.penalty {
                        if p == slot {
                            cost += w_vis * loss;
                        }
                    }
                    if best.as_ref().is_none_or(|b| cost < b.0) {
                        let choice = ViewChoice {
                            ne: p0,
                            d,
                            bearing: geo::wrap_pi(b),
                            radius: r,
                            visibility: vis,
                            lost,
                            cost,
                        };
                        best = Some((cost, slot, choice));
                    }
                }
            }
        }
        let (_, slot, choice) = best?;
        if self.held != Some(slot) {
            self.penalty = None;
        }
        self.held = Some(slot);
        Some(choice)
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
