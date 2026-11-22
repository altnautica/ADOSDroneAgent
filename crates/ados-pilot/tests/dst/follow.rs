//! Follow intents (`follow_intent` goals) flown by the pilot's view
//! planner: the skill names the standoff band, the bearing it prefers, the
//! height band and the subject; the pilot picks the viewpoint that keeps
//! the subject in sight and flies it.
//!
//! The run plays both the vision engine and the skill. A walker walks a
//! trail; every [`GOAL_MS`] the camera fixes it while the line from the
//! vehicle to its torso misses every trunk and wall (all full height) and
//! it is within [`CAMERA_RANGE_M`], and a fix is published as the
//! `vision.subject.v1` record (the fix's position and velocity, its age,
//! predictions in a line from it; through a turn, once the turn has begun,
//! along the trail, as the estimator's turn model predicts it). No fix for
//! 2 s and no record is published (the host forwards none older). The
//! skill sends an intent every [`GOAL_MS`] round its own estimate: the last
//! fix flown on at its velocity, never decayed (a subject walking toward
//! the vehicle keeps coming).
//!
//! What each run measures: the share of goal periods the vehicle saw the
//! subject while it walked (truth geometry), the nearest it came to the
//! subject unless 3.4 m over its head, the nearest trunk or wall, and how
//! often it kept within the band (plus 2 m) of the subject.

use ados_pilot::core::Reply;
use ados_protocol::pilot::{BearingFrame, Candidates, Goal, GoalKind, Request};
use serde_json::json;

use crate::forest::lease;
use crate::scenarios::launch_to_hold;
use crate::sensors::SensorTier;
use crate::sim::Sim;

/// Follow height, m.
const ALT_M: f32 = 4.0;
/// The skill's standoff band, m.
const BAND_M: [f32; 2] = [5.0, 7.0];
/// The skill's height band, m.
const HEIGHT_BAND_M: [f32; 2] = [4.0, 5.0];
/// The skill's goal period, ms; the vision engine's subject records come
/// as often.
const GOAL_MS: u64 = 100;
/// The camera fixes the subject within this range, m.
const CAMERA_RANGE_M: f32 = 35.0;
/// No record once the last fix is this old, ms.
const FEED_MAX_AGE_MS: u64 = 2_000;
/// The subject's height, m.
const HEAD_M: f32 = 1.75;
/// The separation the vehicle keeps from the subject, m …
pub const SEPARATION_M: f32 = 3.0;
/// … unless this far over its head, m.
pub const OVER_HEAD_M: f32 = 3.4;
/// The obstacle margin flown, m.
const MARGIN_M: f32 = 1.5;
/// A walker's pace and a jog's, m/s.
pub const WALK_MPS: f32 = 1.4;
pub const JOG_MPS: f32 = 3.0;
/// The turn model takes over this long after a turn begins, and predicts
/// along the trail until this long after, ms.
const TURN_SEEN_MS: [u64; 2] = [300, 2_000];
/// The vision engine's prediction offsets, s.
const HORIZONS_S: [f32; 5] = [0.5, 1.0, 1.5, 2.0, 3.0];
/// The skill's subject id.
const SUBJECT_ID: u32 = 1;

/// A walker on a polyline at a pace, from `start_ms`, standing at the end.
/// It sets off from standing at [`START_MPS2`] up to its pace.
#[derive(Debug, Clone)]
pub struct Trail {
    pub pts: Vec<[f32; 2]>,
    pub pace: f32,
    pub start_ms: u64,
}

/// A person's acceleration setting off from standing, m/s².
const START_MPS2: f32 = 2.0;

impl Trail {
    fn len(&self) -> f32 {
        self.pts
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
            .sum()
    }

    /// The time the walker takes to reach its pace, s.
    fn ramp_s(&self) -> f32 {
        self.pace / START_MPS2
    }

    /// How far along the trail the walker is at `now_ms`, m.
    fn dist_at(&self, now_ms: u64) -> f32 {
        let t = now_ms.saturating_sub(self.start_ms) as f32 / 1000.0;
        let ramp = self.ramp_s();
        if t < ramp {
            0.5 * START_MPS2 * t * t
        } else {
            self.pace * (t - ramp / 2.0)
        }
    }

