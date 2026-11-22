use serde_json::json;

use super::*;
use crate::obstacles::Hit;

/// The movers `m` holds at `now_ms`, collected.
fn list(m: &Movers, now_ms: u64) -> Vec<Mover> {
    m.list(now_ms).copied().collect()
}

/// A planar scan from `pos` of upright disks (`(centre, radius)`): 360 bins
/// of 1°, each the nearest disk within 12 m as a hit, else a horizon point
/// at 12 m.
fn scan(pos: V2, disks: &[(V2, f32)]) -> (Vec<Hit>, Vec<V2>) {
    let (mut hits, mut horizon) = (Vec::new(), Vec::new());
    for k in 0..360 {
        let b = (k as f32).to_radians();
        let u = geo::bearing_unit(b);
        let range = disks
            .iter()
            .filter_map(|&(c, r)| {
                let rel = geo::sub(c, pos);
                let t = geo::dot(u, rel);
                let h2 = r * r - (geo::dot(rel, rel) - t * t);
                (h2 >= 0.0 && t > 0.0).then(|| t - h2.sqrt())
            })
            .fold(f32::INFINITY, f32::min);
        if range <= 12.0 {
            hits.push(Hit {
                body_bearing: b,
                range_m: range,
                ne: geo::add(pos, geo::scale(u, range)),
                overhead: false,
                lift_m: 0.0,
            });
        } else {
            horizon.push(geo::add(pos, geo::scale(u, 12.0)));
        }
    }
    (hits, horizon)
}

#[test]
fn a_camera_track_is_a_mover_of_its_class_predicted_on_its_velocity() {
    let mut m = Movers::default();
    let batch = |e: f32| {
        json!({"v": 4, "model_id": "person", "camera_id": "front", "detections": [
            {"class_label": "person", "confidence": 0.9, "track_id": 7,
             "track_state": "confirmed", "world_pos": [e, 10.0, 0.0]},
            {"class_label": "car", "confidence": 0.9, "track_id": 8,
             "track_state": "tentative", "world_pos": [0.0, 0.0, 0.0]},
            {"class_label": "person", "confidence": 0.9, "track_id": 9}
        ]})
    };
    for k in 0..=10u64 {
        m.ingest_detections(&batch(0.14 * k as f32), None, 1000 + 100 * k);
    }
    let l = list(&m, 2000);
    assert_eq!(l.len(), 1, "{l:?}");
    let p = l[0];
    assert_eq!(p.kind, MoverKind::Person);
    assert_eq!(p.source, MoverSource::Vision);
    // ENU east is NED east: [north 10, east 1.4].
    assert!(geo::dist(p.ne, [10.0, 1.4]) < 1e-3);
    assert!(
        (p.vel[1] - 1.4).abs() < 0.3 && p.vel[0].abs() < 0.1,
        "{:?}",
        p.vel
    );
    // Predicted 1 s on, held after PREDICT_S.
    assert!((p.at(3000)[1] - (1.4 + p.vel[1])).abs() < 1e-3);
    assert_eq!(p.at(2000 + 10_000), p.at(2000 + 3_000));
    assert!(p.sigma_at(3000) > p.sigma_m);
    // Unmeasured past the keep, it is gone.
    assert!(list(&m, 2000 + KEEP_MS + 1).is_empty());
}

#[test]
fn a_walker_crossing_the_scan_becomes_a_mover_and_a_passed_trunk_does_not() {
    let mut m = Movers::default();
    for k in 0..30u64 {
        let t = 1000 + 100 * k;
        let s = k as f32 * 0.1;
        // The vehicle flies north at 3 m/s past a trunk at (8, 1.2); a
        // walker crosses at north 12 from the west at 1.4 m/s.
        let pos = [3.0 * s, 0.0];
        let (hits, horizon) = scan(pos, &[([8.0, 1.2], 0.3), ([12.0, -4.0 + 1.4 * s], 0.3)]);
        m.ingest_hits(&hits, &horizon, pos, Some(1.5), t);
    }
    let l = list(&m, 3900);
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(l[0].kind, MoverKind::Unknown);
    assert!(
        geo::dist(l[0].ne, [12.0, -4.0 + 1.4 * 2.9]) < 0.4,
        "{:?}",
        l[0].ne
    );
    assert!((l[0].vel[1] - 1.4).abs() < 0.4, "{:?}", l[0].vel);
}

