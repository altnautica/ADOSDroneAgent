//! Follow flights through trunks: a skill's follow goal behind a walker,
//! flown by the pilot's planner and obstacle brake against a simulated
//! planar lidar casting each scan on the stand ([`Sim::stand`]). They check
//! what a competent pilot does in a forest: full speed down a clear corridor
//! between trunk rows, never into a gap the vehicle does not fit, a held
//! detour around a trunk in the way, and keeping up with a walker through a
//! seeded random stand, never inside the margin.

use ados_protocol::pilot::{Candidates, Goal, GoalKind, Holder, Request};

use crate::scenarios::launch_to_hold;
use crate::sim::Sim;
use ados_pilot::core::Reply;

/// The pilot's obstacle margin in an open place (the default), m.
const MARGIN_M: f32 = 1.5;
/// Follow height, m.
const ALT_M: f32 = 4.0;
/// The follow distance behind the walker, m.
const BEHIND_M: f32 = 6.0;
/// A walker's pace, m/s.
const WALK_MPS: f32 = 1.4;
/// The skill's goal period, ms (the follow skill's 10 Hz).
const GOAL_MS: u64 = 100;

pub(crate) fn lease(s: &mut Sim) -> u64 {
    let r = s.request(Request::LeaseAcquire {
        holder: Holder {
            plugin_id: "com.example.follow".into(),
            skill_id: "follow-me".into(),
        },
        kind: "motion".into(),
    });
    match r {
        Reply::Lease(l) => l.lease_id.expect("lease granted"),
        other => panic!("{other:?}"),
    }
}

/// A follow goal `BEHIND_M` behind the walker at `walker` heading `head`
/// (unit), with the walker's velocity as its feed-forward and the ring of
/// detour spots around the walker.
fn follow_goal(walker: [f32; 2], head: [f32; 2], speed: f32, max_mps: f32) -> Goal {
    let at = [
        walker[0] - BEHIND_M * head[0],
        walker[1] - BEHIND_M * head[1],
    ];
    let mut g = Goal::new(GoalKind::Follow, [at[0], at[1], -ALT_M], max_mps);
    g.vn = head[0] * speed;
    g.ve = head[1] * speed;
    g.subject_ne = Some(walker);
    g.candidates = Some(Candidates {
        ring_center_ne: walker,
        radius_m: BEHIND_M,
        angle_deg: ados_pilot::geo::atan2(-head[1], -head[0]).to_degrees(),
        angle_span_deg: 90.0,
    });
    g
}

/// What one follow flight measured.
#[derive(Debug, Default)]
pub struct Walk {
    /// Nearest any trunk surface came to the vehicle, m.
    pub min_clearance_m: f32,
    /// Horizontal speed per goal period while the walker walked, m/s.
    pub speeds: Vec<f32>,
    /// The vehicle's distance to its goal per goal period, m.
    pub to_goal_m: Vec<f32>,
    /// Vehicle positions per goal period.
    pub track: Vec<[f32; 2]>,
    /// Route switches over the walk ([`ados_pilot::core::Pilot::route_switches`]).
    pub switches: u32,
    /// The copter's tilt from level every tick of the walk, degrees.
    pub tilt_deg: Vec<f32>,
    /// The fly-under height the pilot reported over the way to the walker,
    /// per goal period it reported one (`overhead_ahead`).
    pub fly_under_m: Vec<f32>,
}

impl Walk {
    pub fn mean_speed(&self) -> f32 {
        self.speeds.iter().sum::<f32>() / self.speeds.len().max(1) as f32
    }

    /// The 95th percentile of [`Walk::tilt_deg`].
    pub fn tilt_p95(&self) -> f32 {
        let mut t = self.tilt_deg.clone();
        t.sort_by(f32::total_cmp);
        t.get(t.len() * 95 / 100).copied().unwrap_or(0.0)
    }
}