    /// When the walker is `s` m along the trail, s after it set off.
    fn time_at(&self, s: f32) -> f32 {
        let ramp = self.ramp_s();
        if s < self.pace * ramp / 2.0 {
            (2.0 * s / START_MPS2).sqrt()
        } else {
            s / self.pace + ramp / 2.0
        }
    }

    /// Where the walker is at `now_ms`, and the leg it walks.
    fn at(&self, now_ms: u64) -> ([f32; 2], usize) {
        let mut s = self.dist_at(now_ms);
        for (i, w) in self.pts.windows(2).enumerate() {
            let d = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
            if s <= d {
                let k = s / d.max(1e-6);
                return (
                    [
                        w[0][0] + (w[1][0] - w[0][0]) * k,
                        w[0][1] + (w[1][1] - w[0][1]) * k,
                    ],
                    i,
                );
            }
            s -= d;
        }
        (*self.pts.last().expect("a trail"), self.pts.len() - 1)
    }

    fn walking(&self, now_ms: u64) -> bool {
        now_ms >= self.start_ms && self.dist_at(now_ms) < self.len()
    }

    fn vel(&self, now_ms: u64) -> [f32; 2] {
        let (a, _) = self.at(now_ms);
        let (b, _) = self.at(now_ms + 100);
        [(b[0] - a[0]) * 10.0, (b[1] - a[1]) * 10.0]
    }

    /// When the walker began the leg it walks at `now_ms`.
    fn leg_start_ms(&self, now_ms: u64) -> u64 {
        let (_, leg) = self.at(now_ms);
        let before: f32 = self.pts[..=leg]
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
            .sum();
        self.start_ms + (self.time_at(before) * 1000.0) as u64
    }
}

/// Whether the segment `a → b` misses every full-height disk of `stand`.
pub fn clear_line(stand: &[([f32; 2], f32)], a: [f32; 2], b: [f32; 2]) -> bool {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let l2 = (ab[0] * ab[0] + ab[1] * ab[1]).max(1e-9);
    stand.iter().all(|&(c, r)| {
        let t = (((c[0] - a[0]) * ab[0] + (c[1] - a[1]) * ab[1]) / l2).clamp(0.0, 1.0);
        (a[0] + ab[0] * t - c[0]).hypot(a[1] + ab[1] * t - c[1]) > r
    })
}

/// How a follow run is set up.
#[derive(Debug, Clone, Copy)]
pub struct Setup {
    /// Bearing the skill prefers (degrees from the subject's heading), or
    /// `None` for the best view.
    pub bearing_pref_deg: Option<f32>,
    /// The camera fixes nothing between these times after the walk
    /// begins, ms.
    pub blind_ms: Option<(u64, u64)>,
    /// The run goes on this long after the walker stops, ms.
    pub tail_ms: u64,
    /// The skill sends today's ring follow goal instead of intents (the
    /// comparison; `DST_FOLLOW_GOAL=ring` sets it for every run).
    pub ring: bool,
    /// The skill orbits the subject clockwise at this rate, degrees per
    /// second: its intent's bearing is the orbit's phase over the ground,
    /// held while it leads the vehicle's bearing by more than
    /// [`ORBIT_MAX_LEAD_DEG`] (the skill's lead rule). The run ends once the
    /// vehicle has gone all the way round, or `tail_ms` after the walk.
    pub orbit_dps: Option<f32>,
}

/// The orbit's phase is held while it leads the vehicle by more than this,
/// degrees.
const ORBIT_MAX_LEAD_DEG: f32 = 45.0;
/// An orbit run ends this long after it began, done or not, ms.
const ORBIT_LIMIT_MS: u64 = 90_000;

/// What a follow run measured.
#[derive(Debug, Default)]
pub struct Follow {
    /// Goal periods while the walker walked, and those that saw it.
    pub periods: u32,
    pub seen: u32,
    /// Nearest horizontal distance to the walker while not
    /// [`OVER_HEAD_M`] over its head, m.
    pub min_separation_m: f32,
    /// Nearest trunk or wall surface, m.
    pub min_clearance_m: f32,
    /// Walking periods within the band's far edge plus 2 m.
    pub in_reach: u32,
    /// The farthest the vehicle fell behind while walking, m.
    pub max_dist_m: f32,
    /// How far round the subject the vehicle went (clockwise positive),
    /// degrees, and when it first went all the way round (ms after the
    /// walk began).
    pub orbit_deg: f32,
    pub orbit_full_ms: Option<u64>,
}

