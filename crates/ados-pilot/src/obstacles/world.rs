//! A live 3D voxel map's slab read as the current scan
//! ([`crate::world::VoxWorld::scan`]): its walls are the scan's hits, the
//! edge of its seen space the scan's horizon, and its occupied columns
//! ([`crate::world::VoxWorld::slice`]) go into the grid with them. The
//! brake, the planner and the published sectors read them as they read a
//! planar scan.

use std::f32::consts::TAU;

use super::{Hit, Obstacles};
use crate::geo::{self, V2};
use crate::world::ScanBin;

impl Obstacles {
    /// Take one world scan from local NE `pos` at yaw `yaw`: bin `i` at
    /// body bearing `i·360°/n`, `cells` the slab's occupied columns. A bin
    /// with no wall puts a horizon point where its seen space ends, but no
    /// nearer than `min_seen_m`. `closing_max_mps` bounds the closing speed
    /// as for a planar scan ([`Obstacles::ingest_scan`]).
    #[allow(clippy::too_many_arguments)]
    pub fn ingest_world(
        &mut self,
        bins: &[ScanBin],
        cells: Vec<V2>,
        pos: V2,
        yaw: f32,
        min_seen_m: f32,
        closing_max_mps: f32,
        now_ms: u64,
    ) {
        self.hits.clear();
        self.overhead.clear();
        self.horizon.clear();
        let n = bins.len().max(1) as f32;
        for (i, b) in bins.iter().enumerate() {
            let body = i as f32 * TAU / n;
            let dir = geo::bearing_unit(yaw + body);
            match b.hit_m {
                Some(r) => self.hits.push(Hit {
                    body_bearing: body,
                    range_m: r,
                    ne: geo::add(pos, geo::scale(dir, r)),
                    overhead: false,
                    lift_m: 0.0,
                }),
                None => self
                    .horizon
                    .push(geo::add(pos, geo::scale(dir, b.seen_m.max(min_seen_m)))),
            }
        }
        self.world_cells = cells;
        self.update_closing(yaw, None, closing_max_mps, now_ms);
        self.scan_ms = Some(now_ms);
        self.scan_stamp_ns = None;
    }

    /// The planar scans are the obstacle source again: the map's columns
    /// leave the grid.
    pub fn clear_world(&mut self) {
        self.world_cells.clear();
    }
}
