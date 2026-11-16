//! The scans' closing speed against what the brake would allow knowing the
//! true velocity, past posts off the flight line and on slower lidars.

use std::f32::consts::TAU;

use serde_json::{json, Value};

use super::safety::{wall_scan, G};
use crate::core::control::{
    brake_limit, stop_cap, SCAN_CLOSING_FLOOR_MPS, SCAN_CLOSING_VEL_FACTOR,
};
use crate::geo::{self, V2};
use crate::obstacles::{Level, Obstacles, CELL_M, SCAN_CLOSING_MAX_MPS};

const LEVEL: Level = Level {
    roll: 0.0,
    pitch: 0.0,
    yaw: 0.0,
    agl: Some(2.5),
};
const A_H: f32 = 3.0;
const MARGIN: f32 = 1.5;
/// The lidar's range error the true-velocity reference is allowed: the scans
/// see a surface up to this much nearer than it is (±30 mm 3σ).
const RANGE_SLACK_M: f32 = 0.05;
/// Speed the sweep allows over both references, m/s: a fit through five
/// ranges with ±17 mm noise has a slope error near 0.03 m/s (the replayed
/// column test holds the brake to the same 0.05 m/s).
const SWEEP_TOL_MPS: f32 = 0.05;

/// Uniform noise in ±`amp` from a hash of the scan and the beam.
fn noise(seed: u64, k: u64, i: usize, amp: f32) -> f32 {
    let h = (k.wrapping_add(seed.wrapping_mul(7_919))).wrapping_mul(2_654_435_761)
        ^ (i as u64).wrapping_mul(40_503);
    amp * (2.0 * (h % 1000) as f32 / 1000.0 - 1.0)
}

