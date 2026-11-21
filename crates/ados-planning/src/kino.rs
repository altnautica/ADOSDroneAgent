//! Kinodynamic A*: best-first search over constant-acceleration motion
//! primitives of fixed duration from the vehicle's position and velocity,
//! with one node per voxel and velocity bin (a better arrival replaces a worse one),
//! closed by an analytic shot: the cubic that minimises time plus effort to
//! the goal at rest, accepted when it keeps the limits and stays traversable.
//!
//! Edge cost is `(|u|² + ρ)·Δt`; the heuristic is `ρ` times a lower bound on
//! the time to reach the goal at rest under the per-axis velocity and
//! acceleration limits.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

use crate::lattice::{Space, BLOCKED};
use crate::world::{dist, norm, OccupancyQuery};
use crate::{Limits, PlanError, State};

/// Kinodynamic search settings.
#[derive(Debug, Clone, Copy)]
pub struct KinoConfig {
    /// Primitive duration, seconds.
    pub dt: f32,
    pub max_expansions: u32,
    /// `ρ`: cost per second against squared acceleration.
    pub time_weight: f32,
    /// The analytic shot is tried from nodes this close to the goal, metres.
    pub shot_radius_m: f32,
    /// Inflation of the time heuristic. It ignores the effort term, so at 1
    /// the search floods every state cheaper than the goal; inflating it
    /// trades optimality for a bounded search.
    pub heuristic_weight: f32,
}

impl Default for KinoConfig {
    fn default() -> Self {
        Self {
            dt: 0.1,
            max_expansions: 20_000,
            time_weight: 10.0,
            shot_radius_m: 15.0,
            heuristic_weight: 3.0,
        }
    }
}

/// A kinodynamically feasible path: states every `dt` from the start, the
/// last one at the goal at rest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KinoPath {
    pub states: Vec<State>,
    pub dt: f32,
}

#[derive(Debug, Clone, Copy)]
struct KNode {
    p: [f32; 3],
    v: [f32; 3],
    /// Acceleration held from the parent to here, for `tau` seconds.
    u: [f32; 3],
    tau: f32,
    g: f32,
    parent: u32,
    closed: bool,
}

#[derive(Debug, Clone, Copy)]
struct KOpen {
    f: f32,
    idx: u32,
}