#[test]
fn a_walker_turning_back_stays_a_mover() {
    let mut m = Movers::default();
    for k in 0..60u64 {
        let s = k as f32 * 0.1;
        // Out 4.2 m east and back, 8 m ahead of a hovering vehicle.
        let e = if s < 3.0 { 1.4 * s } else { 1.4 * (6.0 - s) };
        let (hits, horizon) = scan([0.0, 0.0], &[([8.0, e - 2.0], 0.3)]);
        m.ingest_hits(&hits, &horizon, [0.0, 0.0], Some(1.5), 1000 + 100 * k);
        if k >= 15 {
            assert_eq!(list(&m, 1000 + 100 * k).len(), 1, "at {s:.1} s");
        }
    }
}

#[test]
fn a_walker_coming_straight_at_the_sensor_becomes_a_mover() {
    // The place they were first seen at hides behind them all the way.
    let mut m = Movers::default();
    for k in 0..20u64 {
        let s = k as f32 * 0.1;
        let (hits, horizon) = scan([0.0, 0.0], &[([0.3, 8.0 - 1.4 * s], 0.3)]);
        m.ingest_hits(&hits, &horizon, [0.0, 0.0], Some(1.5), 1000 + 100 * k);
    }
    let l = list(&m, 2900);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!((l[0].vel[1] + 1.4).abs() < 0.4, "{:?}", l[0].vel);
}

#[test]
fn walls_and_a_corridor_of_posts_are_no_movers() {
    let mut m = Movers::default();
    // A corridor 6 m wide of touching 0.3 m posts, flown up its middle at
    // 1.4 m/s: the stretch of each wall inside the scan's 12 m reach
    // slides north with the vehicle.
    let posts: Vec<(V2, f32)> = (0..140)
        .flat_map(|i| {
            let n = i as f32 * 0.6;
            [([n, -3.0], 0.3), ([n, 3.0], 0.3)]
        })
        .collect();
    for k in 0..100u64 {
        let pos = [0.14 * k as f32, 0.0];
        let (hits, horizon) = scan(pos, &posts);
        m.ingest_hits(&hits, &horizon, pos, None, 1000 + 100 * k);
        assert!(
            list(&m, 1000 + 100 * k).is_empty(),
            "{:?}",
            list(&m, 1000 + 100 * k)
        );
    }
}

#[test]
fn a_cluster_higher_than_anyone_walking_is_no_mover() {
    // The same walker's track, seen by a scan 4 m over the ground leaning
    // up into a crown: returns 3 m over the ground are leaves, never a
    // person, however they slide.
    let mut m = Movers::default();
    for k in 0..30u64 {
        let s = k as f32 * 0.1;
        let (mut hits, horizon) = scan([0.0, 0.0], &[([12.0, -4.0 + 1.4 * s], 0.3)]);
        for h in &mut hits {
            h.lift_m = -1.0;
        }
        m.ingest_hits(&hits, &horizon, [0.0, 0.0], Some(4.0), 1000 + 100 * k);
    }
    assert!(list(&m, 3900).is_empty(), "{:?}", list(&m, 3900));
}

