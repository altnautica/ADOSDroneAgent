//! Moving obstacles: people, vehicles and anything else the sensors see
//! move. Where `closing` measures how fast the nearest return of a sector
//! closes, this keeps each mover as a track with a position, a velocity and
//! an uncertainty, predicted on constant velocity over [`PREDICT_S`].
//!
//! Two sources feed it. A camera's multi-object tracker (`vision.detection`
//! v4 tracks with a world position, [`vision`]) names what a mover is. The
//! obstacle returns themselves (a planar scan's walls, or the live voxel
//! map's slab read as a scan) are clustered every scan ([`scan`]); a small
//! cluster low enough to stand on the ground, tracked from scan to scan,
//! that travels further than any standing obstacle's visible face can shift
//! becomes a mover of unknown kind, kept clear as a person.
//!
//! Three consumers read it: the separation guard keeps every person (and
//! every unknown mover) [`PERSON_SEP_M`] off horizontally unless the
//! vehicle flies [`OVER_HEAD_M`] over their head; the planners' grid gets
//! each mover's sweep over [`SWEEP_S`] ([`Movers::mark`]); the trajectory
//! planner checks its plans against each mover's predicted position at the
//! plan's own time ([`Mover::plan_keep_m`]). None of them counts the
//! skill's own subject ([`Movers::guarded`]): the subject brake keeps it.

use ados_protocol::pilot::{MoverKind, MoverSource, TrackedMover};

use super::{Grid, CELL_M};
use crate::geo::{self, V2};

/// Movers are predicted this far past their last measurement, s; further on
/// they are held where that prediction ends.
pub const PREDICT_S: f32 = 3.0;
/// The horizontal separation kept from a person (centre to the vehicle's
/// centre), m.
pub const PERSON_SEP_M: f32 = 2.5;
/// ... unless the vehicle flies at least this high over their head, m.
pub const OVER_HEAD_M: f32 = 3.4;
/// A person's height, m, for the head the vehicle flies over.
pub const PERSON_H_M: f32 = 2.0;
/// The horizontal distance kept from a road vehicle's centre, m.
pub const VEHICLE_KEEP_M: f32 = 3.0;
/// A plan keeps this much beyond the separation, on top of the mover's
/// uncertainty then, m.
pub const PLAN_INFLATE_M: f32 = 0.5;
/// The sweep of a mover the planners' grid holds, s.
pub const SWEEP_S: f32 = 2.0;
/// A person this much beyond [`Mover::plan_keep_m`] of the vehicle leaves
/// the grid's sweep to the separation guard, m.
const SWEEP_HANDOFF_M: f32 = 1.0;
/// A mover's uncertainty grows this fast while it goes unmeasured, m/s.
const SIGMA_GROW_MPS: f32 = 0.5;
/// A mover is kept this long after its last measurement, ms: its whole
/// prediction (a person who walked out of a forward camera's view, or
/// behind another, still walks).
const KEEP_MS: u64 = (PREDICT_S * 1000.0) as u64;
/// The re-acquire distance widens by this per second unmeasured, m/s.
const REACQUIRE_GROW_MPS: f32 = 0.5;
/// A cluster track not yet seen moving is dropped this long unmatched, ms.
const CANDIDATE_KEEP_MS: u64 = 400;
/// A velocity is smoothed over this long, s.
const VEL_SMOOTH_S: f32 = 0.5;
/// Fastest mover speed believed, m/s (a running person, a car in a street).
const MAX_MPS: f32 = 15.0;
/// Without the subject's track id, a mover within this of the skill's
/// subject (`subject_ne`) is that subject, m.
const SUBJECT_SAME_M: f32 = 1.0;
/// A cluster mover within this of a camera track is that track, m.
const SAME_M: f32 = 1.0;
/// A new cluster this near a mover that lost its cluster is that mover
/// seen again, m.
const REACQUIRE_M: f32 = 1.2;
/// Most tracks held.
const MAX_TRACKS: usize = 64;
/// A 3D lidar's own scan feeds the clusters for this long after it
/// arrives, ms; the voxel map's slab feeds them otherwise.
const RAW_FRESH_MS: u64 = 300;

