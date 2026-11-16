//! A sloped leg the planar scan judges only a little way ahead: the FC is
//! held to a speed whose stop fits in that lookahead, so a pinch the scan
//! reveals only as the vehicle descends to its height still stops it short.

use ados_protocol::pilot::Owner;
use serde_json::{json, Value};

use super::confined::{area, confined, map, seen, speed_changes};
use super::confined_route::feed;
use super::harness::World;
use crate::core::control::BRAKE_LAG_S;
use crate::modes::copter;
use crate::obstacles::SLICE_HALF_M;

/// The scripted FC's horizontal acceleration and braking, m/s² (the normal
/// profile's).
const ACCEL: f32 = 3.0;
/// The FC's own mission speed (`WP_SPD`), m/s.
const WP_SPD: f32 = 2.0;
/// The descent: 10° below the horizontal.
const SLOPE_DEG: f32 = 10.0;
/// Where the descent starts (item 2), and how far ahead of it the pinch is.
const TOP: [f32; 3] = [10.0, 0.0, -5.0];
const PINCH_AHEAD_M: f32 = 2.0;

fn item_at(s: u32, p: [f32; 3]) -> Value {
    json!({"seq": s, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1, "alt_m": -p[2],
           "ned": p})
}

/// The point `x` metres (horizontally) down the descent from item 2.
fn down(x: f32) -> [f32; 3] {
    [
        TOP[0] + x,
        TOP[1],
        TOP[2] + x * SLOPE_DEG.to_radians().tan(),
    ]
}

/// A ledge 0.5–0.7 m east of the leg over n ± 0.1 around the pinch (0.525 m
/// from it, under the 0.577 m the leg needs), as the planar scan maps it:
/// only while the scan plane is within the slice of the ledge's height.
fn ledge(n_p: f32) -> impl Fn(f32, f32) -> bool {
    move |n, e| (n - n_p).abs() < 0.1 && (0.5..0.7).contains(&e)
}

/// Fly item 2 → 3 down the slope in AUTO from `TOP`, after `phase_ms` of
/// hovering there (the route check's own one-second cadence then meets the
/// ledge at another moment). The scripted FC flies the leg at the last
/// speed the pilot sent (else `WP_SPD`), changing speed at [`ACCEL`]; once
/// out of AUTO it carries on for the FC's response ([`BRAKE_LAG_S`]) and
/// brakes at [`ACCEL`]. Returns how far down the slope it stopped and
/// whether the route held it.
fn descend(phase_ms: u64) -> (f32, bool) {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), WP_SPD);
    w.airborne(copter::AUTO);
    w.pos = TOP;
    w.run(100);
    let pinch = down(PINCH_AHEAD_M);
    let clear = area(&|_, _| false);
    let with_ledge = area(&ledge(pinch[0]));
    map(&mut w, &clear);
    let items = vec![
        item_at(1, [2.0, 0.0, -5.0]),
        item_at(2, TOP),
        item_at(3, down(10.0)),
        item_at(4, [25.0, 0.0, down(10.0)[2]]),
    ];
    feed(&mut w, "active", 3, items, Some([20.0, 0.0]), json!({}));
    for _ in 0..phase_ms / 20 {
        seen(&mut w);
        w.run(20);
    }
    let (tan, dt) = (SLOPE_DEG.to_radians().tan(), 0.02);
    let (mut x, mut v) = (0.0f32, 0.0f32);
    let mut shown = false;
    let mut out_of_auto: Option<u64> = None;
    for _ in 0..1500 {
        let target = if w.mode == copter::AUTO {
            speed_changes(&w).last().copied().unwrap_or(WP_SPD)
        } else {
            let t = *out_of_auto.get_or_insert(w.now);
            if w.now.saturating_sub(t) < (BRAKE_LAG_S * 1000.0) as u64 {
                v
            } else {
                0.0
            }
        };
        v = if v < target {
            (v + ACCEL * dt).min(target)
        } else {
            (v - ACCEL * dt).max(target)
        };
        x += v * dt;
        w.pos = down(x);
        w.vel = [v, 0.0, v * tan];
        // The planar scan maps the ledge only from within its slice.
        let in_slice = (w.pos[2] - pinch[2]).abs() <= SLICE_HALF_M;
        if in_slice != shown {
            shown = in_slice;
            map(&mut w, if in_slice { &with_ledge } else { &clear });
        }
        seen(&mut w);
        w.run(20);
        if out_of_auto.is_some() && v == 0.0 {
            break;
        }
    }
    let held = w.p.state(w.now).hold_reason.as_deref() == Some("route-infeasible")
        && w.p.owner() == Owner::Pilot;
    (x, held)
}

#[test]
fn a_pinch_past_the_slice_on_a_10_degree_descent_stops_the_vehicle_before_it() {
    // At 10° the scan judges the leg 0.13/tan 10° = 0.74 m ahead: less the
    // 0.577 m clearance that leaves 0.16 m, under the crawl's stop, so the
    // FC crawls at 0.2 m/s. At the plain 0.5 m/s confined cap the stop
    // (0.94 m) overruns the lookahead, and a ledge that first shows just
    // after a check is flown past.
    for phase_ms in (0..1000).step_by(100) {
        let (x, held) = descend(phase_ms);
        assert!(held, "phase {phase_ms} ms: never held");
        assert!(
            x < PINCH_AHEAD_M,
            "phase {phase_ms} ms: stopped {x:.3} m down the slope, past the pinch at {PINCH_AHEAD_M} m"
        );
    }
}

#[test]
fn a_10_degree_descent_is_capped_at_the_crawl() {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), WP_SPD);
    w.airborne(copter::AUTO);
    w.pos = TOP;
    w.run(100);
    map(&mut w, &area(&|_, _| false));
    let items = vec![item_at(2, TOP), item_at(3, down(10.0))];
    feed(&mut w, "active", 3, items, Some([20.0, 0.0]), json!({}));
    seen(&mut w);
    w.run(200);
    assert_eq!(speed_changes(&w), vec![0.2]);
    let cap = w.p.state(w.now).speed_cap.expect("cap");
    assert_eq!(cap.v_h_mps, 0.2);
}

#[test]
fn a_gentle_descent_is_capped_by_its_own_stop() {
    // At 5° the lookahead is 0.13/tan 5° = 1.49 m: 0.91 m of room past the
    // clearance, a stop at 0.483 m/s (v·1.8 + v²/6 = 0.91), sent as 0.45
    // (0.05 m/s steps, rounded down).
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), WP_SPD);
    w.airborne(copter::AUTO);
    w.pos = TOP;
    w.run(100);
    map(&mut w, &area(&|_, _| false));
    let end = [20.0, 0.0, TOP[2] + 10.0 * 5f32.to_radians().tan()];
    let items = vec![item_at(2, TOP), item_at(3, end)];
    feed(&mut w, "active", 3, items, Some([20.0, 0.0]), json!({}));
    seen(&mut w);
    w.run(200);
    let sent = speed_changes(&w);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!((sent[0] - 0.45).abs() < 1e-4, "{sent:?}");
}
