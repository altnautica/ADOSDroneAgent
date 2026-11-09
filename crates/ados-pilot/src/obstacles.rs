//! The obstacle model (C2): live occupancy tiles plus the current planar scan,
//! levelled with attitude, rasterised into a 128×128 grid at 0.25 m around the
//! vehicle with a chamfer 3-4 clearance transform.
//!
//! Frames: occupancy tiles arrive in `map_ned` (north-aligned NED whose origin
//! is the first scan); the `plugin.world-engine.pose` topic gives the vehicle
//! in that frame, so `LOCAL_POSITION_NED − pose.t` is the shift into the FC's
//! local frame. Scans arrive in body FRD, bins clockwise from the nose.

mod grid;
mod project;
mod tiles;

use std::collections::HashMap;

use serde_json::Value;

pub use grid::{Grid, CELL_M, GRID_N};
pub use project::{project_camera, Attitude, Scratch};

use crate::geo::{self, V2};

/// A return closer to the ground than this, after levelling, is ground.
pub const GROUND_REJECT_M: f32 = 0.4;
/// Scans older than this are not used.
pub const SCAN_MAX_AGE_MS: u64 = 500;
/// How far the planar scan sees along a bin without a return (the 12 m
/// planar lidar; the sector display marches the grid as far).
pub const SCAN_HORIZON_M: f32 = 12.0;

/// One non-ground scan return.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Body bearing, radians clockwise from the nose (levelled).
    pub body_bearing: f32,
    /// Horizontal range, metres.
    pub range_m: f32,
    /// Local NED horizontal position.
    pub ne: V2,
}

/// The obstacle model.
#[derive(Debug, Default)]
pub struct Obstacles {
    tiles: HashMap<(i32, i32), tiles::Tile>,
    tile_res_m: f32,
    tile_cells: usize,
    /// `local − map` horizontal offset.
    offset: Option<V2>,
    hits: Vec<Hit>,
    /// Where each bin of the current scan stops seeing without an obstacle:
    /// the floor where a tilted beam meets it, or [`SCAN_HORIZON_M`] when it
    /// returns nothing. Nothing beyond is known free, so the stick brake
    /// treats these points as possible obstacles.
    horizon: Vec<V2>,
    scan_ms: u64,
    pub grid: Grid,
}

/// Attitude and height used to level a scan.
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
    /// Height above ground, metres (None: no ground rejection).
    pub agl: Option<f32>,
}

impl Obstacles {
    pub fn new() -> Self {
        Self {
            tile_res_m: 0.05,
            tile_cells: 64,
            ..Default::default()
        }
    }

    /// Ingest a `plugin.world-engine.occupancy` payload (changed tiles only).
    pub fn ingest_occupancy(&mut self, payload: &Value) {
        if let Some(r) = payload.get("res_m").and_then(Value::as_f64) {
            self.tile_res_m = r as f32;
        }
        if let Some(n) = payload.get("tile_cells").and_then(Value::as_u64) {
            self.tile_cells = n as usize;
        }
        let Some(list) = payload.get("tiles").and_then(Value::as_array) else {
            return;
        };
        for t in list {
            let (Some(ix), Some(iy), Some(b64)) = (
                t.get("ix").and_then(Value::as_i64),
                t.get("iy").and_then(Value::as_i64),
                t.get("png_b64").and_then(Value::as_str),
            ) else {
                continue;
            };
            match tiles::decode(b64, self.tile_cells) {
                Ok(tile) => {
                    self.tiles.insert((ix as i32, iy as i32), tile);
                }
                Err(e) => tracing::debug!(error = %e, ix, iy, "occupancy_tile_decode_failed"),
            }
        }
    }

