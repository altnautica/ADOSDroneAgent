//! Flight safety: the obstacle brake on sticks and on skill goals, its
//! reported state, hold capture and the launch's arming mode.

use std::f32::consts::TAU;

use ados_protocol::mavlink::ardupilotmega::MavType;
use ados_protocol::pilot::{AvoidState, Candidates, Goal, GoalKind, Owner, Request};
use serde_json::{json, Value};

use super::harness::World;
use crate::config::DEFAULT_BODY_RADIUS_M;
use crate::core::control::{
    brake_limit, stop_cap, BrakeGeom, Braked, BRAKE_LAG_S, CONFINED_SPEED_PER_M, CREEP_SLACK_M,
    HOLD_OBSTACLE_CLOSE, PUSH_MAX_MPS,
};
use crate::core::Reply;
use crate::fc::{MASK_POS_VEL_YAW, MASK_VELOCITY};
use crate::geo;
use crate::modes::{copter, plane};
use crate::obstacles::{level_hit, Level, Obstacles};

/// A 1.5 m margin on a vehicle of the default body radius.
pub(super) const G: BrakeGeom = BrakeGeom {
    margin: 1.5,
    body_m: DEFAULT_BODY_RADIUS_M,
};

/// Mark a square block of cells centred on `c` with half-width `half`.
fn block(o: &mut Obstacles, c: [f32; 2], half: [f32; 2]) {
    o.grid.reset([0.0, 0.0]);
    let mut n = c[0] - half[0];
    while n <= c[0] + half[0] {
        let mut e = c[1] - half[1];
        while e <= c[1] + half[1] {
            o.grid.mark([n, e]);
            e += 0.1;
        }
        n += 0.1;
    }
    o.grid.transform();
}

/// The brake's cap toward a point `d` away for a vehicle at rest (normal
/// profile, 1.5 m margin).
fn cap(d: f32) -> f32 {
    stop_cap(d, 0.0, 3.0, 1.5)
}

#[test]
fn stop_cap_stops_before_the_margin_after_the_lag() {
    for d in [2.0f32, 4.0, 8.0, 15.0] {
        let v = cap(d);
        let stop = v * BRAKE_LAG_S + v * v / (2.0 * 3.0);
        assert!(
            (stop - (d - 1.5 - CREEP_SLACK_M)).abs() < 1e-3,
            "d {d}: stops in {stop}"
        );
    }
    assert_eq!(cap(1.0), 0.0);
    assert_eq!(cap(1.5 + CREEP_SLACK_M), 0.0);
    // Still closing at 4 m/s, 5 m out: it covers 3.2 m before braking.
    let room = 5.0 - 1.5 - CREEP_SLACK_M - 3.2;
    assert!((stop_cap(5.0, 4.0, 3.0, 1.5) - (6.0f32 * room).sqrt()).abs() < 1e-4);
}

#[test]
fn brake_caps_speed_before_a_wall_and_keeps_sliding() {
    let mut o = Obstacles::new();
    block(&mut o, [3.1, 0.0], [0.05, 15.0]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [8.0, 0.0], 3.0, G, 0);
    assert!(b.cut && !b.close);
    assert!((b.v[0] - cap(3.0)).abs() < 0.35, "capped to {:?}", b.v);
    assert!(b.v[1].abs() < 0.05, "no sideways motion added: {:?}", b.v);
    // Sliding along it 3 m away is kept, at the clearance speed.
    let s = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, 3.0], 3.0, G, 0);
    let conf = CONFINED_SPEED_PER_M * (3.0 - DEFAULT_BODY_RADIUS_M);
    assert!((s.v[1] - conf).abs() < 0.05, "slide kept: {:?}", s.v);
    assert!(s.v[0].abs() < 0.05, "no motion toward it: {:?}", s.v);
}

#[test]
fn brake_sees_a_column_between_sample_rays() {
    // A 0.6 m column 7 m ahead and 1 m right of the track: no ray of a 15°
    // fan from the nose touches it, yet flying at it must be capped.
    let mut o = Obstacles::new();
    block(&mut o, [7.0, 1.0], [0.3, 0.3]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [8.0, 0.0], 3.0, G, 0);
    assert!(b.cut, "column ignored: {:?}", b.v);
    let rel = [7.0f32, 1.0];
    let dir = geo::scale(rel, 1.0 / geo::norm(rel));
    let near_face = geo::norm(rel) - 0.4;
    assert!(
        geo::dot(b.v, dir) <= cap(near_face) + 0.2,
        "speed toward the column {} over {}",
        geo::dot(b.v, dir),
        cap(near_face)
    );
}

#[test]
fn brake_measures_from_where_the_vehicle_coasts_to() {
    // 3.75 m from a wall, commanding 2 m/s, but the vehicle still closes at
    // 5 m/s: in the FC's response time it is inside the margin, so nothing
    // more toward the wall may be asked.
    let mut o = Obstacles::new();
    block(&mut o, [3.95, 0.0], [0.05, 4.0]);
    let b = brake_limit(&o, [0.0, 0.0], [5.0, 0.0], [2.0, 0.0], 3.0, G, 0);
    assert!(
        b.v[0] < 0.05 && b.v[1].abs() < 0.05,
        "still asking {:?}",
        b.v
    );
    // A slower command with the vehicle at rest stays.
    let r = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [1.5, 0.0], 3.0, G, 0);
    assert!((r.v[0] - 1.5).abs() < 1e-3, "{:?}", r.v);
}

