//! The route check's uncertainty is relative to each obstacle: the leg and
//! the map share the frame the pose places the scans in, so a bore mapped
//! after σ grew needs only the matched σ, while one mapped before keeps the
//! growth since and a pose uncertain from the start keeps its whole σ.

use ados_protocol::pilot::RouteState;
use serde_json::{json, Value};

use super::confined::{area, confined, on_pad};
use super::confined_route::{route, LINE};
use super::harness::World;

/// Walls 0.75–0.85 m either side of the route north along e = 0: the
/// nearest cell centre 0.775 m off the legs, 0.740 m less its half diagonal.
fn bore(_n: f32, e: f32) -> bool {
    (0.75..0.85).contains(&e.abs())
}

const BORE_CLEARANCE_M: f64 = 0.740;

/// A pose at the vehicle with `sigma`.
fn pose(w: &mut World, sigma: f64) {
    let p = json!({"pose": {"t": [w.pos[0], w.pos[1], w.pos[2]]}, "sigma_xy_m": sigma});
    w.p.on_feed("plugin.world-engine.pose", &p, w.now);
}

/// One occupancy payload, then 1 s of poses at `sigma` (the tiles' 1 Hz).
fn payload_then(w: &mut World, map: &Value, sigma: f64) {
    w.p.on_feed("plugin.world-engine.occupancy", map, w.now);
    for _ in 0..10 {
        pose(w, sigma);
        w.run(100);
    }
}

fn check(w: &World) -> ados_protocol::pilot::RouteCheck {
    w.p.state(w.now).route_check.expect("route_check")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 2e-3
}

/// The x500 on its pad with the loaded route, the pose at σ `s0`.
fn pad(s0: f64) -> World {
    let mut w = World::new();
    confined(&mut w);
    on_pad(&mut w);
    pose(&mut w, s0);
    route(&mut w, "loaded", 0, &LINE);
    w
}

#[test]
fn a_bore_mapped_after_sigma_grew_needs_only_the_matched_sigma() {
    // A tunnel flight: σ 0.05 on the ground, 0.1 after the take-off climb
    // and a submap the prior placed; the bore ahead is mapped after (the
    // fourth payload: two whole windows past the growth). The map moved
    // with the pose, so the vehicle is the matched 0.05 m from it.
    let mut w = pad(0.05);
    payload_then(&mut w, &area(&|_, _| false), 0.1);
    payload_then(&mut w, &area(&|_, _| false), 0.1);
    payload_then(&mut w, &area(&|_, _| false), 0.1);
    payload_then(&mut w, &area(&bore), 0.1);
    let rc = check(&w);
    let pinch = rc.pinch.clone().expect("pinch");
    // 0.477 + 3 × 0.05.
    assert!(close(pinch.required_m, 0.627), "{rc:?}");
    assert!(close(pinch.clearance_m, BORE_CLEARANCE_M), "{rc:?}");
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
    // The most any obstacle could need stays the whole σ: 0.477 + 3 × 0.1.
    assert!(close(rc.required_m, 0.777), "{rc:?}");
    assert_eq!(rc.sigma_m, Some(0.1));
}

#[test]
fn a_bore_mapped_before_sigma_grew_keeps_the_growth_since() {
    // Mapped at σ 0.05, then unmatched travel grows σ to 0.15 and the same
    // cells stay occupied: relative to them the vehicle is 0.05 + 0.10.
    let mut w = pad(0.05);
    payload_then(&mut w, &area(&bore), 0.05);
    payload_then(&mut w, &area(&bore), 0.15);
    payload_then(&mut w, &area(&bore), 0.15);
    payload_then(&mut w, &area(&bore), 0.15);
    let rc = check(&w);
    let pinch = rc.pinch.clone().expect("pinch");
    // 0.477 + 3 × 0.15.
    assert!(close(pinch.required_m, 0.927), "{rc:?}");
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
}

