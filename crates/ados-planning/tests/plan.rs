//! Corridor, trajectory and backup properties of whole plans.

mod common;

use ados_planning::{
    Aabb, Cell, Config, DenseGrid, FixedHeight, FrontEnd, Limits, OccupancyQuery, Plan, PlanError,
    PlanarQuery, Planner, Shape, State,
};
use common::{Forest, View};

const LIM: Limits = Limits {
    v_max: 4.0,
    a_max: 4.0,
    j_max: 20.0,
};

fn norm(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn near(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
    (0..3).all(|k| (a[k] - b[k]).abs() <= tol)
}

/// Every property a plan promises.
fn check(w: &impl OccupancyQuery, plan: &Plan, start: &State, lim: &Limits, need: f32) {
    // The corridor holds the front-end path.
    for seg in plan.path.windows(2) {
        let l = norm([
            seg[1][0] - seg[0][0],
            seg[1][1] - seg[0][1],
            seg[1][2] - seg[0][2],
        ]);
        let n = ((l / 0.02).ceil() as usize).max(1);
        for i in 0..=n {
            let t = i as f32 / n as f32;
            let p = [
                seg[0][0] + t * (seg[1][0] - seg[0][0]),
                seg[0][1] + t * (seg[1][1] - seg[0][1]),
                seg[0][2] + t * (seg[1][2] - seg[0][2]),
            ];
            assert!(
                plan.corridor.contains(p),
                "path point {p:?} outside the corridor"
            );
        }
    }
    // The trajectory starts at the start state, stays in the corridor and
    // within the limits, and ends at rest.
    let (p0, v0, a0) = plan.exec.sample(0.0);
    assert!(near(p0, start.p, 1e-3) && near(v0, start.v, 1e-3) && near(a0, start.a, 1e-3));
    let dur = plan.exec.duration();
    let n = (dur / 0.005).ceil() as usize;
    for i in 0..=n {
        let t = dur * i as f32 / n as f32;
        let (p, v, a) = plan.exec.sample(t);
        // A stop-and-go fallback follows the path, not the boxes.
        assert!(
            plan.shape == Shape::StopAndGo || plan.corridor.contains(p),
            "trajectory at {t:.3} s ({p:?}) leaves the corridor"
        );
        assert!(norm(v) <= lim.v_max + 1e-3, "speed {} at {t:.3} s", norm(v));
        assert!(
            norm(a) <= lim.a_max + 1e-3,
            "acceleration {} at {t:.3} s",
            norm(a)
        );
        assert!(
            norm(plan.exec.jerk(t)) <= lim.j_max + 1e-3,
            "jerk {} at {t:.3} s",
            norm(plan.exec.jerk(t))
        );
        assert_ne!(w.state(p), Cell::Occupied);
    }
    let (pe, ve, ae) = plan.exec.sample(dur);
    assert!(near(pe, plan.end, 1e-3) && norm(ve) < 1e-4 && norm(ae) < 1e-4);
    // The backup branches off the trajectory at the commit time and stops
    // at rest in observed free space with the plan's clearance.
    assert!(plan.t_commit >= 0.0 && plan.t_commit <= dur + 1e-4);
    let (bp, bv, ba) = plan.backup.sample(0.0);
    let (ep, ev, ea) = plan.exec.sample(plan.t_commit);
    assert!(near(bp, ep, 1e-3) && near(bv, ev, 1e-3) && near(ba, ea, 1e-3));
    let bd = plan.backup.duration();
    let m = ((bd / 0.005).ceil() as usize).max(1);
    for i in 0..=m {
        let t = bd * i as f32 / m as f32;
        let (p, v, a) = plan.backup.sample(t);
        assert_eq!(
            w.state(p),
            Cell::Free,
            "backup at {t:.3} s ({p:?}) outside observed free space"
        );
        assert!(
            w.clearance(p) >= need - 1e-3,
            "backup clearance {} at {p:?}",
            w.clearance(p)
        );
        assert!(norm(v) <= lim.v_max + 1e-3 && norm(a) <= lim.a_max + 1e-3);
    }
    let (_, bv, ba) = plan.backup.sample(bd);
    assert!(
        norm(bv) < 1e-4 && norm(ba) < 1e-4,
        "backup ends moving: {bv:?}"
    );
}

fn forest(seed: u64) -> Forest {
    Forest::poisson(
        seed,
        1.0 / 25.0,
        [0.0, -20.0],
        [60.0, 20.0],
        (0.1, 0.3),
        &[[2.5, 0.0], [57.5, 0.0]],
        1.0,
    )
}

fn view(f: &Forest, centre: [f32; 3]) -> View {
    View::new(f, centre)
}

fn forest_config() -> Config {
    Config {
        z_band: Some([1.0, 4.0]),
        ..Config::default()
    }
}

#[test]
fn forest_plan_keeps_corridor_limits_and_backup() {
    let cfg = forest_config();
    let need = cfg.radius_m + cfg.margin_m;
    let mut planner = Planner::new(cfg);
    for seed in 1..=5 {
        let f = forest(seed);
        let start = State {
            p: [2.5, 0.0, 2.0],
            v: [1.0, 0.0, 0.0],
            a: [0.0; 3],
        };
        let w = view(&f, start.p);
        let plan = planner
            .plan(&w, start, [57.5, 0.0, 2.0], &LIM, 0)
            .expect("forest plan");
        assert!(!plan.reaches_goal, "goal is beyond the window");
        check(&w, &plan, &start, &LIM, need);
        // The window edge is beyond the sensed range: the trajectory enters
        // unknown space, so the backup branches before its end.
        assert!(
            plan.t_commit < plan.exec.duration(),
            "seed {seed}: no branch"
        );
    }
}

#[test]
fn plans_are_deterministic() {
    let f = forest(9);
    let start = State {
        p: [2.5, 0.0, 2.0],
        ..State::default()
    };
    let w = view(&f, start.p);
    let a = Planner::new(forest_config())
        .plan(&w, start, [57.5, 0.0, 2.0], &LIM, 5)
        .unwrap();
    let mut reused = Planner::new(forest_config());
    let _ = reused.plan(&w, start, [30.0, 8.0, 3.0], &LIM, 0);
    let b = reused.plan(&w, start, [57.5, 0.0, 2.0], &LIM, 5).unwrap();
    assert_eq!(a, b);
}

/// A known room with a dividing wall at x = 8 m with doorways spanning the
/// given `y` ranges, and optionally a pillar.
fn walled(doors: &[(f32, f32)], pillar: bool) -> DenseGrid {
    let res = 0.1;
    let mut g = DenseGrid::new([0.0; 3], res, [160, 100, 40], Cell::Free);
    for z in 0..40 {
        for y in 0..100 {
            for x in 0..160 {
                let c = g.centre([x, y, z]);
                let door = doors.iter().any(|&(lo, hi)| c[1] > lo && c[1] < hi);
                let wall = (c[0] - 8.0).abs() < 0.15 && !door;
                let post = pillar && (c[0] - 4.0).powi(2) + (c[1] - 5.0).powi(2) < 0.3f32.powi(2);
                if wall || post || c[2] < 0.2 {
                    g.set([x, y, z], Cell::Occupied);
                }
            }
        }
    }
    g.update_esdf();
    g
}

fn room() -> DenseGrid {
    walled(&[(7.0, 8.4)], true)
}

#[test]
fn goal_behind_a_wall_is_routed_through_the_door() {
    let g = room();
    let cfg = Config::default();
    let need = cfg.radius_m + cfg.margin_m;
    let mut planner = Planner::new(cfg);
    let start = State {
        p: [1.0, 5.0, 1.5],
        ..State::default()
    };
    let goal = [14.0, 2.0, 1.5];
    let plan = planner
        .plan(&g, start, goal, &LIM, 0)
        .expect("route through the door");
    assert!(plan.reaches_goal);
    assert!(near(plan.exec.end(), goal, 1e-3));
    check(&g, &plan, &start, &LIM, need);
    // All known: the trajectory itself is the backup.
    assert!((plan.t_commit - plan.exec.duration()).abs() < 1e-5);
    assert_eq!(plan.backup.duration(), 0.0);
}

/// The `y` where a path crosses the wall.
fn crossing(path: &[[f32; 3]]) -> f32 {
    let w = path
        .windows(2)
        .find(|w| (w[0][0] - 8.0) * (w[1][0] - 8.0) <= 0.0)
        .expect("path crosses the wall");
    let t = (8.0 - w[0][0]) / (w[1][0] - w[0][0]).max(1e-6);
    w[0][1] + t * (w[1][1] - w[0][1])
}

#[test]
fn two_doors_give_two_ways_and_one_door_gives_one() {
    let start = State {
        p: [2.0, 5.0, 1.5],
        ..State::default()
    };
    let goal = [14.0, 5.0, 1.5];
    let two = walled(&[(2.0, 3.6), (6.4, 8.0)], false);
    let mut planner = Planner::new(Config::default());
    let plan = planner.plan(&two, start, goal, &LIM, 0).expect("two doors");
    assert_eq!(plan.candidates, 2, "one way through each door");
    let one = walled(&[(4.2, 5.8)], false);
    let plan = planner.plan(&one, start, goal, &LIM, 0).expect("one door");
    assert_eq!(plan.candidates, 1, "a single doorway is a single way round");
    let y = crossing(&plan.path);
    assert!(y > 4.2 && y < 5.8, "crossed the wall at y = {y}");
}

#[test]
fn moving_into_a_wall_too_close_to_stop_brakes_hard() {
    let g = walled(&[(7.0, 8.4)], false);
    // A few centimetres of air between body and wall, closing at 2 m/s: no
    // trajectory keeps clear, so the plan is the hardest stop the limits
    // allow.
    let start = State {
        p: [7.55, 5.0, 1.5],
        v: [2.0, 0.0, 0.0],
        a: [0.0; 3],
    };
    let mut planner = Planner::new(Config::default());
    let plan = planner
        .plan(&g, start, [14.0, 5.0, 1.5], &LIM, 7)
        .expect("an emergency stop, not an error");
    assert_eq!(plan.shape, Shape::EmergencyStop);
    assert!(!plan.reaches_goal);
    assert_eq!(plan.candidates, 0);
    assert_eq!(plan.made_ms, 7);
    let (p0, v0, a0) = plan.exec.sample(0.0);
    assert!(near(p0, start.p, 1e-4) && near(v0, start.v, 1e-4) && near(a0, start.a, 1e-4));
    let dur = plan.exec.duration();
    let n = (dur / 0.005).ceil() as usize;
    for i in 0..=n {
        let t = dur * i as f32 / n as f32;
        let (_, v, a) = plan.exec.sample(t);
        assert!(norm(v) <= LIM.v_max + 1e-3 && norm(a) <= LIM.a_max + 1e-3);
        assert!(norm(plan.exec.jerk(t)) <= LIM.j_max + 1e-3);
        // Straight ahead, never back toward the start.
        assert!(v[0] >= -1e-3 && v[1].abs() < 1e-4 && v[2].abs() < 1e-4);
    }
    let (pe, ve, ae) = plan.exec.sample(dur);
    assert!(norm(ve) < 1e-3 && norm(ae) < 1e-3, "ends at rest");
    assert!(near(pe, plan.end, 1e-4));
    // Stopping from 2 m/s within 4 m/s² and 20 m/s³ on a quintic takes at
    // least 0.77 s; the hardest stop found is not much longer.
    assert!((0.77..1.0).contains(&dur), "stop takes {dur} s");
    assert_eq!(plan.backup.duration(), 0.0);
    assert!((plan.t_commit - dur).abs() < 1e-6);
}

#[test]
fn kinodynamic_front_end_plans() {
    let g = room();
    let cfg = Config {
        front_end: FrontEnd::Kinodynamic,
        ..Config::default()
    };
    let need = cfg.radius_m + cfg.margin_m;
    let mut planner = Planner::new(cfg);
    let start = State {
        p: [1.0, 5.0, 1.5],
        v: [0.5, 0.0, 0.0],
        a: [0.0; 3],
    };
    let plan = planner
        .plan(&g, start, [6.5, 2.0, 2.0], &LIM, 0)
        .expect("kinodynamic plan");
    check(&g, &plan, &start, &LIM, need);
}

#[test]
fn blocked_goals_snap_or_fail() {
    let g = room();
    let mut planner = Planner::new(Config::default());
    let start = State {
        p: [1.0, 5.0, 1.5],
        ..State::default()
    };
    // Inside the pillar: snapped to the nearest voxel with clearance.
    let plan = planner.plan(&g, start, [4.0, 5.0, 1.5], &LIM, 0).unwrap();
    assert!(!plan.reaches_goal);
    assert!(g.clearance(plan.end) >= 0.55);
    // Deep inside a closed box: nothing within reach.
    let mut boxed = room();
    for z in 0..40 {
        for y in 0..100 {
            for x in 110..160 {
                boxed.set([x, y, z], Cell::Occupied);
            }
        }
    }
    boxed.update_esdf();
    assert_eq!(
        planner.plan(&boxed, start, [14.0, 5.0, 1.5], &LIM, 0),
        Err(PlanError::NoPath)
    );
}

/// A horizontal slice of a dense map as a planar map.
struct Slice<'a>(&'a DenseGrid, f32);