/// Up at the hold, a lease, then the walker walks `trail` (a polyline) at
/// `pace` with the goal following it; the run goes on `tail_ms` after the
/// walker stops.
fn walk(s: &mut Sim, trail: &[[f32; 2]], pace: f32, max_mps: f32, tail_ms: u64) -> Walk {
    s.boot();
    launch_to_hold(s, ALT_M);
    let lease_id = lease(s);
    let mut w = Walk {
        min_clearance_m: s.stand_clearance_m(),
        ..Walk::default()
    };
    let switches0 = s.p.route_switches();
    let mut leg = 0;
    let mut at = trail[0];
    let mut stopped_ms = None;
    loop {
        // The walker steps along the trail.
        let mut step = pace * GOAL_MS as f32 / 1000.0;
        while step > 0.0 && leg + 1 < trail.len() {
            let to = trail[leg + 1];
            let d = (to[0] - at[0]).hypot(to[1] - at[1]);
            if d <= step {
                at = to;
                step -= d;
                leg += 1;
            } else {
                at = [
                    at[0] + (to[0] - at[0]) * step / d,
                    at[1] + (to[1] - at[1]) * step / d,
                ];
                step = 0.0;
            }
        }
        let walking = leg + 1 < trail.len();
        let (a, b) = (
            trail[leg.min(trail.len() - 2)],
            trail[(leg + 1).min(trail.len() - 1)],
        );
        let len = (b[0] - a[0]).hypot(b[1] - a[1]).max(1e-3);
        let head = [(b[0] - a[0]) / len, (b[1] - a[1]) / len];
        let speed = if walking { pace } else { 0.0 };
        let goal = follow_goal(at, head, speed, max_mps);
        let g = [goal.n, goal.e];
        s.request(Request::Goal { lease_id, goal });
        for _ in 0..GOAL_MS / ados_pilot::core::TICK_MS {
            s.step();
            w.min_clearance_m = w.min_clearance_m.min(s.stand_clearance_m());
            let [roll, pitch, _] = s.fc.att;
            w.tilt_deg.push(
                ados_pilot::geo::acos((roll.cos() * pitch.cos()).clamp(-1.0, 1.0)).to_degrees(),
            );
        }
        let p = [s.fc.pos[0], s.fc.pos[1]];
        if let Some(h) = s
            .state
            .overhead_ahead
            .as_ref()
            .and_then(|o| o.fly_under_agl_m)
        {
            w.fly_under_m.push(h);
        }
        w.track.push(p);
        w.to_goal_m.push((g[0] - p[0]).hypot(g[1] - p[1]));
        if walking {
            w.speeds.push(s.fc.vel[0].hypot(s.fc.vel[1]));
        } else {
            let since = *stopped_ms.get_or_insert(s.now);
            if s.now - since >= tail_ms {
                break;
            }
        }
    }
    w.switches = s.p.route_switches() - switches0;
    w
}

/// Two rows of trunks either side of a straight trail, their near faces 2 m
/// off it (0.12-0.18 m radius, one every 2.5 m), the walker 14 m ahead of
/// the vehicle: the vehicle catches up at its goal's 3 m/s, never held to
/// the walker's pace by the trunks beside it, and never inside the margin.
pub fn forest_corridor(seed: u64) -> Sim {
    let mut s = Sim::new("forest_corridor", seed, |_| {});
    let phase = s.rng.range(0.0, 2.5);
    for k in 0..40 {
        let n = -5.0 + phase + 2.5 * k as f32;
        for side in [-1.0f32, 1.0] {
            let r = s.rng.range(0.12, 0.18);
            s.stand.push(([n, side * (2.0 + r)], r));
        }
    }
    let w = walk(&mut s, &[[14.0, 0.0], [70.0, 0.0]], WALK_MPS, 3.0, 2_000);
    // Catching up: once up to 1.6 m/s from the hover (the goal limits'
    // ramp), until within 3 m of the goal, the vehicle never flies slower.
    let caught = w
        .to_goal_m
        .iter()
        .position(|d| *d < 3.0)
        .expect("caught up with the goal");
    let up = w
        .speeds
        .iter()
        .position(|v| *v >= 1.6)
        .expect("up to speed");
    let window = &w.speeds[up..caught.min(w.speeds.len())];
    assert!(
        window.len() >= 20,
        "a catch-up of at least 2 s: {}",
        window.len()
    );
    let slowest = window.iter().copied().fold(f32::INFINITY, f32::min);
    eprintln!(
        "forest_corridor s{seed}: slowest {slowest:.2} m/s over {:.1} s of catch-up, mean {:.2} m/s, clearance {:.2} m, {} switches",
        window.len() as f32 / 10.0,
        w.mean_speed(),
        w.min_clearance_m,
        w.switches
    );
    assert!(slowest >= 1.6, "held to {slowest:.2} m/s between the rows");
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    assert_eq!(
        w.switches, 0,
        "the straight way down the corridor is never left"
    );
    s
}