impl PartialEq for KOpen {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for KOpen {}
impl PartialOrd for KOpen {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for KOpen {
    fn cmp(&self, o: &Self) -> Ordering {
        o.f.total_cmp(&self.f).then(o.idx.cmp(&self.idx))
    }
}

/// Longest primitive, in steps, while it stays in its parent's bin.
const MAX_STEPS: usize = 10;

/// Multiplicative hash for voxel keys.
#[derive(Debug, Default)]
struct KeyHash(u64);

impl Hasher for KeyHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (v ^ (v >> 29)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// Shortest time on one axis from `(0, v0)` to `(d, 0)` with `|a| ≤ a`,
/// `|v| ≤ v`.
fn axis_time(d: f32, v0: f32, a: f32, v: f32) -> f32 {
    let (d, v0) = if d < 0.0 { (-d, -v0) } else { (d, v0) };
    let rest_to_rest = |x: f32| {
        if x >= v * v / a {
            2.0 * v / a + (x - v * v / a) / v
        } else {
            2.0 * (x / a).sqrt()
        }
    };
    let stop = v0 * v0.abs() / (2.0 * a);
    if stop > d {
        // Brake, then come back.
        return v0 / a + rest_to_rest(stop - d);
    }
    let peak2 = a * d + 0.5 * v0 * v0;
    let peak = peak2.sqrt();
    if peak <= v {
        (peak - v0) / a + peak / a
    } else {
        let accel = (v * v - v0 * v0) / (2.0 * a);
        let decel = v * v / (2.0 * a);
        (v - v0).abs() / a + v / a + (d - accel - decel).max(0.0) / v
    }
}

/// Cubic coefficients per axis from `(p0, v0)` to `(p1, 0)` in time `t`.
fn cubic(p0: [f32; 3], v0: [f32; 3], p1: [f32; 3], t: f32) -> [[f32; 4]; 3] {
    let mut c = [[0.0; 4]; 3];
    for k in 0..3 {
        let d = p1[k] - p0[k];
        c[k] = [
            p0[k],
            v0[k],
            (3.0 * d - 2.0 * v0[k] * t) / (t * t),
            (-2.0 * d + v0[k] * t) / (t * t * t),
        ];
    }
    c
}

fn cubic_at(c: &[[f32; 4]; 3], t: f32) -> State {
    let mut s = State::default();
    for k in 0..3 {
        let [a0, a1, a2, a3] = c[k];
        s.p[k] = a0 + t * (a1 + t * (a2 + t * a3));
        s.v[k] = a1 + t * (2.0 * a2 + 3.0 * a3 * t);
        s.a[k] = 2.0 * a2 + 6.0 * a3 * t;
    }
    s
}

/// Effort of the cubic (`∫|a|²`) plus `rho·t`.
fn shot_cost(p0: [f32; 3], v0: [f32; 3], p1: [f32; 3], t: f32, rho: f32) -> f32 {
    let mut j = rho * t;
    for k in 0..3 {
        let d = p1[k] - p0[k];
        j += 12.0 * d * d / (t * t * t) - 12.0 * d * v0[k] / (t * t) + 4.0 * v0[k] * v0[k] / t;
    }
    j
}

#[derive(Debug, Default)]
pub(crate) struct Kino {
    nodes: Vec<KNode>,
    open: BinaryHeap<KOpen>,
    map: HashMap<u64, u32, BuildHasherDefault<KeyHash>>,
    inputs: Vec<[f32; 3]>,
    pub expansions: u32,
}

impl Kino {
    /// Node identity: the voxel and the velocity quantised to a quarter of
    /// the speed limit per axis (nine levels each way).
    fn key(sp: &Space, p: [f32; 3], v: [f32; 3], v_res: f32) -> Option<u64> {
        let c = sp.lat.cell_of(p)?;
        let q = v.map(|x| ((x / v_res).round().clamp(-4.0, 4.0) + 4.0) as u64);
        Some(sp.lat.index(c) as u64 * 1024 + q[0] + 9 * q[1] + 81 * q[2])
    }

    fn point_ok<W: OccupancyQuery + ?Sized>(w: &W, sp: &mut Space, p: [f32; 3]) -> bool {
        match sp.lat.cell_of(p) {
            Some(c) => sp.base(w, c) != BLOCKED,
            None => false,
        }
    }

    fn heuristic(p: [f32; 3], v: [f32; 3], goal: [f32; 3], lim: &Limits, rho: f32) -> f32 {
        let t = (0..3)
            .map(|k| axis_time(goal[k] - p[k], v[k], lim.a_max, lim.v_max))
            .fold(0.0f32, f32::max);
        rho * t
    }

    /// The analytic shot from `n` to the goal at rest: `Some(t)` when one of
    /// a few durations keeps the limits and stays traversable.
    fn shot<W: OccupancyQuery + ?Sized>(
        w: &W,
        sp: &mut Space,
        n: &KNode,
        goal: [f32; 3],
        lim: &Limits,
        cfg: &KinoConfig,
    ) -> Option<f32> {
        // Golden-section minimum of time plus effort.
        let d = dist(n.p, goal);
        let (mut lo, mut hi) = (0.05f32, 4.0 * d / lim.v_max.max(0.1) + 4.0);
        let r = 0.618_034f32;
        for _ in 0..30 {
            let a = hi - r * (hi - lo);
            let b = lo + r * (hi - lo);
            if shot_cost(n.p, n.v, goal, a, cfg.time_weight)
                < shot_cost(n.p, n.v, goal, b, cfg.time_weight)
            {
                hi = b;
            } else {
                lo = a;
            }
        }
        let best = 0.5 * (lo + hi);
        'try_t: for scale in [1.0f32, 1.3, 1.7, 2.2] {
            let t = best * scale;
            let c = cubic(n.p, n.v, goal, t);
            let steps = ((t / (0.5 * cfg.dt)).ceil() as usize).max(2);
            for i in 0..=steps {
                let s = cubic_at(&c, t * i as f32 / steps as f32);
                if norm(s.v) > lim.v_max || norm(s.a) > lim.a_max {
                    continue 'try_t;
                }
            }
            let along = ((d / (0.5 * sp.lat.res)).ceil() as usize).max(steps);
            for i in 0..=along {
                if !Self::point_ok(w, sp, cubic_at(&c, t * i as f32 / along as f32).p) {
                    continue 'try_t;
                }
            }
            return Some(t);
        }
        None
    }

    /// Search from `start` to `goal` at rest; the path goes to `out`.
    #[allow(clippy::too_many_arguments)]
    pub fn search<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        sp: &mut Space,
        start: &State,
        goal: [f32; 3],
        lim: &Limits,
        cfg: &KinoConfig,
        out: &mut KinoPath,
    ) -> Result<(), PlanError> {
        out.states.clear();
        out.dt = cfg.dt;
        self.nodes.clear();
        self.open.clear();
        self.map.clear();
        self.expansions = 0;
        let planar = sp.lat.planar();
        let a = lim.a_max * 0.999;
        self.inputs.clear();
        self.inputs.push([0.0; 3]);
        for dz in -1..=1i32 {
            for dy in -1..=1i32 {
                for dx in -1..=1i32 {
                    if (dx, dy, dz) == (0, 0, 0) || (planar && dz != 0) {
                        continue;
                    }
                    let n = ((dx * dx + dy * dy + dz * dz) as f32).sqrt();
                    self.inputs
                        .push([a * dx as f32 / n, a * dy as f32 / n, a * dz as f32 / n]);
                }
            }
        }
        let v_res = (0.25 * lim.v_max).max(0.1);
        let key = Self::key(sp, start.p, start.v, v_res).ok_or(PlanError::NoPath)?;
        self.nodes.push(KNode {
            p: start.p,
            v: start.v,
            u: start.a,
            tau: 0.0,
            g: 0.0,
            parent: u32::MAX,
            closed: false,
        });
        self.map.insert(key, 0);
        self.open.push(KOpen {
            f: cfg.heuristic_weight * Self::heuristic(start.p, start.v, goal, lim, cfg.time_weight),
            idx: 0,
        });
        let dt = cfg.dt;
        let sub = ((lim.v_max * dt / (0.5 * sp.lat.res)).ceil() as usize).max(2);
        while let Some(o) = self.open.pop() {
            let i = o.idx as usize;
            if self.nodes[i].closed {
                continue;
            }
            self.nodes[i].closed = true;
            self.expansions += 1;
            if self.expansions > cfg.max_expansions {
                return Err(PlanError::Timeout);
            }
            let n = self.nodes[i];
            if dist(n.p, goal) <= cfg.shot_radius_m {
                if let Some(t) = Self::shot(w, sp, &n, goal, lim, cfg) {
                    self.emit(i, goal, t, dt, out);
                    return Ok(());
                }
            }
            // A primitive lasts whole steps until it leaves its parent's
            // voxel and velocity bin: a slow vehicle covers less than a voxel
            // in one step, and a child sharing the parent's key would be
            // dropped as already closed.
            let here = Self::key(sp, n.p, n.v, v_res);
            for ui in 0..self.inputs.len() {
                let u = self.inputs[ui];
                let at = |t: f32| {
                    let mut p = [0.0; 3];
                    let mut v = [0.0; 3];
                    for k in 0..3 {
                        p[k] = n.p[k] + n.v[k] * t + 0.5 * u[k] * t * t;
                        v[k] = n.v[k] + u[k] * t;
                    }
                    (p, v)
                };
                let mut steps = 1;
                let (mut p1, mut v1) = at(dt);
                while Self::key(sp, p1, v1, v_res) == here && steps < MAX_STEPS {
                    steps += 1;
                    (p1, v1) = at(dt * steps as f32);
                }
                let tau = dt * steps as f32;
                if norm(v1) > lim.v_max {
                    continue;
                }
                let Some(key) = Self::key(sp, p1, v1, v_res) else {
                    continue;
                };
                let samples = sub * steps;
                let clear = (1..=samples)
                    .all(|s| Self::point_ok(w, sp, at(tau * s as f32 / samples as f32).0));
                if !clear {
                    continue;
                }
                let g = n.g + (u[0] * u[0] + u[1] * u[1] + u[2] * u[2] + cfg.time_weight) * tau;
                let node = KNode {
                    p: p1,
                    v: v1,
                    u,
                    tau,
                    g,
                    parent: i as u32,
                    closed: false,
                };
                let f =
                    g + cfg.heuristic_weight * Self::heuristic(p1, v1, goal, lim, cfg.time_weight);
                match self.map.get(&key) {
                    Some(&k) => {
                        let k = k as usize;
                        if self.nodes[k].closed || g >= self.nodes[k].g {
                            continue;
                        }
                        self.nodes[k] = node;
                        self.open.push(KOpen { f, idx: k as u32 });
                    }
                    None => {
                        let k = self.nodes.len() as u32;
                        self.nodes.push(node);
                        self.map.insert(key, k);
                        self.open.push(KOpen { f, idx: k });
                    }
                }
            }
        }
        Err(PlanError::NoPath)
    }