impl PlanarQuery for Slice<'_> {
    fn res(&self) -> f32 {
        self.0.res()
    }
    fn bounds(&self) -> [[f32; 2]; 2] {
        let b: Aabb = self.0.bounds();
        [[b.min[0], b.min[1]], [b.max[0], b.max[1]]]
    }
    fn state(&self, p: [f32; 2]) -> Cell {
        self.0.state([p[0], p[1], self.1])
    }
    fn clearance(&self, p: [f32; 2]) -> f32 {
        self.0.clearance([p[0], p[1], self.1])
    }
}

#[test]
fn planar_plans_hold_their_height() {
    let g = room();
    let slice = Slice(&g, 1.55);
    let w = FixedHeight::new(&slice, 1.7);
    let cfg = Config::default();
    let need = cfg.radius_m + cfg.margin_m;
    let mut planner = Planner::new(cfg);
    let start = State {
        p: [1.0, 5.0, 1.7],
        ..State::default()
    };
    let plan = planner
        .plan(&w, start, [14.0, 2.0, 1.7], &LIM, 0)
        .expect("planar plan");
    check(&w, &plan, &start, &LIM, need);
    let z = plan.exec.start()[2];
    for i in 0..=200 {
        let t = plan.exec.duration() * i as f32 / 200.0;
        assert!((plan.exec.sample(t).0[2] - z).abs() < 1e-4);
    }
}
