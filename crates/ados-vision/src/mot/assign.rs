//! Minimum-cost bipartite assignment (the Hungarian method with potentials).
//!
//! The solver keeps its buffers between calls, so a frame's association
//! passes allocate nothing once the buffers have grown to the largest problem
//! seen.

/// Cost of a forbidden or padding pair. Any real cost is far below it.
const FORBIDDEN: f64 = 1e9;

#[derive(Debug, Default)]
pub struct Assigner {
    n: usize,
    cost: Vec<f64>,
    u: Vec<f64>,
    v: Vec<f64>,
    p: Vec<usize>,
    way: Vec<usize>,
    minv: Vec<f64>,
    used: Vec<bool>,
}

impl Assigner {
    /// Start a `rows` x `cols` problem with every pair forbidden.
    pub fn reset(&mut self, rows: usize, cols: usize) {
        let n = rows.max(cols);
        self.n = n;
        self.cost.clear();
        self.cost.resize(n * n, FORBIDDEN);
    }

    /// Allow `row` to take `col` at `cost`.
    pub fn set(&mut self, row: usize, col: usize, cost: f32) {
        self.cost[row * self.n + col] = f64::from(cost);
    }

    /// Solve and push every assigned, allowed `(row, col)` pair onto `out`.
    pub fn solve(&mut self, out: &mut Vec<(usize, usize)>) {
        out.clear();
        let n = self.n;
        if n == 0 {
            return;
        }
        // 1-based arrays of the classic formulation; index 0 is the virtual
        // start column.
        for buf in [&mut self.u, &mut self.v, &mut self.minv] {
            buf.clear();
            buf.resize(n + 1, 0.0);
        }
        for buf in [&mut self.p, &mut self.way] {
            buf.clear();
            buf.resize(n + 1, 0);
        }
        for i in 1..=n {
            self.p[0] = i;
            let mut j0 = 0usize;
            self.minv.iter_mut().for_each(|m| *m = f64::INFINITY);
            self.used.clear();
            self.used.resize(n + 1, false);
            loop {
                self.used[j0] = true;
                let i0 = self.p[j0];
                let mut delta = f64::INFINITY;
                let mut j1 = 0usize;
                for j in 1..=n {
                    if self.used[j] {
                        continue;
                    }
                    let cur = self.cost[(i0 - 1) * n + (j - 1)] - self.u[i0] - self.v[j];
                    if cur < self.minv[j] {
                        self.minv[j] = cur;
                        self.way[j] = j0;
                    }
                    if self.minv[j] < delta {
                        delta = self.minv[j];
                        j1 = j;
                    }
                }
                for j in 0..=n {
                    if self.used[j] {
                        self.u[self.p[j]] += delta;
                        self.v[j] -= delta;
                    } else {
                        self.minv[j] -= delta;
                    }
                }
                j0 = j1;
                if self.p[j0] == 0 {
                    break;
                }
            }
            loop {
                let j1 = self.way[j0];
                self.p[j0] = self.p[j1];
                j0 = j1;
                if j0 == 0 {
                    break;
                }
            }
        }
        for j in 1..=n {
            let i = self.p[j];
            if i == 0 {
                continue;
            }
            if self.cost[(i - 1) * n + (j - 1)] < FORBIDDEN * 0.5 {
                out.push((i - 1, j - 1));
            }
        }
        out.sort_unstable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_cheapest_total_and_skips_forbidden_pairs() {
        let mut a = Assigner::default();
        let mut out = Vec::new();
        a.reset(2, 3);
        // Greedy on row 0 would take col 0 and force row 1 onto col 2.
        a.set(0, 0, 0.1);
        a.set(0, 1, 0.2);
        a.set(1, 0, 0.15);
        a.set(1, 2, 0.9);
        a.solve(&mut out);
        assert_eq!(out, vec![(0, 1), (1, 0)]);

        a.reset(2, 2);
        a.set(0, 0, 0.5);
        a.solve(&mut out);
        assert_eq!(out, vec![(0, 0)], "an all-forbidden row stays unassigned");
    }
}
