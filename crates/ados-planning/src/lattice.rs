//! The search lattice (a box of the map's voxels) and the per-voxel class
//! cache every stage of one plan shares.
//!
//! A voxel's class is decided once per plan from the map: blocked (occupied,
//! or closer to known obstacles than the vehicle needs), free (observed
//! empty) or unknown (traversable at a cost). Searches may lay an overlay of
//! forbidden tubes on top (homotopy alternatives); the overlay and the derived
//! "uniform neighbourhood" bit are cached per search.

use crate::world::{dist, dist_to_segment, Aabb, Cell, OccupancyQuery};

pub(crate) const BLOCKED: u8 = 0;
pub(crate) const FREE: u8 = 1;
pub(crate) const UNKNOWN: u8 = 2;

/// Overlay flags (per search).
const HOMOG_KNOWN: u8 = 4;
const HOMOG: u8 = 8;
const IN_TUBE: u8 = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Lattice {
    /// Minimum corner, metres.
    pub origin: [f32; 3],
    pub res: f32,
    pub dims: [i32; 3],
}

impl Lattice {
    /// The voxels of `map` inside `window` (clipped to the map), aligned to
    /// the map's own voxel grid.
    pub fn over<W: OccupancyQuery + ?Sized>(map: &W, window: &Aabb) -> Option<Self> {
        let b = map.bounds();
        let res = map.res();
        let w = window.intersect(&b)?;
        let mut origin = [0.0; 3];
        let mut dims = [0; 3];
        for k in 0..3 {
            let total = ((b.max[k] - b.min[k]) / res).round().max(1.0);
            let lo = ((w.min[k] - b.min[k]) / res)
                .floor()
                .clamp(0.0, total - 1.0);
            let hi = ((w.max[k] - b.min[k]) / res).ceil().clamp(lo + 1.0, total);
            origin[k] = b.min[k] + lo * res;
            dims[k] = (hi - lo) as i32;
        }
        Some(Self { origin, res, dims })
    }

    pub fn len(&self) -> usize {
        (self.dims[0] * self.dims[1] * self.dims[2]) as usize
    }

    pub fn planar(&self) -> bool {
        self.dims[2] == 1
    }

    pub fn inside(&self, c: [i32; 3]) -> bool {
        (0..3).all(|k| c[k] >= 0 && c[k] < self.dims[k])
    }

    pub fn index(&self, c: [i32; 3]) -> usize {
        (c[0] + self.dims[0] * (c[1] + self.dims[1] * c[2])) as usize
    }

    pub fn coords(&self, i: usize) -> [i32; 3] {
        let i = i as i32;
        let nx = self.dims[0];
        let ny = self.dims[1];
        [i % nx, (i / nx) % ny, i / (nx * ny)]
    }

    pub fn centre(&self, c: [i32; 3]) -> [f32; 3] {
        [
            self.origin[0] + (c[0] as f32 + 0.5) * self.res,
            self.origin[1] + (c[1] as f32 + 0.5) * self.res,
            self.origin[2] + (c[2] as f32 + 0.5) * self.res,
        ]
    }

    /// The voxel holding `p`, `None` outside the lattice.
    pub fn cell_of(&self, p: [f32; 3]) -> Option<[i32; 3]> {
        let mut c = [0; 3];
        for k in 0..3 {
            let f = ((p[k] - self.origin[k]) / self.res).floor();
            if f < 0.0 || f >= self.dims[k] as f32 {
                return None;
            }
            c[k] = f as i32;
        }
        Some(c)
    }

    /// The world box covering voxels `lo..=hi`.
    pub fn span(&self, lo: [i32; 3], hi: [i32; 3]) -> Aabb {
        let mut b = Aabb::new([0.0; 3], [0.0; 3]);
        for k in 0..3 {
            b.min[k] = self.origin[k] + lo[k] as f32 * self.res;
            b.max[k] = self.origin[k] + (hi[k] + 1) as f32 * self.res;
        }
        b
    }

