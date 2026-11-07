//! The 128×128 occupancy grid at 0.25 m with a chamfer 3-4 clearance field.

use crate::geo::V2;

pub const GRID_N: usize = 128;
pub const CELL_M: f32 = 0.25;
const FAR: u32 = u32::MAX / 4;

#[derive(Debug, Clone)]
pub struct Grid {
    /// Local NE of cell (0, 0)'s lower corner.
    pub origin: V2,
    occ: Vec<bool>,
    /// Chamfer distance in thirds of a cell.
    dist: Vec<u32>,
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            origin: [0.0, 0.0],
            occ: vec![false; GRID_N * GRID_N],
            dist: vec![FAR; GRID_N * GRID_N],
        }
    }
}

impl Grid {
    /// Clear and centre on `center`.
    pub fn reset(&mut self, center: V2) {
        let half = GRID_N as f32 * CELL_M / 2.0;
        self.origin = [center[0] - half, center[1] - half];
        self.occ.fill(false);
        self.dist.fill(FAR);
    }

    pub fn cell(&self, ne: V2) -> Option<(usize, usize)> {
        let i = ((ne[0] - self.origin[0]) / CELL_M).floor();
        let j = ((ne[1] - self.origin[1]) / CELL_M).floor();
        if i < 0.0 || j < 0.0 || i >= GRID_N as f32 || j >= GRID_N as f32 {
            return None;
        }
        Some((i as usize, j as usize))
    }

    pub fn center_of(&self, i: usize, j: usize) -> V2 {
        [
            self.origin[0] + (i as f32 + 0.5) * CELL_M,
            self.origin[1] + (j as f32 + 0.5) * CELL_M,
        ]
    }

    /// True when the square `[origin, origin + span)` touches the grid.
    pub fn overlaps(&self, origin: V2, span: f32) -> bool {
        let size = GRID_N as f32 * CELL_M;
        origin[0] + span > self.origin[0]
            && origin[0] < self.origin[0] + size
            && origin[1] + span > self.origin[1]
            && origin[1] < self.origin[1] + size
    }

    pub fn mark(&mut self, ne: V2) {
        if let Some((i, j)) = self.cell(ne) {
            self.occ[i * GRID_N + j] = true;
        }
    }

    pub fn occupied(&self, i: usize, j: usize) -> bool {
        self.occ[i * GRID_N + j]
    }

    /// Two-pass chamfer 3-4 distance transform.
    pub fn transform(&mut self) {
        let n = GRID_N;
        for (k, d) in self.dist.iter_mut().enumerate() {
            *d = if self.occ[k] { 0 } else { FAR };
        }
        let d = &mut self.dist;
        for i in 0..n {
            for j in 0..n {
                let mut v = d[i * n + j];
                if i > 0 {
                    v = v.min(d[(i - 1) * n + j] + 3);
                    if j > 0 {
                        v = v.min(d[(i - 1) * n + j - 1] + 4);
                    }
                    if j + 1 < n {
                        v = v.min(d[(i - 1) * n + j + 1] + 4);
                    }
                }
                if j > 0 {
                    v = v.min(d[i * n + j - 1] + 3);
                }
                d[i * n + j] = v;
            }
        }
        for i in (0..n).rev() {
            for j in (0..n).rev() {
                let mut v = d[i * n + j];
                if i + 1 < n {
                    v = v.min(d[(i + 1) * n + j] + 3);
                    if j + 1 < n {
                        v = v.min(d[(i + 1) * n + j + 1] + 4);
                    }
                    if j > 0 {
                        v = v.min(d[(i + 1) * n + j - 1] + 4);
                    }
                }
                if j + 1 < n {
                    v = v.min(d[i * n + j + 1] + 3);
                }
                d[i * n + j] = v;
            }
        }
    }

    /// Clearance in metres at a cell (`None` far from every obstacle).
    pub fn clearance_cell(&self, i: usize, j: usize) -> Option<f32> {
        let v = self.dist[i * GRID_N + j];
        (v < FAR).then(|| v as f32 / 3.0 * CELL_M)
    }

    /// Clearance at a point: `None` outside the grid; `Some(INFINITY)` when no
    /// obstacle is in the grid.
    pub fn clearance(&self, ne: V2) -> Option<f32> {
        let (i, j) = self.cell(ne)?;
        Some(self.clearance_cell(i, j).unwrap_or(f32::INFINITY))
    }

    /// Distance along `dir` (unit) to the first occupied cell within `max_m`.
    pub fn march(&self, from: V2, dir: V2, max_m: f32) -> Option<f32> {
        let step = CELL_M / 2.0;
        let mut t = step;
        while t <= max_m {
            let p = [from[0] + dir[0] * t, from[1] + dir[1] * t];
            match self.cell(p) {
                Some((i, j)) if self.occupied(i, j) => return Some(t),
                Some(_) => {}
                None => return None,
            }
            t += step;
        }
        None
    }
}