#[test]
fn inside_the_hard_stop_only_motion_away_is_kept() {
    // A wall 0.75 m to the east, inside 0.6 × 1.5 m.
    let mut o = Obstacles::new();
    block(&mut o, [0.0, 0.85], [3.0, 0.05]);
    let slide = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [2.0, 0.0], 3.0, G, 0);
    assert!(slide.close);
    assert!(slide.v[0].abs() < 0.05, "slid along it: {:?}", slide.v);
    assert!(
        slide.v[1] < -0.3,
        "not moved back out of the margin: {:?}",
        slide.v
    );
    let away = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, -1.0], 3.0, G, 0);
    assert!(away.v[1] <= -0.95, "backing off kept: {:?}", away.v);
}

/// Mark several blocks (`(centre, half-width)`) in one grid.
fn blocks(o: &mut Obstacles, list: &[([f32; 2], [f32; 2])]) {
    o.grid.reset([0.0, 0.0]);
    for &(c, half) in list {
        let mut n = c[0] - half[0];
        while n <= c[0] + half[0] {
            let mut e = c[1] - half[1];
            while e <= c[1] + half[1] {
                o.grid.mark([n, e]);
                e += 0.1;
            }
            n += 0.1;
        }
    }
    o.grid.transform();
}

#[test]
fn a_confined_passage_holds_speed_to_the_clearance() {
    // A 4.5 m mine drift running north, the vehicle on its centreline asking
    // 8 m/s along it: nothing ahead, yet next to walls it flies only the
    // clearance speed, straight down the middle.
    let mut o = Obstacles::new();
    blocks(
        &mut o,
        &[([0.0, 2.35], [15.0, 0.05]), ([0.0, -2.35], [15.0, 0.05])],
    );
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [8.0, 0.0], 3.0, G, 0);
    // Cells at ±2.375 m: 2.25 m to their near faces.
    let conf = CONFINED_SPEED_PER_M * (2.25 - DEFAULT_BODY_RADIUS_M);
    assert!(b.cut, "not limited: {:?}", b.v);
    assert!(
        b.v[0] <= conf + 0.05 && b.v[0] >= conf - 0.15,
        "flies {:?} in the drift, clearance speed {conf}",
        b.v
    );
    assert!(b.v[1].abs() < 0.05, "sideways motion added: {:?}", b.v);
    // In the open the clearance speed leaves the ask alone.
    let mut open = Obstacles::new();
    block(&mut open, [0.0, 14.0], [15.0, 0.05]);
    let f = brake_limit(&open, [0.0, 0.0], [0.0, 0.0], [5.0, 0.0], 3.0, G, 0);
    assert!((f.v[0] - 5.0).abs() < 1e-3 && !f.cut, "{:?}", f.v);
}

#[test]
fn the_brake_keeps_the_vehicles_own_radius_clear() {
    // The same drift for a 0.20 m caged vehicle: the clearance speed counts
    // from its own outer extent, not the 0.4 m default.
    let mut o = Obstacles::new();
    blocks(
        &mut o,
        &[([0.0, 2.35], [15.0, 0.05]), ([0.0, -2.35], [15.0, 0.05])],
    );
    let small = BrakeGeom {
        margin: 1.5,
        body_m: 0.2,
    };
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [8.0, 0.0], 3.0, small, 0);
    let conf = CONFINED_SPEED_PER_M * (2.25 - 0.2);
    assert!(
        b.v[0] <= conf + 0.05 && b.v[0] >= conf - 0.15,
        "flies {:?}, clearance speed {conf}",
        b.v
    );
    // The pilot brakes with the configured radius; unset, the default.
    let mut w = World::new();
    assert_eq!(w.p.brake_geom(1.5).body_m, DEFAULT_BODY_RADIUS_M);
    w.p.settings.pilot.prop_tip_radius_m = Some(0.2);
    assert_eq!(w.p.brake_geom(1.5), small);
}

#[test]
fn drift_inside_the_margin_is_pushed_back_out() {
    // Hovering (no ask) 1.0 m from a wall to the east, inside the 1.5 m
    // margin but outside the hard stop: the brake moves it back west.
    let mut o = Obstacles::new();
    block(&mut o, [0.0, 1.1], [5.0, 0.05]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, 0.0], 3.0, G, 0);
    assert!(b.cut && !b.close, "{b:?}");
    assert!(
        b.v[1] < -0.3 && b.v[1] >= -PUSH_MAX_MPS - 1e-3,
        "push {:?}",
        b.v
    );
    assert!(b.v[0].abs() < 0.05, "push along the wall: {:?}", b.v);
    // Flying along the wall keeps going, at no more than the clearance speed
    // 1 m from a wall (~0.5 m/s), and drifts back out as it goes, no faster
    // than the push or the slide alone (not their sum).
    let a = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [1.0, 0.0], 3.0, G, 0);
    assert!(a.v[0] > 0.3 && a.v[0] < 0.6, "{:?}", a.v);
    assert!(a.v[1] < -0.3 && a.v[1] >= -0.6, "{:?}", a.v);
    // Beyond the margin nothing is added.
    let mut far = Obstacles::new();
    block(&mut far, [0.0, 2.1], [5.0, 0.05]);
    let c = brake_limit(&far, [0.0, 0.0], [0.0, 0.0], [0.0, 0.0], 3.0, G, 0);
    assert!(geo::norm(c.v) < 1e-3 && !c.cut, "{c:?}");
}

