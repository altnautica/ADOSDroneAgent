//! Clearance along a straight leg against the live occupancy map: the nearest
//! seen obstacle to the leg, and whether unseen space lies close to it.
//!
//! The clearance is conservative against the map's quantisation, so a pinch
//! a few millimetres under the required clearance never reads clear:
//!
//! - it is measured to the leg itself (the segment from `a` to `b`), not to
//!   samples along it, so no stretch between samples is missed;
//! - an occupied cell counts from its nearest possible point: the distance to
//!   the cell centre less half the cell diagonal (`res · √2 / 2`, 0.035 m on
//!   the 0.05 m live tiles). The true surface inside the cell is never nearer
//!   than that, so the reported clearance is a lower bound.
//!
//! Map cells are the occupancy tiles (0 occupied, 254 free, anything else
//! unseen; a tile never received is unseen) placed with the pose offset, plus
//! the current scan's returns as seen obstacles (points, measured as they
//! are). Without the pose offset the tiles cannot be placed, so the map near
//! the leg is unseen.
//!
//! Each seen obstacle has its own required clearance ([`Bar`]): the pose's
//! uncertainty relative to that obstacle, by when it was seen.

use std::collections::BTreeSet;

use super::{tiles, Obstacles};
use crate::geo::{self, V2};

/// The clearance a leg needs from one seen obstacle: the margin plus
/// max(`min_term`, 3σ), σ the vehicle's horizontal uncertainty relative to
/// that obstacle.
///
/// The leg and the map are in the same frame, the frame the pose places the
/// scans in, so what the vehicle must not miss is the obstacle as the map
/// holds it, not the world: drift of the whole frame moves the map with the
/// vehicle. The planar scan matcher's `sigma_xy_m` bounds the drift since
/// the map began, by the matcher's own model: a matched pose is good to
/// `base` (the least σ it ever publishes) against the map it matched, and σ
/// grows only by travel no scan checked (riding the motion prior through a
/// climb, a new submap placed by the prior). So against an obstacle seen at
/// σ `seen` the vehicle is uncertain by `base` plus the growth since:
/// `base + (σ − max(seen, base))`. That is `base` for one in the current
/// scan or seen since σ last grew, and the whole σ for one whose sighting is
/// unknown (`seen` 0). It is never more than σ. While scans match, σ does
/// not grow and the term trusts the match: a simulated tunnel flight drifted
/// at most 0.08 m against its own map over 6 m of matched travel, inside 3 ×
/// the matched 0.05 m, and a wall the leg passes is in view, so seen again,
/// for the 12 m the planar lidar reaches ahead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub margin: f32,
    /// The least uncertainty term, metres.
    pub min_term: f32,
    /// The pose's σ now; `None` while never known (the term is then
    /// `min_term`).
    pub sigma: Option<f32>,
    /// The least σ the pose has published; `None` while no match holds the
    /// pose to the map (a pose the matcher calls lost, a position the pilot
    /// does not vouch for, σ risen within the settle time): every obstacle
    /// then needs the whole σ.
    pub base: Option<f32>,
}

impl Bar {
    /// `seen` of an obstacle in the current scan.
    pub const NOW: f32 = f32::INFINITY;

    /// A bar that is `required` for every obstacle.
    #[cfg(test)]
    pub fn flat(required: f32) -> Self {
        Self {
            margin: required,
            min_term: 0.0,
            sigma: None,
            base: None,
        }
    }

    /// σ relative to an obstacle seen at σ `seen` (see the type).
    pub fn sigma_rel(&self, seen: f32) -> Option<f32> {
        let s = self.sigma?;
        let base = self.base.map_or(s, |b| b.min(s));
        Some(base + (s - seen.max(base)).max(0.0))
    }

    /// The clearance an obstacle seen at σ `seen` needs.
    pub fn required(&self, seen: f32) -> f32 {
        self.margin
            + self
                .sigma_rel(seen)
                .map_or(self.min_term, |s| (3.0 * s).max(self.min_term))
    }

    /// The clearance an obstacle needs at most: one of unknown sighting.
    pub fn most(&self) -> f32 {
        self.required(0.0)
    }
}

/// What a leg sees.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LegProbe {
    /// The seen obstacle with the least clearance past its own required
    /// clearance, within `report_extra` past it: how far along the leg (m)
    /// and the point of the leg nearest it, the clearance (m) and the
    /// clearance it needs (m).
    pub nearest: Option<(f32, V2, f32, f32)>,
    /// Unseen space lies closer to the leg than [`Bar::most`].
    pub unseen: bool,
    /// The point of the leg first (smallest distance along it) nearest a
    /// seen obstacle closer than it needs: where a tight stretch, such as a
    /// narrowing bore, starts.
    pub first_tight: Option<(f32, V2)>,
}

