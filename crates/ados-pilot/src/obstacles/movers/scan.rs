//! Movers from the obstacle returns: a scan's small clusters, low enough to
//! stand on the ground, tracked from scan to scan; one that walked off the
//! place it was first seen at is a mover ([`Movers::ingest_hits`]).

use ados_protocol::pilot::MoverKind;

use super::{Movers, PERSON_H_M};
use crate::geo::{self, V2};
use crate::obstacles::Hit;

/// The one-sigma error of a cluster's centre, m.
const SCAN_SIGMA_M: f32 = 0.15;
/// Successive returns closer than this belong to one cluster, m.
const CLUSTER_GAP_M: f32 = 0.5;
/// A cluster wider than this is no person or small thing but a wall or a
/// stand, m.
const CLUSTER_MAX_M: f32 = 1.2;
/// A cluster with a return higher than this over the ground is part of
/// something taller than anyone walking (a crown over a trail, a ceiling,
/// a building): a person's height and the levelling's error, m.
const MOVER_TOP_M: f32 = PERSON_H_M + 0.5;
/// A cluster's centre is put this far beyond its returns' mean, along the
/// line of sight: the returns lie on the face toward the sensor, m.
const FACE_DEPTH_M: f32 = 0.15;
/// A cluster continues a track within this of the track's prediction, m.
const GATE_M: f32 = 0.6;
/// A cluster track is a mover once it travelled this far from where it was
/// first seen, m: more than a 0.3 m trunk's visible face can shift as the
/// vehicle moves round it (its diameter) and a cluster's noise.
const MOVE_MIN_M: f32 = 0.8;
/// ... at at least this speed, m/s ...
const MOVE_MIN_MPS: f32 = 0.5;
/// ... after at least this long, ms.
const MOVE_MIN_MS: u64 = 800;
/// ... with its smoothed velocity along the way it went (cosine at least),
/// and the way averaged over its life at [`MOVE_MIN_MPS`]: jittering leaf
/// returns jump about, a walker goes one way.
const MOVE_ALIGN: f32 = 0.7;
/// A cluster track's first place counts as left when no return lies within
/// this of it, m ...
const VACATED_M: f32 = 0.4;
/// ... and a return or the scan's seen edge lies beyond it, within this
/// bearing of it, rad, ...
const VACATED_BEARING_RAD: f32 = 1.5 * std::f32::consts::PI / 180.0;
/// ... at least this much further, m.
const VACATED_BEYOND_M: f32 = 0.3;
/// ... or the track's own cluster now stands in front of it, its centre
/// within this of the line of sight (a person's half width and a
/// cluster's noise), closing on the vehicle at [`MOVE_MIN_MPS`] or more,
/// m: someone walking at the sensor hides where they were. A stretch of
/// wall seen at a grazing angle also lies along the line of sight, but
/// slides with the vehicle: it closes on nothing.
const SHADOW_M: f32 = 0.45;
/// Returns this far apart in bearing are neighbours on one surface: a
/// bin and the next, rad.
const NEIGHBOUR_RAD: f32 = 1.5 * std::f32::consts::PI / 180.0;

impl Movers {
    /// Take one scan's walls (`hits`, in bearing order) seen from `pos`:
    /// cluster them, continue the cluster tracks, and mark the ones that
    /// moved. `floor_agl` is the scan plane's height over the ground (a
    /// voxel slab's floor): a return stands that plus its `lift_m` high.
    /// Unknown, every cluster may be a person.
    pub fn ingest_hits(
        &mut self,
        hits: &[Hit],
        horizon: &[V2],
        pos: V2,
        floor_agl: Option<f32>,
        now_ms: u64,
    ) {
        let own_vel = self.own_motion(pos, now_ms);
        let clusters = clusters(hits, pos, floor_agl);
        let mut claimed = vec![false; self.tracks.len()];
        let mut fresh = Vec::new();
        for c in clusters {
            // Near where the track was last measured, or where it was
            // predicted to be: a person turning back is still the track.
            let best = self
                .tracks
                .iter()
                .enumerate()
                .filter(|(i, t)| t.ext.is_none() && !claimed[*i])
                .map(|(i, t)| {
                    let d = geo::dist(t.m.at(now_ms), c).min(geo::dist(t.m.ne, c));
                    (i, d)
                })
                .filter(|&(_, d)| d <= GATE_M)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let Some((i, _)) = best else {
                fresh.push(c);
                continue;
            };
            claimed[i] = true;
            let t = &mut self.tracks[i];
            t.measure(c, now_ms);
            let age_ms = now_ms.saturating_sub(t.born_ms);
            let way = geo::sub(c, t.first);
            let went = geo::norm(way);
            let speed = geo::norm(t.m.vel);
            if !t.moving
                && age_ms >= MOVE_MIN_MS
                && went >= MOVE_MIN_M
                && went >= MOVE_MIN_MPS * age_ms as f32 / 1000.0
                && speed >= MOVE_MIN_MPS
                && geo::dot(t.m.vel, way) >= MOVE_ALIGN * speed * went
                && vacated(t.first, c, geo::sub(t.m.vel, own_vel), hits, horizon, pos)
            {
                t.moving = true;
            }
        }
        for c in fresh {
            // A cluster where a mover went unmeasured is that mover seen
            // again (a person turning back, or out from behind another).
            match self.reacquire(c, now_ms, None) {
                Some(i) => self.tracks[i].measure(c, now_ms),
                None => self.add(MoverKind::Unknown, None, c, SCAN_SIGMA_M, now_ms),
            }
        }
        self.prune(now_ms);
    }