/// A planar scan (yaw 0, north-facing) from `x` metres north of the origin
/// of a post of radius 0.15 m at (`post_x`, `post_y`), 1° beams to 12 m.
fn post_scan(x: f32, post_x: f32, post_y: f32, seed: u64, k: u64) -> Value {
    let rp = 0.15f32;
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            let th = (i as f32).to_radians();
            let (dx, dy) = (th.cos(), th.sin());
            let (ox, oy) = (x - post_x, -post_y);
            let b = ox * dx + oy * dy;
            let disc = b * b - (ox * ox + oy * oy - rp * rp);
            if disc < 0.0 {
                return 0.0;
            }
            let t = -b - disc.sqrt();
            if t > 0.05 && t <= 12.0 {
                t + noise(seed, k, i, 0.017)
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

/// The 0.112.1 closing speed: per 5° world sector, the nearest return's range
/// rate between consecutive scans, smoothed 0.5 each scan.
struct PairEma {
    prev: [f32; 72],
    prev_ms: Option<u64>,
    closing: [f32; 72],
}

fn sector(dir: V2) -> usize {
    (geo::bearing_of(dir).rem_euclid(TAU) / TAU * 72.0) as usize % 72
}

impl PairEma {
    fn new() -> Self {
        Self {
            prev: [f32::INFINITY; 72],
            prev_ms: None,
            closing: [0.0; 72],
        }
    }

    fn push(&mut self, o: &Obstacles, pos: V2, now: u64) {
        let mut cur = [f32::INFINITY; 72];
        for h in o.hits(now) {
            let s = sector(geo::sub(h.ne, pos));
            cur[s] = cur[s].min(h.range_m);
        }
        let dt = self
            .prev_ms
            .map_or(f32::INFINITY, |t| (now - t) as f32 / 1000.0);
        for (s, (&now_r, c)) in cur.iter().zip(self.closing.iter_mut()).enumerate() {
            let fall = self.prev[s] - now_r;
            let rate = if (0.05..=0.5).contains(&dt)
                && now_r.is_finite()
                && self.prev[s].is_finite()
                && fall <= 16.0 * dt + 0.1
            {
                (fall / dt).max(0.0)
            } else {
                0.0
            };
            *c = 0.5 * rate + 0.5 * *c;
        }
        self.prev = cur;
        self.prev_ms = Some(now);
    }

    fn toward(&self, dir: V2) -> f32 {
        let s = sector(dir);
        [(s + 71) % 72, s, (s + 1) % 72]
            .iter()
            .map(|&i| self.closing[i])
            .fold(0.0, f32::max)
    }
}

/// Speed along north the brake's caps leave for the vehicle at `pos`, each
/// scan return capping the component toward it at `stop_cap` with the
/// closing speed `closing(dir)` (no slide), up to `v`.
fn allowed(
    o: &Obstacles,
    pos: V2,
    now: u64,
    v: f32,
    slack: f32,
    closing: impl Fn(V2) -> f32,
) -> f32 {
    o.hits(now)
        .iter()
        .filter_map(|h| {
            let rel = geo::sub(h.ne, pos);
            let r = geo::norm(rel);
            let dir = geo::scale(rel, 1.0 / r);
            (dir[0] > 0.05).then(|| {
                stop_cap(
                    (r + slack - CELL_M / 2.0).max(0.0),
                    closing(dir),
                    A_H,
                    MARGIN,
                ) / dir[0]
            })
        })
        .fold(v, f32::min)
}

/// Worst excess, m/s, of the allowed speed over the larger of the 0.112.1
/// closing speed's and the true velocity's, flying north at `v` past a post
/// `lateral` metres off the line 14 m ahead, the velocity estimate `scale`
/// times the real one, scans at `hz`; with the range out to the post then.
fn worst_excess(v: f32, lateral: f32, scale: f32, hz: u64, seed: u64) -> (f32, f32) {
    let period = 1000 / hz;
    let post_x = 14.0;
    let est = scale * v;
    let bound = 2.0 * est + 1.0;
    let mut o = Obstacles::new();
    let mut ema = PairEma::new();
    let mut worst = (0.0f32, f32::NAN);
    let mut k = 0u64;
    loop {
        let x = v * (k * period) as f32 / 1000.0;
        if x > post_x - 0.3 {
            return worst;
        }
        let now = 1000 + k * period;
        let pos = [x, 0.0];
        let scan = post_scan(x, post_x, lateral, seed, k);
        o.ingest_scan(&scan, LEVEL, pos, SCAN_CLOSING_MAX_MPS, now);
        ema.push(&o, pos, now);
        let lagging = |scan: f32, dir: V2| (est * dir[0]).max(scan.min(bound));
        let fix = allowed(&o, pos, now, v, 0.0, |d| lagging(o.scan_closing(d, now), d));
        let old = allowed(&o, pos, now, v, 0.0, |d| lagging(ema.toward(d), d));
        let truth = allowed(&o, pos, now, v, RANGE_SLACK_M, |d| v * d[0]);
        let excess = fix - old.max(truth);
        if excess > worst.0 {
            let range = (post_x - x).hypot(lateral) - 0.15;
            worst = (excess, range);
        }
        k += 1;
    }
}

#[test]
fn a_post_off_the_line_keeps_its_closing_speed_across_sectors() {
    // 6 m/s toward a post 1 m off the line, the estimate half the real
    // speed: as the post sweeps from one 5° sector into the next, its
    // closing speed must carry on, or for three scans the brake allows
    // 2.5 m/s 5 m out where the true velocity allows nothing.
    for seed in 0..5 {
        let (excess, range) = worst_excess(6.0, 1.0, 0.5, 10, seed);
        assert!(
            excess <= 1e-3,
            "seed {seed}: allows {excess:.2} m/s more than the true velocity, {range:.1} m from the post"
        );
    }
}

/// Every speed 2-6 m/s, post 0-2.2 m off the line, estimate x1/x0.5/x0.3,
/// 10 and 5 Hz scans: at no range does the brake allow more than the larger
/// of the 0.112.1 closing speed's allowance and the true velocity's.
#[test]
fn past_posts_the_brake_never_allows_more_than_before_or_the_true_speed() {
    let mut runs = 0;
    let mut over = Vec::new();
    for hz in [10, 5] {
        for v in [2.0, 3.0, 4.0, 5.0, 6.0] {
            for lateral in [0.0, 0.5, 1.0, 1.6, 2.2] {
                for scale in [1.0, 0.5, 0.3] {
                    for seed in 0..3 {
                        let (excess, range) = worst_excess(v, lateral, scale, hz, seed);
                        if excess > SWEEP_TOL_MPS {
                            over.push(format!(
                                "{hz} Hz, {v} m/s, post {lateral} m off the line, estimate \
                                 x{scale}, seed {seed}: {excess:.2} m/s over both, {range:.1} m out"
                            ));
                        }
                        runs += 1;
                    }
                }
            }
        }
    }
    assert_eq!(runs, 450);
    assert!(
        over.is_empty(),
        "{} of {runs} runs:\n{}",
        over.len(),
        over.join("\n")
    );
}

#[test]
fn a_braking_approach_on_a_5_hz_lidar_still_measures_its_closing_speed() {
    // A 5 Hz lidar, the vehicle braking at 3 m/s² from 3 m/s toward a wall:
    // five scans span 0.8 s and bend by 10 cm RMS from a line. The bend is
    // the brake working, not leaf jitter; the closing speed must hold.
    let mut o = Obstacles::new();
    let mut d = 8.0f32;
    for k in 0..5u64 {
        if k > 0 {
            d -= 0.2 * (3.0 - 0.6 * (k - 1) as f32);
        }
        let mut scan = wall_scan(d);
        for (i, r) in scan["ranges_m"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            let x = r.as_f64().unwrap() as f32;
            if x > 0.0 {
                *r = json!(x + noise(1, k, i, 0.02));
            }
        }
        o.ingest_scan(
            &scan,
            LEVEL,
            [0.0, 0.0],
            SCAN_CLOSING_MAX_MPS,
            1000 + 200 * k,
        );
    }
    let closing = o.scan_closing([1.0, 0.0], 1800);
    assert!(
        closing > 1.2,
        "braking from 3.0 to 1.2 m/s, measured {closing:.2} m/s"
    );
}

#[test]
fn a_creep_toward_a_wall_holds_outside_the_flown_hold() {
    // Sticks forward against a wall the vehicle already stopped at: it drifts
    // at 0.05 m/s, under what the estimate or the scans resolve. The brake
    // must hold it at 1.69 m or further, where the flown hold ended, not
    // let it creep on to the margin. The scan returns alone (no grid cells,
    // whose rounding brings some nearer) set the caps here.
    for cm in 160..=169u16 {
        let raw = f32::from(cm) / 100.0;
        let mut o = Obstacles::new();
        for k in 0..6u64 {
            o.ingest_scan(
                &wall_scan(raw),
                LEVEL,
                [0.0, 0.0],
                SCAN_CLOSING_MAX_MPS,
                1000 + 100 * k,
            );
        }
        let b = brake_limit(&o, [0.0, 0.0], [0.05, 0.0], [1.0, 0.0], A_H, G, 1500);
        assert!(
            b.v[0] <= 1e-3,
            "{raw:.2} m from the wall the brake still allows {:.2} m/s toward it",
            b.v[0]
        );
    }
}

/// `wall_scan(d)` captured at `t_ms` on the lidar's own clock.
fn stamped(d: f32, t_ms: u64) -> Value {
    let mut s = wall_scan(d);
    s["t_ns"] = json!(t_ms * 1_000_000);
    s
}

#[test]
fn delivery_jitter_does_not_scale_a_range_rate() {
    // A wall closing at 0.5 m/s, captured every 100 ms but delivered
    // bunched: the first four scans 50 ms apart, the fifth 250 ms later.
    // The fit over the capture stamps reads the true 0.5 m/s.
    let arrivals = [1000u64, 1050, 1100, 1150, 1400];
    let mut o = Obstacles::new();
    for (k, at) in arrivals.iter().enumerate() {
        let t = 100 * k as u64;
        let d = 2.0 - 0.05 * k as f32;
        o.ingest_scan(&stamped(d, t), LEVEL, [0.0; 2], SCAN_CLOSING_MAX_MPS, *at);
        if k == 3 {
            // Four scans delivered 50 ms apart: by arrival, 1 m/s.
            let c = o.scan_closing([1.0, 0.0], *at);
            assert!((c - 0.5).abs() < 0.03, "closing {c} after four scans");
        }
    }
    // Straight ahead (an oblique sector's range falls by 1/cos as much).
    let c = o.scan_closing([1.0, 0.0], 1400);
    assert!((c - 0.5).abs() < 0.03, "closing {c}");
    // Scans without a stamp are timed by their arrival.
    let mut a = Obstacles::new();
    for k in 0..5u64 {
        let d = 2.0 - 0.05 * k as f32;
        a.ingest_scan(
            &wall_scan(d),
            LEVEL,
            [0.0; 2],
            SCAN_CLOSING_MAX_MPS,
            1000 + 100 * k,
        );
    }
    let c = a.scan_closing([1.0, 0.0], 1400);
    assert!((c - 0.5).abs() < 0.03, "closing {c}");
}

#[test]
fn a_nearer_wall_at_a_bend_is_not_an_approach_at_the_vehicles_speed() {
    // Flying 0.5 m/s up a tunnel, the bore turns ahead: the sector's nearest
    // return steps from the far wall 3.85 m off to the bend's wall 2.6 m off
    // between two scans. The step never reads faster than the brake's
    // trust bound on that speed (2 × 0.5 + 1 m/s); the bend's wall then
    // nearing at the vehicle's own speed is measured.
    let bound = SCAN_CLOSING_VEL_FACTOR * 0.5 + SCAN_CLOSING_FLOOR_MPS;
    let mut o = Obstacles::new();
    let mut t = 0u64;
    for d in [4.0f32, 3.95, 3.9, 3.85, 2.6] {
        o.ingest_scan(&stamped(d, t), LEVEL, [0.0; 2], bound, 1000 + t);
        let c = o.max_scan_closing(1000 + t);
        assert!(c <= bound + 1e-3, "closing {c} over the bound {bound}");
        t += 100;
    }
    for d in [2.55f32, 2.5, 2.45, 2.4] {
        o.ingest_scan(&stamped(d, t), LEVEL, [0.0; 2], bound, 1000 + t);
        t += 100;
    }
    let c = o.scan_closing([1.0, 0.0], 1000 + t - 100);
    assert!((c - 0.5).abs() < 0.03, "closing {c}");
}

#[test]
fn something_closing_faster_than_the_bound_reads_the_bound() {
    // A hovering vehicle (bound 1 m/s) and a wall closing at 3 m/s: never
    // less than a slower approach would read, it reads the bound the brake
    // trusts.
    let bound = SCAN_CLOSING_FLOOR_MPS;
    let mut o = Obstacles::new();
    for (k, d) in [6.0f32, 5.7, 5.4, 5.1].iter().enumerate() {
        let t = 100 * k as u64;
        o.ingest_scan(&stamped(*d, t), LEVEL, [0.0; 2], bound, 1000 + t);
    }
    let c = o.scan_closing([1.0, 0.0], 1300);
    assert!((c - bound).abs() < 1e-3, "closing {c}");
}
