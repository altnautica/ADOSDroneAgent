//! A live 3D voxel map's slab read as the current scan
//! ([`crate::world::VoxWorld::scan`]): its walls are the scan's hits, the
//! edge of its seen space the scan's horizon, the lowest voxel over the slab
//! along each bearing an overhead return (a canopy's underside, height over
//! the vehicle), and its occupied columns
//! ([`crate::world::VoxWorld::slice`]) go into the grid with them. The
//! brake, the planner, the overhead report and the published sectors read
//! them as they read a planar scan. A 3D lidar's own scan, flattened to
//! bins, still feeds the movers ([`Obstacles::ingest_mover_scan`]).

use std::f32::consts::TAU;

use serde_json::Value;

use super::{band, Hit, Level, Obstacles};
use crate::geo::{self, V2};
use crate::world::ScanBin;

/// A planar scan's layout: its first bin's angle, the angle between bins
/// (radians) and the ranges. `None` for a frame other than `body_frd` or a
/// scan missing its increment or ranges.
pub(super) fn scan_layout(payload: &Value) -> Option<(f32, f32, &[Value])> {
    if payload
        .get("frame")
        .and_then(Value::as_str)
        .is_some_and(|f| f != "body_frd")
    {
        return None;
    }
    let start = payload
        .get("angle_start_rad")
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let inc = payload.get("angle_increment_rad").and_then(Value::as_f64)? as f32;
    let ranges = payload.get("ranges_m").and_then(Value::as_array)?;
    Some((start, inc, ranges))
}

impl Obstacles {
    /// Take one world scan from local NE `pos` at yaw `yaw`: bin `i` at
    /// body bearing `i·360°/n`, `cells` the slab's occupied columns. A bin
    /// with no wall puts a horizon point where its seen space ends, but no
    /// nearer than `min_seen_m` (the scan then stands in for unknown space
    /// beside the vehicle: [`Obstacles::unseen_beside`]). `closing_max_mps`
    /// bounds the closing speed as for a planar scan
    /// ([`Obstacles::ingest_scan`]); `floor_agl` is the slab's floor over the
    /// ground, where the movers' clusters are measured from
    /// ([`super::Movers::ingest_hits`]).
    #[allow(clippy::too_many_arguments)]
    pub fn ingest_world(
        &mut self,
        bins: &[ScanBin],
        cells: Vec<V2>,
        pos: V2,
        yaw: f32,
        min_seen_m: f32,
        closing_max_mps: f32,
        floor_agl: Option<f32>,
        now_ms: u64,
    ) {
        self.hits.clear();
        self.overhead.clear();
        self.horizon.clear();
        self.horizon_unseen = false;
        let n = bins.len().max(1) as f32;
        for (i, b) in bins.iter().enumerate() {
            let body = i as f32 * TAU / n;
            let dir = geo::bearing_unit(yaw + body);
            if let Some((r, lift_m)) = b.overhead {
                self.overhead.push(Hit {
                    body_bearing: body,
                    range_m: r,
                    ne: geo::add(pos, geo::scale(dir, r)),
                    overhead: true,
                    lift_m,
                });
            }
            match b.hit_m {
                Some(r) => self.hits.push(Hit {
                    body_bearing: body,
                    range_m: r,
                    ne: geo::add(pos, geo::scale(dir, r)),
                    overhead: false,
                    lift_m: 0.0,
                }),
                None => {
                    let seen = b.seen_m.max(min_seen_m);
                    self.horizon.push(geo::add(pos, geo::scale(dir, seen)));
                    self.horizon_unseen |= b.seen_m < min_seen_m;
                }
            }
        }
        self.world_cells = cells;
        self.update_closing(yaw, None, closing_max_mps, now_ms);
        self.scan_ms = Some(now_ms);
        self.scan_stamp_ns = None;
        // A 3D lidar's own scan, while one arrives, gives the movers their
        // returns instead.
        if !self.movers.raw_fresh(now_ms) {
            self.movers
                .ingest_hits(&self.hits, &self.horizon, pos, floor_agl, now_ms);
        }
    }

    /// A planar scan while the 3D map is the obstacle source: a 3D lidar's
    /// returns flattened to bins. The map's slab holds what it saw, but the
    /// map fills a voxel only after several hits in a row on space it saw
    /// free, so a person walking there fills none; the movers take their
    /// clusters from this scan's returns, sorted by the band as a planar
    /// scan's are, and the obstacle model stays as the map made it.
    pub fn ingest_mover_scan(&mut self, payload: &Value, level: Level, pos: V2, now_ms: u64) {
        let Some((start, inc, ranges)) = scan_layout(payload) else {
            return;
        };
        let raw = &mut self.movers.raw;
        let sorted = band::sort_scan(
            ranges,
            start,
            inc,
            level,
            pos,
            self.band_climb,
            &mut raw.walls,
            &mut raw.seen,
            now_ms,
        );
        raw.at_ms = Some(now_ms);
        self.movers
            .ingest_hits(&sorted.walls, &sorted.horizon, pos, level.agl, now_ms);
    }

    /// The planar scans are the obstacle source again: the map's columns
    /// leave the grid.
    pub fn clear_world(&mut self) {
        self.world_cells.clear();
    }
}