#[test]
fn the_subject_is_never_guarded() {
    let mut m = Movers::default();
    let batch = |e: f32| {
        json!({"v": 4, "model_id": "person", "camera_id": "front", "detections": [
            {"class_label": "person", "track_id": 7, "track_state": "confirmed",
             "world_pos": [e, 10.0, 0.0]},
            {"class_label": "person", "track_id": 8, "track_state": "confirmed",
             "world_pos": [-e, 6.5, 0.0]}
        ]})
    };
    for k in 0..=5u64 {
        m.ingest_detections(&batch(0.14 * k as f32), None, 1000 + 100 * k);
    }
    let ids = |m: &Movers| {
        let mut v: Vec<_> = m.guarded(1500).map(|x| x.ne[0]).collect();
        v.sort_by(f32::total_cmp);
        v
    };
    assert_eq!(ids(&m), vec![6.5, 10.0]);
    // Named by its track: only that track is left out, wherever the
    // goal's subject point lies.
    m.flight.subject_track = Some(7);
    m.flight.subject = Some([6.5, -0.7]);
    assert_eq!(ids(&m), vec![6.5]);
    // Unnamed: whatever lies within 1 m of the goal's subject point.
    m.flight.subject_track = None;
    assert_eq!(ids(&m), vec![10.0]);
}

#[test]
fn the_subject_walking_in_the_scan_is_never_guarded_while_its_track_is_named() {
    // The subject crossing the scan, no camera track for it: a goal naming
    // its track still leaves its cluster out near the goal's subject point.
    let mut m = Movers::default();
    for k in 0..30u64 {
        let t = 1000 + 100 * k;
        let s = k as f32 * 0.1;
        let pos = [3.0 * s, 0.0];
        let (hits, horizon) = scan(pos, &[([12.0, -4.0 + 1.4 * s], 0.3)]);
        m.ingest_hits(&hits, &horizon, pos, Some(1.5), t);
    }
    let walker = [12.0, -4.0 + 1.4 * 2.9];
    assert_eq!(m.guarded(3900).count(), 1, "the walker is a mover");
    m.flight.subject_track = Some(7);
    m.flight.subject = Some([walker[0] + 0.3, walker[1] - 0.3]);
    assert_eq!(m.guarded(3900).count(), 0, "the subject's own cluster");
    // A stranger's cluster, away from the subject point, is guarded.
    m.flight.subject = Some([walker[0] + 4.0, walker[1]]);
    assert_eq!(m.guarded(3900).count(), 1, "a stranger");
}

#[test]
fn the_sweep_marks_the_grid_and_leaves_the_vehicle_its_margin() {
    let mut m = Movers::default();
    m.flight.margin = 1.0;
    m.tracks.push(Track {
        m: Mover {
            id: 1,
            kind: MoverKind::Person,
            source: MoverSource::Vision,
            ne: [6.0, -4.0],
            vel: [0.0, 1.4],
            sigma_m: 0.3,
            seen_ms: 1000,
        },
        ext: Some((1, 1)),
        first: [6.0, -4.0],
        born_ms: 1000,
        moving: true,
        subject: false,
    });
    let mut g = Grid::default();
    g.reset([0.0, 0.0]);
    m.mark(&mut g, [0.0, 0.0], 1000);
    g.transform();
    // Now and 2 s on, the person's place is occupied.
    for p in [[6.0, -4.0], [6.0, -1.2]] {
        assert_eq!(g.clearance(p), Some(0.0), "{p:?}");
    }
    // The vehicle keeps its margin and a cell.
    assert!(g.clearance([0.0, 0.0]).unwrap() >= 1.0);
    // Over their heads, nothing is guarded or marked.
    m.flight.over_head = true;
    assert_eq!(m.guarded(1000).count(), 0);
}

/// The pixel a point `rel` (NED from the camera) falls on, for a level
/// vehicle facing north and a 1280 x 720 camera of 87° pitched 15° down.
fn pixel(rel: [f32; 3]) -> [f32; 2] {
    let f = 640.0 / (87.0f32.to_radians() / 2.0).tan();
    let (sp, cp) = (-15.0f32).to_radians().sin_cos();
    let c = [cp * rel[0] - sp * rel[2], rel[1], sp * rel[0] + cp * rel[2]];
    [640.0 + f * c[1] / c[0], 360.0 + f * c[2] / c[0]]
}