#[test]
fn a_passage_narrower_than_twice_the_margin_centres_the_vehicle() {
    // A 2.75 m wide passage: centred, the walls' pushes cancel.
    let mut o = Obstacles::new();
    blocks(
        &mut o,
        &[([0.0, 1.4], [5.0, 0.05]), ([0.0, -1.4], [5.0, 0.05])],
    );
    let c = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, 0.0], 3.0, G, 0);
    assert!(geo::norm(c.v) < 0.05, "centred, yet pushed {:?}", c.v);
    // 0.5 m off the centre toward the east wall: pushed back west.
    let off = brake_limit(&o, [0.0, 0.5], [0.0, 0.0], [0.0, 0.0], 3.0, G, 0);
    assert!(off.v[1] < -0.2, "not re-centred: {:?}", off.v);
    // A 2 m passage, 0.3 m off the centre: both walls inside the margin, the
    // push away from the near one still flies toward the farther one.
    blocks(
        &mut o,
        &[([0.0, 1.0], [5.0, 0.05]), ([0.0, -1.0], [5.0, 0.05])],
    );
    let both = brake_limit(&o, [0.0, 0.3], [0.0, 0.0], [0.0, 0.0], 3.0, G, 0);
    assert!(
        both.v[1] < -0.1,
        "not re-centred between both walls: {:?}",
        both.v
    );
}

#[test]
fn a_lagging_velocity_estimate_does_not_make_the_brake_late() {
    // A column approach with delayed external navigation: a quarter stick asks
    // 1.6 m/s and the EKF, behind its delayed external navigation, reports
    // 1.6 m/s, while the vehicle really closes at 2.8 m/s. The scans show
    // the real closing speed (the column 0.28 m nearer every 0.1 s), and the
    // brake must use it: 3.6 m out it can no longer stop outside the margin.
    let level = Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(2.5),
    };
    let mut o = Obstacles::new();
    for (k, d) in [4.44f32, 4.16, 3.88, 3.60].iter().enumerate() {
        o.ingest_scan(&wall_scan(*d), level, [0.0, 0.0], 1000 + 100 * k as u64);
    }
    o.rebuild([0.0, 0.0], 1300);
    let b = brake_limit(&o, [0.0, 0.0], [1.6, 0.0], [1.6, 0.0], 3.0, G, 1300);
    assert!(
        b.cut && b.v[0] < 1.0,
        "still asks {:?} closing at 2.8 m/s",
        b.v
    );
    // The same wall seen standing still (no range rate): the EKF's 1.6 m/s
    // is the closing speed and the ask stands.
    let mut s = Obstacles::new();
    for k in 0..4u64 {
        s.ingest_scan(&wall_scan(3.6), level, [0.0, 0.0], 1000 + 100 * k);
    }
    s.rebuild([0.0, 0.0], 1300);
    let c = brake_limit(&s, [0.0, 0.0], [1.6, 0.0], [1.6, 0.0], 3.0, G, 1300);
    assert!(c.v[0] > 1.5, "a standing wall braked {:?}", c.v);
}

#[test]
fn a_new_nearest_surface_is_not_a_closing_speed() {
    // A post enters a sector in front of a far wall: the sector's nearest
    // range falls 6 m between two scans. That is a new surface, not 60 m/s.
    let level = Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(2.5),
    };
    let mut o = Obstacles::new();
    o.ingest_scan(&wall_scan(8.0), level, [0.0, 0.0], 1000);
    o.ingest_scan(&wall_scan(2.0), level, [0.0, 0.0], 1100);
    assert!(
        o.max_scan_closing(1100) < 1.0,
        "closing {}",
        o.max_scan_closing(1100)
    );
    // A post 1 m in front of a wall, the vehicle hovering: the 1 m step is
    // within what a scan pair accepts, yet a standing vehicle closes on
    // nothing that fast, so an ask toward the post 3 m out stands.
    let mut p = Obstacles::new();
    p.ingest_scan(&wall_scan(4.0), level, [0.0, 0.0], 1000);
    p.ingest_scan(&wall_scan(3.0), level, [0.0, 0.0], 1100);
    p.rebuild([0.0, 0.0], 1100);
    let b = brake_limit(&p, [0.0, 0.0], [0.0, 0.0], [1.0, 0.0], 3.0, G, 1100);
    assert!(b.v[0] > 0.9, "a hover braked by a new surface: {:?}", b.v);
}

/// Body bearings 30-50° and 310-330° (two crowns at ±40° off the nose), each
/// beam's range from `crown(beam)`; nothing elsewhere.
fn crowns_scan(crown: impl Fn(usize) -> f32) -> Value {
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            if (30..=50).contains(&i) || (310..=330).contains(&i) {
                crown(i)
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

/// What the brake leaves of 6 m/s straight ahead for a vehicle hovering
/// through 20 scans at 10 Hz, scan `k`'s beam `i` at `crown(k, i)`.
fn hover_ask_after(crown: impl Fn(u64, usize) -> f32) -> Braked {
    let level = Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(4.0),
    };
    let mut o = Obstacles::new();
    for k in 0..20u64 {
        o.ingest_scan(
            &crowns_scan(|i| crown(k, i)),
            level,
            [0.0, 0.0],
            1000 + 100 * k,
        );
    }
    brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [6.0, 0.0], 3.0, G, 2900)
}

