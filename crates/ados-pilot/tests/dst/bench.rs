//! The obstacle benchmark: a goal 55 m across a Poisson stand of trunks
//! (`bench_forest`, three densities) or down a street pedestrians cross
//! (`bench_crossing`), flown at a set goal speed on each sensor tier
//! ([`SensorTier`]), scored by [`crate::metrics`].
//!
//! The goal is a skill's follow goal 6 m behind a subject that moves from
//! the start to 6 m past the end at the goal speed, its velocity the
//! feed-forward and the ring round the subject the detour spots, the input
//! the pilot routes round trunks today; the subject stops 55 m out and the
//! goal with it.
//!
//! `cargo test -p ados-pilot --test dst -- --ignored bench_matrix` flies
//! the whole matrix (`DST_BENCH_SEEDS`, default 10; `DST_BENCH_THREADS`,
//! default 4; `DST_PLANNER=trajectory` flies the trajectory planner) and
//! writes `target/dst-bench/<scenario>.json` and a table.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ados_protocol::pilot::{Candidates, Goal, GoalKind, Request};

use crate::forest::lease;
use crate::metrics::{Cell, Meter, RunMetrics, REACH_M, TABLE_HEAD};
use crate::rng::Rng;
use crate::scenarios::launch_to_hold;
use crate::sensors::{SensorTier, Walker};
use crate::sim::Sim;

/// The straight distance to the goal, m.
const GOAL_M: f32 = 55.0;
/// The stand's extent: across (east) and along (north), m.
const AREA_M: (f32, f32) = (30.0, 60.0);
/// Trunk radii, m.
const TRUNK_R_M: (f32, f32) = (0.1, 0.3);
/// No trunk surface within this of the start or the goal, m.
const KEEP_OUT_M: f32 = 1.0;
/// Flight height over the stand, and down the street (under a walker's
/// head, so the walkers are in the scan plane), m.
const FOREST_ALT_M: f32 = 4.0;
const STREET_ALT_M: f32 = 1.5;
/// The goal's distance behind its subject, m.
const BEHIND_M: f32 = 6.0;
const GOAL_MS: u64 = 100;
/// A pedestrian's pace, m/s, and the street's width, m.
const WALK_MPS: f32 = 1.4;
const STREET_M: f32 = 10.0;

/// Stand densities, trees per m²: one tree per 49, 36, 25 m².
pub const LAMBDAS: [(&str, f32); 3] = [
    ("bench_forest_l49", 1.0 / 49.0),
    ("bench_forest_l36", 1.0 / 36.0),
    ("bench_forest_l25", 1.0 / 25.0),
];
pub const FOREST_SPEEDS: [f32; 4] = [3.0, 5.0, 7.0, 10.0];
pub const CROSSING_SPEEDS: [f32; 2] = [3.0, 5.0];

/// A homogeneous Poisson stand of `lambda_per_m2` over `area_m` (east,
/// north; the start at its south edge's middle, 2.5 m in), radii uniform in
/// `r_trunk`, none within [`KEEP_OUT_M`] of the start or of the goal
/// [`GOAL_M`] north.
pub fn poisson_stand(
    seed: u64,
    lambda_per_m2: f32,
    area_m: (f32, f32),
    r_trunk: (f32, f32),
) -> Vec<([f32; 2], f32)> {
    let mut rng = Rng::new(seed ^ 0xB3_4C_F0_57);
    let mean = (lambda_per_m2 * area_m.0 * area_m.1) as f64;
    // Knuth's draw of the count.
    let (limit, mut p, mut n) = ((-mean).exp(), 1.0f64, 0usize);
    loop {
        p *= rng.unit() as f64;
        if p <= limit {
            break;
        }
        n += 1;
    }
    let south = -(area_m.1 - GOAL_M) / 2.0;
    let mut stand = Vec::with_capacity(n);
    while stand.len() < n {
        let c = [
            rng.range(south, south + area_m.1),
            rng.range(-area_m.0 / 2.0, area_m.0 / 2.0),
        ];
        let r = rng.range(r_trunk.0, r_trunk.1);
        let clear = |q: [f32; 2]| (c[0] - q[0]).hypot(c[1] - q[1]) - r >= KEEP_OUT_M;
        if clear([0.0, 0.0]) && clear([GOAL_M, 0.0]) {
            stand.push((c, r));
        }
    }
    stand
}

