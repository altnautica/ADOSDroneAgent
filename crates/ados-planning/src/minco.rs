//! MINCO trajectory optimisation: the minimum-jerk piecewise quintic through
//! free waypoints with free durations is a linear map of (waypoints,
//! durations), solved by one banded system; cost gradients reach the
//! waypoints and durations through the adjoint (transpose) solve.
//!
//! Cost = jerk energy + time weight · total time + time-integrated cubic
//! penalties on leaving the corridor box of each piece and on exceeding the
//! velocity, acceleration and jerk limits. Waypoints live in the overlap of
//! consecutive boxes through a sine map; durations stay positive through a
//! smooth rational map. L-BFGS minimises the unconstrained variables.

mod band;

use band::Band;

use crate::lbfgs::{Lbfgs, LbfgsParams};
use crate::traj::{Segment, Trajectory};
use crate::world::Aabb;

/// Cost weights.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    /// Cost per second of trajectory (against jerk energy).
    pub time: f64,
    /// Corridor penalty weight (per m³ of violation, per second).
    pub corridor: f64,
    pub vel: f64,
    pub acc: f64,
    pub jerk: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            time: 64.0,
            corridor: 1.0e5,
            vel: 1.0e4,
            acc: 1.0e4,
            jerk: 1.0e4,
        }
    }
}

/// Position, velocity and acceleration, f64.
pub(crate) type Pva = [[f64; 3]; 3];

/// One optimisation: boundary states, one corridor box per piece and a seed
/// for every interior waypoint (inside the overlap of its two boxes).
pub(crate) struct Problem<'a> {
    pub start: Pva,
    pub end: Pva,
    pub boxes: &'a [Aabb],
    pub seeds: &'a [[f32; 3]],
    /// Velocity, acceleration and jerk the optimiser aims within.
    pub lim: [f64; 3],
    pub samples: usize,
}

/// How far inside each box the optimiser aims, metres.
const SHRINK: f64 = 0.03;

#[derive(Debug, Clone, Copy, Default)]
struct Box64 {
    lo: [f64; 3],
    hi: [f64; 3],
}

impl Box64 {
    fn from(b: &Aabb, shrink: f64) -> Self {
        let mut o = Box64::default();
        for k in 0..3 {
            let (lo, hi) = (b.min[k] as f64, b.max[k] as f64);
            let s = shrink.min(0.3 * (hi - lo));
            o.lo[k] = lo + s;
            o.hi[k] = hi - s;
        }
        o
    }

    fn overlap(&self, o: &Box64) -> Option<Box64> {
        let mut r = Box64::default();
        for k in 0..3 {
            r.lo[k] = self.lo[k].max(o.lo[k]);
            r.hi[k] = self.hi[k].min(o.hi[k]);
            if r.lo[k] > r.hi[k] {
                return None;
            }
        }
        Some(r)
    }
}

fn tmap(tau: f64) -> (f64, f64) {
    if tau > 0.0 {
        ((0.5 * tau + 1.0) * tau + 1.0, tau + 1.0)
    } else {
        let den = (0.5 * tau - 1.0) * tau + 1.0;
        (1.0 / den, -(tau - 1.0) / (den * den))
    }
}

fn tmap_inv(t: f64) -> f64 {
    if t >= 1.0 {
        (2.0 * t - 1.0).sqrt() - 1.0
    } else {
        1.0 - (2.0 / t - 1.0).sqrt()
    }
}

/// `d^order/dt^order` of the monomials `t^0 … t^5` (order ≤ 5).
fn beta(t: f64, order: usize) -> [f64; 6] {
    let mut b = [0.0; 6];
    let mut tp = 1.0;
    for (k, bk) in b.iter_mut().enumerate().skip(order) {
        let mut f = tp;
        for m in 0..order {
            f *= (k - m) as f64;
        }
        *bk = f;
        tp *= t;
    }
    b
}

fn dot6(b: &[f64; 6], c: &[[f64; 3]], k: usize) -> f64 {
    (0..6).map(|i| b[i] * c[i][k]).sum()
}