/// A row of trunks across the trail with one gap narrower than twice the
/// margin (2.3-2.9 m between the surfaces) straight on the way: the vehicle
/// never enters the gap; it goes round the row's end or waits short of it,
/// never inside the margin.
pub fn forest_gap(seed: u64) -> Sim {
    let mut s = Sim::new("forest_gap", seed, |_| {});
    let gap = s.rng.range(2.3, 2.9);
    let r = 0.2;
    let half = gap / 2.0 + r;
    let mut e = half;
    while e <= 8.0 {
        s.stand.push(([12.0, e], r));
        s.stand.push(([12.0, -e], r));
        e += 0.9;
    }
    let w = walk(&mut s, &[[20.0, 0.0], [26.0, 0.0]], WALK_MPS, 3.0, 20_000);
    let through = w
        .track
        .iter()
        .any(|p| (p[0] - 12.0).abs() < 0.5 && p[1].abs() < half);
    eprintln!(
        "forest_gap s{seed}: gap {gap:.2} m, clearance {:.2} m, end {:?}, {} switches",
        w.min_clearance_m,
        w.track.last(),
        w.switches
    );
    assert!(!through, "flew into a {gap:.2} m gap");
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    s
}

/// A 0.8 m trunk straight between the vehicle and its goal: the planner
/// chooses a detour round it and holds that one route (into the detour and
/// back to the straight way once past, nothing else), never inside the
/// margin.
pub fn forest_detour(seed: u64) -> Sim {
    let mut s = Sim::new("forest_detour", seed, |_| {});
    let e = s.rng.range(-0.3, 0.3);
    s.stand.push(([9.0, e], 0.4));
    let w = walk(&mut s, &[[22.0, 0.0], [23.0, 0.0]], WALK_MPS, 3.0, 20_000);
    let end = *w.to_goal_m.last().expect("flew");
    eprintln!(
        "forest_detour s{seed}: {} switches, {end:.2} m from the goal, clearance {:.2} m",
        w.switches, w.min_clearance_m
    );
    assert!(
        w.switches <= 2,
        "{} route switches round one trunk",
        w.switches
    );
    assert!(end < 1.5, "never reached the goal: {end:.2} m off");
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    s
}

/// Horizontal distance from `p` to the polyline `path`, m.
fn from_path(path: &[[f32; 2]], p: [f32; 2]) -> f32 {
    path.windows(2)
        .map(|w| {
            let ab = [w[1][0] - w[0][0], w[1][1] - w[0][1]];
            let ap = [p[0] - w[0][0], p[1] - w[0][1]];
            let t =
                ((ap[0] * ab[0] + ap[1] * ab[1]) / (ab[0] * ab[0] + ab[1] * ab[1])).clamp(0.0, 1.0);
            (ap[0] - t * ab[0]).hypot(ap[1] - t * ab[1])
        })
        .fold(f32::INFINITY, f32::min)
}

/// A walker's trail from 8 m north that turns every 12 m (at most 40° off
/// north), and trunks (0.12-0.3 m) on a jittered 4 m lattice, none within
/// 1.8 m of the trail (of the leg out to it from the take-off too, with
/// `clear_start`) or 3 m of the take-off.
fn seeded_stand(s: &mut Sim, clear_start: bool) -> Vec<[f32; 2]> {
    let mut trail = vec![[8.0f32, 0.0f32]];
    let mut b = 0.0f32;
    for _ in 0..6 {
        b = (b + s.rng.range(-35.0, 35.0)).clamp(-40.0, 40.0);
        let last = *trail.last().expect("a start");
        let r = b.to_radians();
        trail.push([last[0] + 12.0 * r.cos(), last[1] + 12.0 * r.sin()]);
    }
    let mut path = Vec::with_capacity(trail.len() + 1);
    if clear_start {
        path.push([0.0, 0.0]);
    }
    path.extend_from_slice(&trail);
    for i in -1..22 {
        for j in -6..=6 {
            let c = [
                4.0 * i as f32 + s.rng.range(-1.2, 1.2),
                4.0 * j as f32 + s.rng.range(-1.2, 1.2),
            ];
            let r = s.rng.range(0.12, 0.3);
            if from_path(&path, c) - r >= 1.8 && c[0].hypot(c[1]) - r >= 3.0 {
                s.stand.push((c, r));
            }
        }
    }
    trail
}

