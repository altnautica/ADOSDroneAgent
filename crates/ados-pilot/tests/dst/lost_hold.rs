//! A follow skill that lost its subject holds where its search ended,
//! awaiting the operator: a `position` goal that names no subject. The
//! subject walks (or runs) back along their way, toward the vehicle and
//! under it. The planar lidar, level at the hold height, never returns
//! them. The vision engine still boxes them, the way it does on a rendered
//! bench: confirmed `person` tracks with no world position, seen through a
//! forward camera ([`W_PX`] x [`H_PX`], [`HFOV_DEG`], pitched
//! [`MOUNT_PITCH_DEG`]), [`LAG_MS`] late (each batch stamped with its
//! frame's simulation time), a frame every [`FRAME_MS`], with
//! [`EDGE_NOISE_PX`] of noise on each box edge, no box near the frame's
//! sides (a partial person is missed), and frames the tracker calls `lost`
//! (the last box held, coasting). The vehicle keeps the subject's
//! separation unless [`follow::OVER_HEAD_M`] over their head.

use std::collections::VecDeque;

use ados_pilot::core::Reply;
use ados_protocol::pilot::{Candidates, Goal, GoalKind, Request};
use serde_json::{json, Value};

use crate::follow::{self, Follow};
use crate::forest::lease;
use crate::scenarios::launch_to_hold;
use crate::sensors::Walker;
use crate::sim::Sim;

/// The hold's height, m (the rendered bench held 3.6-3.9 m over the ground).
const HOLD_ALT_M: f32 = 3.8;
/// The skill's goal period, ms.
const GOAL_MS: u64 = 100;
/// The camera: frame size, horizontal field of view, mount pitch.
const W_PX: f32 = 1280.0;
const H_PX: f32 = 720.0;
const HFOV_DEG: f32 = 87.0;
const MOUNT_PITCH_DEG: f32 = -15.0;
/// The detector's frame period and the batch's age on arrival, ms.
const FRAME_MS: u64 = 120;
const LAG_MS: u64 = 220;
/// One sigma of noise on each box edge, px.
const EDGE_NOISE_PX: f32 = 3.0;
/// No box while its centre is this near a side of the frame, px.
const SIDE_MISS_PX: f32 = 60.0;
/// The share of frames with no box, and of those boxed that the tracker
/// calls lost.
const MISS_SHARE: f32 = 0.15;
const LOST_SHARE: f32 = 0.25;
/// The subject's height, m.
const HEAD_M: f32 = 1.75;
/// The subject sets off this far out, m, and walks on this far past.
const START_M: f32 = 25.0;
const PAST_M: f32 = 15.0;

/// World NED `v` in the body frame of a vehicle at `att` (roll, pitch, yaw).
fn world_to_body(v: [f32; 3], att: [f32; 3]) -> [f32; 3] {
    let (sr, cr) = att[0].sin_cos();
    let (sp, cp) = att[1].sin_cos();
    let (sy, cy) = att[2].sin_cos();
    let (x1, y1) = (v[0] * cy + v[1] * sy, -v[0] * sy + v[1] * cy);
    let (x2, z2) = (x1 * cp - v[2] * sp, x1 * sp + v[2] * cp);
    [x2, y1 * cr + z2 * sr, -y1 * sr + z2 * cr]
}

/// The pixel of world point `p` (NED) from a vehicle at `veh` and `att`;
/// `None` behind the camera.
fn pixel(p: [f32; 3], veh: [f32; 3], att: [f32; 3]) -> Option<[f32; 2]> {
    let b = world_to_body([p[0] - veh[0], p[1] - veh[1], p[2] - veh[2]], att);
    let (s, c) = MOUNT_PITCH_DEG.to_radians().sin_cos();
    let cam = [b[0] * c - b[2] * s, b[1], b[0] * s + b[2] * c];
    if cam[0] < 0.3 {
        return None;
    }
    let f = W_PX / 2.0 / (HFOV_DEG.to_radians() / 2.0).tan();
    Some([
        W_PX / 2.0 + f * cam[1] / cam[0],
        H_PX / 2.0 + f * cam[2] / cam[0],
    ])
}

/// The box of a person standing at `w` (north, east, on the ground at
/// `d` 0) seen from `veh`, `att`, cut at the frame; `None` when the camera
/// does not see enough of them.
fn person_box(w: [f32; 2], veh: [f32; 3], att: [f32; 3]) -> Option<[f32; 4]> {
    let head = pixel([w[0], w[1], -HEAD_M], veh, att)?;
    let feet = pixel([w[0], w[1], 0.0], veh, att)?;
    let u = (head[0] + feet[0]) / 2.0;
    if !(SIDE_MISS_PX..=W_PX - SIDE_MISS_PX).contains(&u)
        || head[1] >= H_PX - 20.0
        || feet[1] <= 0.0
    {
        return None;
    }
    let (top, bottom) = (head[1].max(0.0), feet[1].min(H_PX));
    let width = 0.3 * (feet[1] - head[1]);
    Some([u - width / 2.0, top, width, bottom - top])
}

