//! The map's slab around the vehicle read as a planar scan: for each bearing,
//! the first column holding a wall, or where seen-free space ends, and the
//! lowest thing over the slab along the way (a canopy's underside).

use super::{Band, VoxWorld};

/// Bearings a world scan reads, one per degree, as a planar scan's bins.
pub const WORLD_SCAN_BINS: usize = 360;

/// One bearing of [`VoxWorld::scan`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanBin {
    /// Horizontal metres to the first column with an occupied voxel in the
    /// wall slab (where the bearing enters it), if one comes before seen
    /// space ends.
    pub hit_m: Option<f32>,
    /// Horizontal metres seen: up to the first column not observed free all
    /// through the seen slab (unknown space is never free), the hit, the
    /// window's edge, or the scan's reach.
    pub seen_m: f32,
    /// Over the columns walked (to `seen_m`), the lowest occupied voxel over
    /// the wall slab: (horizontal metres to its column, height of its
    /// underside over the vehicle).
    pub overhead: Option<(f32, f32)>,
}

/// Layer ranges a march reads, window `k` (down).
#[derive(Clone, Copy)]
struct Layers {
    walls: Option<(i64, i64)>,
    seen: (i64, i64),
    over: Option<(i64, i64)>,
}

impl VoxWorld {
    /// Read [`WORLD_SCAN_BINS`] bearings from local NED position `pos`, bin
    /// `i` at `yaw + i°` (clockwise from north), out to `max_m`: a column is
    /// a wall when `walls` holds an occupied voxel in it, and seen when
    /// every voxel of `seen` in it was observed free; everything in the
    /// window over `walls` counts as overhead. Without a live map tie (or
    /// with a slab outside the window) every bin is seen to 0.
    pub fn scan(
        &self,
        pos: [f32; 3],
        yaw: f32,
        walls: Option<Band>,
        seen: Option<Band>,
        max_m: f32,
    ) -> Vec<ScanBin> {
        let blind = ScanBin {
            hit_m: None,
            seen_m: 0.0,
            overhead: None,
        };
        let (Some(g0), Some(seen)) = (self.to_window(pos), seen.and_then(|b| self.layers(b)))
        else {
            return vec![blind; WORLD_SCAN_BINS];
        };
        let walls = walls.and_then(|b| self.layers(b));
        let over = walls.and_then(|w| (w.0 > 0).then_some((0, w.0 - 1)));
        let layers = Layers { walls, seen, over };
        (0..WORLD_SCAN_BINS)
            .map(|i| {
                let b = yaw + (i as f32).to_radians();
                self.march(g0, [b.cos(), b.sin()], layers, max_m)
            })
            .collect()
    }

    /// The lowest occupied layer of window column `(i, j)` within `ks`.
    fn lowest_occupied(&self, i: i64, j: i64, ks: (i64, i64)) -> Option<i64> {
        (ks.0..=ks.1).rev().find(|&k| {
            self.snap
                .index([i, j, k])
                .is_some_and(|x| self.snap.occupancy[x] == ados_protocol::voxmap::VOX_OCCUPIED)
        })
    }

    /// Walk the columns from window point `g0` along unit `dir` (north,
    /// east) to `max_m`.
    fn march(&self, g0: [f32; 3], dir: [f32; 2], ly: Layers, max_m: f32) -> ScanBin {
        let res = self.snap.meta.res_m;
        let mut cell = [g0[0].floor() as i64, g0[1].floor() as i64];
        let mut step = [0i64; 2];
        // Metres along the bearing to the next column boundary per axis, and
        // between boundaries.
        let mut t_max = [f32::INFINITY; 2];
        let mut t_delta = [f32::INFINITY; 2];
        for a in 0..2 {
            if dir[a].abs() > 1e-6 {
                step[a] = if dir[a] > 0.0 { 1 } else { -1 };
                let edge = cell[a] as f32 + if dir[a] > 0.0 { 1.0 } else { 0.0 };
                t_max[a] = (edge - g0[a]) * res / dir[a];
                t_delta[a] = res / dir[a].abs();
            }
        }
        let mut t = 0.0f32;
        let mut overhead: Option<(f32, f32)> = None;
        let bin = |hit_m, seen_m, overhead| ScanBin {
            hit_m,
            seen_m,
            overhead,
        };
        loop {
            let Some((_, free)) = self.column(cell[0], cell[1], ly.seen) else {
                return bin(None, t, overhead);
            };
            if let Some(k) = ly
                .over
                .and_then(|ks| self.lowest_occupied(cell[0], cell[1], ks))
            {
                let lift = (g0[2] - (k + 1) as f32) * res;
                if overhead.is_none_or(|(_, l)| lift < l) {
                    overhead = Some((t, lift));
                }
            }
            let wall = ly.walls.and_then(|ks| self.column(cell[0], cell[1], ks));
            if wall.is_some_and(|(occupied, _)| occupied) {
                return bin(Some(t), t, overhead);
            }
            if !free {
                return bin(None, t, overhead);
            }
            let a = if t_max[0] <= t_max[1] { 0 } else { 1 };
            t = t_max[a];
            if t >= max_m {
                return bin(None, max_m, overhead);
            }
            cell[a] += step[a];
            t_max[a] += t_delta[a];
        }
    }
}
