//! The height band the vehicle sweeps, and which returns are walls in it.
//!
//! A planar lidar fixed to the frame tilts with every lean, so a return
//! levelled with the attitude ([`super::level`]) lies at some height over
//! the scan plane. The vehicle sweeps a band from the ground band
//! ([`super::GROUND_REJECT_M`]) up to its top plus a vertical clearance
//! ([`BAND_TOP_M`]), raised along the motion by the climb asked
//! ([`band_top_m`]). A return inside it is a wall at flight height. A return
//! above it met something over the vehicle: under a canopy, the crown's
//! underside a leaning scan reaches a few metres off. It is kept as an
//! overhead hit ([`super::Obstacles::overhead_hits`]), not a wall, unless the
//! same surface was a wall in the band within the last [`WALL_MEMORY_MS`]
//! ([`WallMemory`]): a trunk or a column a braking pitch lifts over the band
//! was seen in it on the approach, and stays a wall through the lean.
//!
//! A return reached by a beam that left the band (rising over it, or down
//! into the ground band) says nothing about the band past where the beam
//! left: the band there is unknown, not free. The scan's horizon for that
//! bin (the seen space the brake keeps its stop inside) is where the beam
//! left the band, unless a beam cast from on the same line that stayed in
//! the band saw further within the last [`FREE_MEMORY_MS`] ([`SeenFree`]);
//! a lean never reads as clearance it did not see. A beam with no return
//! within reach is open air: its horizon is the lidar's reach however the
//! lean tilts it, so a stop in open air is not cut short by its own pitch.

use std::collections::HashMap;

use serde_json::Value;

use super::level::{beam, level_return};
use super::{Hit, Level, FRAME_TOP_M, GROUND_REJECT_M, MIN_RETURN_M, SCAN_HORIZON_M};
use crate::geo::{self, V2};

/// The vertical clearance kept over the airframe's top, m: position error
/// in height and a margin over the props.
pub const OVERHEAD_MARGIN_M: f32 = 0.3;
/// The swept band's top over the scan plane in level flight, m.
pub const BAND_TOP_M: f32 = FRAME_TOP_M + OVERHEAD_MARGIN_M;
/// The horizontal speed under which a climb counts as straight up: the band
/// rises by the climb over the time this speed takes to cover a metre.
pub const BAND_CLIMB_MIN_MPS: f32 = 0.5;
/// How long a wall seen in the band keeps the returns over the band near it
/// walls, ms of the pilot clock: longer than a braking pitch lasts.
pub const WALL_MEMORY_MS: u64 = 2_000;
/// A return over the band within this of a remembered wall is that wall,
/// m: a trunk's face read from a little further round, or range noise.
pub const WALL_JOIN_M: f32 = 0.3;
/// How long a bearing seen through in the band stands for the band along
/// it when the beams now leave the band before, ms of the pilot clock:
/// longer than an acceleration or a stop leans the copter, so its own lean
/// does not read as unknown space it saw moments before. What the band
/// held over that time is taken as standing still.
pub const FREE_MEMORY_MS: u64 = 5_000;
/// The bearing bins of [`SeenFree`], one per degree.
const FREE_BINS: usize = 360;
/// The memory's cell, m.
const CELL_M: f32 = 0.15;
/// How far off a stored beam's line a beam may run and still take that
/// beam's free space up to its own return, m: further off, an obstacle
/// short enough to pass under a beam that rose with pitch could stand
/// between the two lines unseen by either. Within it, an obstacle
/// narrower than the gap between the lines may still stand there unseen:
/// this bounds that residual, it does not remove it.
const FREE_OFFSET_M: f32 = 1.25;
/// The lanes [`SeenFree`] keeps per bearing.
const FREE_LANES: usize = 4;

/// The climb the band rises with: per horizontal metre (the height the
/// vehicle gains before it reaches a return that far out), up to the
/// height it still climbs to its setpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Climb {
    pub slope: f32,
    pub up_to_m: f32,
}

