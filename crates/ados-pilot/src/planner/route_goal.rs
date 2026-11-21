//! Goals beyond the trajectory planner's window: a coarse route over 1 m
//! tiles within [`ROUTE_REACH_M`] of the vehicle ([`ados_planning::GlobalRouter`]),
//! the tiles the obstacle grid covers known (blocked where its clearance is
//! under what the plan keeps), every other tile unknown and traversable at a
//! cost. The planner flies to a point [`LOCAL_GOAL_M`] along the route; a
//! goal with no route within the reach is blocked.

use ados_planning::{needs_replan, Cell, GlobalRouter, PlanError, Route, RouteConfig, TileGrid};

use crate::geo::{self, V2};
use crate::obstacles::Grid;

/// Half the side of the square the route may use round the vehicle, m.
pub const ROUTE_REACH_M: f32 = 200.0;
/// Tile edge, m.
const TILE_M: f32 = 1.0;
/// The local goal sits this far along the route, m: inside the grid's
/// window with room to stop.
pub const LOCAL_GOAL_M: f32 = 12.0;
/// A route is made again once its goal has moved this far, m.
const REROUTE_GOAL_M: f32 = 2.0;

/// The tiles round the vehicle: what the grid knows, unknown beyond it.
struct Tiles<'a> {
    grid: &'a Grid,
    origin: V2,
    n: u32,
    need_m: f32,
    /// The vehicle: the tiles it may be leaving are passable.
    from: V2,
}

impl TileGrid for Tiles<'_> {
    fn tile_m(&self) -> f32 {
        TILE_M
    }
    fn origin(&self) -> [f32; 2] {
        self.origin
    }
    fn dims(&self) -> [u32; 2] {
        [self.n, self.n]
    }
    fn state(&self, ix: u32, iy: u32) -> Cell {
        let c = [
            self.origin[0] + (ix as f32 + 0.5) * TILE_M,
            self.origin[1] + (iy as f32 + 0.5) * TILE_M,
        ];
        // Never under a tile: a thin wall between two tile centres would
        // let the route through it. Round the vehicle only a wall's own
        // tile is blocked, so a vehicle inside the clearance can leave.
        let need = if geo::dist(c, self.from) <= self.need_m + TILE_M {
            0.5 * TILE_M
        } else {
            self.need_m.max(TILE_M)
        };
        match self.grid.clearance(c) {
            Some(d) if d < need => Cell::Occupied,
            Some(_) => Cell::Free,
            None => Cell::Unknown,
        }
    }
}

/// The route the planner holds toward a far goal.
#[derive(Debug, Default)]
pub struct Router {
    router: GlobalRouter,
    held: Option<(Route, V2)>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            router: GlobalRouter::new(RouteConfig::default()),
            held: None,
        }
    }

    pub fn clear(&mut self) {
        self.held = None;
    }

    /// A point [`LOCAL_GOAL_M`] along a route from `from` to `goal` (held
    /// from before while it is fresh, its goal has not moved and nothing
    /// blocks it), keeping `need_m` of clearance through the grid's tiles.
    /// `Err(NoPath)` when no route exists within [`ROUTE_REACH_M`].
    pub fn local_goal(
        &mut self,
        grid: &Grid,
        from: V2,
        goal: V2,
        need_m: f32,
        now_ms: u64,
    ) -> Result<V2, PlanError> {
        let n = (2.0 * ROUTE_REACH_M / TILE_M) as u32;
        let origin = [from[0] - ROUTE_REACH_M, from[1] - ROUTE_REACH_M];
        let tiles = Tiles {
            grid,
            origin,
            n,
            need_m,
            from,
        };
        let held = self.held.as_ref().filter(|(r, g)| {
            geo::dist(*g, goal) < REROUTE_GOAL_M && !needs_replan(r, &tiles, now_ms)
        });
        if held.is_none() {
            // The goal held inside the reach, a tile in from its edge.
            let lim = ROUTE_REACH_M - TILE_M;
            let rel = geo::sub(goal, from);
            let far = geo::norm(rel);
            let to = if far > lim {
                geo::add(from, geo::scale(rel, lim / far))
            } else {
                goal
            };
            let route = self.router.route(&tiles, from, to, now_ms);
            self.held = route.ok().map(|r| (r, goal));
        }
        let (route, _) = self.held.as_ref().ok_or(PlanError::NoPath)?;
        Ok(along_from(&route.path, from, LOCAL_GOAL_M))
    }
}

/// The point `dist` along `path` past the point of it nearest `from`.
fn along_from(path: &[V2], from: V2, dist: f32) -> V2 {
    let mut best = (f32::INFINITY, 0usize, 0.0f32);
    for (k, w) in path.windows(2).enumerate() {
        let ab = geo::sub(w[1], w[0]);
        let l2 = geo::dot(ab, ab);
        let t = if l2 > 1e-9 {
            (geo::dot(geo::sub(from, w[0]), ab) / l2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let d = geo::dist(geo::add(w[0], geo::scale(ab, t)), from);
        if d < best.0 {
            best = (d, k, t);
        }
    }
    let (_, k, t) = best;
    let Some(w) = path.get(k..k + 2) else {
        return *path.last().unwrap_or(&from);
    };
    let start = geo::add(w[0], geo::scale(geo::sub(w[1], w[0]), t));
    let mut rest = vec![start];
    rest.extend_from_slice(&path[k + 1..]);
    super::along(&rest, dist)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_across_the_way_is_routed_round_and_an_enclosed_goal_is_not() {
        let mut grid = Grid::default();
        grid.reset([0.0, 0.0]);
        // A wall 6 m north, 20 m wide.
        for k in 0..80 {
            grid.mark([6.0, -10.0 + k as f32 * 0.25]);
        }
        grid.transform();
        let mut r = Router::new();
        let p = r
            .local_goal(&grid, [0.0, 0.0], [40.0, 0.0], 1.5, 0)
            .unwrap();
        assert!(
            p[1].abs() > 8.0,
            "the route goes round the wall's end: {p:?}"
        );
        // A box round the goal, inside the grid (it ends 16 m out).
        let mut grid = Grid::default();
        grid.reset([0.0, 0.0]);
        for k in 0..=24 {
            let s = -3.0 + k as f32 * 0.25;
            for q in [[6.0, s], [12.0, s], [9.0 + s, -3.0], [9.0 + s, 3.0]] {
                grid.mark(q);
            }
        }
        grid.transform();
        let mut r = Router::new();
        assert_eq!(
            r.local_goal(&grid, [0.0, 0.0], [9.0, 0.0], 0.5, 0),
            Err(PlanError::NoPath)
        );
    }

    #[test]
    fn the_local_goal_runs_on_from_the_nearest_point_of_the_route() {
        let path = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]];
        let p = along_from(&path, [4.0, 1.0], 8.0);
        assert!(geo::dist(p, [10.0, 2.0]) < 1e-3, "{p:?}");
    }
}
