//! The trajectory planner's map: the obstacle grid ([`Grid`], the planar
//! scans or the live voxel map's slab, whichever is the obstacle source)
//! with what the sensors see from the vehicle now. A cell is occupied when
//! the grid marks it, observed free when it lies inside the seen range of
//! its bearing from the vehicle (the nearer of the scan's wall and horizon
//! there, the same seen space the brake keeps to), and unknown otherwise.
//! No space round the vehicle reads free unseen: a forward camera never
//! sees beside the vehicle, and a plan that may stop in unseen space round
//! it lets the vehicle creep on, replan by replan, into what it never saw.
//! The vehicle's own voxel is seen: the sensor's rays clear it.

use std::f32::consts::TAU;

use ados_planning::{Cell, PlanarQuery};

use super::Keepout;
use crate::geo::{self, V2};
use crate::obstacles::{Grid, Obstacles, CELL_M, GRID_N};
use crate::world::VoxWorld;

/// Bearing bins of the seen ranges (2° each).
pub const SEEN_BINS: usize = 180;
/// Empty bins take the nearest seen range within this many bins either side.
const FILL_BINS: usize = 2;
/// A column reads seen when a voxel within 0.3 m of flight height (two of
/// the voxel map's 0.15 m voxels either side, or at it) was observed free:
/// the heights tried, nearest first, in [`SEEN_SLAB_STEP_M`].
const SEEN_SLAB_STEPS: [f32; 5] = [0.0, -1.0, 1.0, -2.0, 2.0];
const SEEN_SLAB_STEP_M: f32 = 0.15;

fn bin_of(rel: V2) -> usize {
    let b = geo::bearing_of(rel).rem_euclid(TAU);
    ((b / TAU * SEEN_BINS as f32) as usize).min(SEEN_BINS - 1)
}

/// How far the sensors see from `pos` along each bearing bin, into `out`:
/// the nearest wall or horizon point of the current scan in that bin, an
/// empty bin the nearest of its neighbours' within [`FILL_BINS`], else 0
/// (nothing known). Without a fresh scan every bin is 0.
pub fn seen_ranges(obs: &Obstacles, pos: V2, now_ms: u64, out: &mut Vec<f32>) {
    out.clear();
    out.resize(SEEN_BINS, f32::INFINITY);
    let walls = obs.hits(now_ms).iter().map(|h| h.ne);
    for p in walls.chain(obs.horizon_near(pos, f32::INFINITY, now_ms)) {
        let rel = geo::sub(p, pos);
        let d = geo::norm(rel);
        if d < 1e-3 {
            continue;
        }
        let b = bin_of(rel);
        out[b] = out[b].min(d);
    }
    let raw = out.clone();
    for (b, v) in out.iter_mut().enumerate() {
        if v.is_finite() {
            continue;
        }
        *v = (1..=FILL_BINS)
            .flat_map(|k| [(b + k) % SEEN_BINS, (b + SEEN_BINS - k) % SEEN_BINS])
            .map(|i| raw[i])
            .find(|r| r.is_finite())
            .unwrap_or(0.0);
    }
}

/// What the sensors have seen free: the live voxel map's observed-free
/// voxels at flight height (a 3D tier, with the memory of what it saw
/// before), or the planar scan's seen ranges from the vehicle.
#[derive(Clone, Copy)]
pub enum Seen<'a> {
    /// The map and the flight height (local down, m).
    Voxels(&'a VoxWorld, f32),
    /// [`seen_ranges`] from the vehicle.
    Ranges(&'a [f32]),
}

/// A plan's start reads free unseen within this of it, m: a rounding
/// allowance round the one point the vehicle is known to take up.
pub const OWN_M: f32 = 0.05;

/// The grid and what was seen, as the planner's planar map (north, east).
pub struct PlanMap<'a> {
    pub grid: &'a Grid,
    /// The vehicle, where the seen ranges are measured from.
    pub pos: V2,
    pub seen: Seen<'a>,
    /// The plan's start when it lies in the vehicle's own space (the
    /// reference within a cell of it): its grid cell reads free, seen or
    /// not (a sensor never sees the voxel it sits beside, and the search
    /// and the backup's corridor start from that cell's centre), and so
    /// does anything within [`OWN_M`] of it. What a plan does inside that
    /// cell is held to the forward view where there is one.
    pub own: Option<V2>,
    /// The subject's disk ([`Keepout`]), occupied like an obstacle.
    pub subject: Option<Keepout>,
}