impl Climb {
    /// A climb of `up_mps` at a horizontal speed of `h_mps` with `up_to_m`
    /// still to climb.
    pub fn of(up_mps: f32, h_mps: f32, up_to_m: f32) -> Self {
        Self {
            slope: up_mps.max(0.0) / h_mps.max(BAND_CLIMB_MIN_MPS),
            up_to_m: up_to_m.max(0.0),
        }
    }

    /// How much higher the vehicle is by the time it reaches `range_m` out.
    pub fn rise_at(self, range_m: f32) -> f32 {
        (self.slope * range_m).min(self.up_to_m)
    }
}

/// The band's top over the scan plane at `range_m` for the `climb`, m.
pub fn band_top_m(range_m: f32, climb: Climb) -> f32 {
    BAND_TOP_M + climb.rise_at(range_m)
}

/// Where walls were seen in the band lately: local NE cells with the time
/// they were last seen.
#[derive(Debug, Default)]
pub struct WallMemory {
    cells: HashMap<(i32, i32), u64>,
}

fn cell(p: V2) -> (i32, i32) {
    (
        (p[0] / CELL_M).floor() as i32,
        (p[1] / CELL_M).floor() as i32,
    )
}

fn centre(c: (i32, i32)) -> V2 {
    [(c.0 as f32 + 0.5) * CELL_M, (c.1 as f32 + 0.5) * CELL_M]
}

impl WallMemory {
    /// A wall at `p` now.
    pub fn note(&mut self, p: V2, now_ms: u64) {
        self.cells.insert(cell(p), now_ms);
    }

    /// A wall was seen within [`WALL_JOIN_M`] of `p` (to the cell's
    /// quantisation) in the last [`WALL_MEMORY_MS`].
    pub fn holds(&self, p: V2, now_ms: u64) -> bool {
        let reach = (WALL_JOIN_M / CELL_M).ceil() as i32 + 1;
        let (ci, cj) = cell(p);
        let near = WALL_JOIN_M + CELL_M * std::f32::consts::FRAC_1_SQRT_2;
        (-reach..=reach).any(|di| {
            (-reach..=reach).any(|dj| {
                let c = (ci + di, cj + dj);
                self.cells.get(&c).is_some_and(|&t| {
                    now_ms.saturating_sub(t) <= WALL_MEMORY_MS && geo::dist(centre(c), p) <= near
                })
            })
        })
    }

    /// Forget walls older than [`WALL_MEMORY_MS`].
    pub fn prune(&mut self, now_ms: u64) {
        self.cells
            .retain(|_, t| now_ms.saturating_sub(*t) <= WALL_MEMORY_MS);
    }

    /// The local frame stepped by `d` (an EKF position reset): every wall
    /// moves with it.
    pub fn shift(&mut self, d: V2) {
        let moved: Vec<_> = self
            .cells
            .drain()
            .map(|(c, t)| (cell(geo::add(centre(c), d)), t))
            .collect();
        for (c, t) in moved {
            let e = self.cells.entry(c).or_insert(t);
            *e = (*e).max(t);
        }
    }
}

/// Per world bearing (one-degree bins), the latest beams that stayed in the
/// band along it, one per lane of lateral position: where each was cast
/// from, where it stopped seeing (its return, or the lidar's reach), and
/// when.
#[derive(Debug)]
pub struct SeenFree {
    beams: Vec<[Option<Seen>; FREE_LANES]>,
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    from: V2,
    end: V2,
    at_ms: u64,
}

impl Default for SeenFree {
    fn default() -> Self {
        Self {
            beams: vec![[None; FREE_LANES]; FREE_BINS],
        }
    }
}

fn bin_of(dir: V2) -> usize {
    let deg = geo::bearing_of(dir).to_degrees().rem_euclid(360.0);
    (deg as usize).min(FREE_BINS - 1)
}

