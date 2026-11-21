//! The map's slab around the vehicle read as a planar scan: for each bearing,
//! the first column holding a wall, or where seen-free space ends.

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
}

impl VoxWorld {
    /// Read [`WORLD_SCAN_BINS`] bearings from local NED position `pos`, bin
    /// `i` at `yaw + i°` (clockwise from north), out to `max_m`: a column is
    /// a wall when `walls` holds an occupied voxel in it, and seen when
    /// every voxel of `seen` in it was observed free. Without a live map
    /// tie (or with a slab outside the window) every bin is seen to 0.
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
        };
        let (Some(g0), Some(seen)) = (self.to_window(pos), seen.and_then(|b| self.layers(b)))
        else {
            return vec![blind; WORLD_SCAN_BINS];
        };
        let walls = walls.and_then(|b| self.layers(b));
        (0..WORLD_SCAN_BINS)
            .map(|i| {
                let b = yaw + (i as f32).to_radians();
                self.march([g0[0], g0[1]], [b.cos(), b.sin()], walls, seen, max_m)
            })
            .collect()
    }

    /// Walk the columns from window point `g0` along unit `dir` (north,
    /// east) to `max_m`.
    fn march(
        &self,
        g0: [f32; 2],
        dir: [f32; 2],
        walls: Option<(i64, i64)>,
        seen: (i64, i64),
        max_m: f32,
    ) -> ScanBin {
        let res = self.snap.meta.res_m;
        let mut cell = g0.map(|v| v.floor() as i64);
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
        loop {
            let wall = walls.and_then(|ks| self.column(cell[0], cell[1], ks));
            let Some((_, free)) = self.column(cell[0], cell[1], seen) else {
                return ScanBin {
                    hit_m: None,
                    seen_m: t,
                };
            };
            if wall.is_some_and(|(occupied, _)| occupied) {
                return ScanBin {
                    hit_m: Some(t),
                    seen_m: t,
                };
            }
            if !free {
                return ScanBin {
                    hit_m: None,
                    seen_m: t,
                };
            }
            let a = if t_max[0] <= t_max[1] { 0 } else { 1 };
            t = t_max[a];
            if t >= max_m {
                return ScanBin {
                    hit_m: None,
                    seen_m: max_m,
                };
            }
            cell[a] += step[a];
            t_max[a] += t_delta[a];
        }
    }
}
