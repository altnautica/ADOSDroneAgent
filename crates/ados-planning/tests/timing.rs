//! Full replan time (front end, alternatives, corridor, trajectory, backup)
//! in a 30 × 30 × 12 m window at 0.15 m: a dense voxel map of a Poisson
//! forest with ground, known within 12 m of the window centre. Time is the
//! calling thread's CPU time, so other load on the host does not count.

mod common;

use ados_planning::{Config, Limits, OccupancyQuery, Planner, SearchKind, Shape, State, Stats};
use common::{dense_forest, Forest};

/// CPU time of the calling thread, milliseconds.
fn thread_cpu_ms() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec and the clock id is a
    // constant the C library defines.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime");
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 * 1e-6
}

/// p50 and p95 of `v` (sorted in place).
fn quantiles(v: &mut [f64]) -> (f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[(v.len() * 95).div_ceil(100) - 1])
}

fn stage_table(stats: &[Stats]) {
    type Stage = (&'static str, fn(&Stats) -> f64);
    let stages: [Stage; 7] = [
        ("prepare", |s| s.prepare),
        ("front end", |s| s.front),
        ("alternatives", |s| s.alternatives),
        ("corridor", |s| s.corridor),
        ("optimiser", |s| s.optimise),
        ("checks", |s| s.checks),
        ("backup", |s| s.backup),
    ];
    println!("stage          mean    p50    p95   (ms CPU)");
    for (name, f) in stages {
        let mut v: Vec<f64> = stats.iter().map(f).collect();
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        let (p50, p95) = quantiles(&mut v);
        println!("{name:<12} {mean:6.2} {p50:6.2} {p95:6.2}");
    }
}

#[test]
fn full_replan_p95_within_budget() {
    let forest = Forest::poisson(
        42,
        1.0 / 25.0,
        [0.0, 0.0],
        [30.0, 30.0],
        (0.1, 0.3),
        &[[15.0, 15.0]],
        1.5,
    );
    let built = thread_cpu_ms();
    let map = dense_forest(&forest, 30.0, 12.0, 0.15, 0.3, [15.0, 15.0], 12.0);
    println!(
        "map: {:?} voxels, built in {:.0} ms",
        map.dims(),
        thread_cpu_ms() - built
    );
    let cfg = Config {
        z_band: Some([1.0, 5.0]),
        ..Config::default()
    };
    let lim = Limits {
        v_max: 5.0,
        a_max: 5.0,
        j_max: 25.0,
    };
    let mut planner = Planner::new(cfg);
    planner.set_stage_clock(Some(thread_cpu_ms));
    let mut times = Vec::new();
    let mut stats = Vec::new();
    let mut failures = 0;
    let mut shapes = [0usize; 4];
    let mut classes = [0usize; 4];
    for (i, start_off) in [
        [0.0f32, 0.0],
        [1.5, -1.0],
        [-2.0, 1.0],
        [1.0, 2.0],
        [-1.0, -2.0],
    ]
    .iter()
    .enumerate()
    {
        for k in 0..8 {
            let a = std::f32::consts::FRAC_PI_4 * k as f32 + 0.1 * i as f32;
            let start = State {
                p: [15.0 + start_off[0], 15.0 + start_off[1], 2.0],
                v: [1.5 * a.cos(), 1.5 * a.sin(), 0.0],
                a: [0.0; 3],
            };
            let goal = [
                15.0 + 13.5 * a.cos(),
                15.0 + 13.5 * a.sin(),
                2.0 + 0.5 * (k % 3) as f32,
            ];
            let t0 = thread_cpu_ms();
            let r = planner.plan(&map, start, goal, &lim, 0);
            times.push(thread_cpu_ms() - t0);
            stats.push(*planner.stats());
            match r {
                Ok(p) => {
                    classes[p.candidates.min(3) as usize] += 1;
                    shapes[match p.shape {
                        Shape::Optimised => 0,
                        Shape::Rescaled => 1,
                        Shape::StopAndGo => 2,
                        Shape::EmergencyStop => 3,
                    }] += 1
                }
                Err(e) => {
                    let plain = Planner::new(cfg).search_path(&map, start.p, goal, SearchKind::Jps);
                    println!(
                        "replan {i}/{k} failed: {e} (start clearance {:.2} m, plain search {:?})",
                        map.clearance(start.p),
                        plain.map(|(p, c)| (p.len(), c))
                    );
                    failures += 1;
                }
            }
        }
    }
    stage_table(&stats);
    let (p50, p95) = quantiles(&mut times);
    println!(
        "replans {}: cpu p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms, failures {failures}, optimised/rescaled/stop-and-go/emergency {shapes:?}, plans by ways round (0-3) {classes:?}",
        times.len(),
        p50,
        p95,
        times.last().unwrap()
    );
    assert_eq!(failures, 0, "{failures} replans failed");
    if !cfg!(debug_assertions) {
        assert!(p95 <= 30.0, "p95 replan {p95:.2} ms over 30 ms");
    }
}