    /// Ingest a `plugin.world-engine.pose` payload with the FC local position
    /// at the same moment.
    pub fn ingest_pose(&mut self, payload: &Value, local_ne: Option<V2>) {
        let t = payload
            .get("pose")
            .and_then(|p| p.get("t"))
            .and_then(Value::as_array);
        let (Some(t), Some(local)) = (t, local_ne) else {
            return;
        };
        let (Some(x), Some(y)) = (
            t.first().and_then(Value::as_f64),
            t.get(1).and_then(Value::as_f64),
        ) else {
            return;
        };
        self.offset = Some([local[0] - x as f32, local[1] - y as f32]);
    }

    /// Ingest a `plugin.world-engine.scan` payload.
    pub fn ingest_scan(&mut self, payload: &Value, level: Level, pos_ne: V2, now_ms: u64) {
        if payload
            .get("frame")
            .and_then(Value::as_str)
            .is_some_and(|f| f != "body_frd")
        {
            return;
        }
        let start = payload
            .get("angle_start_rad")
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32;
        let Some(inc) = payload.get("angle_increment_rad").and_then(Value::as_f64) else {
            return;
        };
        let Some(ranges) = payload.get("ranges_m").and_then(Value::as_array) else {
            return;
        };
        self.hits.clear();
        self.horizon.clear();
        for (i, r) in ranges.iter().enumerate() {
            let r = r.as_f64().unwrap_or(0.0) as f32;
            let theta = start + inc as f32 * i as f32;
            if r <= 0.05 || !r.is_finite() {
                let far = geo::body_to_ned(
                    SCAN_HORIZON_M * theta.cos(),
                    SCAN_HORIZON_M * theta.sin(),
                    level.yaw,
                );
                self.horizon.push(geo::add(pos_ne, far));
                continue;
            }
            match level_return(r, theta, level, pos_ne) {
                (hit, false) => self.hits.push(hit),
                (floor, true) => self.horizon.push(floor.ne),
            }
        }
        self.scan_ms = now_ms;
    }

