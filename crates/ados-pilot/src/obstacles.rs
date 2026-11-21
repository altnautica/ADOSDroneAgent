//! The obstacle model (C2): live occupancy tiles plus the current planar scan,
//! levelled with attitude, rasterised into a 128×128 grid at 0.25 m around the
//! vehicle with a chamfer 3-4 clearance transform.
//!
//! Frames: occupancy tiles arrive in `map_ned` (north-aligned NED whose origin
//! is the first scan); the `plugin.navigation.pose` topic gives the vehicle
//! in that frame, so `LOCAL_POSITION_NED − pose.t` is the shift into the FC's
//! local frame. Scans arrive in body FRD, bins clockwise from the nose.

mod band;
mod closing;
mod grid;
mod level;
mod probe;
mod project;
mod sight;
mod tiles;

use std::collections::{HashMap, HashSet};

use serde_json::Value;

pub use band::{band_top_m, Climb, BAND_TOP_M, OVERHEAD_MARGIN_M, WALL_MEMORY_MS};
pub use grid::{Grid, CELL_M, GRID_N};
pub use level::level_hit;
pub use probe::{Bar, LegProbe};
pub use project::{project_camera, Attitude, Scratch};
#[cfg(test)]
pub use tiles::encode_for_test as encode_tile_for_test;

use crate::geo::{self, V2};

/// A return closer to the ground than this, after levelling, is ground.
pub const GROUND_REJECT_M: f32 = 0.4;
/// The airframe's top over the planar scan plane: the planar lidar is
/// mounted above every other part so nothing cuts its ring, and its own
/// housing rises at most this far over its plane (an RPLIDAR is about
/// 41 mm tall).
pub const FRAME_TOP_M: f32 = 0.03;
/// Half the height of the flight slice the planar scan stands for: the
/// airframe's top ([`FRAME_TOP_M`]) plus the confined margin's 0.1 m past
/// the airframe ([`crate::config::CONFINED_TIP_EXTRA_M`]). A return levelled
/// higher than this over the scan plane met something over the slice: a
/// tunnel roof a nose-up scan reaches, or the upper part of a tall obstacle
/// the brake's own nose-up pitch lifts the beam onto. The route check's
/// feasibility ignores it ([`Hit::overhead`]), where a roof read as a wall
/// beside the leg made a phantom pinch, and judges a leg only where it runs
/// within this of the scan plane. The brake's readers take the swept band
/// instead ([`BAND_TOP_M`]): a return over the band is a wall only on a
/// wall the band held lately, so a column the brake's pitch lifts stays one
/// while a canopy's underside does not. Nothing below the plane is set
/// apart this way down to the ground band: the body, guards and props sit
/// under it, so a return there is collision geometry.
pub const SLICE_HALF_M: f32 = FRAME_TOP_M + crate::config::CONFINED_TIP_EXTRA_M;
/// Scans older than this are not used.
pub const SCAN_MAX_AGE_MS: u64 = 500;
/// How far the planar scan sees along a bin without a return (the 12 m
/// planar lidar; the sector display marches the grid as far).
pub const SCAN_HORIZON_M: f32 = 12.0;
/// A scan bin at or under this range returned nothing.
pub const MIN_RETURN_M: f32 = 0.05;
/// Neighbouring returns (in bearing order) this close are one unbroken
/// surface when measuring how wide it is ([`Obstacles::nearest_surface`]).
pub const SURFACE_GAP_M: f32 = 0.5;

/// The bins of a `plugin.navigation.scan` payload that returned a surface
/// (a finite range above [`MIN_RETURN_M`]), whatever its frame.
pub fn scan_returns(payload: &Value) -> usize {
    payload
        .get("ranges_m")
        .and_then(Value::as_array)
        .map_or(0, |ranges| {
            ranges
                .iter()
                .filter_map(Value::as_f64)
                .filter(|r| r.is_finite() && *r as f32 > MIN_RETURN_M)
                .count()
        })
}
/// Sectors either side a surface new to a sector may have come from in one
/// scan: a post 2 m abeam at 3 m/s sweeps 7° (more than a 5° sector) per
/// 10 Hz scan.
pub const HANDOVER_SECTORS: usize = 2;
pub use closing::SCAN_CLOSING_MAX_MPS;

/// The 5° sector (of `n`) of a bearing, radians clockwise from north.
fn sector_of(bearing: f32, n: usize) -> usize {
    (bearing.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU * n as f32) as usize % n
}

/// One non-ground scan return.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Body bearing, radians clockwise from the nose (levelled).
    pub body_bearing: f32,
    /// Horizontal range, metres.
    pub range_m: f32,
    /// Local NED horizontal position.
    pub ne: V2,
    /// Levelled more than [`SLICE_HALF_M`] over the scan plane. The route
    /// check's feasibility skips it.
    pub overhead: bool,
    /// Height over the scan plane after levelling, m (negative below).
    pub lift_m: f32,
}

