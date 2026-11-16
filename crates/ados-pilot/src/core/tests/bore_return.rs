//! A return held in a confined bore resumes back along the path flown out:
//! the way it resumes on is the one SmartRTL flies (back down the bore, not
//! the straight line home through the rock at the bend), judged at the
//! confined cap by the brake's own rule. A blocked return keeps looking
//! during its landing countdown, so a way back that clears is flown; only
//! one that stays blocked lands in place.

use ados_protocol::pilot::{Owner, PlaceClass};

use super::confined::{map, map_payload, run_seen};
use super::harness::World;
use crate::core::control::supervisor::RETURN_BLOCKED_RESUMES;
use crate::modes::copter;

/// An L-shaped bore 1.8 m wide: north from home to n 10, then east to e 10.
fn free(n: f32, e: f32) -> bool {
    let north = (-2.0..10.9).contains(&n) && e.abs() < 0.9;
    let east = (9.1..10.9).contains(&n) && (-0.9..10.0).contains(&e);
    north || east
}

/// The planar scan from `pos` (yaw 0: body bins are world bearings): each
/// bin's range to the bore's rock, 12 m at most.
fn scan_from(pos: [f32; 3]) -> Vec<f32> {
    (0..360)
        .map(|b| {
            let th = (b as f32).to_radians();
            let mut r = 0.05;
            while r < 12.0 {
                if !free(pos[0] + r * th.cos(), pos[1] + r * th.sin()) {
                    return r;
                }
                r += 0.02;
            }
            0.0
        })
        .collect()
}

/// The quad5 airframe (0.2 m tip) in the bore at 1 m, flown out from home
/// up the north leg and along the east leg to (9.55, 5.0), 0.45 m off the
/// south wall, then on its SmartRTL home.
fn returning() -> World {
    let mut w = World::new();
    let p = &mut w.p.settings.pilot;
    p.place_class = PlaceClass::Confined;
    p.prop_tip_radius_m = Some(0.20209);
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.2);
    w.gps_fix = None;
    w.airborne(copter::LOITER);
    w.pos = [0.0, 0.0, -1.0];
    w.agl = Some(1.0);
    map(
        &mut w,
        &map_payload(-3.2, 12.8, -3.2, 12.8, &|n, e| !free(n, e)),
    );
    let mut path: Vec<[f32; 2]> = (0..=100).map(|k| [k as f32 * 0.1, 0.0]).collect();
    path.extend((1..=50).map(|k| [10.0, k as f32 * 0.1]));
    path.push([9.55, 5.0]);
    for [n, e] in path {
        // The FC's position first: the pose places the map against it.
        w.pos = [n, e, -1.0];
        w.feed();
        w.scan_ranges = scan_from(w.pos);
        run_seen(&mut w, 100);
    }
    assert!(super::confined::ok(&w.action("rth")));
    run_seen(&mut w, 200);
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w
}

/// Something 0.15 m off the nose (inside the hard stop) for `ms`: BRAKE,
/// then the GUIDED hold.
fn post_ahead(w: &mut World, ms: u64) {
    let clear = scan_from(w.pos);
    let mut post = clear.clone();
    post[270] = 0.15;
    w.scan_ranges = post;
    run_seen(w, ms);
    w.scan_ranges = clear;
}

#[test]
fn a_held_return_in_a_bore_resumes_back_along_the_flown_path() {
    let mut w = returning();
    post_ahead(&mut w, 1000);
    assert!(w.set_modes(copter::BRAKE) >= 1);
    assert_eq!(w.mode, copter::GUIDED);
    // The bore keeps 0.45 m to the south wall and 1.35 m to the north one:
    // back west along the east leg the brake would not slow a 0.5 m/s
    // return. The straight line home runs into the south wall.
    run_seen(&mut w, 5000);
    assert_eq!(w.mode, copter::SMART_RTL);
    // The operator's return, then its resumption.
    assert_eq!(w.set_modes(copter::SMART_RTL), 2);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert_eq!(w.set_modes(copter::LAND), 0);
}

/// Hold the post in the way until the no-progress rule declares the return
/// blocked (at most 40 s); returns the stall's landing countdown, s.
fn until_blocked(w: &mut World) -> f64 {
    for _ in 0..40 {
        post_ahead(w, 1000);
        if let Some(stall) = w.p.state(w.now).mission_stall {
            assert_eq!(stall.kind, "return");
            assert_eq!(w.mode, copter::GUIDED);
            return stall.skip_in_s.expect("the landing countdown");
        }
    }
    panic!("the return never read blocked");
}

#[test]
fn a_blocked_return_in_a_bore_resumes_once_its_way_back_clears() {
    let mut w = returning();
    let left = until_blocked(&mut w);
    assert!(left > 40.0, "{left}");
    // The way back clears: the return flies on, the countdown ends.
    run_seen(&mut w, 4000);
    assert_eq!(w.mode, copter::SMART_RTL);
    assert!(w.p.state(w.now).mission_stall.is_none());
    assert_eq!(w.set_modes(copter::LAND), 0);
}

#[test]
fn a_return_that_keeps_reading_blocked_lands_once_its_resumes_are_spent() {
    // The way back reads clear, yet the return never moves: each time it
    // reads blocked it resumes, twice a flight; the third time the
    // countdown lands it.
    let mut w = returning();
    until_blocked(&mut w);
    run_seen(&mut w, 100_000);
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.p.sup.return_resumes, RETURN_BLOCKED_RESUMES);
}

#[test]
fn a_return_blocked_for_the_whole_countdown_lands_in_place() {
    let mut w = returning();
    let left = until_blocked(&mut w);
    post_ahead(&mut w, (left * 1000.0) as u64 - 500);
    assert_eq!(w.mode, copter::GUIDED);
    post_ahead(&mut w, 1000);
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.set_modes(copter::LAND), 1);
}