/// `p` lies within [`CELL_M`] of the segment `a`–`b`.
fn on_ray(p: V2, a: V2, b: V2) -> bool {
    let ab = geo::sub(b, a);
    let n2 = geo::dot(ab, ab);
    let t = if n2 > 1e-9 {
        (geo::dot(geo::sub(p, a), ab) / n2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    geo::dist(p, geo::add(a, geo::scale(ab, t))) <= CELL_M
}

/// Distance from `p` to the line through `a` and `b`.
fn lateral(p: V2, a: V2, b: V2) -> f32 {
    let ab = geo::sub(b, a);
    let n = geo::dot(ab, ab).sqrt();
    if n < 1e-6 {
        return geo::dist(p, a);
    }
    let ap = geo::sub(p, a);
    (ap[0] * ab[1] - ap[1] * ab[0]).abs() / n
}

impl SeenFree {
    /// Keeps `from → end` as a lane of its bearing: it replaces a lane
    /// running within half [`FREE_OFFSET_M`] of it, else an empty or the
    /// oldest one, so beams cast from a few lines side by side stand.
    fn saw(&mut self, dir: V2, from: V2, end: V2, now_ms: u64) {
        let lanes = &mut self.beams[bin_of(dir)];
        let near = lanes
            .iter()
            .position(|l| l.is_some_and(|l| lateral(from, l.from, l.end) <= FREE_OFFSET_M / 2.0));
        let i = near.unwrap_or_else(|| {
            (0..FREE_LANES)
                .min_by_key(|&i| lanes[i].map_or(0, |l| l.at_ms + 1))
                .unwrap_or(0)
        });
        lanes[i] = Some(Seen {
            from,
            end,
            at_ms: now_ms,
        });
    }

    /// The horizon of a beam along `dir` from `pos` that left the band at
    /// `exit` and returned at `ret_m` (horizontal m; `None` for no return):
    /// the furthest a lane in the band saw along that bearing within
    /// [`FREE_MEMORY_MS`] vouches for, when it is further than `exit`. Seen
    /// from on a lane (`pos` within [`CELL_M`] of it), its end stands.
    /// From a lane within [`FREE_OFFSET_M`] of `pos`, it ran along a
    /// parallel line, past whatever this beam's own return met: the
    /// horizon goes no further than that return, so a trunk lifted over the
    /// band stays the edge of the seen space. A lane further off vouches
    /// for nothing (a short obstacle under this risen beam may stand
    /// between the lines; within the offset, one narrower than the gap
    /// still may), nor does any lane with no return to bound it: `exit`.
    fn horizon(&self, dir: V2, pos: V2, exit: V2, ret_m: Option<f32>, now_ms: u64) -> V2 {
        let exit_m = geo::dist(exit, pos);
        let mut best = (exit_m, exit);
        let fresh = self.beams[bin_of(dir)]
            .iter()
            .flatten()
            .filter(|s| now_ms.saturating_sub(s.at_ms) <= FREE_MEMORY_MS);
        for s in fresh {
            let end_m = geo::dist(s.end, pos);
            let h = if on_ray(pos, s.from, s.end) {
                Some((end_m, s.end))
            } else if lateral(pos, s.from, s.end) <= FREE_OFFSET_M + 1e-3 {
                ret_m.map(|r| {
                    let m = r.min(end_m);
                    (m, geo::add(pos, geo::scale(dir, m)))
                })
            } else {
                None
            };
            if let Some(h) = h.filter(|h| h.0 > best.0) {
                best = h;
            }
        }
        best.1
    }

    pub fn shift(&mut self, d: V2) {
        for s in self.beams.iter_mut().flatten().flatten() {
            s.from = geo::add(s.from, d);
            s.end = geo::add(s.end, d);
        }
    }
}

/// One scan sorted by the band.
#[derive(Debug, Default)]
pub(super) struct Sorted {
    /// Walls at flight height: returns in the band, and returns over it on
    /// a wall the band held lately.
    pub walls: Vec<Hit>,
    /// Returns over the band on no remembered wall.
    pub overhead: Vec<Hit>,
    /// Where each beam stops telling the band anything: its wall, the
    /// lidar's reach, or where it left the band ([`SeenFree::horizon`]).
    /// Walls are not horizon points; the rest are.
    pub horizon: Vec<V2>,
}

/// Sort the scan `ranges` (bins from `start` every `inc`, radians) from
/// `pos_ne` at the attitude `level`, the band rising with `climb`.
#[allow(clippy::too_many_arguments)]
pub(super) fn sort_scan(
    ranges: &[Value],
    start: f32,
    inc: f32,
    level: Level,
    pos_ne: V2,
    climb: Climb,
    walls: &mut WallMemory,
    seen: &mut SeenFree,
    now_ms: u64,
) -> Sorted {
    let mut out = Sorted::default();
    walls.prune(now_ms);
    for (i, r) in ranges.iter().enumerate() {
        let r = r.as_f64().unwrap_or(0.0) as f32;
        let theta = start + inc * i as f32;
        let (dir, hz, rise) = beam(theta, level);
        // How far out (horizontally) the beam stays in the band: over its
        // top when it rises faster than the band, into the ground band
        // when it falls.
        let over = (rise > 0.0).then(|| {
            let rising = (rise > climb.slope).then(|| BAND_TOP_M / (rise - climb.slope));
            match rising {
                Some(x) if climb.slope * x <= climb.up_to_m => x,
                _ => (BAND_TOP_M + climb.up_to_m) / rise,
            }
        });
        let under = match level.agl {
            Some(agl) if rise < 0.0 => Some((agl - GROUND_REJECT_M).max(0.0) / -rise),
            _ => None,
        };
        let exit_m = over.into_iter().chain(under).fold(f32::INFINITY, f32::min);
        let at = |m: f32| geo::add(pos_ne, geo::scale(dir, m));
        if r <= MIN_RETURN_M || !r.is_finite() {
            // No return within reach: open air. A beam the vehicle's own
            // lean lifts out of the band still found nothing anywhere along
            // it, so its horizon stays at the lidar's reach along the
            // heading (a stop in open air is not cut short by its own
            // pitch); only a beam that stayed in the band is remembered as
            // seen through.
            let far = geo::body_to_ned(
                SCAN_HORIZON_M * theta.cos(),
                SCAN_HORIZON_M * theta.sin(),
                level.yaw,
            );
            out.horizon.push(geo::add(pos_ne, far));
            let reach = SCAN_HORIZON_M * hz;
            if exit_m >= reach {
                seen.saw(dir, pos_ne, at(reach), now_ms);
            }
            continue;
        }
        let (hit, ground) = level_return(r, theta, level, pos_ne);
        if ground {
            let exit = at(exit_m.min(hit.range_m));
            let ret = Some(hit.range_m);
            out.horizon
                .push(seen.horizon(dir, pos_ne, exit, ret, now_ms));
            continue;
        }
        if hit.lift_m <= band_top_m(hit.range_m, climb) {
            walls.note(hit.ne, now_ms);
            seen.saw(dir, pos_ne, hit.ne, now_ms);
            out.walls.push(hit);
        } else if walls.holds(hit.ne, now_ms) {
            out.walls.push(hit);
        } else {
            out.overhead.push(hit);
            let exit = at(exit_m.min(hit.range_m));
            let ret = Some(hit.range_m);
            out.horizon
                .push(seen.horizon(dir, pos_ne, exit, ret, now_ms));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_holds_returns_near_it_for_the_memory() {
        let mut m = WallMemory::default();
        m.note([3.0, 0.0], 1_000);
        assert!(m.holds([3.2, 0.1], 1_000));
        assert!(!m.holds([3.6, 0.0], 1_000), "0.6 m off is another surface");
        assert!(m.holds([3.0, 0.0], 1_000 + WALL_MEMORY_MS));
        assert!(!m.holds([3.0, 0.0], 1_001 + WALL_MEMORY_MS));
        m.shift([1.0, -2.0]);
        assert!(m.holds([4.0, -2.0], 1_000));
        m.prune(1_001 + WALL_MEMORY_MS);
        assert!(!m.holds([4.0, -2.0], 1_000));
    }

    #[test]
    fn the_band_rises_with_the_climb_asked_up_to_the_height_left() {
        assert_eq!(band_top_m(5.0, Climb::default()), BAND_TOP_M);
        // Climbing 1 m/s at 2 m/s, 3 m to go: 0.5 m higher 1 m out.
        let c = Climb::of(1.0, 2.0, 3.0);
        assert!((band_top_m(1.0, c) - (BAND_TOP_M + 0.5)).abs() < 1e-6);
        // Never higher than the setpoint: 3 m at most.
        assert!((band_top_m(10.0, c) - (BAND_TOP_M + 3.0)).abs() < 1e-6);
        // Straight up: as if at the least speed.
        assert!((Climb::of(1.0, 0.0, 3.0).slope - 2.0).abs() < 1e-6);
        assert_eq!(
            Climb::of(-1.0, 2.0, 3.0).rise_at(5.0),
            0.0,
            "a descent lowers nothing"
        );
    }

    #[test]
    fn seen_free_space_counts_only_from_on_the_beam_that_saw_it() {
        // A trunk 4 m north is seen in the band, then a sustained lean lifts
        // its return over the band for longer than the wall memory.
        let mut walls = WallMemory::default();
        let mut seen = SeenFree::default();
        let north = [1.0, 0.0];
        walls.note([4.0, 0.0], 0);
        // Earlier, from 1 m east, a beam north stayed in the band to 12 m.
        seen.saw(north, [0.0, 1.0], [12.0, 1.0], 0);
        let now = WALL_MEMORY_MS + 500;
        assert!(!walls.holds([4.0, 0.0], now), "the wall has lapsed");
        // Now at the origin (moved 1 m sideways), the beam north leaves the
        // band at 2 m and returns from the trunk over it at 4 m: the stored
        // ray ran 1 m off this one, past the trunk, so the horizon stops at
        // the trunk, not at the stored 12 m.
        let exit = [2.0, 0.0];
        let h = seen.horizon(north, [0.0, 0.0], exit, Some(4.0), now);
        assert!(geo::dist(h, [4.0, 0.0]) < 1e-5, "{h:?}");
        // With no return to bound it, the exit.
        assert_eq!(seen.horizon(north, [0.0, 0.0], exit, None, now), exit);
        // From on that stored ray its end stands, past a return.
        let on = [3.0, 1.0];
        let h = seen.horizon(north, on, [5.0, 1.0], Some(3.0), now);
        assert_eq!(h, [12.0, 1.0]);
        // Within a cell of it too; and the frame shift carries the origin.
        let h = seen.horizon(north, [3.0, 1.1], [5.0, 1.1], None, now);
        assert_eq!(h, [12.0, 1.0]);
        seen.shift([0.0, -1.0]);
        let h = seen.horizon(north, [3.0, 0.0], [5.0, 0.0], None, now);
        assert_eq!(h, [12.0, 0.0]);
        // Stale past the memory: the exit.
        let late = FREE_MEMORY_MS + 1;
        let h = seen.horizon(north, [3.0, 0.0], [5.0, 0.0], None, late);
        assert_eq!(h, [5.0, 0.0]);
    }

    #[test]
    fn a_remembered_beam_far_off_this_one_clears_nothing_under_a_risen_beam() {
        // A beam north stayed in the band to 12 m from 2 m east. Now at the
        // origin a pitch lifts this beam over the band at 2 m; it passes
        // over a short obstacle at 4 m and returns from a trunk at 9 m. The
        // stored line runs 2 m off: nothing says the 2-9 m stretch is clear.
        let mut seen = SeenFree::default();
        let north = [1.0, 0.0];
        seen.saw(north, [0.0, 2.0], [12.0, 2.0], 0);
        let exit = [2.0, 0.0];
        assert_eq!(seen.horizon(north, [0.0, 0.0], exit, Some(9.0), 100), exit);
        // Within a metre off, the stored beam vouches up to the return.
        let h = seen.horizon(north, [0.0, 1.0], [2.0, 1.0], Some(9.0), 100);
        assert!(geo::dist(h, [9.0, 1.0]) < 1e-5, "{h:?}");
        // A beam cast later from the origin's line keeps its own lane: the
        // one 2 m east still stands for a beam cast from there.
        seen.saw(north, [0.0, 0.0], [6.0, 0.0], 200);
        assert_eq!(
            seen.horizon(north, [1.0, 0.0], [2.0, 0.0], None, 300),
            [6.0, 0.0]
        );
        assert_eq!(
            seen.horizon(north, [1.0, 2.0], [2.0, 2.0], None, 300),
            [12.0, 2.0]
        );
    }
}
