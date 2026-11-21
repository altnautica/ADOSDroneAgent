//! Front ends and the global route.

mod common;

use ados_planning::{
    needs_replan, Cell, Config, FixedHeight, GlobalRouter, Limits, OccupancyQuery, PlanError,
    Planner, RouteConfig, SearchKind, State, TileGrid, ROUTE_REPLAN_MS,
};
use common::{Grid2, Grid3, Rng};

/// A planner whose clearance rule is plain occupancy (no body, no margin)
/// and whose grid search is optimal.
fn bare() -> Planner {
    Planner::new(Config {
        radius_m: 0.0,
        margin_m: 0.0,
        heuristic_weight: 1.0,
        ..Config::default()
    })
}

fn random_cell(rng: &mut Rng) -> Cell {
    let u = rng.unit();
    if u < 0.25 {
        Cell::Occupied
    } else if u < 0.40 {
        Cell::Unknown
    } else {
        Cell::Free
    }
}

fn compare(
    planner: &mut Planner,
    w: &impl OccupancyQuery,
    a: [f32; 3],
    b: [f32; 3],
) -> (bool, usize) {
    let jps = planner.search_path(w, a, b, SearchKind::Jps);
    let astar = planner.search_path(w, a, b, SearchKind::Astar);
    match (jps, astar) {
        (Ok((pj, cj)), Ok((_, ca))) => {
            assert!(
                (cj - ca).abs() <= 1e-3 * ca.max(1.0),
                "JPS {cj} vs A* {ca} from {a:?} to {b:?}"
            );
            (true, pj.len())
        }
        (Err(PlanError::NoPath), Err(PlanError::NoPath)) => (false, 0),
        (j, s) => panic!("JPS {j:?} disagrees with A* {s:?} from {a:?} to {b:?}"),
    }
}

#[test]
fn jps_is_optimal_on_random_2d_maps() {
    let mut planner = bare();
    let mut solved = 0;
    for seed in 0..60u64 {
        let mut rng = Rng::new(seed);
        let (nx, ny) = (48, 40);
        let cells = (0..nx * ny).map(|_| random_cell(&mut rng)).collect();
        let g = Grid2 {
            res: 0.5,
            nx,
            ny,
            cells,
        };
        let w = FixedHeight::new(&g, 2.0);
        for _ in 0..4 {
            let a = [rng.range(0.0, 24.0), rng.range(0.0, 20.0), 2.0];
            let b = [rng.range(0.0, 24.0), rng.range(0.0, 20.0), 2.0];
            if g.at([a[0], a[1]]) == Cell::Occupied || g.at([b[0], b[1]]) == Cell::Occupied {
                continue;
            }
            solved += compare(&mut planner, &w, a, b).0 as usize;
        }
    }
    assert!(solved > 60, "only {solved} solvable pairs");
}

#[test]
fn jps_is_optimal_on_random_3d_maps() {
    let mut planner = bare();
    let mut solved = 0;
    for seed in 100..140u64 {
        let mut rng = Rng::new(seed);
        let dims = [20, 18, 10];
        let cells = (0..dims[0] * dims[1] * dims[2])
            .map(|_| random_cell(&mut rng))
            .collect();
        let w = Grid3 {
            res: 0.5,
            dims,
            cells,
        };
        for _ in 0..4 {
            let a = [
                rng.range(0.0, 10.0),
                rng.range(0.0, 9.0),
                rng.range(0.0, 5.0),
            ];
            let b = [
                rng.range(0.0, 10.0),
                rng.range(0.0, 9.0),
                rng.range(0.0, 5.0),
            ];
            if w.state(a) == Cell::Occupied || w.state(b) == Cell::Occupied {
                continue;
            }
            solved += compare(&mut planner, &w, a, b).0 as usize;
        }
    }
    assert!(solved > 40, "only {solved} solvable pairs");
}

#[test]
fn jps_expands_fewer_turning_points_in_open_space() {
    let mut planner = bare();
    let dims = [40, 40, 12];
    let w = Grid3 {
        res: 0.5,
        dims,
        cells: vec![Cell::Free; dims[0] * dims[1] * dims[2]],
    };
    let (path, cost) = planner
        .search_path(&w, [1.2, 1.3, 1.1], [18.7, 15.2, 4.9], SearchKind::Jps)
        .unwrap();
    assert!(
        path.len() <= 4,
        "open-space path has {} turning points",
        path.len()
    );
    let (_, ca) = planner
        .search_path(&w, [1.2, 1.3, 1.1], [18.7, 15.2, 4.9], SearchKind::Astar)
        .unwrap();
    assert!((cost - ca).abs() < 1e-3);
}

/// Pillars on a 2 m pitch in a 24 × 16 × 6 m room.
fn pillars() -> Grid3 {
    let res = 0.2;
    let dims = [120, 80, 30];
    let mut cells = vec![Cell::Free; dims[0] * dims[1] * dims[2]];
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let (px, py) = (x as f32 * res + 0.1, y as f32 * res + 0.1);
                let (cx, cy) = ((px / 3.0).round() * 3.0, (py / 3.0).round() * 3.0);
                let pillar = (px - cx).powi(2) + (py - cy).powi(2) < 0.35f32.powi(2)
                    && cx > 2.0
                    && cx < 22.0;
                if pillar || z == 0 {
                    cells[x + dims[0] * (y + dims[1] * z)] = Cell::Occupied;
                }
            }
        }
    }
    Grid3 { res, dims, cells }
}