    /// The vehicle's own velocity over the scans, from `pos` now.
    fn own_motion(&mut self, pos: V2, now_ms: u64) -> V2 {
        if let Some((p, t)) = self.own {
            let dt = now_ms.saturating_sub(t) as f32 / 1000.0;
            if dt > 0.0 {
                let raw = geo::scale(geo::sub(pos, p), 1.0 / dt);
                self.own_vel = super::smooth(self.own_vel, raw, dt);
            }
        }
        self.own = Some((pos, now_ms));
        self.own_vel
    }
}

/// Whether the scan from `pos` sees through `p` now: no return (`hits`)
/// near it, and a return or the edge of what the scan sees (`horizon`)
/// beyond it along its bearing, or its track's own cluster (centre `c`,
/// velocity `rel_vel` relative to the vehicle) in front of it, hiding it
/// and closing on the vehicle. A stretch of wall the scan's reach or a
/// passing trunk cut off slides along the wall as the vehicle moves, and
/// trunks' faces shift as the vehicle passes, but the place each was first
/// seen at still returns; only something that moved leaves it clear, and
/// only something that moved closes on the vehicle in front of where it
/// was.
fn vacated(p: V2, c: V2, rel_vel: V2, hits: &[Hit], horizon: &[V2], pos: V2) -> bool {
    if hits.iter().any(|h| geo::dist(h.ne, p) < VACATED_M) {
        return false;
    }
    // A wall seen at a grazing angle: neighbouring returns lie metres apart
    // along it, none near `p`, but the wall between them still runs
    // through it.
    let on_surface = hits.iter().zip(hits.iter().cycle().skip(1)).any(|(a, q)| {
        geo::wrap_pi(q.body_bearing - a.body_bearing).abs() <= NEIGHBOUR_RAD
            && seg_dist(p, a.ne, q.ne) < VACATED_M
    });
    if on_surface {
        return false;
    }
    let rel = geo::sub(p, pos);
    let (d, b) = (geo::norm(rel), geo::bearing_of(rel));
    if d > 1e-3 {
        let u = geo::scale(rel, 1.0 / d);
        let cr = geo::sub(c, pos);
        let along = geo::dot(cr, u);
        let off = geo::norm(geo::sub(cr, geo::scale(u, along)));
        let closing = -geo::dot(rel_vel, u);
        if along > 0.0
            && along + VACATED_BEYOND_M <= d
            && off <= SHADOW_M
            && closing >= MOVE_MIN_MPS
        {
            return true;
        }
    }
    hits.iter()
        .map(|h| h.ne)
        .chain(horizon.iter().copied())
        .any(|q| {
            let r = geo::sub(q, pos);
            geo::norm(r) >= d + VACATED_BEYOND_M
                && geo::wrap_pi(geo::bearing_of(r) - b).abs() <= VACATED_BEARING_RAD
        })
}

/// The distance from `p` to the segment `a`–`b`, m.
fn seg_dist(p: V2, a: V2, b: V2) -> f32 {
    let ab = geo::sub(b, a);
    let n2 = geo::dot(ab, ab);
    let t = if n2 > 1e-9 {
        (geo::dot(geo::sub(p, a), ab) / n2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    geo::dist(p, geo::add(a, geo::scale(ab, t)))
}

/// The small clusters among `hits` (bearing order, wrapping round) seen from
/// `pos`, none with a return over [`MOVER_TOP_M`] (heights from
/// `floor_agl`): each one's centre, set [`FACE_DEPTH_M`] behind its
/// returns' mean.
fn clusters(hits: &[Hit], pos: V2, floor_agl: Option<f32>) -> Vec<V2> {
    let mut groups: Vec<Vec<&Hit>> = Vec::new();
    for h in hits {
        match groups.last_mut() {
            Some(g)
                if g.last()
                    .is_some_and(|q| geo::dist(q.ne, h.ne) < CLUSTER_GAP_M) =>
            {
                g.push(h)
            }
            _ => groups.push(vec![h]),
        }
    }
    if groups.len() > 1 {
        let joins = match (groups.first(), groups.last()) {
            (Some(a), Some(b)) => {
                matches!((a.first(), b.last()), (Some(p), Some(q)) if geo::dist(p.ne, q.ne) < CLUSTER_GAP_M)
            }
            _ => false,
        };
        if joins {
            let head = groups.remove(0);
            if let Some(last) = groups.last_mut() {
                last.extend(head);
            }
        }
    }
    let tall = |h: &&Hit| floor_agl.is_some_and(|f| f + h.lift_m > MOVER_TOP_M);
    groups
        .into_iter()
        .filter(|g| g.len() >= 2 && !g.iter().any(tall))
        .filter_map(|g| {
            let n = g.len() as f32;
            let mean = geo::scale(g.iter().fold([0.0, 0.0], |a, h| geo::add(a, h.ne)), 1.0 / n);
            if g.iter()
                .any(|h| 2.0 * geo::dist(h.ne, mean) > CLUSTER_MAX_M)
            {
                return None;
            }
            let rel = geo::sub(mean, pos);
            let d = geo::norm(rel);
            Some(if d > 1e-3 {
                geo::add(mean, geo::scale(rel, FACE_DEPTH_M / d))
            } else {
                mean
            })
        })
        .collect()
}
