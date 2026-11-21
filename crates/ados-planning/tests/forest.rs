//! Closed-loop crossings of seeded Poisson forests (1 trunk per 25 m²) at
//! 3, 5 and 7 m/s: replan at 10 Hz from the flown state through a moving
//! sensing window, fly the plan (and its backup when a replan fails), and
//! check the true clearance of every flown sample.

mod common;

use ados_planning::{Config, Limits, Planner, State};
use common::{Forest, View};

const SEEDS: u64 = 20;
const START: [f32; 3] = [2.5, 0.0, 2.0];
const GOAL: [f32; 3] = [57.5, 0.0, 2.0];
const BODY_M: f32 = 0.25;

struct Run {
    reached: bool,
    min_clear: f32,
    time_s: f32,
    failed_plans: u32,
}

fn cross(seed: u64, speed: f32) -> Run {
    let forest = Forest::poisson(
        seed,
        1.0 / 25.0,
        [0.0, -20.0],
        [60.0, 20.0],
        (0.1, 0.3),
        &[[START[0], START[1]], [GOAL[0], GOAL[1]]],
        1.0,
    );
    let cfg = Config {
        z_band: Some([1.0, 4.0]),
        ..Config::default()
    };
    let lim = Limits {
        v_max: speed,
        a_max: 5.0,
        j_max: 25.0,
    };
    let mut planner = Planner::new(cfg);
    let mut state = State {
        p: START,
        ..State::default()
    };
    let timeout = 3.0 * 55.0 / speed + 10.0;
    let mut plan = None;
    let mut plan_t = 0.0f32;
    let mut t = 0.0f32;
    let mut run = Run {
        reached: false,
        min_clear: f32::INFINITY,
        time_s: 0.0,
        failed_plans: 0,
    };
    while t < timeout {
        let view = View::new(&forest, state.p);
        match planner.plan(&view, state, GOAL, &lim, (t * 1000.0) as u64) {
            Ok(p) => {
                plan = Some(p);
                plan_t = t;
            }
            Err(_) => run.failed_plans += 1,
        }
        let Some(p) = &plan else {
            return run;
        };
        for k in 1..=5 {
            let (pos, _, _) = p.sample(t + 0.02 * k as f32 - plan_t, true);
            run.min_clear = run.min_clear.min(forest.truth_exact([pos[0], pos[1]]));
        }
        t += 0.1;
        let (pos, vel, acc) = p.sample(t - plan_t, true);
        state = State {
            p: pos,
            v: vel,
            a: acc,
        };
        let d =
            ((pos[0] - GOAL[0]).powi(2) + (pos[1] - GOAL[1]).powi(2) + (pos[2] - GOAL[2]).powi(2))
                .sqrt();
        if d < 1.5 {
            run.reached = true;
            run.time_s = t;
            return run;
        }
    }
    run.time_s = t;
    run
}

#[test]
fn poisson_forest_crossings_are_collision_free() {
    let need = Config::default().radius_m + Config::default().margin_m;
    for speed in [3.0f32, 5.0, 7.0] {
        let runs: Vec<Run> = std::thread::scope(|s| {
            let handles: Vec<_> = (1..=SEEDS)
                .map(|seed| s.spawn(move || cross(seed, speed)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut ok = 0;
        let mut collisions = 0;
        for (i, r) in runs.iter().enumerate() {
            let clean = r.min_clear >= need - 1e-3;
            if r.min_clear < BODY_M {
                collisions += 1;
            }
            if r.reached && clean {
                ok += 1;
            }
            println!(
                "speed {speed} seed {:2}: reached {} in {:5.1} s, min clearance {:.3} m, failed replans {}",
                i + 1,
                r.reached,
                r.time_s,
                r.min_clear,
                r.failed_plans
            );
        }
        println!("speed {speed}: {ok}/{SEEDS} reached with clearance ≥ {need} m, {collisions} collisions");
        assert_eq!(
            collisions, 0,
            "speed {speed}: {collisions} runs touched a trunk"
        );
        assert!(
            ok * 100 >= 95 * SEEDS as usize,
            "speed {speed}: only {ok}/{SEEDS} clean crossings"
        );
    }
}
