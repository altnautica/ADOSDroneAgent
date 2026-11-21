//! A deterministic identity benchmark on synthetic sequences, and the
//! per-frame cost with 30 detections.

use std::time::Instant;

use super::scene_tests::{run, score, IdMetrics, Rng, Scene, Walker};
use super::*;

fn crossing() -> Scene {
    let mut far = Walker::new(720.0, 380.0, -80.0, 44.0, 110.0);
    far.hidden.push((1.75, 2.25));
    Scene {
        jitter_px: 1.0,
        ..Scene::new(vec![Walker::new(400.0, 400.0, 80.0, 60.0, 150.0), far])
    }
}

/// Ten people walking both ways at different depths, hiding each other where
/// they pass, with detector jitter and a 3 % miss rate.
fn crowd(seed: u64) -> Scene {
    let mut rng = Rng::new(seed);
    let walkers = (0..10)
        .map(|_| {
            let y = rng.range(280.0, 520.0);
            let h = 0.35 * y;
            let dir = if rng.unit() < 0.5 { -1.0 } else { 1.0 };
            Walker::new(
                rng.range(150.0, 1130.0),
                y,
                dir * rng.range(15.0, 50.0),
                0.4 * h,
                h,
            )
        })
        .collect();
    Scene {
        jitter_px: 1.5,
        miss_rate: 0.03,
        occlusion: true,
        ..Scene::new(walkers)
    }
}

fn pan(t: f64) -> f32 {
    let rate = 640.0 * 30f64.to_radians();
    let phase = t % 2.0;
    let x = if phase < 1.0 { phase } else { 2.0 - phase };
    (rate * (x - 0.5)) as f32
}

fn panning_pair() -> Scene {
    Scene {
        dt: 0.2,
        pan,
        jitter_px: 1.0,
        ..Scene::new(vec![
            Walker::new(600.0, 400.0, 20.0, 50.0, 120.0),
            Walker::new(670.0, 400.0, 20.0, 50.0, 120.0),
        ])
    }
}

fn bench(scene: &Scene, duration: f64, seed: u64, warp: bool) -> IdMetrics {
    let frames = scene.frames(duration, seed);
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let ids = run(&mut mot, &frames, warp);
    score(&frames, &ids)
}

#[test]
fn identity_benchmark_on_synthetic_sequences() {
    let rows = [
        (
            "crossing, 0.5 s occlusion",
            bench(&crossing(), 4.0, 7, false),
        ),
        ("crowd of 10, seed 1", bench(&crowd(1), 12.0, 1, false)),
        ("crowd of 10, seed 2", bench(&crowd(2), 12.0, 2, false)),
        ("crowd of 10, seed 3", bench(&crowd(3), 12.0, 3, false)),
        ("pan 30 deg/s, warped", bench(&panning_pair(), 6.0, 3, true)),
        (
            "pan 30 deg/s, not warped",
            bench(&panning_pair(), 6.0, 3, false),
        ),
    ];
    println!(
        "{:<28} {:>8} {:>6} {:>6}",
        "sequence", "gt dets", "IDSW", "IDF1"
    );
    for (name, m) in &rows {
        println!(
            "{name:<28} {:>8} {:>6} {:>6.3}",
            m.gt_dets, m.id_switches, m.idf1
        );
    }
    let get = |i: usize| rows[i].1;
    assert_eq!(get(0).id_switches, 0);
    assert!(get(0).idf1 > 0.9);
    for (name, m) in &rows[1..4] {
        assert!(m.id_switches <= 1, "{name}: {m:?}");
        assert!(m.idf1 >= 0.95, "{name}: {m:?}");
    }
    assert_eq!(get(4).id_switches, 0);
    assert!(get(4).idf1 > get(5).idf1, "the warp is what keeps the ids");
}

#[test]
fn the_same_input_gives_the_same_ids() {
    let frames = crowd(5).frames(6.0, 5);
    let a = run(
        &mut MultiObjectTracker::new(MotConfig::default()),
        &frames,
        false,
    );
    let b = run(
        &mut MultiObjectTracker::new(MotConfig::default()),
        &frames,
        false,
    );
    assert_eq!(a, b);
}

#[test]
fn per_frame_cost_with_30_detections() {
    // A 6 x 5 grid of people drifting with jitter: 30 detections every frame,
    // every one with a descriptor, and a designated subject, so the
    // re-identification path runs too.
    let mut rng = Rng::new(9);
    let mut frames = Vec::new();
    for k in 0..400u64 {
        let t = k as f32 * 0.1;
        let mut dets = Vec::new();
        let mut feats = Vec::new();
        for i in 0..30 {
            let (col, row) = ((i % 6) as f32, (i / 6) as f32);
            let x = 100.0 + 200.0 * col + 10.0 * (t + i as f32).sin() + rng.range(-1.0, 1.0);
            let y = 100.0 + 120.0 * row + rng.range(-1.0, 1.0);
            dets.push(super::scene_tests::person(x, y, 36.0, 90.0, 0.9));
            let mut f = [0.0; FEAT_DIM];
            f[i % FEAT_DIM] = 1.0;
            feats.push(Some(f));
        }
        frames.push((k * 100_000_000, dets, feats));
    }
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let mut times = Vec::with_capacity(frames.len());
    for (k, (ts, dets, feats)) in frames.iter().enumerate() {
        let start = Instant::now();
        mot.update(dets, feats, Some(*ts));
        times.push(start.elapsed().as_secs_f64() * 1e3);
        if k == 5 {
            mot.designate(dets[0].bbox.as_ref().unwrap(), "person");
        }
    }
    let ids: Vec<_> = (0..30).filter_map(|i| mot.output(i)?.track_id).collect();
    assert_eq!(ids.len(), 30, "every detection tracked");
    let warm = &mut times[20..];
    warm.sort_by(f64::total_cmp);
    let mean = warm.iter().sum::<f64>() / warm.len() as f64;
    let p99 = warm[warm.len() * 99 / 100];
    println!("per-frame update, 30 detections: mean {mean:.3} ms, p99 {p99:.3} ms");
    if !cfg!(debug_assertions) {
        assert!(mean <= 1.0, "mean {mean:.3} ms");
    }
}