/// Derivative order of each row of a junction block (see [`Eval::build`]).
const JUNCTION_ORDERS: [usize; 6] = [0, 3, 4, 0, 1, 2];

#[derive(Debug, Default)]
struct Eval {
    m: usize,
    start: Pva,
    end: Pva,
    boxes: Vec<Box64>,
    mid: Vec<[f64; 3]>,
    half: Vec<[f64; 3]>,
    band: Band,
    c: Vec<[f64; 3]>,
    gc: Vec<[f64; 3]>,
    t: Vec<f64>,
    dt: Vec<f64>,
    gt: Vec<f64>,
    lim: [f64; 3],
    w: Weights,
    samples: usize,
}

impl Eval {
    /// Rows: start p, v, a; per junction i the waypoint, then jerk, snap,
    /// position, velocity and acceleration continuity; end p, v, a. Ordered so
    /// the matrix has 8 sub- and 5 super-diagonals.
    fn build(&mut self) -> bool {
        let m = self.m;
        let n = 6 * m;
        self.band.reset(n, 8, 5);
        self.band.set(0, 0, 1.0);
        self.band.set(1, 1, 1.0);
        self.band.set(2, 2, 2.0);
        for i in 0..m.saturating_sub(1) {
            let r = 3 + 6 * i;
            for (s, &o) in JUNCTION_ORDERS.iter().enumerate() {
                let b = beta(self.t[i], o);
                for (k, v) in b.iter().enumerate().skip(o) {
                    self.band.set(r + s, 6 * i + k, *v);
                }
                if s > 0 {
                    let b0 = beta(0.0, o);
                    self.band.set(r + s, 6 * (i + 1) + o, -b0[o]);
                }
            }
        }
        let r = n - 3;
        for o in 0..3 {
            let b = beta(self.t[m - 1], o);
            for (k, v) in b.iter().enumerate().skip(o) {
                self.band.set(r + o, 6 * (m - 1) + k, *v);
            }
        }
        self.band.factor()
    }

    fn cost(&mut self, x: &[f64], grad: &mut [f64]) -> f64 {
        let m = self.m;
        let nq = 3 * (m - 1);
        for i in 0..m {
            let (t, dt) = tmap(x[nq + i]);
            self.t[i] = t;
            self.dt[i] = dt;
        }
        if !self.build() {
            return f64::INFINITY;
        }
        self.c.iter_mut().for_each(|v| *v = [0.0; 3]);
        self.c[0] = self.start[0];
        self.c[1] = self.start[1];
        self.c[2] = self.start[2];
        for i in 0..m - 1 {
            for k in 0..3 {
                self.c[3 + 6 * i][k] = self.mid[i][k] + self.half[i][k] * x[3 * i + k].sin();
            }
        }
        let n = 6 * m;
        self.c[n - 3] = self.end[0];
        self.c[n - 2] = self.end[1];
        self.c[n - 1] = self.end[2];
        self.band.solve(&mut self.c);
        self.gc.iter_mut().for_each(|v| *v = [0.0; 3]);
        self.gt.iter_mut().for_each(|v| *v = 0.0);
        let mut j = 0.0;
        for i in 0..m {
            j += self.energy(i) + self.w.time * self.t[i];
            self.gt[i] += self.w.time;
            j += self.penalty(i);
        }
        // Adjoint: G = A⁻ᵀ ∂J/∂c.
        self.band.solve_t(&mut self.gc);
        for i in 0..m {
            let ci = &self.c[6 * i..6 * i + 6];
            let (r, orders): (usize, &[usize]) = if i + 1 < m {
                (3 + 6 * i, &JUNCTION_ORDERS)
            } else {
                (n - 3, &[0, 1, 2])
            };
            for (s, &o) in orders.iter().enumerate() {
                let b = beta(self.t[i], o + 1);
                for k in 0..3 {
                    self.gt[i] -= self.gc[r + s][k] * dot6(&b, ci, k);
                }
            }
        }
        for i in 0..m - 1 {
            for k in 0..3 {
                let xi = x[3 * i + k];
                grad[3 * i + k] = self.gc[3 + 6 * i][k] * self.half[i][k] * xi.cos();
            }
        }
        for i in 0..m {
            grad[nq + i] = self.gt[i] * self.dt[i];
        }
        j
    }