/// One mover at its last measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mover {
    pub id: u32,
    pub kind: MoverKind,
    pub source: MoverSource,
    /// Local NED north, east at `seen_ms`, m.
    pub ne: V2,
    pub vel: V2,
    pub sigma_m: f32,
    pub seen_ms: u64,
}

impl Mover {
    /// Seconds of prediction to `t_ms` (0 before the measurement, at most
    /// [`PREDICT_S`]).
    fn ahead_s(&self, t_ms: u64) -> f32 {
        (t_ms.saturating_sub(self.seen_ms) as f32 / 1000.0).min(PREDICT_S)
    }

    /// Where it is predicted at `t_ms`.
    pub fn at(&self, t_ms: u64) -> V2 {
        geo::add(self.ne, geo::scale(self.vel, self.ahead_s(t_ms)))
    }

    /// Its one-sigma uncertainty at `t_ms`, m.
    pub fn sigma_at(&self, t_ms: u64) -> f32 {
        self.sigma_m + SIGMA_GROW_MPS * self.ahead_s(t_ms)
    }

    /// Whether the separation guard keeps it clear: a person, or a mover of
    /// unknown kind (it may be one).
    pub fn is_person(&self) -> bool {
        self.kind != MoverKind::Vehicle
    }

    /// The horizontal distance kept from it (centre to centre), m.
    pub fn keep_m(&self) -> f32 {
        if self.is_person() {
            PERSON_SEP_M
        } else {
            VEHICLE_KEEP_M
        }
    }

    /// The distance a plan keeps from where it is predicted at `t_ms`: its
    /// separation, [`PLAN_INFLATE_M`] and its uncertainty then, m.
    pub fn plan_keep_m(&self, t_ms: u64) -> f32 {
        self.keep_m() + PLAN_INFLATE_M + self.sigma_at(t_ms)
    }

    /// The published form, predicted to `now_ms`.
    pub fn tracked(&self, now_ms: u64) -> TrackedMover {
        let w = ados_protocol::pilot::wire;
        let p = self.at(now_ms);
        TrackedMover::new(
            self.id,
            self.kind,
            self.source,
            [w(p[0]), w(p[1])],
            [w(self.vel[0]), w(self.vel[1])],
            w(self.sigma_at(now_ms)),
            now_ms.saturating_sub(self.seen_ms),
        )
    }
}

/// What the flight is now, set by the pilot before each grid rebuild: the
/// obstacle margin the planners keep, whether the vehicle flies
/// [`OVER_HEAD_M`] over a person's head (then no mover is kept clear), and
/// the skill's own subject.
#[derive(Debug, Clone, Copy, Default)]
pub struct MoverFlight {
    pub margin: f32,
    pub over_head: bool,
    /// The skill goal's subject (`subject_ne`), when it names one.
    pub subject: Option<V2>,
    /// The subject's camera track id (the goal's `subject_ref`, the
    /// `track_id` of `vision.detection`), when the goal names one.
    pub subject_track: Option<u32>,
}

#[derive(Debug, Clone)]
struct Track {
    m: Mover,
    /// The camera and its tracker's id, for a camera track.
    ext: Option<(u64, u64)>,
    /// Where a cluster track was first seen, and when.
    first: V2,
    born_ms: u64,
    /// A cluster track seen moving (a camera track always counts).
    moving: bool,
}

/// The tracked movers.
#[derive(Debug, Default)]
pub struct Movers {
    tracks: Vec<Track>,
    next_id: u32,
    pub flight: MoverFlight,
    /// The band's memories for a 3D lidar's own scan, and when the last
    /// arrived ([`super::Obstacles::ingest_mover_scan`]).
    pub(super) raw: RawScan,
    /// Where the vehicle was at the last scan, and when; its velocity over
    /// the scans ([`scan`]).
    own: Option<(V2, u64)>,
    own_vel: V2,
}