/// `n` pedestrians crossing the street (east-west, kerb to kerb) at points
/// along it, each from a seeded start time after `t0_ms`.
pub fn crossing_walkers(seed: u64, n: usize, t0_ms: u64) -> Vec<Walker> {
    let mut rng = Rng::new(seed ^ 0xC4_05_51_06);
    (0..n)
        .map(|_| {
            let north = rng.range(10.0, GOAL_M - 5.0);
            let dir = if rng.unit() < 0.5 { 1.0 } else { -1.0 };
            let half = STREET_M / 2.0 + 1.0;
            Walker {
                path: vec![[north, -dir * half], [north, dir * half]],
                speed: WALK_MPS,
                start_ms: t0_ms + (rng.range(0.0, 8.0) * 1000.0) as u64,
            }
        })
        .collect()
}

fn goal(subject: [f32; 2], speed: f32, alt_m: f32, max_mps: f32) -> Goal {
    let mut g = Goal::new(
        GoalKind::Follow,
        [subject[0] - BEHIND_M, subject[1], -alt_m],
        max_mps,
    );
    g.vn = speed;
    g.subject_ne = Some(subject);
    g.candidates = Some(Candidates {
        ring_center_ne: subject,
        radius_m: BEHIND_M,
        angle_deg: 180.0,
        angle_span_deg: 90.0,
    });
    g
}

/// Fly one run: up to `alt_m`, then the goal to [`GOAL_M`] north at
/// `speed`, until reached or timed out; with `timing`, each tick of the
/// goal's flight timed into it, seconds of wall time.
fn fly(
    s: &mut Sim,
    alt_m: f32,
    speed: f32,
    crossing: Option<usize>,
    mut timing: Option<&mut Vec<f64>>,
) -> RunMetrics {
    s.boot();
    launch_to_hold(s, alt_m);
    let lease_id = lease(s);
    let t0 = s.now;
    if let Some(n) = crossing {
        s.walkers = crossing_walkers(s.seed, n, t0);
    }
    let start = [s.fc.truth()[0], s.fc.truth()[1]];
    let end = [GOAL_M, 0.0];
    let mut meter = Meter::new(s.seed, start, GOAL_M);
    let timeout_ms = ((3.0 * GOAL_M / speed + 10.0) * 1000.0) as u64;
    let mut subject = [BEHIND_M, 0.0];
    while s.now - t0 < timeout_ms {
        subject[0] = (subject[0] + speed * GOAL_MS as f32 / 1000.0).min(GOAL_M + BEHIND_M);
        let moving = subject[0] < GOAL_M + BEHIND_M;
        let g = goal(subject, if moving { speed } else { 0.0 }, alt_m, speed);
        s.request(Request::Goal { lease_id, goal: g });
        if !s.walkers.is_empty() && tracker_on() {
            let batch = s.tracker_batch();
            let now = s.now;
            s.p.on_feed("vision.detection", &batch, now);
        }
        for _ in 0..GOAL_MS / ados_pilot::core::TICK_MS {
            let at = std::time::Instant::now();
            s.step();
            if let Some(t) = timing.as_deref_mut() {
                t.push(at.elapsed().as_secs_f64());
            }
            let t = s.fc.truth();
            meter.tick(
                [t[0], t[1]],
                s.stand_clearance_m().min(s.walker_clearance_m()),
            );
            meter.person(s.walker_sep_m());
        }
        let t = s.fc.truth();
        let every = if trace_fine() { 100 } else { 1000 };
        if trace_on() && (s.now - t0).is_multiple_of(every) {
            trace_line(s, t0, subject);
        }
        if (t[0] - end[0]).hypot(t[1] - end[1]) <= REACH_M {
            meter.reached((s.now - t0) as f32 / 1000.0);
            break;
        }
    }
    meter.finish()
}

