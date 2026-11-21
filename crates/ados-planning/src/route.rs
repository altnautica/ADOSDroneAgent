//! Coarse global route beyond the local window: 2D jump point search over a
//! tile grid (unknown tiles traversable at a cost), cut into waypoints a
//! fixed distance apart that become the local planner's successive goals.

use crate::jps::{Budget, Search, SearchKind};
use crate::lattice::{Lattice, Rules, Space};
use crate::world::{Aabb, Cell, OccupancyQuery};
use crate::PlanError;

/// A route is stale this long after it was made, milliseconds.
pub const ROUTE_REPLAN_MS: u64 = 5_000;

/// A coarse 2D occupancy grid (live-map tiles).
pub trait TileGrid {
    /// Tile edge, metres.
    fn tile_m(&self) -> f32;
    /// Minimum corner of tile `(0, 0)`, metres.
    fn origin(&self) -> [f32; 2];
    /// Tiles along x and y.
    fn dims(&self) -> [u32; 2];
    fn state(&self, ix: u32, iy: u32) -> Cell;
}

fn tile_at<G: TileGrid + ?Sized>(g: &G, p: [f32; 2]) -> Cell {
    let o = g.origin();
    let t = g.tile_m();
    let [nx, ny] = g.dims();
    let fx = ((p[0] - o[0]) / t).floor();
    let fy = ((p[1] - o[1]) / t).floor();
    if fx < 0.0 || fy < 0.0 || fx >= nx as f32 || fy >= ny as f32 {
        return Cell::Occupied;
    }
    g.state(fx as u32, fy as u32)
}

/// A tile grid seen as a one-layer occupancy map with no clearance rule.
struct TileWorld<'a, G: ?Sized>(&'a G);

impl<G: TileGrid + ?Sized> OccupancyQuery for TileWorld<'_, G> {
    fn res(&self) -> f32 {
        self.0.tile_m()
    }
    fn bounds(&self) -> Aabb {
        let o = self.0.origin();
        let t = self.0.tile_m();
        let [nx, ny] = self.0.dims();
        Aabb::new(
            [o[0], o[1], -0.5 * t],
            [o[0] + nx as f32 * t, o[1] + ny as f32 * t, 0.5 * t],
        )
    }
    fn state(&self, p: [f32; 3]) -> Cell {
        tile_at(self.0, [p[0], p[1]])
    }
    fn clearance(&self, p: [f32; 3]) -> f32 {
        if self.state(p) == Cell::Occupied {
            0.0
        } else {
            f32::INFINITY
        }
    }
}

/// Global route settings.
#[derive(Debug, Clone, Copy)]
pub struct RouteConfig {
    /// Cost multiplier for unknown tiles.
    pub unknown_cost: f32,
    /// Distance between waypoints, metres.
    pub spacing_m: f32,
    /// Tiles a search may scan before giving up.
    pub max_scan: u64,
    /// Heuristic inflation (1 = shortest route).
    pub heuristic_weight: f32,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            unknown_cost: 3.0,
            spacing_m: 10.0,
            max_scan: 8_000_000,
            heuristic_weight: 1.0,
        }
    }
}

/// A route: the searched polyline and the waypoints along it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Route {
    /// Every `spacing_m` along `path` after the start, ending at the goal.
    pub waypoints: Vec<[f32; 2]>,
    /// Start, turning points, goal.
    pub path: Vec<[f32; 2]>,
    /// Tile centres of the searched path's turning points, start and goal
    /// tiles included: the tiles a replan check walks.
    pub tiles: Vec<[f32; 2]>,
    /// Weighted length, metres.
    pub cost: f32,
    pub made_ms: u64,
}

/// The global router and its reusable buffers.
#[derive(Debug, Default)]
pub struct GlobalRouter {
    cfg: RouteConfig,
    space: Space,
    search: Search,
    jumps: Vec<[i32; 3]>,
}

impl GlobalRouter {
    pub fn new(cfg: RouteConfig) -> Self {
        Self {
            cfg,
            ..Default::default()
        }
    }