/// A batch from that camera of one track `label` boxed from `head` to
/// `feet` (NED from the camera), its bottom cut at the frame.
fn box_batch(label: &str, head: [f32; 3], feet: [f32; 3]) -> serde_json::Value {
    let (h, f) = (pixel(head), pixel(feet));
    let bottom = f[1].min(720.0);
    json!({"v": 4, "model_id": "person", "camera_id": "main", "hfov_deg": 87.0,
        "mount_pitch_deg": -15.0,
        "frame_width": 1280, "frame_height": 720, "detections": [
        {"class_label": label, "confidence": 0.8, "track_id": 3, "track_state": "confirmed",
         "bbox": {"x": h[0] - 20.0, "y": h[1], "width": 40.0, "height": bottom - h[1]}}]})
}

#[test]
fn a_person_box_without_a_world_position_is_placed_from_the_camera() {
    let from = Some(CameraFrom {
        ned: [0.0, 0.0, -4.0],
        att: [0.0, 0.0, 0.0],
        agl: 4.0,
    });
    let at = |label: &str, head: [f32; 3], feet: [f32; 3]| {
        let mut m = Movers::default();
        m.ingest_detections(&box_batch(label, head, feet), from, 1000);
        list(&m, 1000).first().map(|p| (p.ne, p.sigma_m))
    };
    // A person 8 m ahead, 3 m right: both edges in the frame.
    let (ne, sigma) = at("person", [8.0, 3.0, 2.25], [8.0, 3.0, 4.0]).expect("placed");
    assert!(geo::dist(ne, [8.0, 3.0]) < 0.05, "{ne:?}");
    assert!(sigma > 1.0, "{sigma}");
    // Close below, feet under the frame: the head edge alone places them.
    let (ne, _) = at("person", [2.5, 0.0, 2.25], [2.5, 0.0, 4.0]).expect("placed");
    assert!(geo::dist(ne, [2.5, 0.0]) < 0.05, "{ne:?}");
    // A trunk read as a person (its top 4 m up) places nothing, nor does a
    // car's box, nor a batch from no known pose.
    assert!(at("person", [6.0, 0.0, 0.0], [6.0, 0.0, 4.0]).is_none());
    assert!(at("car", [8.0, 3.0, 2.25], [8.0, 3.0, 4.0]).is_none());
    let mut m = Movers::default();
    m.ingest_detections(
        &box_batch("person", [8.0, 3.0, 2.25], [8.0, 3.0, 4.0]),
        None,
        1000,
    );
    assert!(list(&m, 1000).is_empty());
}

#[test]
fn a_camera_lower_than_a_head_places_a_box_on_its_foot_edge() {
    let from = Some(CameraFrom {
        ned: [0.0, 0.0, -1.5],
        att: [0.0, 0.0, 0.0],
        agl: 1.5,
    });
    let mut m = Movers::default();
    m.ingest_detections(
        &box_batch("person", [6.0, 0.0, -0.25], [6.0, 0.0, 1.5]),
        from,
        1000,
    );
    let p = list(&m, 1000)[0];
    assert!(geo::dist(p.ne, [6.0, 0.0]) < 0.05, "{:?}", p.ne);
    assert!(p.sigma_m > 1.5, "{}", p.sigma_m);
}

#[test]
fn a_ranged_box_near_the_subject_within_its_error_is_the_subject() {
    let from = Some(CameraFrom {
        ned: [0.0, 0.0, -4.0],
        att: [0.0, 0.0, 0.0],
        agl: 4.0,
    });
    // The skill's subject 1.8 m beyond the person the pilot boxed at 8 m
    // (its own range read long): further than SUBJECT_SAME_M, within twice
    // the box's error.
    let guarded = |world: bool| {
        let mut m = Movers::default();
        let mut b = box_batch("person", [8.0, 3.0, 2.25], [8.0, 3.0, 4.0]);
        if world {
            b["detections"][0]["world_pos"] = json!([3.0, 8.0, 0.0]);
        }
        m.ingest_detections(&b, from, 1000);
        m.flight.subject = Some([9.8, 3.0]);
        m.guarded(1000).count()
    };
    assert_eq!(guarded(false), 0);
    // A track placed to 0.3 m is another person that far off.
    assert_eq!(guarded(true), 1);
}
