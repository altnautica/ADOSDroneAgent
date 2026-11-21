//! Limited-memory BFGS with a weak Wolfe line search (bisection and
//! doubling), on reusable buffers.

/// Stopping rules.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LbfgsParams {
    pub max_iters: u32,
    /// Stop when `‖g‖∞ ≤ g_tol · max(1, ‖x‖∞)`.
    pub g_tol: f64,
    /// Stop when the cost fell by less than this fraction over the last
    /// `PAST` iterations.
    pub f_tol: f64,
}

const MEM: usize = 8;
const PAST: usize = 3;
const C1: f64 = 1e-4;
const C2: f64 = 0.9;
const MAX_LINE: u32 = 40;

#[derive(Debug, Default)]
pub(crate) struct Lbfgs {
    s: Vec<f64>,
    y: Vec<f64>,
    rho: [f64; MEM],
    alpha: [f64; MEM],
    d: Vec<f64>,
    g: Vec<f64>,
    xt: Vec<f64>,
    gt: Vec<f64>,
    hist: [f64; PAST],
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn inf_norm(a: &[f64]) -> f64 {
    a.iter().fold(0.0, |m, v| m.max(v.abs()))
}

impl Lbfgs {
    /// Minimise `f` from `x` (updated in place); `f(x, g)` returns the cost
    /// and writes its gradient. Returns the final cost.
    pub fn minimize<F>(&mut self, x: &mut [f64], mut f: F, prm: LbfgsParams) -> f64
    where
        F: FnMut(&[f64], &mut [f64]) -> f64,
    {
        let n = x.len();
        for v in [&mut self.d, &mut self.g, &mut self.xt, &mut self.gt] {
            v.clear();
            v.resize(n, 0.0);
        }
        self.s.clear();
        self.s.resize(n * MEM, 0.0);
        self.y.clear();
        self.y.resize(n * MEM, 0.0);
        let mut fx = f(x, &mut self.g);
        if !fx.is_finite() {
            return fx;
        }
        let mut stored = 0usize;
        let mut head = 0usize;
        self.hist.fill(fx);
        for (d, g) in self.d.iter_mut().zip(&self.g) {
            *d = -g;
        }
        let mut step = 1.0 / inf_norm(&self.g).max(1.0);
        for iter in 0..prm.max_iters {
            if inf_norm(&self.g) <= prm.g_tol * inf_norm(x).max(1.0) {
                break;
            }
            let dg0 = dot(&self.d, &self.g);
            if dg0 >= 0.0 {
                // Not a descent direction: restart from steepest descent.
                for (d, g) in self.d.iter_mut().zip(&self.g) {
                    *d = -g;
                }
                stored = 0;
                step = 1.0 / inf_norm(&self.d).max(1.0);
                continue;
            }
            let (mut lo, mut hi) = (0.0f64, f64::INFINITY);
            let mut ft = fx;
            let mut ok = false;
            for _ in 0..MAX_LINE {
                for i in 0..n {
                    self.xt[i] = x[i] + step * self.d[i];
                }
                ft = f(&self.xt, &mut self.gt);
                if !ft.is_finite() || ft > fx + C1 * step * dg0 {
                    hi = step;
                } else if dot(&self.gt, &self.d) < C2 * dg0 {
                    lo = step;
                } else {
                    ok = true;
                    break;
                }
                step = if hi.is_finite() {
                    0.5 * (lo + hi)
                } else {
                    2.0 * step
                };
            }
            if !ok && !(ft.is_finite() && ft < fx) {
                // No decrease along this direction: stop at the best point.
                break;
            }
            // s = x_t − x, y = g_t − g.
            let base = head * n;
            for i in 0..n {
                self.s[base + i] = self.xt[i] - x[i];
                self.y[base + i] = self.gt[i] - self.g[i];
            }
            x.copy_from_slice(&self.xt);
            self.g.copy_from_slice(&self.gt);
            fx = ft;
            let sy = dot(&self.s[base..base + n], &self.y[base..base + n]);
            let yy = dot(&self.y[base..base + n], &self.y[base..base + n]);
            if sy > 1e-12 * yy.max(1e-300) && sy > 0.0 {
                self.rho[head] = 1.0 / sy;
                head = (head + 1) % MEM;
                stored = (stored + 1).min(MEM);
            }
            let k = iter as usize % PAST;
            let past = self.hist[k];
            self.hist[k] = fx;
            if iter as usize >= PAST && (past - fx) <= prm.f_tol * fx.abs().max(1e-12) {
                break;
            }
            // Two-loop recursion.
            for (d, g) in self.d.iter_mut().zip(&self.g) {
                *d = -g;
            }
            for j in 0..stored {
                let m = (head + MEM - 1 - j) % MEM;
                let sm = &self.s[m * n..(m + 1) * n];
                let a = self.rho[m] * dot(sm, &self.d);
                self.alpha[m] = a;
                let ym = &self.y[m * n..(m + 1) * n];
                for (d, y) in self.d.iter_mut().zip(ym) {
                    *d -= a * y;
                }
            }
            if stored > 0 {
                let m = (head + MEM - 1) % MEM;
                let ym = &self.y[m * n..(m + 1) * n];
                let gamma = 1.0 / (self.rho[m] * dot(ym, ym));
                for d in &mut self.d {
                    *d *= gamma;
                }
            }
            for j in (0..stored).rev() {
                let m = (head + MEM - 1 - j) % MEM;
                let ym = &self.y[m * n..(m + 1) * n];
                let b = self.rho[m] * dot(ym, &self.d);
                let sm = &self.s[m * n..(m + 1) * n];
                for (d, s) in self.d.iter_mut().zip(sm) {
                    *d += (self.alpha[m] - b) * s;
                }
            }
            // Unit step on a quasi-Newton direction; a steepest-descent
            // restart (no curvature pair yet) moves the largest variable by
            // at most one unit.
            step = if stored > 0 {
                1.0
            } else {
                1.0 / inf_norm(&self.d).max(1.0)
            };
        }
        fx
    }
}
