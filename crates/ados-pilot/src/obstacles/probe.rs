//! Clearance along a straight leg against the live occupancy map: the nearest
//! seen obstacle to the leg's samples, and whether unseen space lies close to
//! any of them.
//!
//! The leg is sampled every `step` metres from `a` to `b` (both ends
//! included). A cell's distance to the samples is its distance to the sample
//! nearest it along the leg, so every map cell near the leg is visited once
//! instead of once per sample, and the answer is exactly that of checking a
//! disc around every sample.
//!
//! Map cells are the occupancy tiles (0 occupied, 254 free, anything else
//! unseen; a tile never received is unseen) placed with the pose offset, plus
//! the current scan's returns as seen obstacles. Without the pose offset the
//! tiles cannot be placed, so the map near the leg is unseen.

use std::collections::BTreeSet;

use super::{tiles, Obstacles};
use crate::geo::{self, V2};

/// What a leg's samples see.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LegProbe {
    /// The seen obstacle nearest a sample, within the report radius: the
    /// sample's index and position, and the distance (m).
    pub nearest: Option<(usize, V2, f32)>,
    /// Unseen space lies closer than the unseen radius to some sample.
    pub unseen: bool,
    /// How many samples the leg has.
    pub samples: usize,
}

/// One leg's sampling: `n` samples at `k · step` along it, the last at its
/// end.
struct Samples {
    a: V2,
    dir: V2,
    len: f32,
    step: f32,
    n: usize,
}

impl Samples {
    fn new(a: V2, b: V2, step: f32) -> Self {
        let len = geo::dist(a, b);
        let dir = if len > 1e-6 {
            geo::scale(geo::sub(b, a), 1.0 / len)
        } else {
            [0.0, 0.0]
        };
        let n = (len / step).ceil() as usize + 1;
        Self {
            a,
            dir,
            len,
            step,
            n,
        }
    }

    fn at(&self, k: usize) -> V2 {
        let t = (k as f32 * self.step).min(self.len);
        geo::add(self.a, geo::scale(self.dir, t))
    }

    /// The sample nearest `p`, and the distance to it.
    fn nearest(&self, p: V2) -> (usize, f32) {
        let t = geo::dot(geo::sub(p, self.a), self.dir).clamp(0.0, self.len);
        let k0 = ((t / self.step).floor() as usize).min(self.n - 1);
        let k1 = (k0 + 1).min(self.n - 1);
        let d0 = geo::dist(p, self.at(k0));
        let d1 = geo::dist(p, self.at(k1));
        if d1 < d0 {
            (k1, d1)
        } else {
            (k0, d0)
        }
    }

    /// The distance from `p` to the leg itself (a lower bound of
    /// [`Self::nearest`]).
    fn to_leg(&self, p: V2) -> f32 {
        let t = geo::dot(geo::sub(p, self.a), self.dir).clamp(0.0, self.len);
        geo::dist(p, geo::add(self.a, geo::scale(self.dir, t)))
    }
}

impl Obstacles {
    /// Probe the leg `a → b` (local NE) sampled every `step` m: the seen
    /// obstacle nearest a sample within `report_m`, and whether unseen space
    /// lies within `unseen_m` of a sample.
    pub fn probe_leg(
        &self,
        a: V2,
        b: V2,
        step: f32,
        unseen_m: f32,
        report_m: f32,
        now_ms: u64,
    ) -> LegProbe {
        let s = Samples::new(a, b, step);
        let reach = unseen_m.max(report_m);
        let mut out = LegProbe {
            samples: s.n,
            ..LegProbe::default()
        };
        let seen_obstacle = |p: V2, out: &mut LegProbe| {
            if s.to_leg(p) > report_m {
                return;
            }
            let (k, d) = s.nearest(p);
            if d <= report_m && out.nearest.is_none_or(|(_, _, best)| d < best) {
                out.nearest = Some((k, s.at(k), d));
            }
        };
        for h in self.hits(now_ms) {
            seen_obstacle(h.ne, &mut out);
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
        // Every tile within `reach` of the leg, in the map frame.
        let (ma, mb) = (geo::sub(a, off), geo::sub(b, off));
        let mut keys = BTreeSet::new();
        let walk = Samples::new(ma, mb, span / 2.0);
        for k in 0..walk.n {
            let p = walk.at(k);
            let lo = |v: f32| ((v - reach) / span).floor() as i32;
            let hi = |v: f32| ((v + reach) / span).floor() as i32;
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
                        seen_obstacle(p, &mut out);
                    } else if s.to_leg(p) < unseen_m && s.nearest(p).1 < unseen_m {
                        out.unseen = true;
                    }
                }
            }
        }
        out
    }
}