    /// Current (fresh) scan horizon points within `r` of `pos`.
    pub fn horizon_near(&self, pos: V2, r: f32, now_ms: u64) -> impl Iterator<Item = V2> + '_ {
        let fresh = now_ms.saturating_sub(self.scan_ms) <= SCAN_MAX_AGE_MS;
        self.horizon
            .iter()
            .copied()
            .filter(move |p| fresh && geo::dist(*p, pos) <= r)
    }

    /// Current (fresh) non-ground scan hits.
    pub fn hits(&self, now_ms: u64) -> &[Hit] {
        if now_ms.saturating_sub(self.scan_ms) <= SCAN_MAX_AGE_MS {
            &self.hits
        } else {
            &[]
        }
    }

    /// Rebuild the grid around `center` from tiles and the current scan.
    pub fn rebuild(&mut self, center: V2, now_ms: u64) {
        self.grid.reset(center);
        if let Some(off) = self.offset {
            let res = self.tile_res_m;
            let span = res * self.tile_cells as f32;
            for (&(ix, iy), tile) in &self.tiles {
                let origin = [ix as f32 * span + off[0], iy as f32 * span + off[1]];
                if !self.grid.overlaps(origin, span) {
                    continue;
                }
                for (lx, ly) in tile.occupied() {
                    let ne = [
                        origin[0] + (lx as f32 + 0.5) * res,
                        origin[1] + (ly as f32 + 0.5) * res,
                    ];
                    self.grid.mark(ne);
                }
            }
        }
        let fresh = now_ms.saturating_sub(self.scan_ms) <= SCAN_MAX_AGE_MS;
        if fresh {
            for h in &self.hits {
                self.grid.mark(h.ne);
            }
        }
        self.grid.transform();
    }

    /// Distance along `dir` (unit) from `from` to the first obstacle, within
    /// `max_m`: grid cells and current scan hits.
    pub fn ray_distance(&self, from: V2, dir: V2, max_m: f32, now_ms: u64) -> Option<f32> {
        let mut best = self.grid.march(from, dir, max_m);
        for h in self.hits(now_ms) {
            let rel = geo::sub(h.ne, from);
            let t = geo::dot(rel, dir);
            if t <= 0.0 || t > max_m {
                continue;
            }
            let lateral = (rel[0] * dir[1] - rel[1] * dir[0]).abs();
            if lateral < 0.35 && best.is_none_or(|b| t < b) {
                best = Some(t);
            }
        }
        best
    }

    /// Nearest current raw return, metres.
    pub fn min_raw_m(&self, now_ms: u64) -> Option<f32> {
        self.hits(now_ms).iter().map(|h| h.range_m).reduce(f32::min)
    }

    /// Obstacle points within `r` of `pos`: occupied grid cell centres and
    /// current scan hits. The stick brake limits motion against every one of
    /// them, so a narrow obstacle cannot fall between sample rays.
    pub fn points_near(&self, pos: V2, r: f32, now_ms: u64) -> impl Iterator<Item = V2> + '_ {
        self.grid.occupied_near(pos, r).chain(
            self.hits(now_ms)
                .iter()
                .map(|h| h.ne)
                .filter(move |p| geo::dist(*p, pos) <= r),
        )
    }

    /// The 72 body sectors of 5° (yaw 0 = nose, clockwise): nearest return,
    /// from the scan and the grid.
    pub fn sectors(&self, pos: V2, yaw: f32, now_ms: u64) -> Vec<Option<f32>> {
        let mut out: Vec<Option<f32>> = vec![None; ados_protocol::pilot::SECTORS];
        for h in self.hits(now_ms) {
            let b = h.body_bearing.rem_euclid(std::f32::consts::TAU);
            let i = ((b.to_degrees() / 5.0) as usize).min(71);
            out[i] = Some(out[i].map_or(h.range_m, |d: f32| d.min(h.range_m)));
        }
        for (i, slot) in out.iter_mut().enumerate() {
            let body = ((i as f32) * 5.0 + 2.5).to_radians();
            if let Some(d) = self.grid.march(pos, geo::bearing_unit(yaw + body), 12.0) {
                *slot = Some(slot.map_or(d, |s| s.min(d)));
            }
        }
        out
    }

    /// True when no occupied cell lies between `a` and `b`.
    pub fn line_of_sight(&self, a: V2, b: V2) -> bool {
        let d = geo::dist(a, b);
        if d < 1e-3 {
            return true;
        }
        let dir = geo::scale(geo::sub(b, a), 1.0 / d);
        self.grid.march(a, dir, d - 0.3).is_none()
    }
}

/// Level one return and drop it when it is ground.
pub fn level_hit(range: f32, theta: f32, l: Level, pos_ne: V2) -> Option<Hit> {
    match level_return(range, theta, l, pos_ne) {
        (hit, false) => Some(hit),
        (_, true) => None,
    }
}

/// Level one return: its horizontal position, and whether it is the ground
/// (closer to it than [`GROUND_REJECT_M`] after levelling).
fn level_return(range: f32, theta: f32, l: Level, pos_ne: V2) -> (Hit, bool) {
    // Body FRD point, then rotate by roll (about x) and pitch (about y).
    let (bx, by) = (range * theta.cos(), range * theta.sin());
    let (sr, cr) = l.roll.sin_cos();
    let (sp, cp) = l.pitch.sin_cos();
    // Rx(roll): y' = y·cr, z' = y·sr
    let (y1, z1) = (by * cr, by * sr);
    // Ry(pitch): x'' = x·cp + z'·sp, z'' = −x·sp + z'·cp
    let x2 = bx * cp + z1 * sp;
    let z2 = -bx * sp + z1 * cp;
    let ground = l.agl.is_some_and(|agl| agl - z2 < GROUND_REJECT_M);
    let hit = Hit {
        body_bearing: y1.atan2(x2),
        range_m: x2.hypot(y1),
        ne: geo::add(pos_ne, geo::body_to_ned(x2, y1, l.yaw)),
    };
    (hit, ground)
}

#[cfg(test)]
mod tests;
