//! Returns over the flight slice: the brake's own nose-up pitch lifts a
//! tall obstacle ahead over the slice, and every safety reader must still
//! see it. Only the route check sets such returns apart (`route_hold`'s
//! roof tests).

use std::f32::consts::TAU;

use serde_json::{json, Value};

use crate::obstacles::{Level, Obstacles};

fn f32s(v: &Value) -> Vec<f32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
        .collect()
}

/// The recorded open-air approach of a 0.6 m column at full stick, braking
/// from 2 m/s: from 3219 ms the brake pitches the vehicle 2.4–10.8° nose up,
/// which levels the column's returns 3.9 → 2.7 m ahead more than 0.13 m over
/// the scan plane. Replayed scan by scan with the attitude, AGL and
/// position of its moment, the column stays an obstacle (the hard stop and
/// the push read it) and its closing speed stays measured.
#[test]
fn a_column_lifted_over_the_slice_by_a_braking_pitch_stays_an_obstacle() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/column_full_stick.json")).unwrap();
    let mut o = Obstacles::new();
    let mut braking = 0;
    for f in fx["frames"].as_array().unwrap() {
        let num = |k: &str| f[k].as_f64().unwrap() as f32;
        let t = f["t_ms"].as_u64().unwrap();
        let p = f32s(&f["pos_ne"]);
        let now = 1000 + t;
        let level = Level {
            roll: num("roll"),
            pitch: num("pitch"),
            yaw: num("yaw"),
            agl: Some(num("agl_m")),
        };
        let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                          "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
        o.ingest_scan(&scan, level, [p[0], p[1]], now);
        o.rebuild([p[0], p[1]], now);
        if !(3219..=4087).contains(&t) {
            continue;
        }
        braking += 1;
        // The column: the nearest return within 30° of the nose.
        let column = f32s(&f["ranges_m"])
            .iter()
            .enumerate()
            .filter(|(i, r)| (*i < 30 || *i > 330) && **r > 0.05)
            .map(|(_, r)| *r * level.pitch.cos())
            .reduce(f32::min)
            .unwrap();
        let near = o.nearest_hit_m();
        assert!(
            near.is_some_and(|n| (n - column).abs() < 0.1),
            "t {t} ms, pitch {:.1}°: the column {column:.2} m ahead is no obstacle (nearest {near:?})",
            level.pitch.to_degrees()
        );
        let lifted = o.hits(now).iter().any(|h| h.overhead);
        assert!(lifted, "t {t} ms: the column was not over the slice");
        let closing = o.scan_closing([1.0, 0.0], now);
        assert!(
            closing > 0.3,
            "t {t} ms: closing toward the column {closing:.2} m/s"
        );
    }
    assert_eq!(braking, 10, "the scans 3219–4087 ms");
}
