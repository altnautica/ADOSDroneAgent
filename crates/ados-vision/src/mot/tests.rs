use ados_protocol::framebus::TrackState;

use super::scene_tests::{person, run, score, Scene, Walker};
use super::*;

fn ids_of(
    ids: &[Vec<Option<u32>>],
    frames: &[super::scene_tests::Frame],
    walker: usize,
) -> Vec<u32> {
    let mut out: Vec<u32> = frames
        .iter()
        .zip(ids)
        .flat_map(|(f, fi)| {
            f.truth
                .iter()
                .zip(fi)
                .filter(move |(gt, _)| **gt == walker)
                .filter_map(|(_, id)| *id)
        })
        .collect();
    out.dedup();
    out
}

#[test]
fn a_track_is_born_after_three_hits_with_a_monotonic_id() {
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    for k in 0..2 {
        mot.update(
            &[person(100.0 + k as f32, 100.0, 40.0, 100.0, 0.9)],
            &[],
            None,
        );
        let out = mot.output(0).unwrap();
        assert_eq!((out.track_id, out.state), (None, TrackState::Tentative));
    }
    mot.update(&[person(102.0, 100.0, 40.0, 100.0, 0.9)], &[], None);
    assert_eq!(mot.output(0).unwrap().track_id, Some(1));
    // A one-frame blob never gets an id, and the next real track gets id 2.
    mot.update(
        &[
            person(103.0, 100.0, 40.0, 100.0, 0.9),
            person(600.0, 100.0, 40.0, 100.0, 0.9),
        ],
        &[],
        None,
    );
    assert_eq!(mot.output(1).unwrap().track_id, None);
    for _ in 0..3 {
        mot.update(&[person(500.0, 300.0, 40.0, 100.0, 0.9)], &[], None);
    }
    assert_eq!(mot.output(0).unwrap().track_id, Some(2));
    assert_eq!(
        mot.tracks().count(),
        2,
        "the first track is still held, lost"
    );
}

#[test]
fn a_lost_track_is_removed_after_max_age() {
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let ms = |k: u64| Some(k * 100_000_000);
    for k in 0..3 {
        mot.update(&[person(100.0, 100.0, 40.0, 100.0, 0.9)], &[], ms(k));
    }
    // Last seen at 0.2 s: lost from 0.3 s, still held at 1.7 s (1.5 s unseen).
    for k in 3..18 {
        mot.update(&[], &[], ms(k));
        assert_eq!(
            mot.tracks().next().map(|t| t.state),
            Some(TrackState::Lost),
            "k={k}"
        );
    }
    mot.update(&[], &[], ms(18));
    assert_eq!(mot.tracks().count(), 0);
}

#[test]
fn two_crossing_walkers_keep_their_ids_through_a_half_second_occlusion() {
    // The near walker (larger, lower) passes in front of the far one, which
    // the detector misses for 0.5 s around the crossing.
    let near = Walker::new(400.0, 400.0, 80.0, 60.0, 150.0);
    let mut far = Walker::new(720.0, 380.0, -80.0, 44.0, 110.0);
    far.hidden.push((1.75, 2.25));
    let scene = Scene {
        jitter_px: 1.0,
        ..Scene::new(vec![near, far])
    };
    let frames = scene.frames(4.0, 7);
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let ids = run(&mut mot, &frames, false);
    let (a, b) = (ids_of(&ids, &frames, 0), ids_of(&ids, &frames, 1));
    assert_eq!(a.len(), 1, "near walker ids {a:?}");
    assert_eq!(b.len(), 1, "far walker ids {b:?}");
    assert_ne!(a, b);
    assert_eq!(score(&frames, &ids).id_switches, 0);
}

#[test]
fn a_partly_hidden_walker_keeps_its_track_on_low_score_boxes() {
    let mut w = Walker::new(300.0, 400.0, 60.0, 50.0, 130.0);
    w.partial.push((1.0, 2.0));
    let frames = Scene::new(vec![w]).frames(3.0, 1);
    let partial: Vec<usize> = (0..frames.len())
        .filter(|&k| frames[k].dets.first().is_some_and(|d| d.confidence < 0.5))
        .collect();
    assert_eq!(partial.len(), 10);

    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let ids = run(&mut mot, &frames, false);
    let id = ids[5][0].expect("confirmed before the occlusion");
    for &k in &partial {
        assert_eq!(
            ids[k][0],
            Some(id),
            "frame {k} keeps the id on a low score box"
        );
    }
    assert_eq!(ids_of(&ids, &frames, 0), vec![id]);

    // Without the low-score pass the same boxes are dropped and the track
    // goes lost: the second pass is what holds it.
    let cfg = MotConfig {
        low_score: 0.5,
        ..MotConfig::default()
    };
    let mut plain = MultiObjectTracker::new(cfg);
    let ids = run(&mut plain, &frames, false);
    assert!(partial.iter().all(|&k| ids[k][0].is_none()));
}