#[test]
fn a_cell_seen_in_the_window_before_sigma_grew_keeps_the_growth() {
    // σ grows between two payloads: cells the second one brings may have
    // been seen before the growth (a scan reaches the tiles a payload late),
    // so they keep it.
    let mut w = pad(0.05);
    payload_then(&mut w, &area(&|_, _| false), 0.15);
    payload_then(&mut w, &area(&bore), 0.15);
    let rc = check(&w);
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.927),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
}

#[test]
fn a_pose_uncertain_from_the_start_needs_its_whole_sigma() {
    // σ 0.15 from the first pose on: the matched σ is 0.15 itself.
    let mut w = pad(0.15);
    payload_then(&mut w, &area(&|_, _| false), 0.15);
    payload_then(&mut w, &area(&|_, _| false), 0.15);
    payload_then(&mut w, &area(&bore), 0.15);
    let rc = check(&w);
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.927),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
}

#[test]
fn a_map_with_no_sigma_before_it_needs_the_whole_sigma() {
    // Tiles before any pose σ: their sighting is unknown, as old as the
    // map, so they keep the whole σ the pose grows to (0.05, then 0.1).
    let mut w = World::new();
    confined(&mut w);
    on_pad(&mut w);
    route(&mut w, "loaded", 0, &LINE);
    w.p.on_feed("plugin.world-engine.occupancy", &area(&bore), w.now);
    for s in [0.05, 0.1, 0.1] {
        for _ in 0..10 {
            pose(&mut w, s);
            w.run(100);
        }
    }
    let rc = check(&w);
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.777),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
}

/// `map` with its payload `seq`.
fn with_seq(mut map: Value, seq: u64) -> Value {
    map["seq"] = json!(seq);
    map
}

#[test]
fn a_new_map_caps_the_sightings_the_old_one_left() {
    // The last flight ended at σ 0.2 with the bore mapped; a new map starts
    // at 0.05 over the same cells, and its climb grows σ to 0.1. The cells
    // were seen in the new map at 0.05 at the earliest: they keep the climb.
    let mut w = pad(0.2);
    payload_then(&mut w, &area(&bore), 0.2);
    payload_then(&mut w, &area(&bore), 0.2);
    payload_then(&mut w, &area(&bore), 0.05);
    payload_then(&mut w, &area(&bore), 0.1);
    payload_then(&mut w, &area(&bore), 0.1);
    let rc = check(&w);
    // 0.477 + 3 × (0.05 + 0.05).
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.777),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
}

#[test]
fn cells_after_a_lost_payload_cannot_be_dated() {
    // Mapped after σ grew, but the payload before them was lost (`seq` 4
    // never came): the bore may have turned occupied in it, before.
    let free = area(&|_, _| false);
    let mut w = pad(0.05);
    payload_then(&mut w, &with_seq(free.clone(), 1), 0.1);
    payload_then(&mut w, &with_seq(free.clone(), 2), 0.1);
    payload_then(&mut w, &with_seq(free, 3), 0.1);
    payload_then(&mut w, &with_seq(area(&bore), 5), 0.1);
    let rc = check(&w);
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.777),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
    // In sequence they date: the matched σ.
    let mut w = pad(0.05);
    payload_then(&mut w, &with_seq(area(&|_, _| false), 1), 0.1);
    payload_then(&mut w, &with_seq(area(&|_, _| false), 2), 0.1);
    payload_then(&mut w, &with_seq(area(&|_, _| false), 3), 0.1);
    payload_then(&mut w, &with_seq(area(&bore), 4), 0.1);
    let rc = check(&w);
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.627),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
}