/// `DST_BENCH_TRACE=1`: a line a second per run (debugging a cell);
/// `DST_BENCH_TRACE=2`: a line every 100 ms with the command flown.
pub fn trace_on() -> bool {
    std::env::var("DST_BENCH_TRACE").is_ok_and(|v| v == "1" || v == "2")
}

fn trace_fine() -> bool {
    std::env::var("DST_BENCH_TRACE").is_ok_and(|v| v == "2")
}

/// The walkers reach the pilot through a forward camera's tracker
/// ([`Sim::tracker_batch`]) as well as its obstacle sensor;
/// `DST_BENCH_TRACKER=0` flies on the obstacle sensor alone.
fn tracker_on() -> bool {
    std::env::var("DST_BENCH_TRACKER").map_or(true, |v| v != "0")
}

/// The pilot's current obstacle returns more than 0.5 m off every walker's
/// surface and every trunk's: returns of nothing that is there now.
fn ghost_hits(s: &Sim) -> usize {
    let near = |p: [f32; 2]| {
        let walkers = s
            .walkers
            .iter()
            .map(|w| (w.at(s.now), crate::sensors::WALKER_R_M));
        s.stand
            .iter()
            .copied()
            .chain(walkers)
            .any(|(c, r)| (p[0] - c[0]).hypot(p[1] - c[1]) - r < 0.5)
    };
    s.p.obstacles
        .hits(s.now)
        .iter()
        .filter(|h| !near(h.ne))
        .count()
}

pub fn trace_line(s: &Sim, t0: u64, subject: [f32; 2]) {
    let t = s.fc.truth();
    let st = s.p.state(s.now);
    let frame = serde_json::to_value(&st).unwrap_or_default();
    let end = st
        .plan_ned
        .last()
        .map_or(0.0, |e| (e[0] as f32 - t[0]).hypot(e[1] as f32 - t[1]));
    eprintln!(
        "{} s{} t={:5.1} pos=({:5.1},{:5.1}) v={:4.2} subj={:5.1} clr={:4.2} sep={:4.2} movers={} ghosts={} avoid={:?} hold={:?} sense={:?} traj={:?} end={:4.1} limits={}",
        s.name,
        s.seed,
        (s.now - t0) as f32 / 1000.0,
        t[0],
        t[1],
        s.fc.vel[0].hypot(s.fc.vel[1]),
        subject[0],
        s.stand_clearance_m().min(s.walker_clearance_m()),
        s.walker_sep_m(),
        st.movers.len(),
        ghost_hits(s),
        st.avoid_state,
        st.hold_reason,
        s.p.sensing().map(|x| x.speed_limit_mps),
        s.p.trajectory_outcome(),
        end,
        frame["command"]["limits"],
    );
    if trace_fine() {
        let c = &frame["command"];
        let plan: Vec<_> = st.plan_ned.iter().take(4).collect();
        eprintln!(
            "    vel=({:5.2},{:5.2}) yaw={:6.1} ask={} sp_vel={} target={} plan={:?}",
            s.fc.vel[0],
            s.fc.vel[1],
            s.fc.att[2].to_degrees(),
            c["ask_vel_ne"],
            c["sp_vel_ned"],
            c["target_ne"],
            plan,
        );
    }
}

/// One forest run.
pub fn bench_forest(
    name: &'static str,
    lambda: f32,
    tier: SensorTier,
    speed: f32,
    seed: u64,
) -> RunMetrics {
    let mut s = Sim::new(name, seed, |_| {});
    s.tier = tier;
    s.flatten_3d = flatten_3d();
    s.stand = poisson_stand(seed, lambda, AREA_M, TRUNK_R_M);
    fly(&mut s, FOREST_ALT_M, speed, None, None)
}

/// One street run: 2-6 pedestrians by seed.
pub fn bench_crossing(tier: SensorTier, speed: f32, seed: u64) -> RunMetrics {
    let mut s = Sim::new("bench_crossing", seed, |_| {});
    s.tier = tier;
    s.flatten_3d = flatten_3d();
    fly(
        &mut s,
        STREET_ALT_M,
        speed,
        Some(2 + (seed % 5) as usize),
        None,
    )
}