#[test]
fn jittering_leaf_returns_are_not_a_closing_obstacle() {
    // Leaf returns inside a crown (a beam grazing a trunk's edge, rain, dust)
    // land at a random depth every scan: a hovering vehicle closes on none of
    // them. Each scan draws every crown beam somewhere in 0.6 m beyond the
    // nearest range (2.0 m plus the brake's creep slack); the goal ahead must
    // be braked no harder than for the same crowns seen steadily at their
    // nearest range.
    let near = 2.0 + CREEP_SLACK_M;
    let jitter = |k: u64, i: usize| {
        let h = (k.wrapping_mul(2_654_435_761) ^ (i as u64).wrapping_mul(40_503)) % 1000;
        near + 0.6 * h as f32 / 1000.0
    };
    let steady = hover_ask_after(|_, _| near).v[0];
    let leafy = hover_ask_after(jitter).v[0];
    assert!(
        steady > 0.3,
        "steady crowns at 2 m leave the open nose flyable"
    );
    assert!(
        leafy >= 0.9 * steady,
        "jittering returns braked the goal to {leafy:.2} m/s, steady ones at their nearest range to {steady:.2}"
    );
}

/// The first two numbers of a JSON array.
fn v2_of(v: &Value) -> [f32; 2] {
    let v = f32s(v);
    [v[0], v[1]]
}

/// Ingest one recorded frame's scan into `o` as the pilot does (levelled with
/// the frame's attitude and AGL, at its position) and rebuild the grid there.
/// Returns the position, the ask and the time.
fn ingest_frame(o: &mut Obstacles, f: &Value) -> ([f32; 2], [f32; 2], u64) {
    let num = |k: &str| f[k].as_f64().unwrap() as f32;
    let pos = v2_of(&f["pos_ne"]);
    let now = 1000 + f["t_ms"].as_u64().unwrap();
    let level = Level {
        roll: num("roll"),
        pitch: num("pitch"),
        yaw: num("yaw"),
        agl: Some(num("agl_m")),
    };
    let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                      "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
    o.ingest_scan(&scan, level, pos, now);
    o.rebuild(pos, now);
    (pos, v2_of(&f["ask_ne"]), now)
}

/// A recorded follow past tree crowns whose leaf returns jitter scan to
/// scan, replayed scan by scan. The goal's pull is never stopped while
/// nothing on the way to it (±30°) lies within the margin plus what the
/// vehicle covers at its speed in the FC's response time, plus 0.5 m: no
/// return there is near enough to stop for, so a stop could only come from
/// a closing speed the vehicle does not have.
#[test]
fn replayed_follow_past_leafy_crowns_is_not_stopped_by_their_jitter() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/foliage_follow.json")).unwrap();
    let mut o = Obstacles::new();
    let mut checked = 0;
    for f in fx["frames"].as_array().unwrap() {
        let (pos, ask, now) = ingest_frame(&mut o, f);
        let vel = v2_of(&f["vel_ne"]);
        let dir = geo::scale(ask, 1.0 / geo::norm(ask));
        let ahead = o
            .hits(now)
            .iter()
            .map(|h| geo::sub(h.ne, pos))
            .filter(|rel| geo::dot(*rel, dir) >= geo::norm(*rel) * 30f32.to_radians().cos())
            .map(geo::norm)
            .fold(f32::INFINITY, f32::min);
        if ahead <= 1.5 + BRAKE_LAG_S * geo::norm(vel) + 0.5 {
            continue;
        }
        checked += 1;
        let b = brake_limit(&o, pos, vel, ask, 3.0, G, now);
        let toward = geo::dot(b.v, dir);
        assert!(
            toward >= 0.1,
            "{} ms: the goal braked to {toward:.2} m/s, nothing within {ahead:.1} m of its way",
            f["t_ms"]
        );
    }
    assert!(checked >= 20, "only {checked} frames checked");
}

/// Scans of a wall approached at `closing(k)` m/s (scan `k` of 10 Hz, from
/// `d0` metres), each beam with up to ±2 cm of range noise (the planar lidar
/// is good to ±30 mm 3σ). Returns the obstacles after the last scan, at
/// `1000 + 100 · (scans − 1)` ms, and the wall's distance then.
fn noisy_wall_approach(d0: f32, scans: u64, closing: impl Fn(u64) -> f32) -> (Obstacles, f32) {
    let level = Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(2.5),
    };
    let mut o = Obstacles::new();
    let mut d = d0;
    for k in 0..scans {
        if k > 0 {
            d -= 0.1 * closing(k);
        }
        let mut scan = wall_scan(d);
        for (i, r) in scan["ranges_m"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            let x = r.as_f64().unwrap();
            if x > 0.0 {
                let h = (k.wrapping_mul(2_654_435_761) ^ (i as u64).wrapping_mul(40_503)) % 1000;
                *r = json!(x + 0.04 * (h as f64 / 1000.0 - 0.5));
            }
        }
        o.ingest_scan(&scan, level, [0.0, 0.0], 1000 + 100 * k);
    }
    o.rebuild([0.0, 0.0], 1000 + 100 * (scans - 1));
    (o, d)
}

