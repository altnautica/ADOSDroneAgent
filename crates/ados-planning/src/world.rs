//! The planner's view of the world: an occupancy query over a bounded window
//! of space, and the planar adapter that flies a 2D map at a fixed height.
//!
//! Positions are metres in one local Cartesian frame whose third axis is
//! vertical. The planner never assumes a sign for "up"; height bands are given
//! in the same frame.

/// What a point of space is known to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    /// Observed empty.
    Free,
    /// Observed solid.
    Occupied,
    /// Never observed (or decayed back to unknown).
    Unknown,
}

/// An axis-aligned box, metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb {
    pub fn new(min: [f32; 3], max: [f32; 3]) -> Self {
        Self { min, max }
    }

    /// The smallest box holding both points.
    pub fn around(a: [f32; 3], b: [f32; 3]) -> Self {
        Self {
            min: [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])],
            max: [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])],
        }
    }

    pub fn contains(&self, p: [f32; 3]) -> bool {
        (0..3).all(|k| p[k] >= self.min[k] && p[k] <= self.max[k])
    }

    /// The box grown by `d[k]` on both sides of axis `k` (shrunk for negative
    /// `d`; an axis never inverts, it collapses to its centre).
    pub fn grow(&self, d: [f32; 3]) -> Self {
        let mut out = *self;
        for k in 0..3 {
            out.min[k] -= d[k];
            out.max[k] += d[k];
            if out.min[k] > out.max[k] {
                let c = 0.5 * (self.min[k] + self.max[k]);
                out.min[k] = c;
                out.max[k] = c;
            }
        }
        out
    }

    /// The overlap of two boxes, `None` when they are disjoint.
    pub fn intersect(&self, o: &Aabb) -> Option<Aabb> {
        let mut out = *self;
        for k in 0..3 {
            out.min[k] = self.min[k].max(o.min[k]);
            out.max[k] = self.max[k].min(o.max[k]);
            if out.min[k] > out.max[k] {
                return None;
            }
        }
        Some(out)
    }

    pub fn centre(&self) -> [f32; 3] {
        [
            0.5 * (self.min[0] + self.max[0]),
            0.5 * (self.min[1] + self.max[1]),
            0.5 * (self.min[2] + self.max[2]),
        ]
    }

    /// The point of the box nearest `p`.
    pub fn clamp(&self, p: [f32; 3]) -> [f32; 3] {
        [
            p[0].clamp(self.min[0], self.max[0]),
            p[1].clamp(self.min[1], self.max[1]),
            p[2].clamp(self.min[2], self.max[2]),
        ]
    }
}

/// A 3D occupancy map the planner reads: a voxel map with its distance field,
/// or anything that answers the same questions.
pub trait OccupancyQuery {
    /// Voxel edge, metres. The planner searches a lattice aligned to the
    /// map's voxels at this spacing.
    fn res(&self) -> f32;
    /// The window the map covers. Nothing outside it is ever queried.
    fn bounds(&self) -> Aabb;
    /// What the point is known to be.
    fn state(&self, p: [f32; 3]) -> Cell;
    /// Distance from `p` to the nearest known occupied point, metres
    /// (`f32::INFINITY` when nothing occupied is known nearby; a map may cap
    /// it). Unknown space does not count as occupied here.
    fn clearance(&self, p: [f32; 3]) -> f32;
    /// State and clearance together (one lookup for maps that can share
    /// it); the clearance of an occupied point is not needed and may be 0.
    fn sample(&self, p: [f32; 3]) -> (Cell, f32) {
        match self.state(p) {
            Cell::Occupied => (Cell::Occupied, 0.0),
            s => (s, self.clearance(p)),
        }
    }
}

impl<T: OccupancyQuery + ?Sized> OccupancyQuery for &T {
    fn res(&self) -> f32 {
        (**self).res()
    }
    fn bounds(&self) -> Aabb {
        (**self).bounds()
    }
    fn state(&self, p: [f32; 3]) -> Cell {
        (**self).state(p)
    }
    fn clearance(&self, p: [f32; 3]) -> f32 {
        (**self).clearance(p)
    }
    fn sample(&self, p: [f32; 3]) -> (Cell, f32) {
        (**self).sample(p)
    }
}

/// A 2D occupancy map (the planar lidar tier's grid).
pub trait PlanarQuery {
    fn res(&self) -> f32;
    /// `[min, max]` corners of the covered window.
    fn bounds(&self) -> [[f32; 2]; 2];
    fn state(&self, p: [f32; 2]) -> Cell;
    fn clearance(&self, p: [f32; 2]) -> f32;
}

/// A planar map flown at one height: an [`OccupancyQuery`] one voxel thick
/// centred on `z`, so the planner's lattice has a single layer and every
/// search, corridor and trajectory stays at that height.
#[derive(Debug, Clone, Copy)]
pub struct FixedHeight<'a, Q: ?Sized> {
    pub map: &'a Q,
    pub z: f32,
}

impl<'a, Q: PlanarQuery + ?Sized> FixedHeight<'a, Q> {
    pub fn new(map: &'a Q, z: f32) -> Self {
        Self { map, z }
    }
}

impl<Q: PlanarQuery + ?Sized> OccupancyQuery for FixedHeight<'_, Q> {
    fn res(&self) -> f32 {
        self.map.res()
    }
    fn bounds(&self) -> Aabb {
        let [lo, hi] = self.map.bounds();
        let h = 0.5 * self.map.res();
        Aabb::new([lo[0], lo[1], self.z - h], [hi[0], hi[1], self.z + h])
    }
    fn state(&self, p: [f32; 3]) -> Cell {
        self.map.state([p[0], p[1]])
    }
    fn clearance(&self, p: [f32; 3]) -> f32 {
        self.map.clearance([p[0], p[1]])
    }
}

pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn norm(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

pub(crate) fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    norm(sub(a, b))
}

pub(crate) fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Distance from `p` to the segment `a → b`.
pub(crate) fn dist_to_segment(p: [f32; 3], a: [f32; 3], b: [f32; 3]) -> f32 {
    let ab = sub(b, a);
    let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
    if l2 <= 1e-12 {
        return dist(p, a);
    }
    let ap = sub(p, a);
    let t = ((ap[0] * ab[0] + ap[1] * ab[1] + ap[2] * ab[2]) / l2).clamp(0.0, 1.0);
    dist(p, lerp(a, b, t))
}
