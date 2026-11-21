//! The sensing tier the pilot flies on and its speed rule.
//!
//! The tier comes from the active world model: a live 3D voxel map's tier
//! (`lidar3d`, `depth`, `camera`), else `planar`. The seen-clear range is
//! how far the sensors have seen along the vehicle's motion (its velocity;
//! from a hover, the way to the plan's target, else the nose): on a 3D
//! tier, rays through the map at flight height, across the body's width a
//! voxel apart, to the first voxel never observed (an obstacle's shadow, a
//! camera's unseen side, the window's edge); on the planar tier, the scans'
//! horizon beside that line (where they stop seeing). Obstacles inside the
//! seen space are the brake's and the planner's: they do not shorten the
//! range. The speed limit is the one that still stops inside it:
//! `v·0.3 + v²/(2·a_brk) ≤ clear − margin`, with the camera tier's range
//! taken at 0.6 and the planar tier held to 3 m/s (a planar lidar sees
//! nothing over or under its band). A goal flies under it while avoidance
//! is on (`command.limits` code `sensing` when it binds), and the state
//! publishes it as `sensing`.

use ados_protocol::pilot::{AvoidMode, Sensing, SensingTier};

use crate::core::world_feed::WORLD_SCAN_MAX_M;
use crate::core::Pilot;
use crate::geo::{self, V2};
use crate::obstacles::SCAN_HORIZON_M;

/// The reaction time the speed rule plans for, s.
pub const SENSING_REACT_S: f32 = 0.3;
/// The share of its seen range the camera tier is trusted with.
pub const CAMERA_RANGE_SHARE: f32 = 0.6;
/// The planar tier's speed ceiling, m/s.
pub const PLANAR_MAX_MPS: f32 = 3.0;
/// Slower than this the vehicle's velocity names no direction, m/s.
const MOVING_MPS: f32 = 0.3;
/// The most gaps between the seen-range rays across the body.
const MAX_RAY_GAPS: f32 = 16.0;
/// A planar horizon point this near the line of motion ends its seen
/// range, m (as a scan hit ends [`crate::obstacles::Obstacles::ray_distance`]).
const LATERAL_M: f32 = 0.35;

/// The speed that stops within `room_m` braking at `a_brk` after
/// [`SENSING_REACT_S`]: the root of `v·t + v²/(2a) = room`. 0 without room.
pub fn speed_within(room_m: f32, a_brk: f32) -> f32 {
    if room_m <= 0.0 || a_brk <= 0.0 {
        return 0.0;
    }
    let t = SENSING_REACT_S;
    a_brk * ((t * t + 2.0 * room_m / a_brk).sqrt() - t)
}

impl Pilot {
    /// Work out this tick's sensing ([`Pilot::sensing`]).
    pub(in crate::core) fn sensing_tick(&mut self, now_ms: u64) {
        self.sensing = self.sensing_now(now_ms);
    }

    /// The sensing goals fly under, as of this tick; `None` without an
    /// obstacle sensor.
    pub fn sensing(&self) -> Option<Sensing> {
        self.sensing
    }

    /// The speed limit the sensing rule puts on a goal now; `None` with
    /// avoid off or no obstacle sensor.
    pub(in crate::core) fn sensing_cap(&self) -> Option<f32> {
        if self.settings.pilot.avoid == AvoidMode::Off {
            return None;
        }
        self.sensing.map(|s| s.speed_limit_mps)
    }

    fn sensing_now(&self, now_ms: u64) -> Option<Sensing> {
        let pos = self.pos(now_ms)?;
        let world = self.world.live_3d(now_ms);
        if !world && !self.obstacles.scan_seen() {
            return None;
        }
        let tier = if world {
            self.world.tier()
        } else {
            SensingTier::Planar
        };
        let dir = self.sensing_dir(pos, now_ms);
        let seen = if world {
            self.world_clear_m(pos, dir)
        } else if self.obstacles.scan_fresh(now_ms) {
            self.planar_clear_m([pos[0], pos[1]], dir, now_ms)
        } else {
            0.0
        };
        let clear = match tier {
            SensingTier::Camera => seen * CAMERA_RANGE_SHARE,
            _ => seen,
        };
        let mut v = speed_within(clear - self.margin(), self.profile_limits().a_brk);
        if tier == SensingTier::Planar {
            v = v.min(PLANAR_MAX_MPS);
        }
        Some(Sensing::new(tier, clear, v))
    }

    /// The unit direction the range is read along: the velocity, else the
    /// way to the plan's target, else the nose.
    fn sensing_dir(&self, pos: [f32; 3], now_ms: u64) -> V2 {
        if let Some(l) = self.fc.fresh_local(now_ms) {
            let v = [l.vn, l.ve];
            let s = geo::norm(v);
            if s >= MOVING_MPS {
                return geo::scale(v, 1.0 / s);
            }
        }
        if let Some(t) = self.plan.target {
            let d = geo::sub(t, [pos[0], pos[1]]);
            let s = geo::norm(d);
            if s > 0.5 {
                return geo::scale(d, 1.0 / s);
            }
        }
        geo::bearing_unit(self.yaw(now_ms))
    }

    /// The range the map is seen along `dir` at flight height: rays across
    /// the body's width from side to side, no further apart than a voxel (a
    /// narrow obstacle's shadow between sparser rays would read as seen),
    /// from the body's edge (the vehicle's own voxels are where it is, seen
    /// or not); the shortest.
    fn world_clear_m(&self, pos: [f32; 3], dir: V2) -> f32 {
        let body = self.settings.pilot.body_radius_m();
        let side = [-dir[1], dir[0]];
        let len = WORLD_SCAN_MAX_M - body;
        let res = self.world.snapshot().meta.res_m;
        let gaps = (2.0 * body / res).ceil().clamp(2.0, MAX_RAY_GAPS) as usize;
        (0..=gaps)
            .map(|k| {
                let off = body * (2.0 * k as f32 / gaps as f32 - 1.0);
                let s = [
                    pos[0] + dir[0] * body + side[0] * off,
                    pos[1] + dir[1] * body + side[1] * off,
                    pos[2],
                ];
                let e = [s[0] + dir[0] * len, s[1] + dir[1] * len, pos[2]];
                body + self.world.ray_seen(s, e).clear_m(len)
            })
            .fold(f32::INFINITY, f32::min)
    }

    /// The range the planar scans see along `dir`: the nearest horizon
    /// point beside the line, else the lidar's range.
    fn planar_clear_m(&self, pos: V2, dir: V2, now_ms: u64) -> f32 {
        self.obstacles
            .horizon_near(pos, SCAN_HORIZON_M + 1.0, now_ms)
            .filter_map(|p| {
                let rel = geo::sub(p, pos);
                let t = geo::dot(rel, dir);
                (t > 0.0 && (rel[0] * dir[1] - rel[1] * dir[0]).abs() < LATERAL_M).then_some(t)
            })
            .fold(SCAN_HORIZON_M, f32::min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_speed_rule_stops_inside_the_room() {
        for (room, a) in [(5.0, 2.5), (12.0, 3.0), (0.4, 2.0)] {
            let v = speed_within(room, a);
            let stop = v * SENSING_REACT_S + v * v / (2.0 * a);
            assert!(
                (stop - room).abs() < 1e-3,
                "{room} {a}: {v} stops in {stop}"
            );
        }
        assert_eq!(speed_within(0.0, 2.5), 0.0);
        assert_eq!(speed_within(-1.0, 2.5), 0.0);
    }
}
