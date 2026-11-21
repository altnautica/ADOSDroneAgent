//! Safe flight corridors: overlapping boxes of traversable voxels grown along
//! a front-end path. Each box first stretches along the path as far as it
//! stays traversable, then inflates face by face; the next box starts at the
//! last path voxel the previous one holds, so consecutive boxes always share
//! at least that voxel. Boxes whose stretch of path their neighbours already
//! hold are dropped.

use crate::lattice::{Lattice, Space, BLOCKED, FREE};
use crate::world::{Aabb, OccupancyQuery};

/// Boxes in flight order, and the point each consecutive pair shares (the
/// seed of the waypoint between them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Corridor {
    pub boxes: Vec<Aabb>,
    pub seeds: Vec<[f32; 3]>,
    /// Path voxel range `(first, last)` each box was grown over.
    spans: Vec<(usize, usize)>,
}

impl Corridor {
    /// True when some box holds `p`.
    pub fn contains(&self, p: [f32; 3]) -> bool {
        self.boxes.iter().any(|b| b.contains(p))
    }

    pub fn clear(&mut self) {
        self.boxes.clear();
        self.seeds.clear();
        self.spans.clear();
    }
}

struct Grower<'a, W: ?Sized> {
    w: &'a W,
    sp: &'a mut Space,
    known_only: bool,
}

impl<W: OccupancyQuery + ?Sized> Grower<'_, W> {
    fn ok(&mut self, c: [i32; 3]) -> bool {
        let cls = self.sp.base(self.w, c);
        if self.known_only {
            cls == FREE
        } else {
            cls != BLOCKED
        }
    }

    fn region(&mut self, lo: [i32; 3], hi: [i32; 3]) -> bool {
        for z in lo[2]..=hi[2] {
            for y in lo[1]..=hi[1] {
                for x in lo[0]..=hi[0] {
                    if !self.ok([x, y, z]) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Check the voxels of box `n` outside box `o` (`o ⊆ n`), as six slabs.
    fn grown(&mut self, o: ([i32; 3], [i32; 3]), n: ([i32; 3], [i32; 3])) -> bool {
        let ((ol, oh), (nl, nh)) = (o, n);
        let mut lo = nl;
        let mut hi = nh;
        for k in 0..3 {
            if nl[k] < ol[k] {
                let mut h = hi;
                h[k] = ol[k] - 1;
                if !self.region(lo, h) {
                    return false;
                }
            }
            if nh[k] > oh[k] {
                let mut l = lo;
                l[k] = oh[k] + 1;
                if !self.region(l, hi) {
                    return false;
                }
            }
            // Later slabs only cover the old extent of this axis.
            lo[k] = ol[k];
            hi[k] = oh[k];
        }
        true
    }
}

fn fits(lo: [i32; 3], hi: [i32; 3], max: [i32; 3]) -> bool {
    (0..3).all(|k| hi[k] - lo[k] < max[k])
}

/// Grow a corridor along `cells` (consecutive voxels a valid move apart).
/// `known_only` admits observed-free voxels only (a backup's corridor);
/// otherwise unknown voxels are admitted too. `max` caps a box's extent in
/// voxels per axis. `false` when the path leaves admissible space.
pub(crate) fn build<W: OccupancyQuery + ?Sized>(
    w: &W,
    sp: &mut Space,
    cells: &[[i32; 3]],
    known_only: bool,
    max: [i32; 3],
    out: &mut Corridor,
) -> bool {
    out.clear();
    let n = cells.len();
    if n == 0 {
        return false;
    }
    let lat = sp.lat;
    let mut g = Grower { w, sp, known_only };
    let mut i = 0;
    loop {
        let (mut lo, mut hi) = (cells[i], cells[i]);
        if !g.region(lo, hi) {
            return false;
        }
        let mut j = i;
        while j + 1 < n {
            let c = cells[j + 1];
            let nl = [lo[0].min(c[0]), lo[1].min(c[1]), lo[2].min(c[2])];
            let nh = [hi[0].max(c[0]), hi[1].max(c[1]), hi[2].max(c[2])];
            if !fits(nl, nh, max) || !g.grown((lo, hi), (nl, nh)) {
                break;
            }
            lo = nl;
            hi = nh;
            j += 1;
        }
        if j == i && j + 1 < n {
            return false;
        }
        loop {
            let mut grew = false;
            for face in 0..6 {
                let k = face / 2;
                let (mut nl, mut nh) = (lo, hi);
                if face % 2 == 0 {
                    nl[k] -= 1;
                } else {
                    nh[k] += 1;
                }
                if nl[k] < 0 || nh[k] >= lat.dims[k] || !fits(nl, nh, max) {
                    continue;
                }
                if g.grown((lo, hi), (nl, nh)) {
                    lo = nl;
                    hi = nh;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        out.boxes.push(lat.span(lo, hi));
        out.spans.push((i, j));
        if j + 1 >= n {
            prune(&lat, cells, out);
            return true;
        }
        out.seeds.push(lat.centre(cells[j]));
        i = j;
    }
}

/// True when `b` holds both voxels and the box they span.
fn holds(lat: &Lattice, b: &Aabb, a: [i32; 3], c: [i32; 3]) -> bool {
    let s = lat.span(
        [a[0].min(c[0]), a[1].min(c[1]), a[2].min(c[2])],
        [a[0].max(c[0]), a[1].max(c[1]), a[2].max(c[2])],
    );
    let e = 1e-4;
    (0..3).all(|k| s.min[k] >= b.min[k] - e && s.max[k] <= b.max[k] + e)
}

/// Drop each box whose path stretch (and the steps into and out of it) its
/// two neighbours already hold, when those neighbours overlap by at least a
/// voxel: fewer, longer pieces for the optimiser, the same path covered.
fn prune(lat: &Lattice, cells: &[[i32; 3]], out: &mut Corridor) {
    let res = lat.res;
    let mut k = 1;
    while k + 1 < out.boxes.len() {
        let (prev, next) = (out.boxes[k - 1], out.boxes[k + 1]);
        let overlap = prev
            .intersect(&next)
            .filter(|o| (0..3).all(|a| o.max[a] - o.min[a] >= res * 0.999));
        let Some(o) = overlap else {
            k += 1;
            continue;
        };
        let (i, j) = out.spans[k];
        let from = i.saturating_sub(1);
        let to = (j + 1).min(cells.len() - 1);
        let covered = (from..to).all(|m| {
            holds(lat, &prev, cells[m], cells[m + 1]) || holds(lat, &next, cells[m], cells[m + 1])
        });
        if !covered {
            k += 1;
            continue;
        }
        out.boxes.remove(k);
        out.spans.remove(k);
        out.seeds.remove(k);
        // The seed between the new neighbours: a voxel centre in their
        // overlap.
        let c = o.centre();
        let v = lat.cell_of(c).map(|v| lat.centre(v)).unwrap_or(c);
        out.seeds[k - 1] = if o.contains(v) { v } else { c };
    }
}