#[test]
fn a_wall_approached_through_range_noise_still_brakes_a_lagging_estimate() {
    // 2.2 m/s at a wall, the EKF behind delayed external navigation reporting
    // 0.7 m/s: the scans' closing speed is the real one, and 3 m out the brake
    // caps the ask as it would knowing the real speed (here: nothing more).
    let (o, d) = noisy_wall_approach(4.5, 8, |_| 2.2);
    let now = 1700;
    let closing = o.scan_closing([1.0, 0.0], now);
    assert!(
        (closing - 2.2).abs() < 0.3,
        "measured {closing:.2} m/s closing on a wall approached at 2.2 m/s"
    );
    let lagging = brake_limit(&o, [0.0, 0.0], [0.7, 0.0], [2.2, 0.0], 3.0, G, now);
    let known = stop_cap(d - crate::obstacles::CELL_M / 2.0, 2.2, 3.0, 1.5);
    assert!(
        lagging.cut && lagging.v[0] <= known + 0.1,
        "{d:.2} m out still asks {:?}, the real closing speed allows {known:.2}",
        lagging.v
    );
    // Braking at 3 m/s² bends the ranges, and the closing speed holds: from
    // 2.2 m/s down 0.3 m/s per scan to 1.0 m/s at the last.
    let slowing = |k: u64| 2.2 - 0.3 * k.saturating_sub(3) as f32;
    let (o, _) = noisy_wall_approach(4.5, 8, slowing);
    let closing = o.scan_closing([1.0, 0.0], now);
    assert!(
        closing > 1.0,
        "a braking approach (1.0-1.9 m/s over the last scans) measured {closing:.2} m/s"
    );
}

/// The recorded full-stick approach of a column, every scan replayed into one
/// obstacle model as it arrived, with a velocity estimate trailing the real
/// motion (50 % and 30 % of the recorded one: delayed external navigation).
/// The brake must allow no more toward the column than it does knowing the
/// simulator's true velocity (within 0.05 m/s), the scans' closing speed
/// standing in for the estimate, and it must still stop the stick before the
/// column.
#[test]
fn replayed_column_approach_brakes_a_lagging_estimate_as_the_true_speed() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/column_full_stick.json")).unwrap();
    for scale in [0.5, 0.3] {
        let mut o = Obstacles::new();
        let mut stopped = false;
        for f in fx["frames"].as_array().unwrap() {
            let (pos, ask, now) = ingest_frame(&mut o, f);
            let lagging = geo::scale(v2_of(&f["vel_ne"]), scale);
            let dir = geo::scale(ask, 1.0 / geo::norm(ask));
            let allowed = geo::dot(brake_limit(&o, pos, lagging, ask, 3.0, G, now).v, dir);
            let known = brake_limit(&o, pos, v2_of(&f["truth_vel_ne"]), ask, 3.0, G, now);
            let known = geo::dot(known.v, dir);
            assert!(
                allowed <= known + 0.05,
                "x{scale}, {} ms: allows {allowed:.2} m/s toward the column, {known:.2} knowing the true speed",
                f["t_ms"]
            );
            stopped |= allowed < 0.1;
        }
        assert!(
            stopped,
            "x{scale}: the stick was never stopped short of the column"
        );
    }
}

#[test]
fn the_push_never_closes_on_a_nearer_post() {
    // A wall 1.3 m east fills many sectors, a post 1.05 m west one or two:
    // the summed push points west, at the post. It must not.
    let mut o = Obstacles::new();
    blocks(
        &mut o,
        &[([0.0, 1.3], [5.0, 0.05]), ([0.0, -1.05], [0.05, 0.05])],
    );
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, 0.0], 3.0, G, 0);
    assert!(b.v[1] >= -1e-3, "pushed toward the post: {:?}", b.v);
}

#[test]
fn a_centred_stick_inside_the_margin_is_pushed_out_not_held() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    // Sticks centred 1.2 m from a wall ahead, long past the hold capture.
    stick_with_scan(&mut w, [0.0; 4], &wall_scan(1.2), 3000);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_VELOCITY, "held inside the margin");
    assert!(sp.vx < -0.1, "not pushed back out: {}", sp.vx);
}

#[test]
fn a_goto_arrival_hold_inside_the_margin_moves_back_out() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    // Arrives at once (within 1 m) into a hold, a wall 1.55 m ahead: inside
    // the margin (by its grid cells) by less than a hold capture's worth of
    // push, so the vehicle stays in a position hold.
    w.req(Request::Goto {
        n: 0.5,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    let scan = wall_scan(1.55);
    let feed = |w: &mut World, ms: u64| {
        let end = w.now + ms;
        while w.now < end {
            w.p.on_feed("plugin.world-engine.scan", &scan, w.now);
            w.run(20);
        }
    };
    feed(&mut w, 1000);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let a = w.last_setpoint().unwrap().x;
    feed(&mut w, 2000);
    let b = w.last_setpoint().unwrap();
    assert_eq!(b.type_mask.bits(), MASK_POS_VEL_YAW, "not a hold");
    assert!(b.x < a - 0.05, "push dropped by the hold: {a} -> {}", b.x);
}

/// The recorded approach of a stick-flight crash: full forward at a column
/// 14.8 m ahead in SITL. Pitched nose-down while accelerating, the planar
/// lidar met the floor 7-9 m ahead, so the column (beyond that) was in no
/// return until 6 m out, when the vehicle already did 7 m/s. Replay each
/// recorded scan with the attitude, rangefinder and EKF state of its moment:
/// the speed the brake allows toward the column must let the vehicle stop
/// short of it (FC lag, 3 m/s², 1 m clearance plus a 0.35 m body), using the
/// simulator's true distance.
#[test]
fn replayed_column_approach_never_allows_more_than_it_can_stop_from() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/wall_approach.json")).unwrap();
    let f32s = |v: &Value| -> Vec<f32> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect()
    };
    for f in fx["frames"].as_array().unwrap() {
        let num = |k: &str| f[k].as_f64().unwrap() as f32;
        let (pos, vel) = (f32s(&f["pos_ne"]), f32s(&f["vel_ne"]));
        let (pos, vel) = ([pos[0], pos[1]], [vel[0], vel[1]]);
        let level = crate::obstacles::Level {
            roll: num("roll"),
            pitch: num("pitch"),
            yaw: num("yaw"),
            agl: Some(num("rangefinder_m")),
        };
        let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                          "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
        let mut o = Obstacles::new();
        o.ingest_scan(&scan, level, pos, 1000);
        o.rebuild(pos, 1000);
        let b = brake_limit(&o, pos, vel, [8.0, 0.0], 3.0, G, 1000);
        let truth = num("truth_to_column_m");
        let allowed = b.v[0];
        let safe = stop_cap(truth, 0.0, 3.0, 1.0 + 0.35);
        assert!(
            allowed <= safe + 0.05,
            "t {}: allows {allowed:.2} m/s toward a column {truth} m away (can stop from {safe:.2})",
            num("t_s")
        );
    }
    // Hovering 14.8 m out, a gentle push is not refused.
    let f = &fx["frames"][0];
    let level = crate::obstacles::Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(f["rangefinder_m"].as_f64().unwrap() as f32),
    };
    let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                      "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
    let mut o = Obstacles::new();
    o.ingest_scan(&scan, level, [0.0, 0.0], 1000);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [2.0, 0.0], 3.0, G, 1000);
    assert!((b.v[0] - 2.0).abs() < 1e-3, "{:?}", b.v);
}

