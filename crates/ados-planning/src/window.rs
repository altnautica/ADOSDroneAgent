//! The search window of one plan: where the lattice lies, how the start and
//! goal sit in it.

use crate::lattice::{Lattice, BLOCKED};
use crate::planner::Planner;
use crate::world::{Aabb, OccupancyQuery};
use crate::{PlanError, State};

/// The point where `s → g` leaves `b` (or `g` clamped into `b`).
fn clip(s: [f32; 3], g: [f32; 3], b: &Aabb) -> [f32; 3] {
    let mut t = 1.0f32;
    for k in 0..3 {
        let d = g[k] - s[k];
        if d > 1e-9 {
            t = t.min((b.max[k] - s[k]) / d);
        } else if d < -1e-9 {
            t = t.min((b.min[k] - s[k]) / d);
        }
    }
    let t = t.clamp(0.0, 1.0);
    b.clamp([
        s[0] + t * (g[0] - s[0]),
        s[1] + t * (g[1] - s[1]),
        s[2] + t * (g[2] - s[2]),
    ])
}

impl Planner {
    /// Window, lattice and rules for one plan; the start (and, on a single
    /// layer map, its height) and the goal clipped into the window.
    pub(crate) fn prepare<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        mut start: State,
        mut goal: [f32; 3],
    ) -> Result<(State, [f32; 3], bool), PlanError> {
        let b = w.bounds();
        let res = w.res();
        let cfg = self.cfg;
        let planar = b.max[2] - b.min[2] < 1.5 * res;
        let band = match cfg.z_band {
            Some([lo, hi]) if !planar => [lo.max(b.min[2]), hi.min(b.max[2])],
            _ => [b.min[2], b.max[2]],
        };
        if planar {
            let z = 0.5 * (b.min[2] + b.max[2]);
            start.p[2] = z;
            start.v[2] = 0.0;
            start.a[2] = 0.0;
            goal[2] = z;
        }
        if !b.contains(start.p) || band[0] > band[1] {
            return Err(PlanError::NoPath);
        }
        // One voxel in from the window's sides: the edge voxels are where
        // the map knows least.
        let mut inner = b.grow([-res, -res, 0.0]);
        inner.min[2] = band[0];
        inner.max[2] = band[1];
        let (local, reaches) = if inner.contains(goal) {
            (goal, true)
        } else {
            (clip(start.p, goal, &inner), false)
        };
        let mut window =
            Aabb::around(start.p, local).grow([cfg.pad_m[0], cfg.pad_m[0], cfg.pad_m[1]]);
        window.min[2] = window.min[2].max(band[0].min(start.p[2]));
        window.max[2] = window.max[2].min(band[1].max(start.p[2]));
        let lat = Lattice::over(w, &window).ok_or(PlanError::NoPath)?;
        let rules = self.rules(w, start.p);
        self.space.begin(lat, rules);
        Ok((start, local, reaches))
    }

    /// The goal's voxel, or the nearest traversable one within
    /// [`crate::Config::goal_snap_m`]: `(voxel, end point, snapped)`.
    pub(crate) fn snap_goal<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        p: [f32; 3],
    ) -> Result<([i32; 3], [f32; 3], bool), PlanError> {
        let lat = self.space.lat;
        // The goal lies in the window; clamp so a point on its far face
        // still names a voxel.
        let c: [i32; 3] = std::array::from_fn(|k| {
            (((p[k] - lat.origin[k]) / lat.res).floor() as i32).clamp(0, lat.dims[k] - 1)
        });
        if self.space.base(w, c) != BLOCKED {
            return Ok((c, p, false));
        }
        let r = (self.cfg.goal_snap_m / lat.res).ceil() as i32;
        let mut best: Option<(i32, [i32; 3])> = None;
        for dz in -r..=r {
            for dy in -r..=r {
                for dx in -r..=r {
                    let d2 = dx * dx + dy * dy + dz * dz;
                    let q = [c[0] + dx, c[1] + dy, c[2] + dz];
                    if d2 > r * r || best.is_some_and(|(b, _)| b <= d2) || !lat.inside(q) {
                        continue;
                    }
                    if self.space.base(w, q) != BLOCKED {
                        best = Some((d2, q));
                    }
                }
            }
        }
        best.map(|(_, q)| (q, lat.centre(q), true))
            .ok_or(PlanError::NoPath)
    }
}