impl Follow {
    pub fn visibility(&self) -> f32 {
        self.seen as f32 / self.periods.max(1) as f32
    }

    pub fn reach_share(&self) -> f32 {
        self.in_reach as f32 / self.periods.max(1) as f32
    }
}

/// The skill's intent round its estimate `est` moving at `vel`, at the
/// orbit's `phase` (rad, over the ground) when it orbits; with
/// `DST_FOLLOW_GOAL=ring`, the follow goal a skill sends a pilot without
/// intents instead (the point behind on the band's middle, the velocity
/// fed forward, the ring of detour spots), for the comparison.
fn intent(est: [f32; 2], vel: [f32; 2], unseen_s: f32, phase: Option<f32>, setup: &Setup) -> Goal {
    let speed = vel[0].hypot(vel[1]);
    // The way the point stands off the subject: behind its way, or at the
    // orbit's phase.
    let head = match phase {
        Some(p) => [-p.cos(), -p.sin()],
        None if speed > 0.3 => [vel[0] / speed, vel[1] / speed],
        None => [1.0, 0.0],
    };
    let r = (BAND_M[0] + BAND_M[1]) / 2.0;
    let ring = setup.ring || std::env::var("DST_FOLLOW_GOAL").is_ok_and(|v| v == "ring");
    let kind = if ring {
        GoalKind::Follow
    } else {
        GoalKind::FollowIntent
    };
    let mut g = Goal::new(
        kind,
        [est[0] - head[0] * r, est[1] - head[1] * r, -ALT_M],
        6.0,
    );
    g.vn = vel[0];
    g.ve = vel[1];
    g.subject_ne = Some(est);
    g.subject_sigma_m = Some((0.2 + 0.3 * unseen_s).min(0.8));
    g.margin_m = Some(MARGIN_M);
    if ring {
        g.candidates = Some(Candidates {
            ring_center_ne: est,
            radius_m: r,
            angle_deg: ados_pilot::geo::atan2(-head[1], -head[0]).to_degrees(),
            angle_span_deg: 90.0,
        });
        return g;
    }
    g.standoff_band_m = Some(BAND_M);
    match phase {
        Some(p) => {
            g.bearing_pref_deg = Some(p.to_degrees());
            g.bearing_frame = BearingFrame::World;
        }
        None => g.bearing_pref_deg = setup.bearing_pref_deg,
    }
    g.height_band_m = Some(HEIGHT_BAND_M);
    g.subject_ref = Some(SUBJECT_ID);
    g.require_los = true;
    g
}

/// The vision engine's record of the fix `(t, pos, vel)` at `now_ms`.
fn record(trail: &Trail, fix: (u64, [f32; 2], [f32; 2]), now_ms: u64) -> serde_json::Value {
    let (t, p, v) = fix;
    let turning = {
        let since = t.saturating_sub(trail.leg_start_ms(t));
        trail.at(t).1 > 0 && (TURN_SEEN_MS[0]..=TURN_SEEN_MS[1]).contains(&since)
    };
    // The estimate coasted `dt` s past the fix: along the trail through a
    // turn, else on at the fix's velocity. The record is the state at
    // `now_ms`, as an estimator stepping every frame publishes it.
    let coast = |dt: f32| {
        if turning {
            trail.at(t + (dt * 1000.0) as u64).0
        } else {
            [p[0] + v[0] * dt, p[1] + v[1] * dt]
        }
    };
    let age = (now_ms - t) as f32 / 1000.0;
    let now_p = coast(age);
    let preds: Vec<_> = HORIZONS_S
        .iter()
        .map(|&dt| {
            let q = coast(age + dt);
            json!({"dt_s": dt, "pos_ned": [q[0], q[1], 0.0], "cov": [0.1, 0, 0, 0, 0.1, 0, 0, 0, 0.1]})
        })
        .collect();
    let modes = if turning {
        [0.3, 0.6, 0.1]
    } else {
        [0.9, 0.05, 0.05]
    };
    json!({"v": 1, "camera_id": "front", "id": SUBJECT_ID, "stamp_ns": now_ms * 1_000_000,
        "fix_age_ms": now_ms - t, "pos_ned": [now_p[0], now_p[1], 0.0],
        "cov": [0.1, 0, 0, 0, 0.1, 0, 0, 0, 0.1], "vel_ned": [v[0], v[1], 0.0],
        "mode_probs": modes, "height_m": HEAD_M, "predictions": preds, "sources": ["box"]})
}

