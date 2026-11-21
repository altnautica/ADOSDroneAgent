//! The local planner: front end (jump point search with homotopy
//! alternatives, or kinodynamic A*), safe flight corridor, MINCO back end
//! and a backup trajectory that stops in observed free space.

use crate::corridor;
use crate::fallback::{self, Shape, RESCALE_A_TOL, RESCALE_V_TOL, STOP_AND_GO_COST};
use crate::jps::{densify, Budget, Search, SearchKind};
use crate::kino::{Kino, KinoPath};
use crate::lattice::{Lattice, Rules, Space, BLOCKED};
use crate::lbfgs::LbfgsParams;
use crate::minco::{Minco, Problem, Pva};
use crate::topo;
use crate::traj::Trajectory;
use crate::world::{dist, norm, OccupancyQuery};
use crate::{Config, Corridor, FrontEnd, Limits, Plan, PlanError, State, Stats};

/// A front-end path: polyline and every voxel along it.
type Candidate = (Vec<[f32; 3]>, Vec<[i32; 3]>);

/// The planner and its reusable buffers.
#[derive(Debug, Default)]
pub struct Planner {
    pub(crate) cfg: Config,
    pub(crate) space: Space,
    search: Search,
    kino: Kino,
    pub(crate) minco: Minco,
    jumps: Vec<[i32; 3]>,
    pub(crate) cells: Vec<[i32; 3]>,
    pub(crate) corridor: Corridor,
    kpath: KinoPath,
    /// Front-end candidates: polyline and dense voxels; `ncand` are live.
    cands: Vec<Candidate>,
    ncand: usize,
    poly: Vec<[f32; 3]>,
    topo_scratch: (Vec<[f32; 3]>, Vec<[f32; 3]>),
    pub(crate) samples: Vec<[f32; 3]>,
    pub(crate) arclen: Vec<f32>,
    /// Stage clock and the last plan's stage times.
    clock: Option<fn() -> f64>,
    stats: Stats,
}

pub(crate) fn pva(s: &State) -> Pva {
    let f = |v: [f32; 3]| [v[0] as f64, v[1] as f64, v[2] as f64];
    [f(s.p), f(s.v), f(s.a)]
}

/// All alternative searches together may scan this many times the voxels
/// the first search did …
const ALT_SCAN_SHARE: u64 = 1;
/// … plus this many.
const ALT_SCAN_FLOOR: u64 = 50_000;
/// Alternatives search greedier than the first path: the back end reshapes
/// them anyway, and a greedy search scans far less round the tubes.
const ALT_H_WEIGHT: f32 = 3.0;
/// Give up after this many alternatives in a row turn out equivalent to an
/// accepted path.
const ALT_MAX_REJECTED: usize = 2;

fn poly_len(p: &[[f32; 3]]) -> f32 {
    p.windows(2).map(|w| dist(w[0], w[1])).sum()
}

