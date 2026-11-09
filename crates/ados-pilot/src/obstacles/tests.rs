use super::*;
use serde_json::json;

fn tile_with(occupied: &[(usize, usize)]) -> Vec<u8> {
    let mut px = vec![254u8; 64 * 64];
    for &(lx, ly) in occupied {
        px[(63 - lx) * 64 + ly] = tiles::OCCUPIED;
    }
    px
}

#[test]
fn tile_decode_undoes_every_png_filter() {
    let px = tile_with(&[(0, 0), (10, 20), (63, 63)]);
    for f in [
        png::FilterType::NoFilter,
        png::FilterType::Sub,
        png::FilterType::Up,
        png::FilterType::Avg,
        png::FilterType::Paeth,
    ] {
        let b64 = tiles::encode_for_test(&px, 64, f);
        let t = tiles::decode(&b64, 64).unwrap();
        let mut got: Vec<_> = t.occupied().collect();
        got.sort();
        assert_eq!(got, vec![(0, 0), (10, 20), (63, 63)], "filter {f:?}");
    }
}

#[test]
fn occupancy_tiles_land_in_the_local_frame_with_the_pose_offset() {
    let mut o = Obstacles::new();
    let b64 = tiles::encode_for_test(&tile_with(&[(20, 0)]), 64, png::FilterType::Paeth);
    o.ingest_occupancy(
        &json!({"res_m": 0.05, "tile_cells": 64, "tiles": [{"ix": 0, "iy": 0, "png_b64": b64}]}),
    );
    // Vehicle at map (0,0) is at local (10, 5).
    o.ingest_pose(&json!({"pose": {"t": [0.0, 0.0, -2.0]}}), Some([10.0, 5.0]));
    o.rebuild([10.0, 5.0], 0);
    // lx 20 → map north 1.025 m → local 11.025; ly 0 → east 0.025 → 5.025.
    let (i, j) = o.grid.cell([11.025, 5.025]).unwrap();
    assert!(o.grid.occupied(i, j));
    let c = o.grid.clearance([11.025 - 1.0, 5.025]).unwrap();
    assert!((c - 1.0).abs() < 0.2, "clearance {c}");
}

#[test]
fn level_scan_keeps_obstacle_and_drops_ground() {
    let l = |pitch_deg: f32| Level {
        roll: 0.0,
        pitch: pitch_deg.to_radians(),
        yaw: 0.0,
        agl: Some(4.0),
    };
    // Nose down 10° at 4 m: a 12 m forward return is 2.1 m below the vehicle,
    // 1.9 m above ground: an obstacle.
    let h = level_hit(12.0, 0.0, l(-10.0), [0.0, 0.0]).expect("kept");
    assert!((h.range_m - 12.0 * 10f32.to_radians().cos()).abs() < 1e-3);
    // Nose down 20°: the same return is 4.1 m down, i.e. the ground: dropped.
    assert!(level_hit(12.0, 0.0, l(-20.0), [0.0, 0.0]).is_none());
    // Level flight: kept at full range.
    assert!((level_hit(12.0, 0.0, l(0.0), [0.0, 0.0]).unwrap().range_m - 12.0).abs() < 1e-4);
}

#[test]
fn scan_hits_feed_rays_sectors_and_raw_minimum() {
    let mut o = Obstacles::new();
    let mut ranges = vec![0.0f64; 360];
    ranges[0] = 3.0;
    ranges[90] = 6.0;
    let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
        "angle_increment_rad": std::f64::consts::TAU / 360.0, "ranges_m": ranges});
    let level = Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(4.0),
    };
    o.ingest_scan(&scan, level, [0.0, 0.0], 1000);
    o.rebuild([0.0, 0.0], 1000);
    let d = o.ray_distance([0.0, 0.0], [1.0, 0.0], 20.0, 1000).unwrap();
    assert!((d - 3.0).abs() < 0.3, "ray {d}");
    assert_eq!(o.min_raw_m(1000), Some(3.0));
    let s = o.sectors([0.0, 0.0], 0.0, 1000);
    assert!((s[0].unwrap() - 3.0).abs() < 0.3);
    assert!((s[18].unwrap() - 6.0).abs() < 0.3);
    assert!(s[36].is_none());
    // Stale scan: hits ignored.
    assert_eq!(o.min_raw_m(1000 + SCAN_MAX_AGE_MS + 1), None);
}

#[test]
fn chamfer_clearance_grows_with_distance() {
    let mut g = Grid::default();
    g.reset([0.0, 0.0]);
    g.mark([0.1, 0.1]);
    g.transform();
    let c1 = g.clearance([1.1, 0.1]).unwrap();
    let c2 = g.clearance([3.1, 0.1]).unwrap();
    assert!(
        (c1 - 1.0).abs() < 0.1 && (c2 - 3.0).abs() < 0.1,
        "{c1} {c2}"
    );
    let mut empty = Grid::default();
    empty.reset([0.0, 0.0]);
    empty.transform();
    assert_eq!(empty.clearance([0.0, 0.0]), Some(f32::INFINITY));
}