#[derive(Debug, Default)]
pub(super) struct RawScan {
    pub(super) walls: super::band::WallMemory,
    pub(super) seen: super::band::SeenFree,
    pub(super) at_ms: Option<u64>,
}

/// A velocity step toward `raw` over `dt` seconds.
fn smooth(vel: V2, raw: V2, dt: f32) -> V2 {
    let k = (dt / VEL_SMOOTH_S).min(1.0);
    geo::clamp_len(geo::add(vel, geo::scale(geo::sub(raw, vel), k)), MAX_MPS)
}

impl Track {
    /// Measured at `c` at `now_ms`: the velocity steps toward the one the
    /// last measurement implies.
    fn measure(&mut self, c: V2, now_ms: u64) {
        let dt = now_ms.saturating_sub(self.m.seen_ms) as f32 / 1000.0;
        if dt > 0.0 {
            let raw = geo::scale(geo::sub(c, self.m.ne), 1.0 / dt);
            self.m.vel = smooth(self.m.vel, raw, dt);
        }
        self.m.ne = c;
        self.m.seen_ms = now_ms;
    }
}

impl Movers {
    fn new_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// A new track measured at `ne` at `now_ms`: a camera's (`ext`, its
    /// camera and tracker id) is a mover at once, a cluster's a candidate.
    fn add(&mut self, kind: MoverKind, ext: Option<(u64, u64)>, ne: V2, sigma_m: f32, now_ms: u64) {
        let source = if ext.is_some() {
            MoverSource::Vision
        } else {
            MoverSource::Scan
        };
        let id = self.new_id();
        self.tracks.push(Track {
            m: Mover {
                id,
                kind,
                source,
                ne,
                vel: [0.0, 0.0],
                sigma_m,
                seen_ms: now_ms,
            },
            ext,
            first: ne,
            born_ms: now_ms,
            moving: ext.is_some(),
        });
    }

    fn prune(&mut self, now_ms: u64) {
        self.tracks.retain(|t| {
            let keep = if t.moving { KEEP_MS } else { CANDIDATE_KEEP_MS };
            now_ms.saturating_sub(t.m.seen_ms) <= keep
        });
        if self.tracks.len() > MAX_TRACKS {
            self.tracks.sort_by_key(|t| std::cmp::Reverse(t.m.seen_ms));
            self.tracks.truncate(MAX_TRACKS);
        }
    }