/// One straight leg.
struct Segment {
    a: V2,
    dir: V2,
    len: f32,
}

impl Segment {
    fn new(a: V2, b: V2) -> Self {
        let len = geo::dist(a, b);
        let dir = if len > 1e-6 {
            geo::scale(geo::sub(b, a), 1.0 / len)
        } else {
            [0.0, 0.0]
        };
        Self { a, dir, len }
    }

    /// How far along the leg the point nearest `p` is, that point, and the
    /// distance to it.
    fn nearest(&self, p: V2) -> (f32, V2, f32) {
        let t = geo::dot(geo::sub(p, self.a), self.dir).clamp(0.0, self.len);
        let q = geo::add(self.a, geo::scale(self.dir, t));
        (t, q, geo::dist(p, q))
    }
}

impl Obstacles {
    /// Probe the leg `a → b` (local NE) against `bar`: the seen obstacle
    /// tightest against its own required clearance, within `report_extra`
    /// past it, and whether unseen space lies within [`Bar::most`] of the
    /// leg. Map cells count from their nearest possible point (see the
    /// module doc).
    pub fn probe_leg(&self, a: V2, b: V2, bar: &Bar, report_extra: f32, now_ms: u64) -> LegProbe {
        let s = Segment::new(a, b);
        let unseen_m = bar.most();
        let reach = unseen_m + report_extra;
        let mut out = LegProbe::default();
        let seen_obstacle = |p: V2, shrink: f32, seen: f32, out: &mut LegProbe| {
            let (t, q, d) = s.nearest(p);
            let d = (d - shrink).max(0.0);
            let need = bar.required(seen);
            if d <= need + report_extra
                && out
                    .nearest
                    .is_none_or(|(_, _, best, best_need)| d - need < best - best_need)
            {
                out.nearest = Some((t, q, d, need));
            }
            if d < need && out.first_tight.is_none_or(|(ft, _)| t < ft) {
                out.first_tight = Some((t, q));
            }
        };
        // A return over the flight slice is no pinch on a leg flown in it
        // (a roof a nose-up scan reaches); the brake still reads it.
        for h in self.hits(now_ms).iter().filter(|h| !h.overhead) {
            seen_obstacle(h.ne, 0.0, Bar::NOW, &mut out);
        }
        let Some(off) = self.offset else {
            // The map cannot be placed: everything near the leg is unseen.
            out.unseen = true;
            return out;
        };
        let res = self.tile_res_m;
        let span = res * self.tile_cells as f32;
        if res <= 0.0 || span <= 0.0 {
            out.unseen = true;
            return out;
        }
        let half_diag = res * std::f32::consts::FRAC_1_SQRT_2;
        // Every tile within `reach` of the leg, in the map frame, found by
        // walking the leg in half-tile steps.
        let (ma, mb) = (geo::sub(a, off), geo::sub(b, off));
        let walk = Segment::new(ma, mb);
        let steps = (walk.len / (span / 2.0)).ceil() as usize;
        let mut keys = BTreeSet::new();
        for k in 0..=steps {
            let t = (k as f32 * span / 2.0).min(walk.len);
            let p = geo::add(walk.a, geo::scale(walk.dir, t));
            let lo = |v: f32| ((v - reach - res) / span).floor() as i32;
            let hi = |v: f32| ((v + reach + res) / span).floor() as i32;
            for ix in lo(p[0])..=hi(p[0]) {
                for iy in lo(p[1])..=hi(p[1]) {
                    keys.insert((ix, iy));
                }
            }
        }
        for (ix, iy) in keys {
            let origin = [ix as f32 * span + off[0], iy as f32 * span + off[1]];
            let tile = self.tiles.get(&(ix, iy));
            let n = tile.map_or(self.tile_cells, |t| t.cells().min(self.tile_cells));
            for lx in 0..n {
                for ly in 0..n {
                    let v = tile.map_or(tiles::UNSEEN, |t| t.value(lx, ly));
                    if v == tiles::FREE || (out.unseen && v != tiles::OCCUPIED) {
                        continue;
                    }
                    let p = [
                        origin[0] + (lx as f32 + 0.5) * res,
                        origin[1] + (ly as f32 + 0.5) * res,
                    ];
                    if v == tiles::OCCUPIED {
                        let seen = tile.map_or(0.0, |t| t.seen_sigma(lx, ly));
                        seen_obstacle(p, half_diag, seen, &mut out);
                    } else if s.nearest(p).2 - half_diag < unseen_m {
                        out.unseen = true;
                    }
                }
            }
        }
        out
    }
}
