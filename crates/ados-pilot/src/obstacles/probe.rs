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

use std::collections::BTreeSet;

use super::{tiles, Obstacles};
use crate::geo::{self, V2};

/// What a leg sees.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LegProbe {
    /// The seen obstacle nearest the leg, within the report radius: how far
    /// along the leg (m) and the point of the leg nearest it, and the
    /// clearance (m).
    pub nearest: Option<(f32, V2, f32)>,
    /// Unseen space lies closer than the unseen radius to the leg.
    pub unseen: bool,
    /// The point of the leg first (smallest distance along it) nearest a
    /// seen obstacle closer than the unseen radius: where a tight stretch,
    /// such as a narrowing bore, starts.
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
    /// Probe the leg `a → b` (local NE): the seen obstacle nearest the leg
    /// within `report_m`, and whether unseen space lies within `unseen_m`
    /// of it. Map cells count from their nearest possible point (see the
    /// module doc).
    pub fn probe_leg(&self, a: V2, b: V2, unseen_m: f32, report_m: f32, now_ms: u64) -> LegProbe {
        let s = Segment::new(a, b);
        let reach = unseen_m.max(report_m);
        let mut out = LegProbe::default();
        let seen_obstacle = |p: V2, shrink: f32, out: &mut LegProbe| {
            let (t, q, d) = s.nearest(p);
            let d = (d - shrink).max(0.0);
            if d <= report_m && out.nearest.is_none_or(|(_, _, best)| d < best) {
                out.nearest = Some((t, q, d));
            }
            if d < unseen_m && out.first_tight.is_none_or(|(ft, _)| t < ft) {
                out.first_tight = Some((t, q));
            }
        };
        for h in self.hits(now_ms) {
            seen_obstacle(h.ne, 0.0, &mut out);
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
                        seen_obstacle(p, half_diag, &mut out);
                    } else if s.nearest(p).2 - half_diag < unseen_m {
                        out.unseen = true;
                    }
                }
            }
        }
        out
    }
}