/// A planar scan of a wall `d` ahead (vehicle yaw 0), as world-engine sends it.
pub(super) fn wall_scan(d: f32) -> Value {
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            let th = if i > 180 { i as f32 - 360.0 } else { i as f32 }.to_radians();
            if th.abs() < 1.2 {
                d / th.cos()
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

/// A planar scan of a trunk about 0.5 m wide `d` ahead (vehicle yaw 0).
pub(super) fn trunk_scan(d: f32) -> Value {
    let half = (0.25 / d).atan();
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            let th = if i > 180 { i as f32 - 360.0 } else { i as f32 }.to_radians();
            if th.abs() <= half {
                d
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

fn stick_with_scan(w: &mut World, axes: [f32; 4], scan: &Value, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.input(axes);
        w.p.on_feed("plugin.world-engine.scan", scan, w.now);
        w.run(20);
    }
}

fn scan_for(w: &mut World, scan: &Value, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.p.on_feed("plugin.world-engine.scan", scan, w.now);
        w.run(20);
    }
}

/// Walls 1.0 m east and 1.6 m west: a 2.6 m passage, narrower than twice the
/// 1.5 m margin, so the planner finds no route along it.
fn passage_scan() -> Value {
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            let s = (i as f32).to_radians().sin();
            let r = if s > 1e-3 {
                1.0 / s
            } else if s < -1e-3 {
                1.6 / -s
            } else {
                0.0
            };
            if r <= 12.0 {
                r
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

#[test]
fn a_goto_held_by_the_hard_stop_is_pushed_out() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 0.0,
        e: 10.0,
        d: -5.0,
        yaw: None,
    });
    scan_for(&mut w, &wall_scan(0.8), 2000);
    assert_eq!(w.p.owner(), Owner::Goto);
    let sp = w.last_setpoint().unwrap().clone();
    assert!(
        sp.vx < -0.1 || sp.x < -0.05,
        "held 0.8 m from the wall without the push: x {} vx {}",
        sp.x,
        sp.vx
    );
}

#[test]
fn a_goto_in_a_passage_narrower_than_twice_the_margin_is_centred() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 10.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    scan_for(&mut w, &passage_scan(), 2000);
    assert_eq!(w.p.owner(), Owner::Goto);
    let sp = w.last_setpoint().unwrap().clone();
    assert!(
        sp.vy < -0.1 || sp.y < -0.05,
        "not centred: y {} vy {}",
        sp.y,
        sp.vy
    );
}

#[test]
fn a_pushing_goto_hold_keeps_its_altitude() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 0.0,
        e: 10.0,
        d: -5.0,
        yaw: None,
    });
    let scan = wall_scan(0.8);
    for _ in 0..150 {
        w.p.on_feed("plugin.world-engine.scan", &scan, w.now);
        w.run(20);
        // Sinking 0.1 m/s while the push moves it off the wall.
        w.pos[2] += 0.1 * 0.02;
    }
    assert_eq!(w.p.owner(), Owner::Goto);
    let sp = w.last_setpoint().cloned().unwrap();
    assert!(sp.vx < -0.1, "the push flies: vx {}", sp.vx);
    assert!(
        (sp.z + 5.0).abs() < 0.05,
        "hold altitude followed the vehicle: {}",
        sp.z
    );
}

#[test]
fn stick_brake_reports_avoid_state_and_hard_stop() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    let fwd = [0.0, 1.0, 0.0, 0.0];
    stick_with_scan(&mut w, fwd, &wall_scan(3.0), 1500);
    assert_eq!(w.p.state(w.now).avoid_state, AvoidState::Detour);
    assert!(w.last_setpoint().unwrap().vx < cap(2.9) + 0.05);
    stick_with_scan(&mut w, fwd, &wall_scan(1.2), 400);
    assert_eq!(w.p.state(w.now).avoid_state, AvoidState::Blocked);
    // Inside the margin the brake stops the ask and pushes back out.
    let vx = w.last_setpoint().unwrap().vx;
    assert!((-PUSH_MAX_MPS - 1e-3..0.05).contains(&vx), "vx {vx}");
    // It stays blocked while the stick is held into the wall.
    for k in 0..10 {
        stick_with_scan(&mut w, fwd, &wall_scan(1.2), 60);
        let s = w.p.state(w.now).avoid_state;
        assert_eq!(s, AvoidState::Blocked, "{} ms later", 60 * (k + 1));
    }
    stick_with_scan(&mut w, fwd, &wall_scan(0.8), 200);
    let s = w.p.state(w.now);
    assert_eq!(s.avoid_state, AvoidState::Blocked);
    assert_eq!(s.hold_reason.as_deref(), Some(HOLD_OBSTACLE_CLOSE));
    stick_with_scan(&mut w, [0.0, -1.0, 0.0, 0.0], &wall_scan(0.8), 600);
    assert!(
        w.last_setpoint().unwrap().vx < -0.1,
        "backing away is allowed"
    );
}