    /// Append the face-connected voxels the segment `a → b` passes through
    /// (voxel traversal), skipping one equal to the last already in `out`.
    /// Stops at the lattice edge.
    pub fn trace(&self, a: [f32; 3], b: [f32; 3], out: &mut Vec<[i32; 3]>) {
        let Some(mut c) = self.cell_of(a) else {
            return;
        };
        if out.last() != Some(&c) {
            out.push(c);
        }
        let mut step = [0i32; 3];
        let mut t_max = [f32::INFINITY; 3];
        let mut t_delta = [f32::INFINITY; 3];
        for k in 0..3 {
            let d = b[k] - a[k];
            if d.abs() < 1e-9 {
                continue;
            }
            step[k] = if d > 0.0 { 1 } else { -1 };
            let edge = self.origin[k] + (c[k] + i32::from(d > 0.0)) as f32 * self.res;
            t_max[k] = (edge - a[k]) / d;
            t_delta[k] = self.res / d.abs();
        }
        loop {
            let k = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
                0
            } else if t_max[1] <= t_max[2] {
                1
            } else {
                2
            };
            if t_max[k] > 1.0 {
                return;
            }
            c[k] += step[k];
            if !self.inside(c) {
                return;
            }
            out.push(c);
            t_max[k] += t_delta[k];
        }
    }
}

/// How close to known obstacles the vehicle may be.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Rules {
    /// Required clearance of any point the vehicle occupies (body radius
    /// plus margin), metres.
    pub need: f32,
    /// The vehicle's position when the plan was made.
    pub start: [f32; 3],
    /// Clearance at `start`. When it is under `need + slack`, points near the
    /// start may be flown at less (backing out of a margin it is already in).
    pub start_clear: f32,
    pub unknown_cost: f32,
}

/// Tubes round earlier paths that one search may not enter. Distances are
/// horizontal: a tube covers the lattice's whole height.
#[derive(Debug, Default)]
pub(crate) struct Tubes {
    pub points: Vec<[f32; 3]>,
    /// End index (exclusive) of each polyline in `points`.
    pub ends: Vec<usize>,
    pub radius: f32,
    /// Tubes never cover points this close (horizontally) to the search's
    /// ends.
    pub keep: [[f32; 3]; 2],
    pub keep_r: f32,
}

#[derive(Debug, Default)]
pub(crate) struct Space {
    pub lat: Lattice,
    pub rules: Rules,
    pub tubes: Tubes,
    /// Per voxel: class in the low two bits, then the uniform-neighbourhood
    /// flags; valid when `base_stamp` equals `plan_gen`. Classified lazily,
    /// on first touch, and never cleared between plans.
    base: Vec<u8>,
    base_stamp: Vec<u32>,
    plan_gen: u32,
    over_stamp: Vec<u32>,
    over: Vec<u8>,
    search_gen: u32,
}

const CLASS_BITS: u8 = 3;

impl Space {
    /// Half the voxel diagonal: how far a point in a voxel may be from its
    /// centre, so how much more than `need` a centre must clear.
    pub fn slack(&self) -> f32 {
        self.lat.res * 0.866_025_4
    }

    fn escape_radius(&self) -> f32 {
        self.rules.need + self.slack() + 2.0 * self.lat.res
    }

    /// Begin a plan on `lat`: every cached class is forgotten (by epoch, not
    /// by clearing).
    pub fn begin(&mut self, lat: Lattice, rules: Rules) {
        self.lat = lat;
        self.rules = rules;
        let n = lat.len();
        if self.base.len() < n {
            self.base.resize(n, 0);
            self.base_stamp.resize(n, 0);
            self.over.resize(n, 0);
            self.over_stamp.resize(n, 0);
        }
        self.plan_gen = self.plan_gen.wrapping_add(1);
        if self.plan_gen == 0 {
            self.base_stamp.fill(0);
            self.plan_gen = 1;
        }
        self.begin_search();
    }
    /// Begin a search with no tubes.
    pub fn begin_search(&mut self) {
        self.tubes.points.clear();
        self.tubes.ends.clear();
        self.next_gen();
    }

    fn next_gen(&mut self) {
        self.search_gen = self.search_gen.wrapping_add(1);
        if self.search_gen == 0 {
            self.over_stamp.fill(0);
            self.search_gen = 1;
        }
    }

