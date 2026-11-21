//! A dense voxel map with an exact Euclidean distance field: the reference
//! [`OccupancyQuery`] for benches, simulations and tests.

use crate::world::{Aabb, Cell, OccupancyQuery};

/// Squared distance standing in for "no occupied voxel on this line".
const FAR: f32 = 1.0e20;

/// Voxels on a regular grid, `x` fastest, with the distance from every voxel
/// centre to the nearest occupied voxel centre.
#[derive(Debug, Clone)]
pub struct DenseGrid {
    origin: [f32; 3],
    res: f32,
    dims: [usize; 3],
    cells: Vec<Cell>,
    dist: Vec<f32>,
}

impl DenseGrid {
    /// A grid whose minimum corner is `origin`, every voxel `fill`. The
    /// distance field is empty (infinite) until [`DenseGrid::update_esdf`].
    pub fn new(origin: [f32; 3], res: f32, dims: [usize; 3], fill: Cell) -> Self {
        let n = dims[0] * dims[1] * dims[2];
        Self {
            origin,
            res,
            dims,
            cells: vec![fill; n],
            dist: vec![f32::INFINITY; n],
        }
    }

    pub fn dims(&self) -> [usize; 3] {
        self.dims
    }

    pub fn origin(&self) -> [f32; 3] {
        self.origin
    }

    fn flat(&self, i: [usize; 3]) -> usize {
        i[0] + self.dims[0] * (i[1] + self.dims[1] * i[2])
    }

    /// The voxel holding `p`, `None` outside the grid.
    pub fn voxel(&self, p: [f32; 3]) -> Option<[usize; 3]> {
        let mut out = [0usize; 3];
        for k in 0..3 {
            let f = ((p[k] - self.origin[k]) / self.res).floor();
            if f < 0.0 || f >= self.dims[k] as f32 {
                return None;
            }
            out[k] = f as usize;
        }
        Some(out)
    }

    /// The centre of voxel `i`.
    pub fn centre(&self, i: [usize; 3]) -> [f32; 3] {
        [
            self.origin[0] + (i[0] as f32 + 0.5) * self.res,
            self.origin[1] + (i[1] as f32 + 0.5) * self.res,
            self.origin[2] + (i[2] as f32 + 0.5) * self.res,
        ]
    }

    pub fn get(&self, i: [usize; 3]) -> Cell {
        self.cells[self.flat(i)]
    }

    pub fn set(&mut self, i: [usize; 3], c: Cell) {
        let f = self.flat(i);
        self.cells[f] = c;
    }

    /// Recompute the distance field: the exact Euclidean distance from every
    /// voxel centre to the nearest occupied voxel centre (separable lower
    /// envelope of parabolas, one pass per axis).
    pub fn update_esdf(&mut self) {
        let [nx, ny, nz] = self.dims;
        let longest = nx.max(ny).max(nz);
        let mut f = vec![0.0f32; longest];
        let mut d = vec![0.0f32; longest];
        let mut v = vec![0usize; longest];
        let mut z = vec![0.0f32; longest + 1];
        for (out, c) in self.dist.iter_mut().zip(&self.cells) {
            *out = if *c == Cell::Occupied { 0.0 } else { FAR };
        }
        for axis in 0..3 {
            let (n, stride) = match axis {
                0 => (nx, 1),
                1 => (ny, nx),
                _ => (nz, nx * ny),
            };
            let lines: Vec<usize> = (0..nx * ny * nz)
                .filter(|&i| {
                    let c = [i % nx, (i / nx) % ny, i / (nx * ny)];
                    c[axis] == 0
                })
                .collect();
            for base in lines {
                for k in 0..n {
                    f[k] = self.dist[base + k * stride];
                }
                envelope(&f[..n], &mut d[..n], &mut v, &mut z);
                for k in 0..n {
                    self.dist[base + k * stride] = d[k];
                }
            }
        }
        for x in &mut self.dist {
            *x = if *x >= FAR * 0.5 {
                f32::INFINITY
            } else {
                x.sqrt() * self.res
            };
        }
    }
}

/// One-dimensional squared distance transform of the sampled function `f`.
fn envelope(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for q in 1..n {
        let fq = f[q] + (q * q) as f32;
        let mut s;
        loop {
            let p = v[k];
            s = (fq - (f[p] + (p * p) as f32)) / (2.0 * (q as f32 - p as f32));
            // z[0] is minus infinity, so this never steps below the first
            // parabola.
            if s <= z[k] {
                k -= 1;
            } else {
                break;
            }
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate().take(n) {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let p = v[k];
        let dq = q as f32 - p as f32;
        *out = dq * dq + f[p];
    }
}

impl OccupancyQuery for DenseGrid {
    fn res(&self) -> f32 {
        self.res
    }

    fn bounds(&self) -> Aabb {
        let mut max = self.origin;
        for (k, m) in max.iter_mut().enumerate() {
            *m += self.dims[k] as f32 * self.res;
        }
        Aabb::new(self.origin, max)
    }

    fn state(&self, p: [f32; 3]) -> Cell {
        match self.voxel(p) {
            Some(i) => self.get(i),
            None => Cell::Unknown,
        }
    }

    /// The voxel's centre distance less half a voxel: the surface of the
    /// nearest occupied voxel rather than its centre.
    fn clearance(&self, p: [f32; 3]) -> f32 {
        match self.voxel(p) {
            Some(i) => (self.dist[self.flat(i)] - 0.5 * self.res).max(0.0),
            None => f32::INFINITY,
        }
    }

    fn sample(&self, p: [f32; 3]) -> (Cell, f32) {
        match self.voxel(p) {
            Some(i) => {
                let f = self.flat(i);
                (self.cells[f], (self.dist[f] - 0.5 * self.res).max(0.0))
            }
            None => (Cell::Unknown, f32::INFINITY),
        }
    }
}