/// The obstacle model.
#[derive(Debug, Default)]
pub struct Obstacles {
    tiles: HashMap<(i32, i32), tiles::Tile>,
    tile_res_m: f32,
    tile_cells: usize,
    /// `local − map` horizontal offset.
    offset: Option<V2>,
    /// The current scan's walls at flight height ([`band`]).
    hits: Vec<Hit>,
    /// The current scan's returns over the swept band on no remembered
    /// wall: the underside of a canopy. Not walls; kept for what flies
    /// under them ([`Obstacles::overhead_hits`]).
    overhead: Vec<Hit>,
    /// Walls seen in the band lately ([`band::WallMemory`]).
    walls: band::WallMemory,
    /// What the band was seen through lately ([`band::SeenFree`]).
    seen: band::SeenFree,
    /// The climb the band rises with ([`Climb`]), set before each scan.
    pub band_climb: Climb,
    /// Where each bin of the current scan stops telling the band anything:
    /// where a tilted beam left it (over its top or into the floor's band,
    /// unless a beam in the band saw further along that bearing lately), or
    /// [`SCAN_HORIZON_M`] when it returns nothing. Nothing beyond is known
    /// free, so the stick brake treats these points as possible obstacles.
    horizon: Vec<V2>,
    /// When the current scan arrived (`None`: no scan yet).
    scan_ms: Option<u64>,
    /// The current scan's capture stamp (`t_ns`), when it carried one.
    scan_stamp_ns: Option<u64>,
    /// Closing speed toward each world sector (north = 0, clockwise, 5°)
    /// measured from the scans themselves: the range rate of the sector's
    /// nearest return over the last scans, while it behaves like one surface
    /// ([`closing::RangeTrack`]), m/s ≥ 0. The brake takes the larger of this
    /// and the EKF's velocity, so an estimate that lags the real motion
    /// (delayed external navigation) cannot make it brake late.
    closing: Vec<f32>,
    /// Per world sector, the nearest ranges the closing speed is fitted to.
    tracks: Vec<closing::RangeTrack>,
    pub grid: Grid,
    /// The pose's horizontal one-sigma uncertainty (m) and when it arrived.
    pose_sigma: Option<(f32, u64)>,
    /// The least σ the poses had in the window before the last occupancy
    /// payload (`[0]`) and since it (`[1]`). A cell newly occupied in a
    /// payload was seen within those two windows: a scan reaches the 1 Hz
    /// tiles by the next payload at the latest.
    seen_floor: [Option<f32>; 2],
    /// The least σ any pose has published: a matched pose's
    /// ([`probe::Bar`]).
    sigma_low: Option<f32>,
    /// The last σ a pose carried.
    sigma_last: Option<f32>,
    /// The last occupancy payload's `seq`.
    occ_seq: Option<u64>,
    /// After a gap in the payloads' `seq` (a payload lost, a reconnect, a
    /// restarted producer): the tiles delivered since. A cell may have
    /// turned occupied in a lost payload, so a tile's first delivery after
    /// the gap cannot date its new cells.
    since_gap: Option<HashSet<(i32, i32)>>,
    /// The last pose reported its health `lost`: no match holds it to the
    /// map.
    pose_lost: bool,
    /// When σ last rose: the pose then rode something no scan checked.
    sigma_rose_ms: Option<u64>,
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

    /// Ingest a `plugin.navigation.pose` payload with the FC local position
    /// at the same moment.
    pub fn ingest_pose(&mut self, payload: &Value, local_ne: Option<V2>, now_ms: u64) {
        // The planar scan matcher's horizontal one-sigma (`sigma_xy_m`); a
        // pose without it says nothing about its uncertainty.
        self.pose_sigma = payload
            .get("sigma_xy_m")
            .and_then(Value::as_f64)
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(|s| (s as f32, now_ms));
        self.note_pose_sigma(payload, now_ms);
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

    /// The last pose's `sigma_xy_m` and when it arrived, however old.
    pub fn last_pose_sigma(&self) -> Option<(f32, u64)> {
        self.pose_sigma
    }

    /// Ingest a `plugin.navigation.scan` payload. `closing_max_mps` is the
    /// fastest closing speed a standing obstacle can show this scan (the
    /// brake's trust bound on the vehicle's own speed, or
    /// [`SCAN_CLOSING_MAX_MPS`] when that speed is unknown): a sector whose
    /// nearest range falls faster counts as closing at this.
    pub fn ingest_scan(
        &mut self,
        payload: &Value,
        level: Level,
        pos_ne: V2,
        closing_max_mps: f32,
        now_ms: u64,
    ) {
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
        let sorted = band::sort_scan(
            ranges,
            start,
            inc as f32,
            level,
            pos_ne,
            self.band_climb,
            &mut self.walls,
            &mut self.seen,
            now_ms,
        );
        self.hits = sorted.walls;
        self.overhead = sorted.overhead;
        self.horizon = sorted.horizon;
        // The scan's own capture time: delivery jitter (two scans arriving
        // 50 ms apart though captured 100 ms apart) must not double a range
        // rate.
        let stamp_ns = payload.get("t_ns").and_then(Value::as_u64);
        self.update_closing(level.yaw, stamp_ns, closing_max_mps, now_ms);
        self.scan_ms = Some(now_ms);
        self.scan_stamp_ns = stamp_ns;
    }

    /// How old the current scan is, ms (`None`: no scan yet).
    pub fn scan_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.scan_ms.map(|t| now_ms.saturating_sub(t))
    }