    /// Begin a search that may not enter the tubes now in `self.tubes`:
    /// mark the voxels inside them, and the band round them whose
    /// neighbourhood they change. A tube is the horizontal distance from
    /// the path over the lattice's whole height: an alternative must be
    /// another way round sideways, not the same way above or below.
    pub fn lay_tubes(&mut self) {
        self.next_gen();
        let lat = self.lat;
        let r = self.tubes.radius;
        let reach = r + 2.0 * lat.res;
        let flat = |p: [f32; 3]| [p[0], p[1], 0.0];
        let mut s = 0;
        for e in 0..self.tubes.ends.len() {
            let end = self.tubes.ends[e];
            for k in s..end.saturating_sub(1) {
                let (a, b) = (flat(self.tubes.points[k]), flat(self.tubes.points[k + 1]));
                let mut lo = [0i32; 2];
                let mut hi = [0i32; 2];
                for ax in 0..2 {
                    let m = a[ax].min(b[ax]) - reach - lat.origin[ax];
                    let x = a[ax].max(b[ax]) + reach - lat.origin[ax];
                    lo[ax] = ((m / lat.res).floor() as i32).max(0);
                    hi[ax] = ((x / lat.res).floor() as i32).min(lat.dims[ax] - 1);
                }
                let dy = b[1] - a[1];
                for y in lo[1]..=hi[1] {
                    // The columns of a row within reach are one run (the
                    // distance is convex along it) round the point of the
                    // row nearest the segment.
                    let qy = lat.origin[1] + (y as f32 + 0.5) * lat.res;
                    let t = if dy.abs() > 1e-6 {
                        ((qy - a[1]) / dy).clamp(0.0, 1.0)
                    } else {
                        0.5
                    };
                    let x0 = a[0] + t * (b[0] - a[0]);
                    let f = ((x0 - lat.origin[0]) / lat.res - 0.5).floor() as i32;
                    let f = f.clamp(lo[0] - 1, hi[0]);
                    let mut x = f;
                    while x >= lo[0] && self.mark_column(x, y, a, b, r, reach) {
                        x -= 1;
                    }
                    let mut x = f + 1;
                    while x <= hi[0] && self.mark_column(x, y, a, b, r, reach) {
                        x += 1;
                    }
                }
            }
            s = end;
        }
    }

    /// Mark the column `(x, y)` against the tube round the (flattened)
    /// segment `a → b`; `false` when it is beyond `reach`.
    fn mark_column(
        &mut self,
        x: i32,
        y: i32,
        a: [f32; 3],
        b: [f32; 3],
        r: f32,
        reach: f32,
    ) -> bool {
        let c = self.lat.centre([x, y, 0]);
        let p = [c[0], c[1], 0.0];
        let d = dist_to_segment(p, a, b);
        if d > reach {
            return false;
        }
        let inside = d < r
            && self.tubes.keep.iter().all(|q| {
                let (ex, ey) = (q[0] - p[0], q[1] - p[1]);
                (ex * ex + ey * ey).sqrt() > self.tubes.keep_r
            });
        let layer = (self.lat.dims[0] * self.lat.dims[1]) as usize;
        let mut i = self.lat.index([x, y, 0]);
        for _ in 0..self.lat.dims[2] {
            if self.over_stamp[i] != self.search_gen {
                self.over_stamp[i] = self.search_gen;
                self.over[i] = 0;
            }
            if inside {
                self.over[i] |= IN_TUBE;
            }
            i += layer;
        }
        true
    }

    /// Clearance a voxel centre at `p` must have.
    fn need_centre(&self, p: [f32; 3]) -> f32 {
        let full = self.rules.need + self.slack();
        if self.rules.start_clear < full && dist(p, self.rules.start) <= self.escape_radius() {
            full.min((self.rules.start_clear - self.slack()).max(0.0))
        } else {
            full
        }
    }