/// Image shift of a camera panning at 30 deg/s, turning back every second, for
/// a 90 deg lens 1280 px wide (focal length 640 px).
fn pan_30_deg_s(t: f64) -> f32 {
    let rate = 640.0 * 30f64.to_radians();
    let phase = t % 2.0;
    let x = if phase < 1.0 { phase } else { 2.0 - phase };
    (rate * (x - 0.5)) as f32
}

#[test]
fn ego_motion_warp_keeps_ids_through_a_30_deg_s_pan() {
    let walkers = vec![
        Walker::new(600.0, 400.0, 20.0, 50.0, 120.0),
        Walker::new(670.0, 400.0, 20.0, 50.0, 120.0),
    ];
    let scene = Scene {
        dt: 0.2,
        pan: pan_30_deg_s,
        jitter_px: 1.0,
        ..Scene::new(walkers)
    };
    let frames = scene.frames(6.0, 3);
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let ids = run(&mut mot, &frames, true);
    let m = score(&frames, &ids);
    assert_eq!(m.id_switches, 0, "with the warp: {m:?}");
    assert_eq!(ids_of(&ids, &frames, 0).len(), 1);
    assert_eq!(ids_of(&ids, &frames, 1).len(), 1);

    // The same frames without the warp lose or swap ids at the turn-backs:
    // the scene is hard enough to need it.
    let mut blind = MultiObjectTracker::new(MotConfig::default());
    let ids = run(&mut blind, &frames, false);
    let m = score(&frames, &ids);
    let distinct = ids_of(&ids, &frames, 0).len() + ids_of(&ids, &frames, 1).len();
    assert!(m.id_switches > 0 || distinct > 2, "without the warp: {m:?}");
}

#[test]
fn homography_maps_points_and_rejects_infinity() {
    let h = Homography::translation(5.0, -2.0);
    assert_eq!(h.map(1.0, 1.0), Some((6.0, -1.0)));
    assert_eq!(Homography::IDENTITY.map(3.0, 4.0), Some((3.0, 4.0)));
    let degenerate = Homography([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(degenerate.map(1.0, 1.0), None);
}

#[test]
fn classes_never_share_a_track() {
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    for _ in 0..3 {
        mot.update(&[person(100.0, 100.0, 40.0, 100.0, 0.9)], &[], None);
    }
    let mut car = person(100.0, 100.0, 40.0, 100.0, 0.9);
    car.class_label = "car".into();
    mot.update(&[car], &[], None);
    assert_eq!(mot.output(0).unwrap().state, TrackState::Tentative);
}

#[test]
fn world_positions_are_filtered_per_track() {
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    for k in 0..10 {
        let mut d = person(100.0, 100.0, 40.0, 100.0, 0.9);
        d.world_pos = Some([k as f64 * 0.1, 2.0, 0.0]);
        mot.update(&[d], &[], None);
    }
    let (pos, vel) = mot.tracks().next().unwrap().world.unwrap();
    assert!((pos[0] - 0.9).abs() < 0.1, "x {}", pos[0]);
    assert!((vel[0] - 1.0).abs() < 0.3, "vx {}", vel[0]);
}

#[test]
fn bind_moves_an_id_onto_another_detection_and_keeps_ids_unique() {
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let two = [
        person(100.0, 100.0, 40.0, 100.0, 0.9),
        person(400.0, 100.0, 40.0, 100.0, 0.9),
    ];
    for _ in 0..3 {
        mot.update(&two, &[], None);
    }
    let (a, b) = (
        mot.output(0).unwrap().track_id.unwrap(),
        mot.output(1).unwrap().track_id.unwrap(),
    );
    mot.bind(1, a);
    assert_eq!(mot.output(1).unwrap().track_id, Some(a));
    assert_eq!(mot.output(0).unwrap().track_id, Some(b));
    // A detection no track took gets a confirmed track on demand.
    mot.update(&[person(800.0, 100.0, 40.0, 100.0, 0.3)], &[], None);
    let id = mot.confirm_at(0).unwrap();
    assert!(id > b);
    assert_eq!(mot.output(0).unwrap().state, TrackState::Confirmed);
}