#[test]
fn camera_projection_marks_cells_on_the_obstacle() {
    let mut o = Obstacles::new();
    let mut ranges = vec![0.0f64; 360];
    for r in ranges.iter_mut().take(20) {
        *r = 4.0;
    }
    for r in ranges.iter_mut().skip(340) {
        *r = 4.0;
    }
    let scan = json!({"frame": "body_frd", "angle_increment_rad": std::f64::consts::TAU / 360.0, "ranges_m": ranges});
    o.ingest_scan(
        &scan,
        Level {
            roll: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            agl: Some(3.0),
        },
        [0.0, 0.0],
        0,
    );
    o.rebuild([0.0, 0.0], 0);
    let cam = ados_protocol::pilot::CameraModel {
        frame_w: 1280,
        frame_h: 720,
        hfov_deg: 80.0,
        vfov_deg: 50.0,
        tilt_down_deg: 0.0,
        yaw_deg: 0.0,
        stabilised: false,
    };
    let mut scratch = Scratch::default();
    let g = project_camera(
        &o,
        &cam,
        [0.0, 0.0],
        Attitude::default(),
        3.0,
        0,
        &mut scratch,
    );
    let near = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, g.near).unwrap();
    // Centre cell (row 9, col 16) sees the wall at ~4 m.
    let d = ados_protocol::pilot::decode_distance(near[9 * 32 + 16]).unwrap();
    assert!((d - 4.0).abs() < 0.4, "{d}");
    // Far left column (az -40°) misses the ±20° wall.
    assert_eq!(near[9 * 32], 0);
}

/// A wall 10 m ahead across ±20°, seen from 3 m up.
fn wall_ahead_at_10_m() -> Obstacles {
    let mut o = Obstacles::new();
    let mut ranges = vec![0.0f64; 360];
    for (i, r) in ranges.iter_mut().enumerate() {
        if !(20..340).contains(&i) {
            *r = 10.0;
        }
    }
    let scan = json!({"frame": "body_frd", "angle_increment_rad": std::f64::consts::TAU / 360.0, "ranges_m": ranges});
    o.ingest_scan(
        &scan,
        Level {
            roll: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            agl: Some(3.0),
        },
        [0.0, 0.0],
        0,
    );
    o.rebuild([0.0, 0.0], 0);
    o
}

/// The topmost grid row marked in column `c`.
fn first_row(near: &[u8], c: usize) -> Option<usize> {
    (0..18).find(|r| near[r * 32 + c] != 0)
}

fn project(o: &Obstacles, cam: &ados_protocol::pilot::CameraModel, att: Attitude) -> Vec<u8> {
    let mut scratch = Scratch::default();
    let g = project_camera(o, cam, [0.0, 0.0], att, 3.0, 0, &mut scratch);
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, g.near).unwrap()
}

#[test]
fn camera_cells_follow_the_pinhole_and_the_airframe_pitch() {
    let o = wall_ahead_at_10_m();
    let body = ados_protocol::pilot::CameraModel {
        frame_w: 1280,
        frame_h: 720,
        hfov_deg: 80.0,
        vfov_deg: 50.0,
        tilt_down_deg: 0.0,
        yaw_deg: 0.0,
        stabilised: false,
    };
    // The wall top (3 m above the camera, 10 m out) sits 16.7° above the
    // axis: through the pinhole at v = 0.5 - tan(16.7°)/tan(25°)/2 = 0.178,
    // so row 3 is the first whose centre is below it.
    let level = project(&o, &body, Attitude::default());
    assert_eq!(first_row(&level, 16), Some(3));
    // The wall's returns span -20°..+19° (a hit counts within 0.35 m of a
    // ray, 2° at 10 m). Through the pinhole column 22's centre looks 18.8°
    // right and column 23's 21.5°, column 9's 18.8° left and column 7's 24°:
    // the wall covers 22 and 9 but not 23 or 7. Uniform angle steps would put
    // 18.75° in column 23 and 21.25° in column 7, both on the wall.
    for (c, on) in [(7, false), (9, true), (22, true), (23, false)] {
        assert_eq!(level[12 * 32 + c] != 0, on, "column {c}");
    }
    // Nose up 10°: the wall top is 6.7° above the axis, v = 0.374, row 7.
    let nose_up = Attitude {
        pitch: 10f32.to_radians(),
        ..Attitude::default()
    };
    assert_eq!(first_row(&project(&o, &body, nose_up), 16), Some(7));
    // A stabilised gimbal holds the horizon whatever the airframe pitch.
    let gimbal = ados_protocol::pilot::CameraModel {
        stabilised: true,
        ..body
    };
    assert_eq!(first_row(&project(&o, &gimbal, nose_up), 16), Some(3));
}

#[test]
fn a_rolled_airframe_tilts_the_wall_edge_across_the_frame() {
    let o = wall_ahead_at_10_m();
    let body = ados_protocol::pilot::CameraModel {
        frame_w: 1280,
        frame_h: 720,
        hfov_deg: 80.0,
        vfov_deg: 50.0,
        tilt_down_deg: 0.0,
        yaw_deg: 0.0,
        stabilised: false,
    };
    // Columns 12 and 19 look 10.4° left and right of the nose, well inside
    // the wall's -20°..+19° span, so a roll that also swings the upper rays
    // sideways keeps both on the wall.
    let (l, r) = (12, 19);
    let level = project(&o, &body, Attitude::default());
    assert_eq!(first_row(&level, l), first_row(&level, r));
    // Right wing down 15°: the horizon turns in the image, so the wall's top
    // edge sits lower on the left of the frame than on the right.
    let rolled = project(
        &o,
        &body,
        Attitude {
            roll: 15f32.to_radians(),
            ..Attitude::default()
        },
    );
    let (left, right) = (
        first_row(&rolled, l).unwrap(),
        first_row(&rolled, r).unwrap(),
    );
    assert!(left > right, "left {left} right {right}");
}
