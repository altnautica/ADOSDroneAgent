//! Replays of a confined flight down a mine tunnel bore (a bench dry run of
//! the quad5 confined airframe): the FC's recorded local position, velocity,
//! attitude and rangefinder, the planar scans as they arrived (with their
//! capture stamps), the scan-matcher poses and the occupancy tiles near the
//! flown path (`fixtures/tunnel_bore.json`). The bore keeps 0.8–1.5 m from
//! the vehicle on every side through these windows, so nothing in them is
//! inside the brake's reach at the 0.5 m/s the FC flies.

use ados_protocol::pilot::PlaceClass;
use serde_json::{json, Value};

use super::harness::World;
use super::supervisor::with_logs;
use crate::modes::copter;

/// quad5-ados-confined's outermost rigid extent about its CG.
const QUAD5_TIP_M: f32 = 0.20209;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/tunnel_bore.json")).expect("fixture")
}

/// The FC's state at one recorded sample: `[t, n, e, d, vn, ve, vd, roll,
/// pitch, yaw, agl]`.
fn fc_sample(w: &mut World, s: &Value) {
    let f = |i: usize| s[i].as_f64().expect("number") as f32;
    w.pos = [f(1), f(2), f(3)];
    w.vel = [f(4), f(5), f(6)];
    w.roll = f(7);
    w.pitch = f(8);
    w.yaw = f(9);
    w.agl = Some(f(10));
}

fn scan_payload(s: &Value) -> Value {
    let ranges: Vec<f64> = s[2]
        .as_array()
        .expect("ranges")
        .iter()
        .map(|r| r.as_f64().unwrap_or(0.0) / 1000.0)
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": std::f64::consts::TAU / ranges.len() as f64,
           "ranges_m": ranges, "t_ns": s[1]})
}

fn pose_payload(p: &Value) -> Value {
    json!({"frame": "map_ned", "source": "lidar_planar", "pose": {"t": [p[1], p[2], p[3]]},
           "sigma_xy_m": p[4], "health": p[5]})
}

/// A confined flight of the quad5 airframe without GNSS, as flown.
fn quad5_confined(w: &mut World) {
    let p = &mut w.p.settings.pilot;
    p.place_class = PlaceClass::Confined;
    p.prop_tip_radius_m = Some(QUAD5_TIP_M);
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.2);
    w.p.fc.params.insert("AVOID_ENABLE".into(), 0.0);
    w.gps_fix = None;
    w.lidar = false;
}

/// The router's mission state with item `seq` active at `ned`.
fn mission_target(w: &mut World, seq: u32, ned: [f64; 3]) {
    let snapshot = json!({
        "armed": w.armed,
        "mission": {"state": "active", "seq": seq, "total": 57, "item_reached": seq - 1,
                    "id": null, "source": "mission_state", "age_ms": 100, "landing_seq": false,
                    "target": {"seq": seq, "cmd": 16, "ned": ned},
                    "rev": 1, "uploads_seen": 0, "items": []},
    });
    let m = crate::router_state::mission_of(snapshot);
    w.p.on_feed(crate::router_state::MISSION_TOPIC, &m, w.now);
    w.collect();
}

/// Fly window `name` of the fixture in FC `mode` (AUTO toward `target`, or
/// a return), feeding every recorded frame at its time; returns the world
/// and the time (ms from the window start) the pilot first commanded BRAKE.
fn replay(name: &str, mode: u32, target: Option<(u32, [f64; 3])>) -> (World, Option<u64>) {
    let fx = fixture();
    let win = &fx["windows"][name];
    let rows = |k: &str| win[k].as_array().expect(k).clone();
    let (fc, scans, poses, occ) = (rows("fc"), rows("scans"), rows("poses"), rows("occupancy"));
    let mut w = World::new();
    quad5_confined(&mut w);
    w.airborne(mode);
    fc_sample(&mut w, &fc[0]);
    if let Some((seq, ned)) = target {
        mission_target(&mut w, seq, ned);
    }
    let t0 = w.now as i64;
    let at = |r: &Value| r[0].as_i64().expect("t");
    let end = at(fc.last().expect("fc"));
    let (mut i_fc, mut i_scan, mut i_pose, mut i_occ) = (0, 0, 0, 0);
    let mut braked = None;
    let mut rel = at(&fc[0]).min(at(&scans[0])).min(at(&occ[0]));
    while rel < end {
        rel += 20;
        // Every frame due by this tick, in time order.
        loop {
            let next = [
                fc.get(i_fc).map(at),
                scans.get(i_scan).map(at),
                poses.get(i_pose).map(at),
                occ.get(i_occ).map(at),
            ];
            let Some((k, t)) = next
                .iter()
                .enumerate()
                .filter_map(|(k, t)| t.map(|t| (k, t)))
                .min_by_key(|&(_, t)| t)
            else {
                break;
            };
            if t > rel {
                break;
            }
            let now = (t0 + t).max(w.now as i64) as u64;
            match k {
                0 => {
                    fc_sample(&mut w, &fc[i_fc]);
                    i_fc += 1;
                }
                1 => {
                    w.p.on_feed(
                        crate::events::TOPIC_SCAN,
                        &scan_payload(&scans[i_scan]),
                        now,
                    );
                    i_scan += 1;
                }
                2 => {
                    w.p.on_feed(
                        crate::events::TOPIC_POSE,
                        &pose_payload(&poses[i_pose]),
                        now,
                    );
                    i_pose += 1;
                }
                _ => {
                    w.p.on_feed(crate::events::TOPIC_OCCUPANCY, &occ[i_occ][1], now);
                    i_occ += 1;
                }
            }
        }
        if (t0 + rel) as u64 > w.now {
            let step = (t0 + rel) as u64 - w.now;
            w.run(step);
        }
        if braked.is_none() && w.set_modes(copter::BRAKE) > 0 {
            braked = Some((w.now as i64 - t0) as u64);
        }
    }
    (w, braked)
}

/// The window replayed with the pilot's log lines of every brake.
fn brakes(name: &str, mode: u32, target: Option<(u32, [f64; 3])>) -> (Option<u64>, String) {
    let ((_, braked), logs) = with_logs(|| replay(name, mode, target));
    let lines: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("pilot_auto_brake"))
        .collect();
    (braked, lines.join("\n"))
}

#[test]
fn a_mission_leg_into_the_bores_bend_flies_without_a_brake() {
    // The bore turns ahead: its far wall comes into view nearer each scan.
    let (braked, logs) = brakes("bend", copter::AUTO, Some((10, [8.300, 1.133, -0.620])));
    assert_eq!(braked, None, "{logs}");
}

#[test]
fn a_return_down_the_bore_flies_without_a_brake() {
    let (braked, logs) = brakes("return", copter::SMART_RTL, None);
    assert_eq!(braked, None, "{logs}");
}