/// The batch's JSON (`vision.detection` v4) of the frame taken at
/// `taken_ms` (stamped in simulation time, as a simulated camera's engine
/// stamps it; unstamped, as a real camera's, with `DST_LH_UNSTAMPED`)
/// with one track boxed at `b`.
fn batch(frame: u64, taken_ms: u64, b: Option<([f32; 4], &str)>) -> Value {
    let dets: Vec<Value> = b
        .map(|(b, state)| {
            json!({"class_label": "person", "confidence": 0.7, "track_id": 7,
                "track_state": state,
                "bbox": {"x": b[0], "y": b[1], "width": b[2], "height": b[3]}})
        })
        .into_iter()
        .collect();
    let mut v = json!({"v": 4, "model_id": "person", "camera_id": "main", "frame_id": frame,
        "hfov_deg": HFOV_DEG, "mount_pitch_deg": MOUNT_PITCH_DEG,
        "frame_width": W_PX as u32, "frame_height": H_PX as u32, "detections": dets});
    if std::env::var("DST_LH_UNSTAMPED").is_err() {
        v["ts_sensor_ms"] = json!(taken_ms);
    }
    v
}

/// The camera and its engine: the vehicle's pose over the last [`LAG_MS`]
/// and the last confirmed box, held while the tracker calls it lost.
#[derive(Default)]
struct Camera {
    seen: VecDeque<(u64, [f32; 3], [f32; 3])>,
    held: Option<[f32; 4]>,
    frame: u64,
}

impl Camera {
    /// After a tick: note the vehicle's pose and, on a frame of the walk
    /// (every [`FRAME_MS`] from its start), feed the batch of the frame
    /// taken [`LAG_MS`] ago with `walker` boxed, counted in `f`.
    fn tick(&mut self, s: &mut Sim, walker: &Walker, f: &mut Follow) {
        let now = s.now;
        self.seen.push_back((now, s.fc.truth(), s.fc.att));
        while self.seen.front().is_some_and(|x| x.0 + LAG_MS < now) {
            self.seen.pop_front();
        }
        if now < walker.start_ms || !(now - walker.start_ms).is_multiple_of(FRAME_MS) {
            return;
        }
        let (then, veh, att) = self.seen[0];
        self.frame += 1;
        f.periods += 1;
        let b = person_box(walker.at(then), veh, att).map(|mut b| {
            for x in &mut b {
                *x += EDGE_NOISE_PX * s.rng.range(-1.7, 1.7);
            }
            b
        });
        let det = match b {
            Some(_) if s.rng.unit() < MISS_SHARE => None,
            Some(_) if s.rng.unit() < LOST_SHARE => self.held.map(|h| (h, "lost")),
            Some(b) => {
                self.held = Some(b);
                f.seen += 1;
                Some((b, "confirmed"))
            }
            None => self.held.map(|h| (h, "lost")),
        };
        s.p.on_feed(
            ados_pilot::events::TOPIC_DETECTION,
            &batch(self.frame, then, det),
            now,
        );
    }
}

/// The nearest the vehicle at `t` comes to `w` horizontally while less than
/// [`follow::OVER_HEAD_M`] over its head, folded into `f`.
fn note_separation(f: &mut Follow, t: [f32; 3], w: [f32; 2]) {
    if -t[2] - HEAD_M < follow::OVER_HEAD_M {
        f.min_separation_m = f.min_separation_m.min((w[0] - t[0]).hypot(w[1] - t[1]));
    }
}

/// Up at the hold, then held by a skill that names no subject while the
/// subject walks at `pace` from [`START_M`] ahead (a seeded bearing within
/// 25° of the nose) straight at the vehicle, passing within half a metre
/// of it, and on [`PAST_M`] past.
pub fn walk_under(seed: u64, pace: f32) -> (Sim, Follow) {
    let mut s = Sim::new("lost_hold_walk_under", seed, |_| {});
    s.boot();
    launch_to_hold(&mut s, HOLD_ALT_M);
    let lease_id = lease(&mut s);
    let hold = s.fc.truth();
    let yaw = s.fc.att[2];
    let off = s.rng.range(-25.0, 25.0).to_radians() + yaw;
    let side = s.rng.range(-0.5, 0.5);
    let (u, n) = ([off.cos(), off.sin()], [-off.sin(), off.cos()]);
    let at = |k: f32| {
        [
            hold[0] + u[0] * k + n[0] * side,
            hold[1] + u[1] * k + n[1] * side,
        ]
    };
    let walker = Walker {
        path: vec![at(START_M), at(-PAST_M)],
        speed: pace,
        start_ms: s.now + 1_000,
    };
    let end_ms = walker.start_ms + ((START_M + PAST_M) / pace * 1000.0) as u64;
    s.walkers = vec![walker.clone()];
    let mut f = Follow {
        min_separation_m: f32::INFINITY,
        min_clearance_m: s.stand_clearance_m(),
        ..Follow::default()
    };
    let mut goal = Goal::new(GoalKind::Position, hold, 6.0);
    goal.yaw = Some(yaw);
    goal.margin_m = Some(1.5);
    let mut cam = Camera::default();
    while s.now < end_ms + 2_000 {
        let r = s.request(Request::Goal {
            lease_id,
            goal: goal.clone(),
        });
        assert!(matches!(r, Reply::None), "{r:?}");
        for _ in 0..GOAL_MS / ados_pilot::core::TICK_MS {
            s.step();
            note_separation(&mut f, s.fc.truth(), walker.at(s.now));
            cam.tick(&mut s, &walker, &mut f);
        }
    }
    eprintln!(
        "{} s{} pace {pace}: nearest {:.2} m, boxed {:.2} of frames",
        s.name,
        s.seed,
        f.min_separation_m,
        f.visibility()
    );
    (s, f)
}