    /// Jerk energy of piece `i`; accumulates its gradients.
    fn energy(&mut self, i: usize) -> f64 {
        let t = self.t[i];
        let (t2, t3) = (t * t, t * t * t);
        let (t4, t5) = (t3 * t, t3 * t2);
        let mut e = 0.0;
        for k in 0..3 {
            let a0 = 6.0 * self.c[6 * i + 3][k];
            let a1 = 24.0 * self.c[6 * i + 4][k];
            let a2 = 60.0 * self.c[6 * i + 5][k];
            e += a0 * a0 * t
                + a0 * a1 * t2
                + (a1 * a1 + 2.0 * a0 * a2) * t3 / 3.0
                + a1 * a2 * t4 / 2.0
                + a2 * a2 * t5 / 5.0;
            self.gc[6 * i + 3][k] += 6.0 * (2.0 * a0 * t + a1 * t2 + 2.0 * a2 * t3 / 3.0);
            self.gc[6 * i + 4][k] += 24.0 * (a0 * t2 + 2.0 * a1 * t3 / 3.0 + a2 * t4 / 2.0);
            self.gc[6 * i + 5][k] +=
                60.0 * (2.0 * a0 * t3 / 3.0 + a1 * t4 / 2.0 + 2.0 * a2 * t5 / 5.0);
            let jt = a0 + a1 * t + a2 * t2;
            self.gt[i] += jt * jt;
        }
        e
    }

    /// Time-integrated penalties of piece `i`; accumulates their gradients.
    fn penalty(&mut self, i: usize) -> f64 {
        let k_n = self.samples;
        let t_i = self.t[i];
        let bx = self.boxes[i];
        let w = self.w;
        let [vl, al, jl] = self.lim;
        let mut total = 0.0;
        for j in 0..=k_n {
            let s = j as f64 / k_n as f64;
            let t = s * t_i;
            let om = if j == 0 || j == k_n { 0.5 } else { 1.0 } * t_i / k_n as f64;
            let b: [[f64; 6]; 5] = [beta(t, 0), beta(t, 1), beta(t, 2), beta(t, 3), beta(t, 4)];
            let ci = &self.c[6 * i..6 * i + 6];
            let mut d = [[0.0f64; 3]; 5];
            for (o, row) in d.iter_mut().enumerate() {
                for (k, v) in row.iter_mut().enumerate() {
                    *v = dot6(&b[o], ci, k);
                }
            }
            let mut phi = 0.0;
            let mut g = [[0.0f64; 3]; 4];
            for k in 0..3 {
                let lo = bx.lo[k] - d[0][k];
                if lo > 0.0 {
                    phi += w.corridor * lo * lo * lo;
                    g[0][k] -= 3.0 * w.corridor * lo * lo;
                }
                let hi = d[0][k] - bx.hi[k];
                if hi > 0.0 {
                    phi += w.corridor * hi * hi * hi;
                    g[0][k] += 3.0 * w.corridor * hi * hi;
                }
            }
            for (o, (wt, l)) in [(w.vel, vl), (w.acc, al), (w.jerk, jl)]
                .into_iter()
                .enumerate()
            {
                let x = d[o + 1];
                // Relative excess of the squared norm, so every limit's
                // penalty has the same scale whatever its units.
                let inv = 1.0 / (l * l);
                let e = (x[0] * x[0] + x[1] * x[1] + x[2] * x[2]) * inv - 1.0;
                if e > 0.0 {
                    phi += wt * e * e * e;
                    let f = 6.0 * wt * e * e * inv;
                    for k in 0..3 {
                        g[o + 1][k] += f * x[k];
                    }
                }
            }
            if phi == 0.0 {
                continue;
            }
            total += om * phi;
            let mut dt = 0.0;
            for k in 0..3 {
                for (o, bo) in b.iter().take(4).enumerate() {
                    for (row, bv) in bo.iter().enumerate() {
                        self.gc[6 * i + row][k] += om * bv * g[o][k];
                    }
                }
                for o in 0..4 {
                    dt += g[o][k] * d[o + 1][k];
                }
            }
            self.gt[i] += om * s * dt + phi * om / t_i;
        }
        total
    }
}