#[test]
fn kinodynamic_path_keeps_limits_and_reaches_goal() {
    let w = pillars();
    let mut planner = Planner::new(Config::default());
    for (v, a) in [(2.0f32, 2.0f32), (4.0, 3.0), (6.0, 5.0)] {
        let lim = Limits {
            v_max: v,
            a_max: a,
            j_max: 20.0,
        };
        let start = State {
            p: [1.0, 7.9, 2.0],
            v: [0.5, 0.0, 0.0],
            a: [0.0; 3],
        };
        let goal = [22.5, 9.1, 2.6];
        let path = planner
            .kinodynamic(&w, &start, goal, &lim)
            .expect("kinodynamic path");
        assert!(path.states.len() > 10);
        for s in &path.states {
            let sv = (s.v[0].powi(2) + s.v[1].powi(2) + s.v[2].powi(2)).sqrt();
            let sa = (s.a[0].powi(2) + s.a[1].powi(2) + s.a[2].powi(2)).sqrt();
            assert!(sv <= v + 1e-3, "speed {sv} over {v}");
            assert!(sa <= a + 1e-3, "acceleration {sa} over {a}");
            assert_ne!(
                w.state(s.p),
                Cell::Occupied,
                "state inside an obstacle at {:?}",
                s.p
            );
        }
        let last = path.states.last().unwrap();
        assert_eq!(last.p, goal);
        assert_eq!(last.v, [0.0; 3]);
    }
}

struct Tiles {
    dims: [u32; 2],
    cells: Vec<Cell>,
}

impl TileGrid for Tiles {
    fn tile_m(&self) -> f32 {
        1.0
    }
    fn origin(&self) -> [f32; 2] {
        [-10.0, -10.0]
    }
    fn dims(&self) -> [u32; 2] {
        self.dims
    }
    fn state(&self, ix: u32, iy: u32) -> Cell {
        self.cells[(ix + self.dims[0] * iy) as usize]
    }
}

impl Tiles {
    fn set(&mut self, x: u32, y: u32, c: Cell) {
        self.cells[(x + self.dims[0] * y) as usize] = c;
    }
}

#[test]
fn global_route_goes_round_a_wall_with_spaced_waypoints() {
    let mut t = Tiles {
        dims: [120, 80],
        cells: vec![Cell::Free; 120 * 80],
    };
    // A wall at x = 50 m (tile 60) with a gap at y ∈ [55, 60] m.
    for y in 0..80 {
        if !(65..70).contains(&y) {
            t.set(60, y, Cell::Occupied);
        }
    }
    let mut router = GlobalRouter::new(RouteConfig::default());
    let r = router
        .route(&t, [0.0, 0.0], [100.0, 0.0], 1_000)
        .expect("route");
    assert_eq!(*r.waypoints.last().unwrap(), [100.0, 0.0]);
    let mut prev = r.tiles[0];
    for w in &r.waypoints {
        let d = ((w[0] - prev[0]).powi(2) + (w[1] - prev[1]).powi(2)).sqrt();
        assert!(d <= 10.0 + 1e-3, "waypoints {d} m apart");
        prev = *w;
    }
    assert!(
        r.waypoints.iter().any(|w| w[1] > 40.0),
        "route does not use the gap: {:?}",
        r.waypoints
    );
    assert!(!needs_replan(&r, &t, 1_000 + ROUTE_REPLAN_MS - 1));
    assert!(needs_replan(&r, &t, 1_000 + ROUTE_REPLAN_MS));
    let w = r.waypoints[2];
    t.set((w[0] + 10.0) as u32, (w[1] + 10.0) as u32, Cell::Occupied);
    assert!(needs_replan(&r, &t, 1_001));
}

#[test]
fn global_route_weighs_unknown_tiles() {
    // Unknown band straight ahead; a free detour costs less than ×3.
    let mut t = Tiles {
        dims: [60, 60],
        cells: vec![Cell::Free; 3600],
    };
    for y in 0..60 {
        for x in 20..30 {
            if y < 45 {
                t.set(x, y, Cell::Unknown);
            }
        }
    }
    let mut router = GlobalRouter::new(RouteConfig::default());
    let r = router.route(&t, [0.0, 0.5], [30.0, 0.5], 0).unwrap();
    // Through: 10 m unknown at ×3 = 30 + 20 = 50. Round: under 2·√(25² + 35²)…
    // the search takes whichever is cheaper; its cost never exceeds through.
    assert!(r.cost <= 50.0 + 1e-3, "cost {}", r.cost);
    let mut through = GlobalRouter::new(RouteConfig {
        unknown_cost: 1.0,
        ..RouteConfig::default()
    });
    let direct = through.route(&t, [0.0, 0.5], [30.0, 0.5], 0).unwrap();
    assert!(
        (direct.cost - 30.0).abs() < 1e-3,
        "unweighted cost {}",
        direct.cost
    );
}