#[test]
fn a_broken_pose_never_reads_as_a_small_relative_sigma() {
    // The pose breaks at lift-off: σ runs to 1.73 and the matcher calls it
    // lost. A bore mapped after the break is no reference for it: every
    // obstacle needs the whole σ.
    let lost = |w: &mut World| {
        let p = json!({"pose": {"t": [w.pos[0], w.pos[1], w.pos[2]]}, "sigma_xy_m": 1.73,
                       "health": "lost"});
        w.p.on_feed("plugin.world-engine.pose", &p, w.now);
    };
    let mut w = pad(0.05);
    for _ in 0..4 {
        w.p.on_feed("plugin.world-engine.occupancy", &area(&|_, _| false), w.now);
        for _ in 0..10 {
            lost(&mut w);
            w.run(100);
        }
    }
    w.p.on_feed("plugin.world-engine.occupancy", &area(&bore), w.now);
    for _ in 0..12 {
        lost(&mut w);
        w.run(100);
    }
    let rc = check(&w);
    // 0.477 + 3 × 1.73.
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 5.667),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
}

/// A room flight's pose around its break at lift-off, one entry per 0.1 s
/// from the first jump: σ and the matcher's health, as the pose topic
/// carried them.
const BREAK: [(f64, &str); 24] = [
    (0.0958, "degraded"),
    (0.0958, "good"),
    (0.0958, "good"),
    (0.1858, "degraded"),
    (0.1858, "good"),
    (0.1858, "good"),
    (0.1858, "good"),
    (0.2312, "degraded"),
    (0.2643, "degraded"),
    (0.2859, "degraded"),
    (0.2969, "degraded"),
    (0.3000, "degraded"),
    (0.3071, "degraded"),
    (0.3071, "degraded"),
    (0.3268, "good"),
    (0.3268, "good"),
    (0.3268, "good"),
    (0.3867, "degraded"),
    (0.4444, "degraded"),
    (0.4444, "good"),
    (0.4888, "degraded"),
    (0.4888, "good"),
    (0.5086, "degraded"),
    (0.5178, "degraded"),
];

#[test]
fn from_a_pose_break_no_obstacle_reads_with_a_small_relative_sigma() {
    // The bore turns up in the map late in the break, while σ still climbs
    // and the matcher calls the pose degraded or good in turn (its cells
    // dated about 0.3, σ then 0.52): at no check, through the break and the
    // settle time after it, does any obstacle need less than the whole σ.
    let mut w = pad(0.05);
    payload_then(&mut w, &area(&|_, _| false), 0.05);
    payload_then(&mut w, &area(&|_, _| false), 0.05);
    let last = BREAK[BREAK.len() - 1];
    let after = std::iter::repeat_n(last, 15);
    let mut checked = 0;
    for (i, (s, health)) in BREAK.into_iter().chain(after).enumerate() {
        if i % 10 == 2 {
            let occ: &dyn Fn(f32, f32) -> bool = if i == 22 { &bore } else { &|_, _| false };
            w.p.on_feed("plugin.world-engine.occupancy", &area(occ), w.now);
        }
        let p = json!({"pose": {"t": [w.pos[0], w.pos[1], w.pos[2]]}, "sigma_xy_m": s,
                       "health": health});
        w.p.on_feed("plugin.world-engine.pose", &p, w.now);
        w.run(100);
        let rc = check(&w);
        if let Some(pinch) = rc.pinch.clone() {
            checked += 1;
            assert!(close(pinch.required_m, rc.required_m), "step {i}: {rc:?}");
            assert_eq!(rc.state, RouteState::Infeasible, "step {i}: {rc:?}");
        }
    }
    assert!(
        checked > 5,
        "the bore was judged after the break ({checked})"
    );
}