/// A wall across a goto holds the vehicle, and the frame still carries the
/// commanded goal and the straight way to it, so every surface draws them in
/// the blocked colour instead of dropping them.
#[test]
fn a_blocked_goto_keeps_its_goal_and_way_on_the_frame() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    let end = w.now + 1500;
    while w.now < end {
        w.p.on_feed("plugin.world-engine.scan", &wall_scan(4.0), w.now);
        w.run(20);
    }
    let s = w.p.state(w.now);
    assert_eq!(s.avoid_state, AvoidState::Blocked);
    assert_eq!(s.hold_reason.as_deref(), Some("blocked"));
    let g = s.goal_ned.expect("the commanded goal stays on the frame");
    assert!(
        (g[0] - 20.0).abs() < 0.01 && g[1].abs() < 0.01 && (g[2] + 5.0).abs() < 0.01,
        "{g:?}"
    );
    assert_eq!(s.plan_ned.len(), 2, "{:?}", s.plan_ned);
    assert!(s.plan_ned[0][0].abs() < 1.0, "{:?}", s.plan_ned);
    assert_eq!(s.plan_ned[1], [g[0], g[1]]);
}

#[test]
fn centred_sticks_hold_only_once_the_vehicle_slows() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 1000);
    // Sticks centred while the vehicle still coasts at 3 m/s.
    w.vel = [3.0, 0.0, 0.0];
    w.stick([0.0; 4], 2500);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(
        sp.type_mask.bits(),
        MASK_VELOCITY,
        "hold point taken while coasting"
    );
    assert!(sp.vx.abs() < 1e-3 && sp.vy.abs() < 1e-3, "commands a stop");
    w.vel = [0.1, 0.0, 0.0];
    w.pos[0] = 6.0;
    w.stick([0.0; 4], 100);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_POS_VEL_YAW);
    assert_eq!(sp.x, 6.0, "the hold is where the vehicle stopped");
}

#[test]
fn centred_sticks_hold_after_settling_even_in_a_drift() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 500);
    w.vel = [0.5, 0.0, 0.0];
    w.stick([0.0; 4], 3500);
    assert_eq!(
        w.last_setpoint().unwrap().type_mask.bits(),
        MASK_POS_VEL_YAW
    );
}

/// A follow skill's position goal with its feed-forward.
fn follow_goal(ned: [f32; 3], vel_ne: [f32; 2]) -> Goal {
    let mut g = Goal::new(GoalKind::Position, ned, 6.0);
    g.vn = vel_ne[0];
    g.ve = vel_ne[1];
    g
}

/// What the FC asks of its velocity loop for a setpoint, at `pos`: the
/// feed-forward, plus its position loop's pull (gain 1, `PSC_POSXY_P`) when
/// the setpoint carries a position.
fn fc_ask(w: &World, pos: [f32; 2]) -> [f32; 2] {
    let sp = w.last_setpoint().unwrap();
    let ff = [sp.vx, sp.vy];
    if sp.type_mask.bits() == MASK_POS_VEL_YAW {
        geo::add(ff, geo::sub([sp.x, sp.y], pos))
    } else {
        ff
    }
}

fn f32s(v: &Value) -> Vec<f32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
        .collect()
}

/// The recorded approach of a follow skill on a tree: its goals jumped
/// metres per batch, the planner judged goal and segment `clear`, and the
/// vehicle flew at 3.4-4.4 m/s from 3.2 m down to the hard stop, which came
/// too late to keep the margin. Replay each scan with the attitude,
/// rangefinder, EKF state and skill goal of its moment: what the FC is asked
/// to fly toward every obstacle the scan shows must let it stop 1 m short of
/// it after its response time (3 m/s²).
#[test]
fn replayed_follow_approach_never_asks_more_than_it_can_stop_from() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/goal_approach.json")).unwrap();
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    for f in fx["frames"].as_array().unwrap() {
        let num = |k: &str| f[k].as_f64().unwrap() as f32;
        let (pos, vel, goal) = (
            f32s(&f["pos_ned"]),
            f32s(&f["vel_ned"]),
            f32s(&f["goal_ned"]),
        );
        let gv = f32s(&f["goal_vel_ne"]);
        w.pos = [pos[0], pos[1], pos[2]];
        w.vel = [vel[0], vel[1], vel[2]];
        (w.roll, w.pitch, w.yaw) = (num("roll"), num("pitch"), num("yaw"));
        w.agl = Some(num("rangefinder_m"));
        w.feed();
        let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                          "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
        w.p.on_feed("plugin.world-engine.scan", &scan, w.now);
        w.req(Request::Goal {
            lease_id: id,
            goal: follow_goal([goal[0], goal[1], goal[2]], [gv[0], gv[1]]),
        });
        w.run(100);
        let here = [pos[0], pos[1]];
        let ask = fc_ask(&w, here);
        let level = Level {
            roll: w.roll,
            pitch: w.pitch,
            yaw: w.yaw,
            agl: w.agl,
        };
        let ranges = f32s(&f["ranges_m"]);
        for (i, r) in ranges.iter().enumerate().filter(|(_, r)| **r > 0.0) {
            let Some(hit) = level_hit(*r, i as f32 * TAU / 360.0, level, here) else {
                continue;
            };
            let rel = geo::sub(hit.ne, here);
            let d = geo::norm(rel);
            let toward = geo::dot(ask, geo::scale(rel, 1.0 / d));
            let safe = stop_cap(d, 0.0, 3.0, 1.0);
            assert!(
                toward <= safe + 0.1,
                "t {}: asks {toward:.2} m/s toward an obstacle {d:.2} m away (can stop from {safe:.2})",
                num("t_s")
            );
        }
    }
}