/// The skill's range to its subject, read from the same box, is this much
/// long (the rendered bench's box ranges read 12-30 % long at the median).
const SKILL_RANGE_LONG: f32 = 0.25;
/// The ring goal's radius, m.
const RING_M: f32 = 6.0;

/// A ring goal names its subject by place alone (`subject_ne`, no
/// `subject_ref`: the goal a skill sends when its hard guard trips, an orbit
/// ring, an older skill), placed from the subject's own box
/// [`SKILL_RANGE_LONG`] long, while the pilot places the same box as a
/// person track. The subject walks at `pace` 10 m away from the vehicle,
/// turns back and walks on past it. Returns the run and the goal periods the
/// person keep bound: the only person is the subject, so it never should.
pub fn ring_own_box(seed: u64, pace: f32) -> (Sim, Follow, u32) {
    let mut s = Sim::new("lost_hold_ring_own_box", seed, |_| {});
    s.boot();
    launch_to_hold(&mut s, HOLD_ALT_M);
    let lease_id = lease(&mut s);
    let hold = s.fc.truth();
    let yaw = s.fc.att[2];
    let back = s.rng.range(-0.5, 0.5);
    let (u, n) = ([yaw.cos(), yaw.sin()], [-yaw.sin(), yaw.cos()]);
    let at = |k: f32, side: f32| {
        [
            hold[0] + u[0] * k + n[0] * side,
            hold[1] + u[1] * k + n[1] * side,
        ]
    };
    let walker = Walker {
        path: vec![at(8.0, 0.0), at(18.0, 0.0), at(-12.0, back)],
        speed: pace,
        start_ms: s.now + 1_000,
    };
    let end_ms = walker.start_ms + (40.0 / pace * 1000.0) as u64;
    s.walkers = vec![walker.clone()];
    let mut f = Follow {
        min_separation_m: f32::INFINITY,
        min_clearance_m: s.stand_clearance_m(),
        ..Follow::default()
    };
    let mut cam = Camera::default();
    let mut bound = 0;
    while s.now < end_ms {
        let now = s.now;
        let t = s.fc.truth();
        let w = walker.at(now);
        let k = 1.0 + SKILL_RANGE_LONG;
        let est = [t[0] + (w[0] - t[0]) * k, t[1] + (w[1] - t[1]) * k];
        let prev = walker.at(now.saturating_sub(GOAL_MS));
        let vel = [(w[0] - prev[0]) * 10.0, (w[1] - prev[1]) * 10.0];
        let speed = vel[0].hypot(vel[1]);
        let head = if speed > 0.3 {
            [vel[0] / speed, vel[1] / speed]
        } else {
            u
        };
        let point = [
            est[0] - head[0] * RING_M,
            est[1] - head[1] * RING_M,
            hold[2],
        ];
        let mut g = Goal::new(GoalKind::Follow, point, 6.0);
        (g.vn, g.ve) = (vel[0], vel[1]);
        g.subject_ne = Some(est);
        g.subject_sigma_m = Some(0.5);
        g.margin_m = Some(1.5);
        g.candidates = Some(Candidates {
            ring_center_ne: est,
            radius_m: RING_M,
            angle_deg: ados_pilot::geo::atan2(-head[1], -head[0]).to_degrees(),
            angle_span_deg: 90.0,
        });
        let r = s.request(Request::Goal { lease_id, goal: g });
        assert!(matches!(r, Reply::None), "{r:?}");
        for _ in 0..GOAL_MS / ados_pilot::core::TICK_MS {
            s.step();
            note_separation(&mut f, s.fc.truth(), walker.at(s.now));
            cam.tick(&mut s, &walker, &mut f);
        }
        let limits = s.state.command.as_ref().map(|c| c.limits.clone());
        bound += u32::from(limits.is_some_and(|l| l.iter().any(|c| c == "person")));
    }
    eprintln!(
        "{} s{} pace {pace}: person keep bound {bound} periods, nearest {:.2} m, boxed {:.2} of frames",
        s.name,
        s.seed,
        f.min_separation_m,
        f.visibility()
    );
    (s, f, bound)
}