    /// A scan arrived within [`SCAN_MAX_AGE_MS`].
    pub fn scan_fresh(&self, now_ms: u64) -> bool {
        self.scan_age_ms(now_ms)
            .is_some_and(|a| a <= SCAN_MAX_AGE_MS)
    }

    /// A scan arrived at some point since start: an obstacle sensor exists.
    pub fn scan_seen(&self) -> bool {
        self.scan_ms.is_some()
    }

    /// The current scan's nearest return that is not the floor, horizontal
    /// metres; `None` when it has none.
    #[cfg(test)]
    pub fn nearest_hit_m(&self) -> Option<f32> {
        self.hits.iter().map(|h| h.range_m).reduce(f32::min)
    }

    /// The current scan's nearest non-floor return as (horizontal range,
    /// local NE position, extent). The extent is the length of the unbroken
    /// run of returns through it: walking outward by bearing from the
    /// nearest return until neighbours are more than [`SURFACE_GAP_M`]
    /// apart. A wall, a car or a room around the vehicle reads metres long;
    /// a pole, a person, a drone, or several of them apart, read short.
    pub fn nearest_surface(&self) -> Option<(f32, V2, f32)> {
        let n = self.hits.len();
        let (i0, near) = self
            .hits
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.range_m.total_cmp(&b.1.range_m))?;
        // The hits are in bearing order and the scan wraps at the nose:
        // walk `step` (1 or n - 1) at most `max` neighbours, returning the
        // run's length and how many it took.
        let walk = |step: usize, max: usize| {
            let (mut at, mut len, mut taken) = (i0, 0.0, 0);
            while taken < max {
                let next = (at + step) % n;
                let gap = geo::dist(self.hits[at].ne, self.hits[next].ne);
                if gap > SURFACE_GAP_M {
                    break;
                }
                len += gap;
                taken += 1;
                at = next;
            }
            (len, taken)
        };
        let (fwd, taken) = walk(1, n - 1);
        let (back, _) = walk(n - 1, n - 1 - taken);
        Some((near.range_m, near.ne, fwd + back))
    }

    /// Closing speed toward the world direction `dir` (unit) measured from
    /// the scans: the largest of its sector and the two beside it, m/s.
    pub fn scan_closing(&self, dir: V2, now_ms: u64) -> f32 {
        let n = self.closing.len();
        if n == 0 || !self.scan_fresh(now_ms) {
            return 0.0;
        }
        let s = sector_of(geo::bearing_of(dir), n);
        [(s + n - 1) % n, s, (s + 1) % n]
            .iter()
            .map(|&i| self.closing[i])
            .fold(0.0, f32::max)
    }

    /// The fastest closing speed toward any sector, m/s.
    pub fn max_scan_closing(&self, now_ms: u64) -> f32 {
        if !self.scan_fresh(now_ms) {
            return 0.0;
        }
        self.closing.iter().copied().fold(0.0, f32::max)
    }

    /// Current (fresh) scan horizon points within `r` of `pos`.
    pub fn horizon_near(&self, pos: V2, r: f32, now_ms: u64) -> impl Iterator<Item = V2> + '_ {
        let fresh = self.scan_fresh(now_ms);
        self.horizon
            .iter()
            .copied()
            .filter(move |p| fresh && geo::dist(*p, pos) <= r)
    }

    /// Current (fresh) walls at flight height: the scan's returns in the
    /// swept band, and over it on a wall seen in the band lately.
    pub fn hits(&self, now_ms: u64) -> &[Hit] {
        if self.scan_fresh(now_ms) {
            &self.hits
        } else {
            &[]
        }
    }

    /// Current (fresh) returns over the swept band on no remembered wall:
    /// what the vehicle flies under (a canopy's underside), with their
    /// height over the scan plane ([`Hit::lift_m`]).
    pub fn overhead_hits(&self, now_ms: u64) -> &[Hit] {
        if self.scan_fresh(now_ms) {
            &self.overhead
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
        let fresh = self.scan_fresh(now_ms);
        if fresh {
            for h in &self.hits {
                self.grid.mark(h.ne);
            }
        }
        self.grid.transform();
    }

    /// The FC's local estimate stepped by `d` (an EKF position reset) while
    /// the vehicle stayed put: what the store holds in the FC's local frame
    /// (the scan, the map-to-local offset) moves with it. The grid follows
    /// at the next [`Obstacles::rebuild`].
    pub fn shift(&mut self, d: V2) {
        for h in self.hits.iter_mut().chain(self.overhead.iter_mut()) {
            h.ne = geo::add(h.ne, d);
        }
        for p in &mut self.horizon {
            *p = geo::add(*p, d);
        }
        self.walls.shift(d);
        self.seen.shift(d);
        if let Some(o) = self.offset.as_mut() {
            *o = geo::add(*o, d);
        }
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

#[cfg(test)]
mod tests;