#[test]
fn a_sigma_step_in_flight_never_holds_a_leg_the_match_keeps_clear() {
    // In AUTO toward item 2 at n 9, a narrow at n 15–16 (0.740 m off the
    // leg) mapped at σ 0.1, base 0.05: it needs 0.627. A height step then
    // raises σ to 0.11: for the 2 s σ settles every obstacle counts the
    // whole 0.11 (0.807), but the narrow is beyond the stopping reach, so
    // nothing holds; settled, it needs 0.657 and reads feasible.
    let narrow = |n: f32, e: f32| (15.0..16.0).contains(&n) && bore(n, e);
    let mut w = World::new();
    confined(&mut w);
    w.airborne(crate::modes::copter::AUTO);
    w.pos = [9.0, 0.0, -5.0];
    pose(&mut w, 0.05);
    w.run(100);
    route(&mut w, "active", 2, &LINE);
    for _ in 0..3 {
        payload_then(&mut w, &area(&|_, _| false), 0.1);
    }
    payload_then(&mut w, &area(&narrow), 0.1);
    payload_then(&mut w, &area(&narrow), 0.1);
    payload_then(&mut w, &area(&narrow), 0.1);
    assert_eq!(check(&w).state, RouteState::Feasible, "{:?}", check(&w));
    let brakes = w.set_modes(crate::modes::copter::BRAKE);
    let mut tight = false;
    for _ in 0..40 {
        pose(&mut w, 0.11);
        w.run(100);
        tight |= check(&w).state == RouteState::Infeasible;
    }
    assert!(tight, "the whole σ read the narrow tight while σ settled");
    assert_eq!(w.set_modes(crate::modes::copter::BRAKE), brakes, "no hold");
    assert_eq!(w.p.state(w.now).hold_reason, None);
    let rc = check(&w);
    assert!(
        close(rc.pinch.clone().expect("pinch").required_m, 0.657),
        "{rc:?}"
    );
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
}

/// The narrow of `a_sigma_step_in_flight_never_holds_a_leg_the_match_keeps_clear`,
/// mapped at σ 0.1 (base 0.05), the vehicle in AUTO at `n` toward `item`.
fn narrow_mapped(n: f32, item: u32) -> World {
    let narrow = |n: f32, e: f32| (15.0..16.0).contains(&n) && bore(n, e);
    let mut w = World::new();
    confined(&mut w);
    w.airborne(crate::modes::copter::AUTO);
    w.pos = [n, 0.0, -5.0];
    pose(&mut w, 0.05);
    w.run(100);
    route(&mut w, "active", item, &LINE);
    for _ in 0..3 {
        payload_then(&mut w, &area(&|_, _| false), 0.1);
    }
    for _ in 0..3 {
        payload_then(&mut w, &area(&narrow), 0.1);
    }
    assert_eq!(check(&w).state, RouteState::Feasible, "{:?}", check(&w));
    w
}

#[test]
fn a_pinch_within_the_stopping_reach_holds_at_once_while_sigma_settles() {
    // Just short of the narrow: under the whole σ its tight stretch is
    // within the stopping reach, so the settle window does not delay the hold.
    let mut w = narrow_mapped(13.6, 3);
    let brakes = w.set_modes(crate::modes::copter::BRAKE);
    for _ in 0..2 {
        pose(&mut w, 0.11);
        w.run(100);
    }
    assert_eq!(w.set_modes(crate::modes::copter::BRAKE), brakes + 1);
    assert_eq!(
        w.p.state(w.now).hold_reason.as_deref(),
        Some("route-infeasible")
    );
}

#[test]
fn a_leg_still_tight_once_sigma_settles_holds_on_the_first_settled_check() {
    // σ 0.1 → 0.2 with the narrow beyond the stopping reach: no hold while
    // σ settles, then the first settled check still reads it tight and holds
    // at once (its verdict has stood longer than the confirm time).
    let mut w = narrow_mapped(9.0, 2);
    let brakes = w.set_modes(crate::modes::copter::BRAKE);
    pose(&mut w, 0.2);
    let rose = w.now;
    while w.now < rose + 1900 {
        w.run(100);
        pose(&mut w, 0.2);
    }
    assert_eq!(w.set_modes(crate::modes::copter::BRAKE), brakes, "settling");
    assert_eq!(check(&w).state, RouteState::Infeasible, "{:?}", check(&w));
    while w.now < rose + 2000 + 1100 {
        w.run(100);
        pose(&mut w, 0.2);
    }
    assert_eq!(w.set_modes(crate::modes::copter::BRAKE), brakes + 1);
    assert_eq!(
        w.p.state(w.now).hold_reason.as_deref(),
        Some("route-infeasible")
    );
}