/// Up at the hold, a lease, then the walker walks `trail` with the skill
/// sending intents and the camera fixing what it sees; `setup.tail_ms`
/// after it stops the run ends.
pub fn fly(s: &mut Sim, trail: &Trail, setup: &Setup) -> Follow {
    s.boot();
    launch_to_hold(s, ALT_M);
    let lease_id = lease(s);
    let trail = Trail {
        start_ms: s.now + trail.start_ms,
        ..trail.clone()
    };
    let mut f = Follow {
        min_separation_m: f32::INFINITY,
        min_clearance_m: s.stand_clearance_m(),
        ..Follow::default()
    };
    let mut fix: Option<(u64, [f32; 2], [f32; 2])> = None;
    let mut stopped_ms = None;
    // The orbit's phase, from where the vehicle is now; the vehicle's
    // bearing from the subject last period.
    let bearing_from = |c: [f32; 2], p: [f32; 2]| ados_pilot::geo::atan2(p[1] - c[1], p[0] - c[0]);
    let mut phase = setup.orbit_dps.map(|_| {
        let t = s.fc.truth();
        bearing_from(trail.at(s.now).0, [t[0], t[1]])
    });
    let mut last_b = phase;
    loop {
        let now = s.now;
        let (truth, _) = trail.at(now);
        let t = s.fc.truth();
        let veh = [t[0], t[1]];
        let sees = clear_line(&s.stand, veh, truth);
        let blind = setup.blind_ms.is_some_and(|(a, b)| {
            let since = now.saturating_sub(trail.start_ms);
            now >= trail.start_ms && (a..b).contains(&since)
        });
        let dist = (truth[0] - veh[0]).hypot(truth[1] - veh[1]);
        if sees && !blind && dist <= CAMERA_RANGE_M {
            let noise = [s.rng.range(-0.1, 0.1), s.rng.range(-0.1, 0.1)];
            fix = Some((
                now,
                [truth[0] + noise[0], truth[1] + noise[1]],
                trail.vel(now),
            ));
        }
        if let Some(fx) = fix {
            if now - fx.0 <= FEED_MAX_AGE_MS {
                let r = record(&trail, fx, now);
                s.p.on_feed(ados_pilot::events::TOPIC_SUBJECT, &r, now);
            }
        }
        let (est, vel, unseen) = match fix {
            Some((ft, p, v)) => {
                let dt = (now - ft) as f32 / 1000.0;
                ([p[0] + v[0] * dt, p[1] + v[1] * dt], v, dt)
            }
            None => (truth, trail.vel(now), 0.0),
        };
        let goal = intent(est, vel, unseen, phase, setup);
        let r = s.request(Request::Goal { lease_id, goal });
        assert!(matches!(r, Reply::None), "{r:?}");
        for _ in 0..GOAL_MS / ados_pilot::core::TICK_MS {
            s.step();
            f.min_clearance_m = f.min_clearance_m.min(s.stand_clearance_m());
            let t = s.fc.truth();
            let (w, _) = trail.at(s.now);
            let over = -t[2] - HEAD_M;
            if over < OVER_HEAD_M {
                f.min_separation_m = f.min_separation_m.min((w[0] - t[0]).hypot(w[1] - t[1]));
            }
        }
        let orbiting = setup.orbit_dps.is_some()
            && f.orbit_full_ms.is_none()
            && now.saturating_sub(trail.start_ms) < ORBIT_LIMIT_MS;
        if let (Some(p), Some(rate), Some(lb)) = (phase.as_mut(), setup.orbit_dps, last_b.as_mut())
        {
            let t = s.fc.truth();
            let (w, _) = trail.at(s.now);
            let b = bearing_from(w, [t[0], t[1]]);
            f.orbit_deg += ados_pilot::geo::wrap_pi(b - *lb).to_degrees();
            *lb = b;
            if f.orbit_deg >= 360.0 && f.orbit_full_ms.is_none() {
                f.orbit_full_ms = Some(s.now.saturating_sub(trail.start_ms));
            }
            if ados_pilot::geo::wrap_pi(*p - b) <= ORBIT_MAX_LEAD_DEG.to_radians() {
                *p = ados_pilot::geo::wrap_pi(*p + rate.to_radians() * GOAL_MS as f32 / 1000.0);
            }
        }
        if trail.walking(now) || orbiting {
            f.periods += 1;
            f.seen += u32::from(sees);
            f.max_dist_m = f.max_dist_m.max(dist);
            f.in_reach += u32::from(dist <= BAND_M[1] + 2.0);
        } else if now >= trail.start_ms {
            let since = *stopped_ms.get_or_insert(now);
            if now - since >= setup.tail_ms {
                break;
            }
        }
    }
    f
}

