//! Shared test worlds: a seeded random source, a Poisson forest of vertical
//! trunks seen through a moving sensing window, and simple grid maps.

#![allow(dead_code)]

use ados_planning::{Aabb, Cell, DenseGrid, OccupancyQuery, PlanarQuery};

/// SplitMix64.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x5851_f42d_4c95_7f2d)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit() as f32
    }

    /// Poisson-distributed count with mean `mean` (inversion by sequential
    /// search).
    pub fn poisson(&mut self, mean: f64) -> usize {
        let u = self.unit();
        let mut k = 0usize;
        let mut p = (-mean).exp();
        let mut cdf = p;
        while u > cdf && k < 100_000 {
            k += 1;
            p *= mean / k as f64;
            cdf += p;
        }
        k
    }
}

/// Vertical trunks (infinite columns from the ground up).
pub struct Forest {
    pub trees: Vec<([f32; 2], f32)>,
    origin: [f32; 2],
    bucket: f32,
    dims: [usize; 2],
    buckets: Vec<Vec<u32>>,
}

/// Clearance reported beyond this distance is capped, metres.
pub const CLEAR_CAP: f32 = 4.0;

impl Forest {
    /// Homogeneous Poisson stand of intensity `lambda` per m² over
    /// `[lo, hi]`, trunk radii uniform in `r`, no trunk within `keep_r` of
    /// any `keep` point.
    pub fn poisson(
        seed: u64,
        lambda: f64,
        lo: [f32; 2],
        hi: [f32; 2],
        r: (f32, f32),
        keep: &[[f32; 2]],
        keep_r: f32,
    ) -> Self {
        let mut rng = Rng::new(seed);
        let area = ((hi[0] - lo[0]) * (hi[1] - lo[1])) as f64;
        let n = rng.poisson(lambda * area);
        let mut trees = Vec::with_capacity(n);
        for _ in 0..n {
            let c = [rng.range(lo[0], hi[0]), rng.range(lo[1], hi[1])];
            let rad = rng.range(r.0, r.1);
            let clear = keep
                .iter()
                .all(|k| ((c[0] - k[0]).powi(2) + (c[1] - k[1]).powi(2)).sqrt() - rad > keep_r);
            if clear {
                trees.push((c, rad));
            }
        }
        Self::from_trees(trees, lo, hi)
    }

    pub fn from_trees(trees: Vec<([f32; 2], f32)>, lo: [f32; 2], hi: [f32; 2]) -> Self {
        let bucket = 2.0;
        let origin = [lo[0] - 10.0, lo[1] - 10.0];
        let dims = [
            ((hi[0] - lo[0] + 20.0) / bucket).ceil() as usize,
            ((hi[1] - lo[1] + 20.0) / bucket).ceil() as usize,
        ];
        let mut buckets = vec![Vec::new(); dims[0] * dims[1]];
        for (i, (c, _)) in trees.iter().enumerate() {
            let bx = ((c[0] - origin[0]) / bucket) as usize;
            let by = ((c[1] - origin[1]) / bucket) as usize;
            buckets[bx + dims[0] * by].push(i as u32);
        }
        Self {
            trees,
            origin,
            bucket,
            dims,
            buckets,
        }
    }

    /// Visit trees whose centre is within `reach` of `p` (horizontally).
    fn near(&self, p: [f32; 2], reach: f32, mut f: impl FnMut(&([f32; 2], f32))) {
        let span = (reach / self.bucket).ceil() as i64 + 1;
        let bx = ((p[0] - self.origin[0]) / self.bucket).floor() as i64;
        let by = ((p[1] - self.origin[1]) / self.bucket).floor() as i64;
        for y in by - span..=by + span {
            for x in bx - span..=bx + span {
                if x < 0 || y < 0 || x >= self.dims[0] as i64 || y >= self.dims[1] as i64 {
                    continue;
                }
                for &i in &self.buckets[x as usize + self.dims[0] * y as usize] {
                    f(&self.trees[i as usize]);
                }
            }
        }
    }

    /// True distance from `p` to the nearest trunk surface (capped), only
    /// counting trees `seen` accepts.
    pub fn clearance_by(&self, p: [f32; 2], seen: impl Fn(&([f32; 2], f32)) -> bool) -> f32 {
        let mut best = CLEAR_CAP;
        self.near(p, CLEAR_CAP + 0.5, |t| {
            if seen(t) {
                let d = ((p[0] - t.0[0]).powi(2) + (p[1] - t.0[1]).powi(2)).sqrt() - t.1;
                best = best.min(d);
            }
        });
        best.max(0.0)
    }

    pub fn truth(&self, p: [f32; 2]) -> f32 {
        self.clearance_by(p, |_| true)
    }