#[test]
fn goal_brake_keeps_open_goals_and_caps_them_to_seen_space() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    // Nothing seen yet: a near goal is flown as planned.
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([3.0, 0.0, -5.0], [0.0, 0.0]),
    });
    w.run(300);
    let sp = w.last_setpoint().unwrap();
    assert_eq!((sp.x, sp.y), (3.0, 0.0), "planned setpoint kept");
    assert_eq!(w.p.state(w.now).avoid_state, AvoidState::Clear);
    // A scan with no return: free space to the lidar's range only. A goal
    // 30 m out asks more than the vehicle can stop from inside it.
    let empty = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                       "angle_increment_rad": TAU / 360.0, "ranges_m": vec![0.0f32; 360]});
    w.p.on_feed("plugin.world-engine.scan", &empty, w.now);
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([30.0, 0.0, -5.0], [0.0, 0.0]),
    });
    w.run(300);
    let ask = fc_ask(&w, [0.0, 0.0]);
    let seen = stop_cap(crate::obstacles::SCAN_HORIZON_M, 0.0, 3.0, 1.5);
    assert!(
        ask[0] > 1.0 && ask[0] <= seen + 0.05,
        "flies {ask:?}, seen space allows {seen:.2}"
    );
    assert_eq!(w.p.state(w.now).avoid_state, AvoidState::Detour);
}

/// A row of trunks `d` ahead, `half` metres either side of the nose (vehicle
/// yaw 0).
fn row_scan(d: f32, half: f32) -> Value {
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            let th = if i > 180 { i as f32 - 360.0 } else { i as f32 }.to_radians();
            if th.abs() < 1.5 && (d * th.tan()).abs() <= half {
                d / th.cos()
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

#[test]
fn a_follow_far_behind_routes_around_clutter_toward_its_subject() {
    // The subject 26 m ahead and the follow goal 6 m short of it, both beyond
    // the planner's 32 m grid, its detour ring with them; a row of trunks
    // 5 m ahead blocks the straight way. The vehicle detours toward the goal
    // instead of holding blocked, so a follow that fell behind can close up.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.pos = [0.0, 0.0, -5.0];
    w.agl = Some(5.0);
    let id = w.lease();
    let mut goal = follow_goal([20.0, 0.0, -5.0], [0.0, 0.0]);
    goal.subject_ne = Some([26.0, 0.0]);
    goal.candidates = Some(Candidates {
        ring_center_ne: [26.0, 0.0],
        radius_m: 6.0,
        angle_deg: 180.0,
        angle_span_deg: 90.0,
    });
    // The skill re-sends its goal well inside the lease's goal timeout.
    for _ in 0..8 {
        w.req(Request::Goal {
            lease_id: id,
            goal: goal.clone(),
        });
        scan_for(&mut w, &row_scan(5.0, 3.0), 200);
    }
    let s = w.p.state(w.now);
    assert_eq!(s.owner, Owner::Skill);
    assert_eq!(s.hold_reason, None);
    assert_eq!(s.avoid_state, AvoidState::Detour);
    let ask = fc_ask(&w, [0.0, 0.0]);
    assert!(geo::norm(ask) > 0.5, "asks {ask:?} with a goal 20 m ahead");
    let end = s.plan_ned.last().copied().unwrap_or_default();
    assert!(end[0] > 16.0, "the plan ends at {end:?}, short of the goal");
}

#[test]
fn plane_launch_selects_takeoff_mode_before_arming() {
    let mut w = World::new();
    w.mavtype = MavType::MAV_TYPE_FIXED_WING;
    for (n, v) in [
        ("FS_GCS_ENABL", 1.0),
        ("FS_LONG_TIMEOUT", 3.0),
        ("FS_LONG_ACTN", 1.0),
        ("BATT_FS_LOW_ACT", 1.0),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
    // ArduPlane boots in RTL and refuses to arm there.
    w.mode = plane::RTL;
    w.unarmable = vec![plane::RTL];
    w.run(100);
    let r = w.action_args("launch", json!({"after": "hold"}));
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
    w.run(2000);
    assert!(w.armed, "armed in a mode the FC accepts");
    assert_eq!(w.mode, plane::TAKEOFF);
    let seq = w.p.state(w.now).sequence.unwrap();
    assert_eq!(seq.step, "takeoff");
    assert!(seq.error.is_none());
}

#[test]
fn copter_launch_from_an_unarmable_mode_arms_in_guided() {
    let mut w = World::new();
    w.mode = copter::RTL;
    w.unarmable = vec![copter::RTL];
    w.run(100);
    w.action("takeoff");
    w.run(1000);
    assert!(w.armed);
    assert_eq!(w.mode, copter::GUIDED);
}