    /// Route from `from` to `to` over `grid`.
    pub fn route<G: TileGrid + ?Sized>(
        &mut self,
        grid: &G,
        from: [f32; 2],
        to: [f32; 2],
        now_ms: u64,
    ) -> Result<Route, PlanError> {
        let w = TileWorld(grid);
        let lat = Lattice::over(&w, &w.bounds()).ok_or(PlanError::NoPath)?;
        let start = [from[0], from[1], 0.0];
        self.space.begin(
            lat,
            Rules {
                need: 0.0,
                start,
                start_clear: f32::INFINITY,
                unknown_cost: self.cfg.unknown_cost,
            },
        );
        let s = lat.cell_of(start).ok_or(PlanError::NoPath)?;
        let g = lat.cell_of([to[0], to[1], 0.0]).ok_or(PlanError::NoPath)?;
        let cost = self.search.run(
            &w,
            &mut self.space,
            SearchKind::Jps,
            s,
            g,
            Budget {
                max_scan: self.cfg.max_scan,
                h_weight: self.cfg.heuristic_weight,
            },
            &mut self.jumps,
        )?;
        let mut path = Vec::with_capacity(self.jumps.len().max(2));
        path.push(from);
        let n = self.jumps.len();
        if n > 2 {
            path.extend(self.jumps[1..n - 1].iter().map(|c| {
                let p = lat.centre(*c);
                [p[0], p[1]]
            }));
        }
        path.push(to);
        let tiles: Vec<[f32; 2]> = self
            .jumps
            .iter()
            .map(|c| {
                let p = lat.centre(*c);
                [p[0], p[1]]
            })
            .collect();
        // Waypoints follow the tile-centre line (it never touches an
        // occupied tile), ending exactly at the goal.
        let mut guide = tiles.clone();
        if let Some(last) = guide.last_mut() {
            *last = to;
        }
        let waypoints = spaced(&guide, self.cfg.spacing_m.max(lat.res));
        Ok(Route {
            waypoints,
            path,
            tiles,
            cost,
            made_ms: now_ms,
        })
    }
}

fn len2(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
}

/// Points every `spacing` along `path` after its start, then its end.
fn spaced(path: &[[f32; 2]], spacing: f32) -> Vec<[f32; 2]> {
    let mut out = Vec::new();
    let mut next = spacing;
    let mut walked = 0.0;
    for w in path.windows(2) {
        let l = len2(w[0], w[1]);
        while next <= walked + l && l > 0.0 {
            let t = (next - walked) / l;
            out.push([
                w[0][0] + t * (w[1][0] - w[0][0]),
                w[0][1] + t * (w[1][1] - w[0][1]),
            ]);
            next += spacing;
        }
        walked += l;
    }
    let end = *path.last().unwrap_or(&[0.0, 0.0]);
    if out.last().is_none_or(|p| len2(*p, end) > 1e-3) {
        out.push(end);
    }
    out
}

/// True when `route` should be planned again: it is [`ROUTE_REPLAN_MS`]
/// old, or a waypoint, or a tile under the route between them, has turned
/// occupied.
pub fn needs_replan<G: TileGrid + ?Sized>(route: &Route, grid: &G, now_ms: u64) -> bool {
    if now_ms.saturating_sub(route.made_ms) >= ROUTE_REPLAN_MS {
        return true;
    }
    if route
        .waypoints
        .iter()
        .any(|p| tile_at(grid, *p) == Cell::Occupied)
    {
        return true;
    }
    let step = 0.5 * grid.tile_m();
    route.tiles.windows(2).any(|w| {
        let n = ((len2(w[0], w[1]) / step).ceil() as usize).max(1);
        (0..=n).any(|i| {
            let t = i as f32 / n as f32;
            let p = [
                w[0][0] + t * (w[1][0] - w[0][0]),
                w[0][1] + t * (w[1][1] - w[0][1]),
            ];
            tile_at(grid, p) == Cell::Occupied
        })
    })
}