    /// Exact distance to the nearest trunk surface, uncapped (all trees).
    pub fn truth_exact(&self, p: [f32; 2]) -> f32 {
        self.trees
            .iter()
            .map(|(c, r)| ((p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sqrt() - r)
            .fold(f32::INFINITY, f32::min)
    }
}

/// The forest as a sensor sees it from `centre`: a square window, trees
/// within `sense_r` known, free space known within `sense_r`, the rest
/// unknown. The ground (below z = 0) is occupied.
pub struct View {
    pub centre: [f32; 3],
    pub half: f32,
    pub z: [f32; 2],
    pub res: f32,
    pub sense_r: f32,
    /// The trees within sensing range.
    seen: Vec<([f32; 2], f32)>,
}

impl View {
    /// A 30 × 30 m window, 0..6 m high, 0.15 m voxels, 12 m sensing.
    pub fn new(forest: &Forest, centre: [f32; 3]) -> Self {
        let sense_r = 12.0;
        let seen = forest
            .trees
            .iter()
            .filter(|t| {
                ((t.0[0] - centre[0]).powi(2) + (t.0[1] - centre[1]).powi(2)).sqrt()
                    <= sense_r + t.1
            })
            .copied()
            .collect();
        Self {
            centre,
            half: 15.0,
            z: [0.0, 6.0],
            res: 0.15,
            sense_r,
            seen,
        }
    }
}

impl OccupancyQuery for View {
    fn res(&self) -> f32 {
        self.res
    }

    fn bounds(&self) -> Aabb {
        Aabb::new(
            [
                self.centre[0] - self.half,
                self.centre[1] - self.half,
                self.z[0],
            ],
            [
                self.centre[0] + self.half,
                self.centre[1] + self.half,
                self.z[1],
            ],
        )
    }

    fn state(&self, p: [f32; 3]) -> Cell {
        if p[2] < 0.0 {
            return Cell::Occupied;
        }
        let inside = self
            .seen
            .iter()
            .any(|t| (p[0] - t.0[0]).powi(2) + (p[1] - t.0[1]).powi(2) <= t.1 * t.1);
        if inside {
            return Cell::Occupied;
        }
        let d = ((p[0] - self.centre[0]).powi(2) + (p[1] - self.centre[1]).powi(2)).sqrt();
        if d <= self.sense_r {
            Cell::Free
        } else {
            Cell::Unknown
        }
    }

    fn clearance(&self, p: [f32; 3]) -> f32 {
        let trees = self
            .seen
            .iter()
            .map(|t| ((p[0] - t.0[0]).powi(2) + (p[1] - t.0[1]).powi(2)).sqrt() - t.1)
            .fold(CLEAR_CAP, f32::min);
        trees.max(0.0).min(p[2].max(0.0))
    }
}

/// A 2D map from a boolean/unknown grid: clearance infinite off obstacles.
pub struct Grid2 {
    pub res: f32,
    pub nx: usize,
    pub ny: usize,
    pub cells: Vec<Cell>,
}

impl Grid2 {
    pub fn at(&self, p: [f32; 2]) -> Cell {
        let x = (p[0] / self.res).floor();
        let y = (p[1] / self.res).floor();
        if x < 0.0 || y < 0.0 || x >= self.nx as f32 || y >= self.ny as f32 {
            return Cell::Occupied;
        }
        self.cells[x as usize + self.nx * y as usize]
    }
}

impl PlanarQuery for Grid2 {
    fn res(&self) -> f32 {
        self.res
    }
    fn bounds(&self) -> [[f32; 2]; 2] {
        [
            [0.0, 0.0],
            [self.nx as f32 * self.res, self.ny as f32 * self.res],
        ]
    }
    fn state(&self, p: [f32; 2]) -> Cell {
        self.at(p)
    }
    fn clearance(&self, p: [f32; 2]) -> f32 {
        if self.at(p) == Cell::Occupied {
            0.0
        } else {
            f32::INFINITY
        }
    }
}

/// A 3D map from a cell grid: clearance infinite off obstacles.
pub struct Grid3 {
    pub res: f32,
    pub dims: [usize; 3],
    pub cells: Vec<Cell>,
}

impl OccupancyQuery for Grid3 {
    fn res(&self) -> f32 {
        self.res
    }
    fn bounds(&self) -> Aabb {
        Aabb::new(
            [0.0; 3],
            [
                self.dims[0] as f32 * self.res,
                self.dims[1] as f32 * self.res,
                self.dims[2] as f32 * self.res,
            ],
        )
    }
    fn state(&self, p: [f32; 3]) -> Cell {
        let mut i = [0usize; 3];
        for k in 0..3 {
            let f = (p[k] / self.res).floor();
            if f < 0.0 || f >= self.dims[k] as f32 {
                return Cell::Occupied;
            }
            i[k] = f as usize;
        }
        self.cells[i[0] + self.dims[0] * (i[1] + self.dims[1] * i[2])]
    }
    fn clearance(&self, p: [f32; 3]) -> f32 {
        if self.state(p) == Cell::Occupied {
            0.0
        } else {
            f32::INFINITY
        }
    }
}

/// A dense forest map over `[0, size]²`, ground below `ground_z`, trunks
/// as columns, everything farther than `known_r` from `centre` unknown.
pub fn dense_forest(
    forest: &Forest,
    size: f32,
    height: f32,
    res: f32,
    ground_z: f32,
    centre: [f32; 2],
    known_r: f32,
) -> DenseGrid {
    let n = (size / res).round() as usize;
    let nz = (height / res).round() as usize;
    let mut g = DenseGrid::new([0.0; 3], res, [n, n, nz], Cell::Free);
    for y in 0..n {
        for x in 0..n {
            let c = g.centre([x, y, 0]);
            let p = [c[0], c[1]];
            let far = ((p[0] - centre[0]).powi(2) + (p[1] - centre[1]).powi(2)).sqrt() > known_r;
            let trunk = forest.truth_exact(p) <= 0.0;
            for z in 0..nz {
                let cz = g.centre([x, y, z])[2];
                let cell = if far {
                    Cell::Unknown
                } else if cz < ground_z || trunk {
                    Cell::Occupied
                } else {
                    Cell::Free
                };
                g.set([x, y, z], cell);
            }
        }
    }
    g.update_esdf();
    g
}