impl Planner {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            ..Default::default()
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
    }

    /// Time every stage of later plans with `clock` (any monotonic unit;
    /// `None` stops timing). The planner reads no clock otherwise.
    pub fn set_stage_clock(&mut self, clock: Option<fn() -> f64>) {
        self.clock = clock;
    }

    /// Stage times of the last [`Planner::plan`] call.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    fn now(&self) -> f64 {
        self.clock.map_or(0.0, |c| c())
    }

    pub(crate) fn rules<W: OccupancyQuery + ?Sized>(&self, w: &W, start: [f32; 3]) -> Rules {
        Rules {
            need: self.cfg.radius_m + self.cfg.margin_m,
            start,
            start_clear: w.clearance(start),
            unknown_cost: self.cfg.unknown_cost,
        }
    }

    fn whole_map<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        start: [f32; 3],
    ) -> Result<Lattice, PlanError> {
        let lat = Lattice::over(w, &w.bounds()).ok_or(PlanError::NoPath)?;
        let rules = self.rules(w, start);
        self.space.begin(lat, rules);
        Ok(lat)
    }

    /// Grid search over the whole map from `from` to `to`: the path's
    /// voxel centres (jump points for JPS) and its cost (metres, unknown
    /// voxels weighted).
    pub fn search_path<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        from: [f32; 3],
        to: [f32; 3],
        kind: SearchKind,
    ) -> Result<(Vec<[f32; 3]>, f32), PlanError> {
        let lat = self.whole_map(w, from)?;
        let s = lat.cell_of(from).ok_or(PlanError::NoPath)?;
        let g = lat.cell_of(to).ok_or(PlanError::NoPath)?;
        let budget = self.budget();
        let cost = self
            .search
            .run(w, &mut self.space, kind, s, g, budget, &mut self.jumps)?;
        Ok((self.jumps.iter().map(|c| lat.centre(*c)).collect(), cost))
    }

    /// Kinodynamic A* over the whole map from `start` to `goal` at rest.
    pub fn kinodynamic<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        start: &State,
        goal: [f32; 3],
        limits: &Limits,
    ) -> Result<KinoPath, PlanError> {
        self.whole_map(w, start.p)?;
        let cfg = self.cfg.kino;
        self.kino.search(
            w,
            &mut self.space,
            start,
            goal,
            limits,
            &cfg,
            &mut self.kpath,
        )?;
        Ok(self.kpath.clone())
    }

    /// Plan from `start` toward `goal` (clipped to the map's window when
    /// outside it). `now_ms` stamps the plan; the planner itself is
    /// deterministic and keeps no time.
    pub fn plan<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        start: State,
        goal: [f32; 3],
        limits: &Limits,
        now_ms: u64,
    ) -> Result<Plan, PlanError> {
        self.stats = Stats::default();
        let t0 = self.now();
        let (start, local, reaches) = self.prepare(w, start, goal)?;
        let lat = self.space.lat;
        let s_cell = lat.cell_of(start.p).ok_or(PlanError::NoPath)?;
        if self.space.base(w, s_cell) == BLOCKED {
            return Err(PlanError::NoPath);
        }
        let (g_cell, end, snapped) = self.snap_goal(w, local)?;
        let t1 = self.now();
        self.stats.prepare = t1 - t0;
        match self.cfg.front_end {
            FrontEnd::Jps => self.jps_candidates(w, s_cell, g_cell)?,
            FrontEnd::Kinodynamic => self.kino_candidate(w, &start, end, limits)?,
        }
        let t2 = self.now();
        if self.cfg.front_end == FrontEnd::Kinodynamic {
            self.stats.front = t2 - t1;
        }
        self.stats.alternatives = t2 - t1 - self.stats.front;
        let mut ranked = Vec::with_capacity(self.ncand);
        for k in 0..self.ncand {
            if let Some((tr, cost, shape)) = self.optimise(w, k, &start, end, limits) {
                ranked.push((cost, k, tr, self.corridor.clone(), shape));
            }
        }
        let t3 = self.now();
        self.stats.checks = t3 - t2 - self.stats.corridor - self.stats.optimise;
        if ranked.is_empty() {
            // Nothing flyable from here: the start state is already past
            // saving. Brake as hard as the limits allow rather than fly on.
            return fallback::emergency_stop(&start, Self::caps(&start, limits), now_ms)
                .ok_or(PlanError::NoPath);
        }
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let candidates = ranked.len() as u8;
        for (_, k, exec, corridor, shape) in ranked {
            let t4 = self.now();
            let found = self.backup(w, &exec, &start, limits);
            self.stats.backup += self.now() - t4;
            if let Some((backup, t_commit)) = found {
                return Ok(Plan {
                    exec,
                    backup,
                    t_commit,
                    made_ms: now_ms,
                    end,
                    reaches_goal: reaches && !snapped,
                    path: self.cands[k].0.clone(),
                    corridor,
                    candidates,
                    shape,
                });
            }
        }
        Err(PlanError::NoBackup)
    }

    /// Store the search's path (in `self.jumps`) as the next candidate: the
    /// voxel centres of its turning points, and every voxel along it.
    fn push_candidate(&mut self) {
        let lat = self.space.lat;
        if self.cands.len() <= self.ncand {
            self.cands.push(Default::default());
        }
        let (poly, cells) = &mut self.cands[self.ncand];
        poly.clear();
        poly.extend(self.jumps.iter().map(|c| lat.centre(*c)));
        densify(&self.jumps, cells);
        self.ncand += 1;
    }

    /// The best JPS path, then up to `alternatives − 1` more that are other
    /// ways round the obstacles: each search may not enter a tube round the
    /// paths accepted so far (except near the start and goal), and a result
    /// equivalent to an accepted path is dropped (the next try uses a wider
    /// tube; [`ALT_MAX_REJECTED`] in a row end the hunt). Alternatives longer
    /// than `alt_cost_ratio` times the best are not offered. All alternative
    /// searches together may scan [`ALT_SCAN_SHARE`] times what the first
    /// one did (plus a floor).
    fn jps_candidates<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        s: [i32; 3],
        g: [i32; 3],
    ) -> Result<(), PlanError> {
        self.ncand = 0;
        let cfg = self.cfg;
        self.space.begin_search();
        let mut budget = self.budget();
        let t0 = self.now();
        self.search.run(
            w,
            &mut self.space,
            SearchKind::Jps,
            s,
            g,
            budget,
            &mut self.jumps,
        )?;
        self.stats.front = self.now() - t0;
        self.push_candidate();
        let best = poly_len(&self.cands[0].0);
        let lat = self.space.lat;
        let (start, end) = (lat.centre(s), lat.centre(g));
        let mut left = ALT_SCAN_SHARE * self.search.scanned + ALT_SCAN_FLOOR;
        budget.h_weight = budget.h_weight.max(ALT_H_WEIGHT);
        let mut rejected = 0;
        for _ in 1..2 * cfg.alternatives {
            if self.ncand >= cfg.alternatives || left == 0 || rejected >= ALT_MAX_REJECTED {
                break;
            }
            let tubes = &mut self.space.tubes;
            tubes.points.clear();
            tubes.ends.clear();
            for (poly, _) in &self.cands[..self.ncand] {
                tubes.points.extend_from_slice(poly);
                tubes.ends.push(tubes.points.len());
            }
            tubes.radius = cfg.tube_m * (1.0 + 0.75 * rejected as f32);
            tubes.keep = [start, end];
            tubes.keep_r = tubes.radius + 2.0 * lat.res;
            self.space.lay_tubes();
            budget.max_scan = left.min(cfg.max_scan);
            let found = self.search.run(
                w,
                &mut self.space,
                SearchKind::Jps,
                s,
                g,
                budget,
                &mut self.jumps,
            );
            left = left.saturating_sub(self.search.scanned);
            if found.is_err() {
                break;
            }
            self.poly.clear();
            self.poly.extend(self.jumps.iter().map(|c| lat.centre(*c)));
            if poly_len(&self.poly) > best * cfg.alt_cost_ratio {
                break;
            }
            let same = (0..self.ncand).any(|k| {
                topo::equivalent(
                    w,
                    &mut self.space,
                    &self.cands[k].0,
                    &self.poly,
                    &mut self.topo_scratch,
                )
            });
            if same {
                rejected += 1;
            } else {
                rejected = 0;
                self.push_candidate();
            }
        }
        self.space.begin_search();
        Ok(())
    }

    fn kino_candidate<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        start: &State,
        end: [f32; 3],
        limits: &Limits,
    ) -> Result<(), PlanError> {
        self.ncand = 0;
        let cfg = self.cfg.kino;
        self.kino.search(
            w,
            &mut self.space,
            start,
            end,
            limits,
            &cfg,
            &mut self.kpath,
        )?;
        if self.cands.is_empty() {
            self.cands.push(Default::default());
        }
        let lat = self.space.lat;
        let (poly, cells) = &mut self.cands[0];
        poly.clear();
        poly.extend(self.kpath.states.iter().map(|s| s.p));
        cells.clear();
        for w2 in poly.windows(2) {
            lat.trace(w2[0], w2[1], cells);
        }
        self.ncand = 1;
        Ok(())
    }

    /// Speed, acceleration and jerk a trajectory from `start` must keep.
    pub(crate) fn caps(start: &State, limits: &Limits) -> [f32; 3] {
        [
            limits.v_max.max(norm(start.v)),
            limits.a_max.max(norm(start.a)),
            limits.j_max,
        ]
    }

    fn budget(&self) -> Budget {
        Budget {
            max_scan: self.cfg.max_scan,
            h_weight: self.cfg.heuristic_weight,
        }
    }

    pub(crate) fn lbfgs_params(&self) -> LbfgsParams {
        LbfgsParams {
            max_iters: self.cfg.max_iters,
            g_tol: 1e-5,
            f_tol: 1e-6,
        }
    }

    pub(crate) fn box_cells(&self) -> [i32; 3] {
        let res = self.space.lat.res;
        self.cfg
            .box_max_m
            .map(|m| ((m / res).round() as i32).max(1))
    }

    /// Trajectory for candidate `k`: the optimised one when it passes the
    /// checks; else the same stretched in time until the limits hold; else
    /// stop-and-go along the front-end path. `None` only when even that
    /// cannot be flown from the current state.
    fn optimise<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        k: usize,
        start: &State,
        end: [f32; 3],
        limits: &Limits,
    ) -> Option<(Trajectory, f64, Shape)> {
        let caps = Self::caps(start, limits);
        let aim = self.cfg.limit_aim as f64;
        let max = self.box_cells();
        let t0 = self.now();
        let built = corridor::build(
            w,
            &mut self.space,
            &self.cands[k].1,
            false,
            max,
            &mut self.corridor,
        );
        self.stats.corridor += self.now() - t0;
        if built {
            // Voxel edges are rebuilt from the origin, so a start or end on
            // one may miss its box by rounding.
            let holds = |b: &crate::world::Aabb, p: [f32; 3]| b.grow([1e-4; 3]).contains(p);
            debug_assert!(holds(&self.corridor.boxes[0], start.p));
            debug_assert!(self.corridor.boxes.last().is_some_and(|b| holds(b, end)));
            debug_assert!(self.corridor.seeds.iter().enumerate().all(|(i, s)| holds(
                &self.corridor.boxes[i],
                *s
            ) && holds(
                &self.corridor.boxes[i + 1],
                *s
            )));
            let e = [end[0] as f64, end[1] as f64, end[2] as f64];
            let prob = Problem {
                start: pva(start),
                end: [e, [0.0; 3], [0.0; 3]],
                boxes: &self.corridor.boxes,
                seeds: &self.corridor.seeds,
                lim: caps.map(|c| c as f64 * aim),
                samples: self.cfg.samples,
            };
            let prm = self.lbfgs_params();
            let t1 = self.now();
            let out = self.minco.optimize(&prob, &self.cfg.weights, prm);
            self.stats.optimise += self.now() - t1;
            if let Some((tr, cost)) = out {
                if crate::backup::flyable(w, &self.space, &tr, caps, false) {
                    return Some((tr, cost, Shape::Optimised));
                }
                // Slower is fine when the start state barely changes.
                let s = fallback::stretch_needed(&tr, caps) * 1.01;
                let v_loss = norm(start.v) as f64 * (1.0 - 1.0 / s);
                let a_loss = norm(start.a) as f64 * (1.0 - 1.0 / (s * s));
                if v_loss <= RESCALE_V_TOL && a_loss <= RESCALE_A_TOL {
                    let slow = fallback::stretch(&tr, s);
                    if crate::backup::flyable(w, &self.space, &slow, caps, false) {
                        return Some((slow, cost * s, Shape::Rescaled));
                    }
                }
            }
        }
        let aimed = caps.map(|c| c * self.cfg.limit_aim);
        let tr = fallback::stop_and_go(start, &self.cands[k].0, end, aimed)?;
        if !crate::backup::flyable(w, &self.space, &tr, caps, false) {
            return None;
        }
        debug_assert!(dist(tr.end(), end) < 1e-3);
        let cost = STOP_AND_GO_COST + tr.duration() as f64 * self.cfg.weights.time;
        Some((tr, cost, Shape::StopAndGo))
    }
}