/// A seeded stand: trunks (0.12-0.3 m) on a jittered 4 m lattice, none
/// within 1.8 m of a walker's trail that turns every 12 m, and the walker
/// walking it at 1.4 m/s. The vehicle keeps up through the turns, weaving
/// where the follow point cuts a corner into the trees, never inside the
/// margin and without flipping between routes.
pub fn forest_stand(seed: u64) -> Sim {
    let mut s = Sim::new("forest_stand", seed, |_| {});
    let trail = seeded_stand(&mut s, false);
    let w = walk(&mut s, &trail, WALK_MPS, 3.0, 5_000);
    let walk_s = w.speeds.len() as f32 / 10.0;
    let end = *w.to_goal_m.last().expect("flew");
    eprintln!(
        "forest_stand s{seed}: {} trunks, mean {:.2} m/s over {walk_s:.0} s, clearance {:.2} m, {} switches, {end:.1} m from the goal at the end, tilt p95 {:.1}°",
        s.stand.len(),
        w.mean_speed(),
        w.min_clearance_m,
        w.switches,
        w.tilt_p95()
    );
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    assert!(
        w.mean_speed() >= 0.9 * WALK_MPS,
        "kept to {:.2} m/s behind a {WALK_MPS} m/s walker",
        w.mean_speed()
    );
    assert!(end < 3.0, "left {end:.1} m behind its goal");
    assert!(
        w.switches as f32 <= walk_s / 3.0,
        "{} route switches in {walk_s:.0} s",
        w.switches
    );
    s
}

/// The tilt the canopy follow keeps under at the 95th percentile, degrees:
/// the goal limits' 2.5 m/s² is 14.3°.
const CANOPY_TILT_P95_DEG: f32 = 15.0;

/// The canopy copter's most horizontal acceleration, m/s²: about 20° of
/// lean, what a 5 kg quad leant braking under trees.
const CANOPY_MAX_ACC: f32 = 3.6;

/// The seeded stand under a conifer canopy, flown on a copter that leans
/// to accelerate (about 20° at most) with a lidar fixed to its frame, so
/// every lean tilts the scan. Each trunk carries a crown, an upright
/// cylinder of leaves 1.5-3 m in radius with its base 3-4.5 m over the
/// ground: the crowns reaching within 2.6 m of the path (take-off to the
/// trail's end) have their bases 0.4-0.5 m over the 4 m flight, so the
/// follow flies under them; the lower ones end further out. A level scan
/// passes under the canopy and a leaning one returns from it, a few
/// metres off. The vehicle keeps pace with the walker under the canopy,
/// never inside the trunks' margin, and leans no more than its goals ask.
pub fn forest_canopy(seed: u64) -> Sim {
    let mut s = Sim::new("forest_canopy", seed, |_| {});
    s.tilted_scan = true;
    s.fc.max_acc = Some(CANOPY_MAX_ACC);
    let trail = seeded_stand(&mut s, true);
    let mut path = vec![[0.0, 0.0]];
    path.extend_from_slice(&trail);
    let trunks = s.stand.clone();
    for (c, _) in trunks {
        let r = s.rng.range(1.5, 3.0);
        let low = s.rng.range(3.0, 4.5);
        let over = s.rng.range(4.4, 4.5);
        let base_m = if from_path(&path, c) - r < 2.6 {
            over
        } else {
            low
        };
        let top_m = base_m + s.rng.range(3.0, 6.0);
        s.crowns.push(crate::sim::Crown {
            c,
            r,
            base_m,
            top_m,
        });
    }
    let w = walk(&mut s, &trail, WALK_MPS, 3.0, 5_000);
    let walk_s = w.speeds.len() as f32 / 10.0;
    let end = *w.to_goal_m.last().expect("flew");
    let behind = w.to_goal_m.iter().copied().fold(0.0, f32::max);
    let lowest = w.fly_under_m.iter().copied().fold(f32::INFINITY, f32::min);
    eprintln!(
        "forest_canopy s{seed}: {} trunks under crowns, mean {:.2} m/s over {walk_s:.0} s, clearance {:.2} m, {end:.1} m from the goal at the end (at most {behind:.1}), tilt p95 {:.1}°, overhead reported on {} goal periods (lowest fly-under {lowest:.2} m)",
        s.stand.len(),
        w.mean_speed(),
        w.min_clearance_m,
        w.tilt_p95(),
        w.fly_under_m.len()
    );
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    assert!(
        w.mean_speed() >= WALK_MPS,
        "kept to {:.2} m/s behind a {WALK_MPS} m/s walker",
        w.mean_speed()
    );
    assert!(end < 3.0, "left {end:.1} m behind its goal");
    assert!(
        w.tilt_p95() <= CANOPY_TILT_P95_DEG,
        "leaned {:.1}° at the 95th percentile",
        w.tilt_p95()
    );
    // The canopy over the way is reported: crown bases 4.4-4.5 m over the
    // ground, the band's top 0.33 m over the scan plane (at the copter's
    // 4 m, its rangefinder reading 0.1 m more from the skids).
    assert!(!w.fly_under_m.is_empty(), "no overhead report");
    assert!(
        (3.9..=4.4).contains(&lowest),
        "lowest fly-under {lowest:.2} m"
    );
    s
}
