//! Point, ray and slab queries on the voxel map, in local NED.

use ados_protocol::voxmap::{VOX_FREE, VOX_OCCUPIED};

use super::{Band, VoxWorld};
use crate::geo::V2;

/// Where a ray through the map stops seeing clear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RayHit {
    /// Every voxel from the start to the end was observed free.
    Clear,
    /// An occupied voxel, entered this many metres along the ray.
    Occupied(f32),
    /// A voxel never observed free, entered this many metres along.
    Unknown(f32),
    /// The ray left the map's window this many metres along.
    Outside(f32),
}

impl RayHit {
    /// How far the ray is seen clear: where it stopped, or `len_m` (its
    /// length) when it is clear to the end.
    pub fn clear_m(self, len_m: f32) -> f32 {
        match self {
            RayHit::Clear => len_m,
            RayHit::Occupied(t) | RayHit::Unknown(t) | RayHit::Outside(t) => t,
        }
    }
}

impl VoxWorld {
    /// The occupancy of the voxel holding local point `p`; `None` outside
    /// the window or without a map tied to the local frame.
    fn class_at(&self, p: [f32; 3]) -> Option<usize> {
        let w = self.to_window(p)?;
        self.snap.index(w.map(|v| v.floor() as i64))
    }

    /// The map observed the voxel holding `p` occupied.
    pub fn occupied(&self, p: [f32; 3]) -> bool {
        self.class_at(p)
            .is_some_and(|i| self.snap.occupancy[i] == VOX_OCCUPIED)
    }

    /// The map observed the voxel holding `p` free.
    pub fn known_free(&self, p: [f32; 3]) -> bool {
        self.class_at(p)
            .is_some_and(|i| self.snap.occupancy[i] == VOX_FREE)
    }

    /// Distance from `p` to the nearest obstacle voxel, metres (capped at
    /// the map's 4 m): 0 unless the voxel holding `p` was observed free.
    pub fn clearance(&self, p: [f32; 3]) -> f32 {
        match self.class_at(p) {
            Some(i) if self.snap.occupancy[i] == VOX_FREE => f32::from(self.snap.esdf_dm[i]) / 10.0,
            _ => 0.0,
        }
    }

    /// Walk the voxels the segment `from` → `to` passes through, in order,
    /// and stop at the first one not observed free.
    pub fn ray(&self, from: [f32; 3], to: [f32; 3]) -> RayHit {
        self.walk(from, to, false)
    }

    /// As [`VoxWorld::ray`], but through occupied voxels: where the space
    /// the map has seen ends along the segment (an obstacle's own voxels
    /// are seen; the shadow behind it is not, unless seen from elsewhere).
    /// Never [`RayHit::Occupied`].
    pub fn ray_seen(&self, from: [f32; 3], to: [f32; 3]) -> RayHit {
        self.walk(from, to, true)
    }

    fn walk(&self, from: [f32; 3], to: [f32; 3], through_occupied: bool) -> RayHit {
        let (Some(g0), Some(g1)) = (self.to_window(from), self.to_window(to)) else {
            return RayHit::Outside(0.0);
        };
        let d = [g1[0] - g0[0], g1[1] - g0[1], g1[2] - g0[2]];
        let len_vox = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let len_m = len_vox * self.snap.meta.res_m;
        let mut cell = g0.map(|v| v.floor() as i64);
        let mut step = [0i64; 3];
        let mut t_max = [f32::INFINITY; 3];
        let mut t_delta = [f32::INFINITY; 3];
        for a in 0..3 {
            if d[a].abs() > 1e-9 {
                step[a] = if d[a] > 0.0 { 1 } else { -1 };
                let edge = cell[a] as f32 + if d[a] > 0.0 { 1.0 } else { 0.0 };
                t_max[a] = (edge - g0[a]) / d[a];
                t_delta[a] = 1.0 / d[a].abs();
            }
        }
        let mut s = 0.0f32;
        loop {
            let t = s * len_m;
            match self.snap.index(cell).map(|i| self.snap.occupancy[i]) {
                None => return RayHit::Outside(t),
                Some(VOX_OCCUPIED) if through_occupied => {}
                Some(VOX_OCCUPIED) => return RayHit::Occupied(t),
                Some(VOX_FREE) => {}
                Some(_) => return RayHit::Unknown(t),
            }
            let a = (0..3)
                .min_by(|&x, &y| t_max[x].total_cmp(&t_max[y]))
                .unwrap_or(0);
            s = t_max[a];
            if s > 1.0 {
                return RayHit::Clear;
            }
            cell[a] += step[a];
            t_max[a] += t_delta[a];
        }
    }

    /// The window's voxel layers (`k`) the slab `band` overlaps, clamped to
    /// the window; `None` when it misses the window.
    pub(super) fn layers(&self, band: Band) -> Option<(i64, i64)> {
        let o = self.offset?;
        let m = &self.snap.meta;
        let k = |d: f32| ((d + o[2]) / m.res_m).floor() as i64 - i64::from(m.origin_cell[2]);
        let nz = i64::from(m.dims[2]);
        let (k0, k1) = (k(band.top_d).max(0), k(band.bottom_d).min(nz - 1));
        (self.snap.valid && k0 <= k1).then_some((k0, k1))
    }

    /// Whether window column `(i, j)` holds an occupied voxel in layers
    /// `ks`, and whether every voxel there was observed free; `None` outside
    /// the window.
    pub(super) fn column(&self, i: i64, j: i64, ks: (i64, i64)) -> Option<(bool, bool)> {
        let mut occupied = false;
        let mut free = true;
        for k in ks.0..=ks.1 {
            match self.snap.occupancy[self.snap.index([i, j, k])?] {
                VOX_OCCUPIED => occupied = true,
                VOX_FREE => {}
                _ => free = false,
            }
        }
        Some((occupied, free && !occupied))
    }

    /// The slab `band` seen from above: the local NED centre of every
    /// window column holding an occupied voxel inside it, into `out`
    /// (cleared first). The 2D obstacle grid marks these, so its planner
    /// and the brake work on the 3D map unchanged.
    pub fn slice(&self, band: Band, out: &mut Vec<V2>) {
        out.clear();
        let Some(ks) = self.layers(band) else {
            return;
        };
        let [nx, ny, _] = self.snap.meta.dims.map(i64::from);
        for j in 0..ny {
            for i in 0..nx {
                if self.column(i, j, ks).is_some_and(|(occ, _)| occ) {
                    out.push(self.column_centre(i, j));
                }
            }
        }
    }
}