/// Reusable optimiser state.
#[derive(Debug, Default)]
pub(crate) struct Minco {
    eval: Eval,
    lbfgs: Lbfgs,
    x: Vec<f64>,
    grad: Vec<f64>,
}

impl Minco {
    /// Optimise; `None` when the corridor is inconsistent or the result is
    /// not finite. Returns the trajectory and its cost.
    pub fn optimize(
        &mut self,
        prob: &Problem,
        w: &Weights,
        prm: LbfgsParams,
    ) -> Option<(Trajectory, f64)> {
        let m = prob.boxes.len();
        if m == 0 || prob.seeds.len() + 1 != m {
            return None;
        }
        let e = &mut self.eval;
        e.m = m;
        e.start = prob.start;
        e.end = prob.end;
        e.lim = prob.lim;
        e.w = *w;
        e.samples = prob.samples.max(2);
        e.boxes.clear();
        e.boxes
            .extend(prob.boxes.iter().map(|b| Box64::from(b, SHRINK)));
        e.mid.clear();
        e.half.clear();
        for i in 0..m - 1 {
            let o = e.boxes[i].overlap(&e.boxes[i + 1]).or_else(|| {
                Box64::from(&prob.boxes[i], 0.0).overlap(&Box64::from(&prob.boxes[i + 1], 0.0))
            })?;
            let mut mid = [0.0; 3];
            let mut half = [0.0; 3];
            for k in 0..3 {
                mid[k] = 0.5 * (o.lo[k] + o.hi[k]);
                half[k] = (0.5 * (o.hi[k] - o.lo[k])).max(1e-6);
            }
            e.mid.push(mid);
            e.half.push(half);
        }
        e.c.clear();
        e.c.resize(6 * m, [0.0; 3]);
        e.gc.clear();
        e.gc.resize(6 * m, [0.0; 3]);
        for v in [&mut e.t, &mut e.dt, &mut e.gt] {
            v.clear();
            v.resize(m, 0.0);
        }
        let nq = 3 * (m - 1);
        self.x.clear();
        self.x.resize(nq + m, 0.0);
        self.grad.clear();
        self.grad.resize(nq + m, 0.0);
        let mut prev = prob.start[0];
        let v_ref = 0.6 * prob.lim[0].max(0.1);
        for i in 0..m {
            let next = if i + 1 < m {
                let s = prob.seeds[i];
                let mut q = [0.0; 3];
                for k in 0..3 {
                    let r = ((s[k] as f64 - e.mid[i][k]) / e.half[i][k]).clamp(-0.95, 0.95);
                    self.x[3 * i + k] = r.asin();
                    q[k] = e.mid[i][k] + e.half[i][k] * r;
                }
                q
            } else {
                prob.end[0]
            };
            let d = ((next[0] - prev[0]).powi(2)
                + (next[1] - prev[1]).powi(2)
                + (next[2] - prev[2]).powi(2))
            .sqrt();
            self.x[nq + i] = tmap_inv((d / v_ref).max(0.25));
            prev = next;
        }
        let eval = &mut self.eval;
        self.lbfgs
            .minimize(&mut self.x, |x, g| eval.cost(x, g), prm);
        let cost = self.eval.cost(&self.x, &mut self.grad);
        if !cost.is_finite() {
            return None;
        }
        let segs = (0..m)
            .map(|i| {
                let mut c = [[0.0; 3]; 6];
                c.copy_from_slice(&self.eval.c[6 * i..6 * i + 6]);
                Segment {
                    dur: self.eval.t[i],
                    c,
                }
            })
            .collect();
        Some((Trajectory::new(segs), cost))
    }
}

#[cfg(test)]
mod tests;