/// The `p`-quantile of `v` (sorted in place).
fn quantile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// What the trajectory planner in shadow costs beside the grid planner: a
/// forest run (one tree per 25 m², 3 m/s, planar tier) flown with
/// `pilot.planner_shadow` off and on. The shadow is never flown, so both
/// flights are the same tick for tick; each replan period's ticks are
/// summed and the difference between the two runs, period by period, is
/// the shadow's cost per replan.
#[test]
#[ignore = "timing: run with --ignored --nocapture shadow_cost"]
fn shadow_cost() {
    let per = (ados_pilot::core::PLAN_PERIOD_MS / ados_pilot::core::TICK_MS) as usize;
    let run = |shadow: bool| {
        let mut s = Sim::new("bench_forest_l25", 1, |c| c.pilot.planner_shadow = shadow);
        s.tier = SensorTier::Planar;
        s.stand = poisson_stand(1, 1.0 / 25.0, AREA_M, TRUNK_R_M);
        let mut t = Vec::new();
        let m = fly(&mut s, FOREST_ALT_M, 3.0, None, Some(&mut t));
        let blocks: Vec<f64> = t.chunks_exact(per).map(|c| c.iter().sum()).collect();
        (m, blocks)
    };
    let (off_m, off) = run(false);
    let (on_m, on) = run(true);
    let (off_m, on_m) = (format!("{off_m:?}"), format!("{on_m:?}"));
    assert_eq!(off_m, on_m, "the shadow changed the flight");
    let mut diff: Vec<f64> = on.iter().zip(&off).map(|(a, b)| (a - b) * 1e3).collect();
    let (mut on_ms, mut off_ms): (Vec<f64>, Vec<f64>) = (
        on.iter().map(|x| x * 1e3).collect(),
        off.iter().map(|x| x * 1e3).collect(),
    );
    println!(
        "shadow_cost: {} replans; per replan period, ms: off p50 {:.3} p95 {:.3}, \
         on p50 {:.3} p95 {:.3}; shadow p50 {:.3} p95 {:.3} max {:.3}",
        diff.len(),
        quantile(&mut off_ms, 0.5),
        quantile(&mut off_ms, 0.95),
        quantile(&mut on_ms, 0.5),
        quantile(&mut on_ms, 0.95),
        quantile(&mut diff, 0.5),
        quantile(&mut diff, 0.95),
        quantile(&mut diff, 1.0),
    );
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `DST_BENCH_FLAT=1`: the 3D tiers fly on Navigation's flattened scan, as
/// before the pilot read the voxel map (for comparison).
fn flatten_3d() -> bool {
    std::env::var("DST_BENCH_FLAT").is_ok_and(|v| v == "1")
}

#[test]
#[ignore = "the full benchmark: minutes; run with --ignored bench_matrix"]
fn bench_matrix() {
    let seeds = env_usize("DST_BENCH_SEEDS", 10) as u64;
    let threads = env_usize("DST_BENCH_THREADS", 4).clamp(1, 4);
    // (scenario index, tier, speed): scenario 0-2 forest, 3 crossing.
    let mut cells: Vec<(usize, SensorTier, f32)> = Vec::new();
    for (i, _) in LAMBDAS.iter().enumerate() {
        for tier in SensorTier::ALL {
            for v in FOREST_SPEEDS {
                cells.push((i, tier, v));
            }
        }
    }
    for tier in SensorTier::ALL {
        for v in CROSSING_SPEEDS {
            cells.push((3, tier, v));
        }
    }
    // `DST_BENCH_TIERS=mid360,planar` and `DST_BENCH_SPEEDS=3,5` fly a
    // part of the matrix.
    let listed = |key: &str, v: &str| {
        std::env::var(key).map_or(true, |l| l.split(',').any(|x| x.trim() == v))
    };
    // `DST_BENCH_SCENARIOS=bench_crossing` flies one family.
    cells.retain(|&(i, tier, v)| {
        let name = LAMBDAS.get(i).map_or("bench_crossing", |l| l.0);
        listed("DST_BENCH_TIERS", tier.name())
            && listed("DST_BENCH_SPEEDS", &format!("{v:.0}"))
            && listed("DST_BENCH_SCENARIOS", name)
    });
    // `DST_BENCH_SEED=3` flies that seed alone (tracing one run).
    let only = std::env::var("DST_BENCH_SEED")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let jobs: Vec<(usize, u64)> = (0..cells.len())
        .flat_map(|c| (1..=seeds).map(move |s| (c, s)))
        .filter(|&(_, s)| only.is_none_or(|o| o == s))
        .collect();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Vec<RunMetrics>>> = Mutex::new(vec![Vec::new(); cells.len()]);
    let wall = std::time::Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(c, seed)) = jobs.get(j) else { break };
                let (i, tier, v) = cells[c];
                let m = if i < 3 {
                    bench_forest(LAMBDAS[i].0, LAMBDAS[i].1, tier, v, seed)
                } else {
                    bench_crossing(tier, v, seed)
                };
                results.lock().unwrap()[c].push(m);
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    let dir = crate::metrics::out_dir();
    std::fs::create_dir_all(&dir).expect("bench dir");
    let mut table = String::from(TABLE_HEAD);
    let mut by_scenario: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for (c, &(i, tier, v)) in cells.iter().enumerate() {
        let mut runs = std::mem::take(&mut results[c]);
        runs.sort_by_key(|r| r.seed);
        let scenario = if i < 3 {
            LAMBDAS[i].0
        } else {
            "bench_crossing"
        }
        .to_string();
        let cell = Cell {
            scenario: scenario.clone(),
            tier: tier.name(),
            speed_mps: v,
            runs,
        };
        table.push('\n');
        table.push_str(&cell.row());
        match by_scenario.iter_mut().find(|(n, _)| *n == scenario) {
            Some((_, v)) => v.push(cell.json()),
            None => by_scenario.push((scenario, vec![cell.json()])),
        }
    }
    for (name, cells) in by_scenario {
        let path = dir.join(format!("{name}.json"));
        let body = serde_json::json!({"scenario": name, "seeds": seeds, "cells": cells});
        std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).expect("bench json");
    }
    std::fs::write(dir.join("table.md"), &table).expect("bench table");
    println!("{table}");
    println!(
        "bench_matrix: {} runs in {:.0} s wall, json in {}",
        jobs.len(),
        wall.elapsed().as_secs_f64(),
        dir.display()
    );
}