/// Full-height wall from `a` to `b` as touching 0.3 m disks.
fn wall(s: &mut Sim, a: [f32; 2], b: [f32; 2]) {
    let len = (b[0] - a[0]).hypot(b[1] - a[1]);
    let n = (len / 0.4).ceil() as usize;
    for k in 0..=n {
        let t = k as f32 / n as f32;
        s.stand
            .push(([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t], 0.3));
    }
}

/// Trunks (0.12-0.3 m) on a jittered 4 m lattice over `n` (north rows) by
/// `e` (east columns) around the origin, none within `keep` m of `trail` or
/// 3 m of the take-off.
fn stand(s: &mut Sim, trail: &[[f32; 2]], rows: std::ops::Range<i32>, keep: f32) {
    for i in rows {
        for j in -6..=6 {
            let c = [
                4.0 * i as f32 + s.rng.range(-1.2, 1.2),
                4.0 * j as f32 + s.rng.range(-1.2, 1.2),
            ];
            let r = s.rng.range(0.12, 0.3);
            let off = trail
                .windows(2)
                .map(|w| {
                    let ab = [w[1][0] - w[0][0], w[1][1] - w[0][1]];
                    let ap = [c[0] - w[0][0], c[1] - w[0][1]];
                    let l2 = (ab[0] * ab[0] + ab[1] * ab[1]).max(1e-9);
                    let t = ((ap[0] * ab[0] + ap[1] * ab[1]) / l2).clamp(0.0, 1.0);
                    (ap[0] - t * ab[0]).hypot(ap[1] - t * ab[1])
                })
                .fold(f32::INFINITY, f32::min);
            if off - r >= keep && c[0].hypot(c[1]) - r >= 3.0 {
                s.stand.push((c, r));
            }
        }
    }
}

fn report(s: &Sim, f: &Follow) {
    eprintln!(
        "{} s{} [{}]: visibility {:.2} over {} periods, min separation {:.2} m, min clearance {:.2} m, within band+2 {:.2}, farthest {:.1} m",
        s.name,
        s.seed,
        if std::env::var("DST_PLANNER").is_ok_and(|v| v == "trajectory") {
            "trajectory"
        } else {
            "reactive"
        },
        f.visibility(),
        f.periods,
        f.min_separation_m,
        f.min_clearance_m,
        f.reach_share(),
        f.max_dist_m,
    );
}

/// A walker up a straight way that steps round a trunk standing on it
/// every 10 m (0.25-0.35 m, the walker passing 0.8 m to one side): a
/// vehicle straight behind at the standoff has the trunk between it and
/// the walker for metres after each one. The vehicle keeps the walker in
/// sight, out of the margin and its separation.
pub fn trunks(seed: u64, ring: bool) -> (Sim, Follow) {
    let mut s = Sim::new("follow_trunks", seed, |_| {});
    let mut trail = vec![[8.0f32, 0.0f32]];
    for k in 0..6 {
        let n = 14.0 + 10.0 * k as f32 + s.rng.range(-1.0, 1.0);
        let side = if s.rng.unit() < 0.5 { -1.0 } else { 1.0 };
        let r = s.rng.range(0.25, 0.35);
        s.stand.push(([n, 0.0], r));
        trail.extend([[n - 1.5, 0.0], [n, side * (r + 0.8)], [n + 1.5, 0.0]]);
    }
    trail.push([80.0, 0.0]);
    let t = Trail {
        pts: trail,
        pace: WALK_MPS,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: Some(180.0),
        blind_ms: None,
        tail_ms: 5_000,
        ring,
        orbit_dps: None,
    };
    let f = fly(&mut s, &t, &setup);
    report(&s, &f);
    (s, f)
}

