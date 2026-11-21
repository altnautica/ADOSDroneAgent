//! Banded LU factorisation with partial pivoting, and solves with the matrix
//! and its transpose, for three right-hand sides at once.

/// An `n × n` matrix with `kl` sub- and `ku` super-diagonals, stored by row
/// with room for the fill partial pivoting adds (`kl` more super-diagonals).
#[derive(Debug, Default)]
pub(crate) struct Band {
    n: usize,
    kl: usize,
    ku: usize,
    width: usize,
    a: Vec<f64>,
    piv: Vec<usize>,
}

impl Band {
    /// Zero an `n × n` matrix with bandwidths `kl`, `ku`.
    pub fn reset(&mut self, n: usize, kl: usize, ku: usize) {
        self.n = n;
        self.kl = kl;
        self.ku = ku;
        self.width = 2 * kl + ku + 1;
        let len = n * self.width;
        self.a.clear();
        self.a.resize(len, 0.0);
        self.piv.clear();
        self.piv.resize(n, 0);
    }

    fn at(&self, r: usize, c: usize) -> usize {
        debug_assert!(c + self.kl >= r && c <= r + self.kl + self.ku);
        r * self.width + c + self.kl - r
    }

    pub fn set(&mut self, r: usize, c: usize, v: f64) {
        let i = self.at(r, c);
        self.a[i] = v;
    }

    fn get(&self, r: usize, c: usize) -> f64 {
        self.a[self.at(r, c)]
    }

    fn last_col(&self, j: usize) -> usize {
        (j + self.kl + self.ku).min(self.n - 1)
    }

    /// Factor in place. `false` when the matrix is singular.
    pub fn factor(&mut self) -> bool {
        let n = self.n;
        for j in 0..n {
            let rmax = (j + self.kl).min(n - 1);
            let mut p = j;
            let mut best = self.get(j, j).abs();
            for r in j + 1..=rmax {
                let v = self.get(r, j).abs();
                if v > best {
                    best = v;
                    p = r;
                }
            }
            if best <= 1e-300 {
                return false;
            }
            self.piv[j] = p;
            let cmax = self.last_col(j);
            if p != j {
                for c in j..=cmax {
                    let (x, y) = (self.at(j, c), self.at(p, c));
                    self.a.swap(x, y);
                }
            }
            let d = self.get(j, j);
            for r in j + 1..=rmax {
                let l = self.get(r, j) / d;
                if l == 0.0 {
                    continue;
                }
                let i = self.at(r, j);
                self.a[i] = l;
                for c in j + 1..=cmax {
                    let u = self.get(j, c);
                    let i = self.at(r, c);
                    self.a[i] -= l * u;
                }
            }
        }
        true
    }

    /// Solve `A x = b` in place (after [`Band::factor`]).
    pub fn solve(&self, b: &mut [[f64; 3]]) {
        let n = self.n;
        for j in 0..n {
            let p = self.piv[j];
            if p != j {
                b.swap(j, p);
            }
            let bj = b[j];
            for r in j + 1..=(j + self.kl).min(n - 1) {
                let l = self.get(r, j);
                for k in 0..3 {
                    b[r][k] -= l * bj[k];
                }
            }
        }
        for j in (0..n).rev() {
            let mut s = b[j];
            for c in j + 1..=self.last_col(j) {
                let u = self.get(j, c);
                for k in 0..3 {
                    s[k] -= u * b[c][k];
                }
            }
            let d = self.get(j, j);
            b[j] = [s[0] / d, s[1] / d, s[2] / d];
        }
    }

    /// Solve `Aᵀ x = b` in place (after [`Band::factor`]).
    pub fn solve_t(&self, b: &mut [[f64; 3]]) {
        let n = self.n;
        let reach = self.kl + self.ku;
        for j in 0..n {
            let mut s = b[j];
            for c in j.saturating_sub(reach)..j {
                let u = self.get(c, j);
                for k in 0..3 {
                    s[k] -= u * b[c][k];
                }
            }
            let d = self.get(j, j);
            b[j] = [s[0] / d, s[1] / d, s[2] / d];
        }
        for j in (0..n).rev() {
            let mut s = b[j];
            for r in j + 1..=(j + self.kl).min(n - 1) {
                let l = self.get(r, j);
                for k in 0..3 {
                    s[k] -= l * b[r][k];
                }
            }
            b[j] = s;
            let p = self.piv[j];
            if p != j {
                b.swap(j, p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Band;

    fn dense_mul(a: &[Vec<f64>], x: &[[f64; 3]], t: bool) -> Vec<[f64; 3]> {
        let n = a.len();
        (0..n)
            .map(|r| {
                let mut s = [0.0; 3];
                for c in 0..n {
                    let v = if t { a[c][r] } else { a[r][c] };
                    for k in 0..3 {
                        s[k] += v * x[c][k];
                    }
                }
                s
            })
            .collect()
    }

    #[test]
    fn solves_match_dense_products() {
        let (n, kl, ku) = (24usize, 4usize, 3usize);
        let mut seed = 7u64;
        let mut rnd = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f64 / (1u64 << 31) as f64) - 0.5
        };
        let mut dense = vec![vec![0.0; n]; n];
        let mut band = Band::default();
        band.reset(n, kl, ku);
        for r in 0..n {
            for c in r.saturating_sub(kl)..=(r + ku).min(n - 1) {
                // Weak diagonal so pivoting is exercised.
                let v = rnd() + if r == c { 0.01 } else { 0.0 };
                dense[r][c] = v;
                band.set(r, c, v);
            }
        }
        assert!(band.factor());
        let x: Vec<[f64; 3]> = (0..n).map(|_| [rnd(), rnd(), rnd()]).collect();
        for t in [false, true] {
            let mut b = dense_mul(&dense, &x, t);
            if t {
                band.solve_t(&mut b);
            } else {
                band.solve(&mut b);
            }
            for (got, want) in b.iter().zip(&x) {
                for k in 0..3 {
                    assert!(
                        (got[k] - want[k]).abs() < 1e-8,
                        "transpose={t}: {got:?} vs {want:?}"
                    );
                }
            }
        }
    }
}