impl PlanMap<'_> {
    /// Observed free at `p` (a cell not marked): the start's own point, or
    /// seen ([`Seen`]: on the map, a voxel observed free within 0.3 m of
    /// flight height ([`SEEN_SLAB_STEPS`]), as a sensor's rays cross the
    /// flight height at a slant; on the ranges, a cell short of its
    /// bearing's).
    fn seen_free(&self, p: V2) -> bool {
        if let Some(o) = self.own {
            if geo::dist(o, p) <= OWN_M
                || self
                    .grid
                    .cell(o)
                    .is_some_and(|c| self.grid.cell(p) == Some(c))
            {
                return true;
            }
        }
        match self.seen {
            Seen::Voxels(w, down) => SEEN_SLAB_STEPS
                .iter()
                .any(|k| w.known_free([p[0], p[1], down + k * SEEN_SLAB_STEP_M])),
            Seen::Ranges(r) => {
                let rel = geo::sub(p, self.pos);
                geo::norm(rel) + CELL_M <= r[bin_of(rel)]
            }
        }
    }
}

impl PlanarQuery for PlanMap<'_> {
    fn res(&self) -> f32 {
        CELL_M
    }

    fn bounds(&self) -> [[f32; 2]; 2] {
        let o = self.grid.origin;
        let span = GRID_N as f32 * CELL_M;
        [o, [o[0] + span, o[1] + span]]
    }

    fn state(&self, p: [f32; 2]) -> Cell {
        match self.grid.cell(p) {
            None => Cell::Unknown,
            Some((i, j)) if self.grid.occupied(i, j) => Cell::Occupied,
            Some(_) if self.subject.is_some_and(|k| k.clearance(p) <= 0.0) => Cell::Occupied,
            Some(_) if self.seen_free(p) => Cell::Free,
            Some(_) => Cell::Unknown,
        }
    }

    fn clearance(&self, p: [f32; 2]) -> f32 {
        let c = self.grid.clearance(p).unwrap_or(0.0);
        self.subject.map_or(c, |k| c.min(k.clearance(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_wrap_and_split_the_circle() {
        assert_eq!(bin_of([1.0, 0.0]), 0);
        assert_eq!(bin_of([0.0, 1.0]), SEEN_BINS / 4);
        assert_eq!(bin_of([-1.0, 0.0]), SEEN_BINS / 2);
        assert_eq!(bin_of([1.0, -1e-4]), SEEN_BINS - 1);
    }

    #[test]
    fn cells_read_free_inside_the_seen_range_and_unknown_past_it() {
        let mut grid = Grid::default();
        grid.reset([0.0, 0.0]);
        grid.mark([5.0, 0.0]);
        grid.transform();
        let mut seen = vec![10.0; SEEN_BINS];
        seen[0] = 5.0;
        let m = PlanMap {
            grid: &grid,
            pos: [0.0, 0.0],
            seen: Seen::Ranges(&seen),
            own: None,
            subject: None,
        };
        assert_eq!(m.state([2.0, 0.0]), Cell::Free);
        assert_eq!(m.state([5.0, 0.0]), Cell::Occupied);
        assert_eq!(m.state([7.0, 0.05]), Cell::Unknown);
        assert_eq!(m.state([0.0, 8.0]), Cell::Free);
        assert_eq!(m.state([0.0, 11.0]), Cell::Unknown);
        assert!((m.clearance([3.0, 0.0]) - 2.0).abs() < 0.3);
    }

    #[test]
    fn the_subjects_disk_reads_occupied_and_counts_in_the_clearance() {
        let mut grid = Grid::default();
        grid.reset([0.0, 0.0]);
        grid.transform();
        let seen = vec![10.0; SEEN_BINS];
        let m = PlanMap {
            grid: &grid,
            pos: [0.0, 0.0],
            seen: Seen::Ranges(&seen),
            own: None,
            subject: Keepout::new([4.0, 0.0], 2.3, 0.5, [0.0, 0.0]),
        };
        assert_eq!(m.state([4.0, 1.0]), Cell::Occupied);
        assert_eq!(m.state([4.0, 2.0]), Cell::Free);
        // The margin from the disk is the kept distance from the subject.
        assert!((m.clearance([4.0, 2.3]) - 0.5).abs() < 1e-4);
    }
}