/// A walker north along a wall 2 m to its right that turns right round
/// the wall's end: a vehicle behind at the standoff has the wall between
/// it and the walker until it reaches the corner. The vehicle keeps the
/// walker in sight round the corner.
pub fn corner(seed: u64, ring: bool) -> (Sim, Follow) {
    let mut s = Sim::new("follow_corner", seed, |_| {});
    let end = 30.0 + s.rng.range(-1.0, 1.0);
    wall(&mut s, [-6.0, 2.0], [end, 2.0]);
    let t = Trail {
        pts: vec![[8.0, 0.0], [end + 2.0, 0.0], [end + 2.0, 25.0]],
        pace: WALK_MPS,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: Some(180.0),
        blind_ms: None,
        tail_ms: 5_000,
        ring,
        orbit_dps: None,
    };
    let f = fly(&mut s, &t, &setup);
    report(&s, &f);
    (s, f)
}

/// A walker up a 5 m-wide corridor that ends in a T, turning east into a
/// 5 m-wide cross corridor and out of sight of a vehicle behind it; the
/// bearing is the planner's (auto). The vehicle keeps the walker in sight
/// round the corner.
pub fn t_junction(seed: u64, ring: bool) -> (Sim, Follow) {
    let mut s = Sim::new("follow_t_junction", seed, |_| {});
    let j = s.rng.range(-0.5, 0.5);
    // The corridor north, its walls ending at the cross corridor.
    wall(&mut s, [-6.0, -2.5], [27.5, -2.5]);
    wall(&mut s, [-6.0, 2.5], [27.5, 2.5]);
    // The cross corridor, 5 m wide, and the T's far wall.
    wall(&mut s, [27.5, 2.5], [27.5, 30.0]);
    wall(&mut s, [27.5, -2.5], [27.5, -20.0]);
    wall(&mut s, [32.5, -20.0], [32.5, 30.0]);
    let t = Trail {
        pts: vec![[8.0, 0.0], [30.0 + j, 0.0], [30.0 + j, 24.0]],
        pace: WALK_MPS,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: None,
        blind_ms: None,
        tail_ms: 5_000,
        ring,
        orbit_dps: None,
    };
    let f = fly(&mut s, &t, &setup);
    report(&s, &f);
    (s, f)
}

/// A walker turns back and walks toward the vehicle, and the camera loses
/// it a second later for 8 s (no fix, then no record): the vehicle keeps
/// its separation from where the walker really is.
pub fn toward_at_loss(seed: u64, ring: bool) -> (Sim, Follow) {
    let mut s = Sim::new("follow_toward_at_loss", seed, |_| {});
    let back = s.rng.range(-0.5, 0.5);
    let t = Trail {
        pts: vec![[8.0, 0.0], [18.0, 0.0], [-12.0, back]],
        pace: WALK_MPS,
        start_ms: 0,
    };
    // The turn comes 10 m into the walk.
    let turn_ms = (10.0 / WALK_MPS * 1000.0) as u64;
    let setup = Setup {
        bearing_pref_deg: Some(180.0),
        blind_ms: Some((turn_ms + 1_000, turn_ms + 9_000)),
        tail_ms: 8_000,
        ring,
        orbit_dps: None,
    };
    let f = fly(&mut s, &t, &setup);
    report(&s, &f);
    (s, f)
}

/// A walker (or a jogger) straight away from the vehicle through a stand
/// whose trunks keep 1.8 m off its way, sensed on `tier` (the planar tier's
/// speed rule holds it to 3 m/s, so a jog is followed on the 3D lidar
/// tier): the vehicle keeps up, within the band's far edge plus 2 m.
pub fn walk_away(seed: u64, pace: f32, tier: SensorTier, ring: bool) -> (Sim, Follow) {
    let mut s = Sim::new("follow_walk_away", seed, |_| {});
    s.tier = tier;
    let trail = vec![[8.0, 0.0], [68.0, 0.0]];
    let mut path = vec![[0.0, 0.0]];
    path.extend_from_slice(&trail);
    stand(&mut s, &path, -1..20, 1.8);
    let t = Trail {
        pts: trail,
        pace,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: Some(180.0),
        blind_ms: None,
        tail_ms: 4_000,
        ring,
        orbit_dps: None,
    };
    let f = fly(&mut s, &t, &setup);
    report(&s, &f);
    (s, f)
}