    /// Whether a 3D lidar's own scan feeds the clusters now.
    pub(super) fn raw_fresh(&self, now_ms: u64) -> bool {
        self.raw
            .at_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= RAW_FRESH_MS)
    }

    /// The FC's local frame stepped by `d` (an EKF reset): every track
    /// moves with it.
    pub fn shift(&mut self, d: V2) {
        for t in &mut self.tracks {
            t.m.ne = geo::add(t.m.ne, d);
            t.first = geo::add(t.first, d);
        }
        self.raw.walls.shift(d);
        self.raw.seen.shift(d);
        if let Some((p, _)) = self.own.as_mut() {
            *p = geo::add(*p, d);
        }
    }

    /// The mover a measurement at `c` takes up again: the nearest one of
    /// the source (`cam` for a camera's tracks, `None` for clusters) seen
    /// moving and unmeasured since before `now_ms`, within [`REACQUIRE_M`]
    /// of where it was last measured or is predicted, widened by
    /// [`REACQUIRE_GROW_MPS`] for each second unmeasured.
    fn reacquire(&self, c: V2, now_ms: u64, cam: Option<u64>) -> Option<usize> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.moving && t.m.seen_ms < now_ms && t.ext.map(|e| e.0) == cam)
            .filter_map(|(i, t)| {
                let d = geo::dist(t.m.at(now_ms), c).min(geo::dist(t.m.ne, c));
                let gone_s = now_ms.saturating_sub(t.m.seen_ms) as f32 / 1000.0;
                (d <= REACQUIRE_M + REACQUIRE_GROW_MPS * gone_s).then_some((i, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Every mover track held at `now_ms` (a cluster mover a camera track
    /// already holds left out).
    fn live(&self, now_ms: u64) -> impl Iterator<Item = &Track> {
        let live = move |t: &Track| t.moving && now_ms.saturating_sub(t.m.seen_ms) <= KEEP_MS;
        self.tracks.iter().filter(move |t| {
            live(t)
                && (t.ext.is_some() || {
                    let at = t.m.at(now_ms);
                    self.tracks
                        .iter()
                        .filter(|v| v.ext.is_some() && live(v))
                        .all(|v| geo::dist(v.m.at(now_ms), at) > SAME_M)
                })
        })
    }

    /// Every mover held at `now_ms` (a cluster mover a camera track already
    /// holds left out).
    pub fn list(&self, now_ms: u64) -> impl Iterator<Item = &Mover> {
        self.live(now_ms).map(|t| &t.m)
    }

    /// The movers the vehicle keeps clear of at `now_ms`: none while it
    /// flies over their heads ([`MoverFlight::over_head`]), and never the
    /// skill's own subject: its camera track when the goal names it
    /// ([`MoverFlight::subject_track`]), else (no track id, or a scan
    /// cluster, which no camera names) whatever lies within
    /// [`SUBJECT_SAME_M`] of the goal's subject.
    pub fn guarded(&self, now_ms: u64) -> impl Iterator<Item = &Mover> {
        let f = self.flight;
        let near_subject = move |t: &Track| {
            f.subject
                .is_some_and(|s| geo::dist(t.m.at(now_ms), s) <= SUBJECT_SAME_M)
        };
        self.live(now_ms)
            .filter(move |t| {
                !f.over_head
                    && match (f.subject_track, t.ext) {
                        (Some(id), Some(e)) => e.1 != u64::from(id),
                        _ => !near_subject(t),
                    }
            })
            .map(|t| &t.m)
    }

    /// Mark each guarded mover's sweep over [`SWEEP_S`] into `grid`, the
    /// vehicle at `vehicle`: a disk at each predicted position that a route
    /// keeping the margin keeps [`Mover::plan_keep_m`] from, shrunk to leave
    /// the vehicle its margin and a cell where it is (a route still leaves
    /// from there, and the guard pushes it out). A person within
    /// [`SWEEP_HANDOFF_M`] of that distance is the separation guard's alone:
    /// backing away from someone walking at the vehicle goes along their way,
    /// which their sweep would close to the brake.
    pub fn mark(&self, grid: &mut Grid, vehicle: V2, now_ms: u64) {
        let margin = self.flight.margin;
        for m in self.guarded(now_ms) {
            let near = geo::dist(vehicle, m.at(now_ms)) < m.plan_keep_m(now_ms) + SWEEP_HANDOFF_M;
            if m.is_person() && near {
                continue;
            }
            let mut last: Option<V2> = None;
            let steps = (SWEEP_S * 10.0) as u64;
            for k in 0..=steps {
                let t = now_ms + k * 100;
                let p = m.at(t);
                if last.is_some_and(|q| geo::dist(p, q) < CELL_M) {
                    continue;
                }
                last = Some(p);
                let fits = geo::dist(vehicle, p) - margin - CELL_M;
                let r = (m.plan_keep_m(t) - margin).min(fits);
                if r > 0.0 {
                    mark_disk(grid, p, r);
                }
            }
        }
    }
}

/// Mark every cell whose centre lies within `r` of `c`.
fn mark_disk(grid: &mut Grid, c: V2, r: f32) {
    let n = (r / CELL_M).ceil() as i32;
    for i in -n..=n {
        for j in -n..=n {
            let d = [i as f32 * CELL_M, j as f32 * CELL_M];
            if geo::norm(d) <= r {
                grid.mark(geo::add(c, d));
            }
        }
    }
}

mod scan;
mod vision;

#[cfg(test)]
mod tests;