    /// Clearance an arbitrary point `p` of a flown trajectory must have.
    pub fn need_point(&self, p: [f32; 3]) -> f32 {
        let full = self.rules.need + self.slack();
        let r = self.escape_radius() + self.slack();
        if self.rules.start_clear < full && dist(p, self.rules.start) <= r {
            self.rules
                .need
                .min((self.rules.start_clear - 2.0 * self.slack()).max(0.0))
        } else {
            self.rules.need
        }
    }

    /// The map's class of voxel `c` (no overlay), `BLOCKED` off the lattice.
    pub fn base<W: OccupancyQuery + ?Sized>(&mut self, w: &W, c: [i32; 3]) -> u8 {
        if !self.lat.inside(c) {
            return BLOCKED;
        }
        let i = self.lat.index(c);
        if self.base_stamp[i] == self.plan_gen {
            return self.base[i] & CLASS_BITS;
        }
        let p = self.lat.centre(c);
        let (st, clear) = w.sample(p);
        let cls = match st {
            Cell::Occupied => BLOCKED,
            _ if clear < self.need_centre(p) => BLOCKED,
            Cell::Free => FREE,
            Cell::Unknown => UNKNOWN,
        };
        self.base_stamp[i] = self.plan_gen;
        self.base[i] = cls;
        cls
    }

    fn near_tube(&self, i: usize) -> bool {
        self.over_stamp[i] == self.search_gen
    }

    /// The class the current search sees (tubes applied).
    pub fn class<W: OccupancyQuery + ?Sized>(&mut self, w: &W, c: [i32; 3]) -> u8 {
        if !self.lat.inside(c) {
            return BLOCKED;
        }
        let i = self.lat.index(c);
        if self.near_tube(i) && self.over[i] & IN_TUBE != 0 {
            return BLOCKED;
        }
        self.base(w, c)
    }

    /// True when the voxel and all 26 neighbours are traversable and of one
    /// class: no neighbour of it is ever forced, every move from it is valid.
    pub fn uniform<W: OccupancyQuery + ?Sized>(&mut self, w: &W, c: [i32; 3]) -> bool {
        let own = self.class(w, c);
        if own == BLOCKED {
            return false;
        }
        let i = self.lat.index(c);
        // Away from tubes the flag belongs to the map and lives in `base`;
        // near them it belongs to this search and lives in `over`.
        let tube = self.near_tube(i);
        let f = if tube { self.over[i] } else { self.base[i] };
        if f & HOMOG_KNOWN != 0 {
            return f & HOMOG != 0;
        }
        let mut same = true;
        'outer: for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if self.class(w, [c[0] + dx, c[1] + dy, c[2] + dz]) != own {
                        same = false;
                        break 'outer;
                    }
                }
            }
        }
        let flag = HOMOG_KNOWN | if same { HOMOG } else { 0 };
        if tube {
            self.over[i] |= flag;
        } else {
            self.base[i] |= flag;
        }
        same
    }

    /// Classes of the 3×3×3 cube around `c` and the mask of its blocked cells.
    pub fn cube<W: OccupancyQuery + ?Sized>(&mut self, w: &W, c: [i32; 3]) -> ([u8; 27], u32) {
        let mut cls = [BLOCKED; 27];
        let mut blocked = 0u32;
        let mut k = 0;
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let v = self.class(w, [c[0] + dx, c[1] + dy, c[2] + dz]);
                    cls[k] = v;
                    if v == BLOCKED {
                        blocked |= 1 << k;
                    }
                    k += 1;
                }
            }
        }
        (cls, blocked)
    }

    /// Cost multiplier of a class.
    pub fn mult(&self, cls: u8) -> f32 {
        if cls == UNKNOWN {
            self.rules.unknown_cost
        } else {
            1.0
        }
    }

    /// True when a flown point keeps the clearance the plan needs (and, with
    /// `known_only`, lies in observed free space).
    pub fn point_ok<W: OccupancyQuery + ?Sized>(
        &self,
        w: &W,
        p: [f32; 3],
        known_only: bool,
    ) -> bool {
        match w.state(p) {
            Cell::Occupied => false,
            Cell::Unknown if known_only => false,
            _ => w.clearance(p) >= self.need_point(p) - 1e-4,
        }
    }
}