    /// States from the start to node `last`, then the shot to the goal.
    fn emit(&self, last: usize, goal: [f32; 3], t: f32, dt: f32, out: &mut KinoPath) {
        let mut chain = Vec::new();
        let mut j = last;
        loop {
            chain.push(j);
            let p = self.nodes[j].parent;
            if p == u32::MAX {
                break;
            }
            j = p as usize;
        }
        chain.reverse();
        for w in chain.windows(2) {
            let (a, b) = (&self.nodes[w[0]], &self.nodes[w[1]]);
            let steps = (b.tau / dt).round().max(1.0) as usize;
            for s in 0..steps {
                let t = dt * s as f32;
                let mut st = State {
                    a: b.u,
                    ..State::default()
                };
                for k in 0..3 {
                    st.p[k] = a.p[k] + a.v[k] * t + 0.5 * b.u[k] * t * t;
                    st.v[k] = a.v[k] + b.u[k] * t;
                }
                out.states.push(st);
            }
        }
        let n = &self.nodes[last];
        let c = cubic(n.p, n.v, goal, t);
        let steps = ((t / dt).ceil() as usize).max(1);
        for i in 0..=steps {
            let s = cubic_at(&c, (i as f32 * dt).min(t));
            out.states.push(s);
        }
        if let Some(s) = out.states.last_mut() {
            s.p = goal;
            s.v = [0.0; 3];
        }
    }
}