/// The skill orbits a standing subject clockwise at 10°/s with its intent's
/// bearing over the ground (the orbit's phase), under its lead rule; trunks
/// stand 2.5 m round the subject (inside the band) and 11-12 m out
/// (outside it, 3 m off the take-off at least), on seeded bearings. The
/// vehicle goes all the way round.
pub fn orbit(seed: u64) -> (Sim, Follow) {
    let mut s = Sim::new("follow_orbit", seed, |_| {});
    let c = [8.0f32, 0.0f32];
    for k in 0..3 {
        let a = (120.0 * k as f32 + s.rng.range(0.0, 120.0)).to_radians();
        let r = s.rng.range(0.15, 0.25);
        s.stand
            .push(([c[0] + 2.5 * a.cos(), c[1] + 2.5 * a.sin()], r));
    }
    for k in 0..6 {
        let a = (60.0 * k as f32 + s.rng.range(0.0, 60.0)).to_radians();
        let d = s.rng.range(11.0, 12.0);
        let r = s.rng.range(0.15, 0.3);
        s.stand.push(([c[0] + d * a.cos(), c[1] + d * a.sin()], r));
    }
    let t = Trail {
        pts: vec![c],
        pace: WALK_MPS,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: None,
        blind_ms: None,
        tail_ms: 2_000,
        ring: false,
        orbit_dps: Some(10.0),
    };
    let f = fly(&mut s, &t, &setup);
    report(&s, &f);
    eprintln!(
        "{} s{}: orbited {:.0}°, all the way round at {:?} ms",
        s.name, s.seed, f.orbit_deg, f.orbit_full_ms
    );
    (s, f)
}

/// Every follow case, with today's ring goal and with intents, on seeds
/// 1-4 (`DST_FOLLOW_CASE` keeps the cases whose name contains it,
/// `DST_FOLLOW_SEED` one seed, `DST_FOLLOW_ONLY` one goal, `ring` or
/// `intent`): one table row per case, goal and seed.
pub fn table() {
    type Case = (&'static str, fn(u64, bool) -> (Sim, Follow));
    let cases: [Case; 6] = [
        ("corner", corner),
        ("trunks", trunks),
        ("t_junction", t_junction),
        ("toward_at_loss", toward_at_loss),
        ("walk_away 1.4", |s, r| {
            walk_away(s, WALK_MPS, SensorTier::Planar, r)
        }),
        ("jog_away 3.0", |s, r| {
            walk_away(s, JOG_MPS, SensorTier::Mid360, r)
        }),
    ];
    let only = std::env::var("DST_FOLLOW_CASE").unwrap_or_default();
    let seed: Option<u64> = std::env::var("DST_FOLLOW_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut rows = Vec::new();
    for (name, case) in cases.iter().filter(|c| c.0.contains(only.as_str())) {
        for s in (1..=4).filter(|s| seed.is_none_or(|k| k == *s)) {
            for ring in [true, false].into_iter().filter(|r| {
                std::env::var("DST_FOLLOW_ONLY")
                    .is_ok_and(|g| g == if *r { "ring" } else { "intent" })
                    || std::env::var("DST_FOLLOW_ONLY").is_err()
            }) {
                let (_, f) = case(s, ring);
                rows.push(format!(
                    "| {name} | s{s} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} |",
                    if ring { "ring" } else { "intent" },
                    f.visibility(),
                    f.min_separation_m,
                    f.min_clearance_m,
                    f.reach_share(),
                    f.max_dist_m,
                ));
            }
        }
    }
    eprintln!("| case | seed | goal | visibility | min sep m | min clear m | within band+2 | farthest m |");
    eprintln!("|---|---|---|---|---|---|---|---|");
    for r in rows {
        eprintln!("{r}");
    }
}
