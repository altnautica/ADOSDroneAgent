//! Two recorded follows under a conifer canopy in simulation, every scan
//! replayed into one obstacle model with the attitude, height and position
//! of its moment (fixtures `canopy_walk.json`, `canopy_run.json`, the truth
//! labels from the simulated place). The copter leant up to 20° braking and
//! pushing, and its fixed lidar returned from trunks lifted over the swept
//! band. No return that truly hit a trunk may stop being a wall at flight
//! height, however the lean lifted it. (Those crowns hung into the band at
//! the 4 m flight, so their leaves stay walls too; the simulated canopy
//! the follow flies under is the DST `forest_canopy`.)

use std::f32::consts::TAU;

use serde_json::{json, Value};

use crate::geo;
use crate::obstacles::{level_hit, Level, Obstacles, BAND_TOP_M, SCAN_CLOSING_MAX_MPS};

/// A wall return found this near the true return's levelled spot is the
/// same surface, m (bins are 1° apart: 5 cm at 3 m).
const SAME_M: f32 = 0.1;

#[derive(Default, Debug)]
struct Replay {
    trunk: usize,
    /// Trunk returns the lean lifted over the swept band.
    trunk_over: usize,
    trunk_lost: Vec<(u64, usize)>,
    crown_over: usize,
    crown_over_walls: usize,
    crown_in_band: usize,
}

fn replay(fx: &str) -> Replay {
    let fx: Value = serde_json::from_str(fx).unwrap();
    let mut o = Obstacles::new();
    let mut r = Replay::default();
    for f in fx["frames"].as_array().unwrap() {
        let num = |k: &str| f[k].as_f64().unwrap() as f32;
        let now = 1_000 + f["t_ms"].as_u64().unwrap();
        let pos = [
            f["pos_ne"][0].as_f64().unwrap() as f32,
            f["pos_ne"][1].as_f64().unwrap() as f32,
        ];
        let level = Level {
            roll: num("roll"),
            pitch: num("pitch"),
            yaw: num("yaw"),
            agl: Some(num("agl_m")),
        };
        let mut ranges = vec![0.0f32; 360];
        for h in f["hits"].as_array().unwrap() {
            ranges[h[0].as_u64().unwrap() as usize] = h[1].as_f64().unwrap() as f32 / 100.0;
        }
        let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                          "angle_increment_rad": TAU / 360.0, "ranges_m": ranges});
        o.ingest_scan(&scan, level, pos, SCAN_CLOSING_MAX_MPS, now);
        let walls = o.hits(now);
        let wall_at = |p: [f32; 2]| walls.iter().any(|w| geo::dist(w.ne, p) <= SAME_M);
        let spot = |k: usize| level_hit(ranges[k], (k as f32).to_radians(), level, pos);
        for k in f["trunk"].as_array().unwrap() {
            let k = k.as_u64().unwrap() as usize;
            let Some(h) = spot(k) else { continue };
            r.trunk += 1;
            r.trunk_over += usize::from(h.lift_m > BAND_TOP_M);
            if !wall_at(h.ne) {
                r.trunk_lost.push((now, k));
            }
        }
        for k in f["crown"].as_array().unwrap() {
            let k = k.as_u64().unwrap() as usize;
            let Some(h) = spot(k) else { continue };
            if h.lift_m > BAND_TOP_M {
                r.crown_over += 1;
                r.crown_over_walls += usize::from(wall_at(h.ne));
            } else {
                r.crown_in_band += 1;
            }
        }
    }
    r
}

fn check(name: &str, fx: &str, least_trunks: usize, least_lifted: usize) {
    let r = replay(fx);
    eprintln!(
        "{name}: {} trunk returns ({} over the band), {} lost; {} crown returns over the band ({} walls), {} in it",
        r.trunk,
        r.trunk_over,
        r.trunk_lost.len(),
        r.crown_over,
        r.crown_over_walls,
        r.crown_in_band
    );
    assert!(r.trunk >= least_trunks, "{name}: {} trunk returns", r.trunk);
    assert!(
        r.trunk_over >= least_lifted,
        "{name}: {} trunk returns over the band",
        r.trunk_over
    );
    assert!(
        r.trunk_lost.is_empty(),
        "{name}: trunk returns not walls (ms, bin): {:?}",
        r.trunk_lost
    );
}

#[test]
fn a_leaning_scan_under_a_canopy_keeps_every_trunk_a_wall_walking() {
    check("walk", include_str!("fixtures/canopy_walk.json"), 500, 0);
}

#[test]
fn a_leaning_scan_under_a_canopy_keeps_every_trunk_a_wall_running() {
    check("run", include_str!("fixtures/canopy_run.json"), 200, 1);
}