/// The stand's density is the one asked, and nothing stands in the start's
/// or the goal's keep-out; one seed draws one stand.
#[test]
fn dst_bench_poisson_stand_density_and_keep_out() {
    for (_, lambda) in LAMBDAS {
        let mut total = 0usize;
        for seed in 1..=20 {
            let st = poisson_stand(seed, lambda, AREA_M, TRUNK_R_M);
            assert_eq!(st, poisson_stand(seed, lambda, AREA_M, TRUNK_R_M));
            for &(c, r) in &st {
                assert!((0.1..0.3).contains(&r));
                assert!(c[0].hypot(c[1]) - r >= KEEP_OUT_M);
                assert!((c[0] - GOAL_M).hypot(c[1]) - r >= KEEP_OUT_M);
            }
            total += st.len();
        }
        let want = lambda * AREA_M.0 * AREA_M.1 * 20.0;
        assert!(
            (total as f32 - want).abs() < 0.15 * want,
            "{total} trunks for {want:.0}"
        );
    }
}

/// A short run of each family flies and scores (the matrix's plumbing).
#[test]
fn dst_bench_runs_score() {
    let m = bench_forest("bench_forest_l49", 1.0 / 49.0, SensorTier::Mid360, 3.0, 1);
    assert!(m.min_clearance_m.is_finite() && m.path_ratio > 0.5, "{m:?}");
    let m = bench_crossing(SensorTier::Planar, 3.0, 1);
    assert!(m.path_ratio > 0.5, "{m:?}");
    // The walkers are tracked and kept their separation from.
    assert!(!m.collision && m.person_breaches == 0, "{m:?}");
}
