//! The leaves overhead along a skill's way ([`OverheadAhead`] on the state
//! frame). A planar lidar under a canopy passes under the leaves while the
//! vehicle is level and reaches their underside only while it leans, so
//! the returns over the swept band ([`crate::obstacles::Obstacles::overhead_hits`])
//! are kept here for [`KEEP_MS`], each as the height the vehicle would fly
//! under it: its height over the ground at the scan, plus the return's
//! height over the scan plane, less the band's top. Along the way from the
//! vehicle to the subject a lease goal names, the lowest of them within
//! [`WAY_HALF_M`] is reported: a skill flies under it, or holds a height of
//! its own and lets the brake keep it out of the leaves.

use std::collections::VecDeque;

use ados_protocol::pilot::OverheadAhead;

use crate::geo::{self, V2};
use crate::obstacles::BAND_TOP_M;

/// How long an overhead return counts, ms of the pilot clock: several
/// leans' worth, so the level flight between them does not read as clear.
pub const KEEP_MS: u64 = 10_000;
/// Overhead returns kept at most (the oldest go first).
const MAX_POINTS: usize = 4_096;
/// A return within this of the way (either side) lies over it, m: the
/// airframe and a margin for the way flown off the straight line.
pub const WAY_HALF_M: f32 = 1.0;

/// Overhead returns: local NE, the height to fly under it, when seen.
#[derive(Debug, Default)]
pub struct OverheadLog {
    pts: VecDeque<(V2, f32, u64)>,
}

impl OverheadLog {
    /// A return over the band at `ne`, `fly_under_m` the height over the
    /// ground the band clears it at.
    pub fn note(&mut self, ne: V2, fly_under_m: f32, now_ms: u64) {
        if !fly_under_m.is_finite() {
            return;
        }
        while self
            .pts
            .front()
            .is_some_and(|p| now_ms.saturating_sub(p.2) > KEEP_MS)
            || self.pts.len() >= MAX_POINTS
        {
            self.pts.pop_front();
        }
        self.pts.push_back((ne, fly_under_m, now_ms));
    }

    /// The lowest fly-under height within [`WAY_HALF_M`] of the way from
    /// `from` to `to` (seen in the last [`KEEP_MS`]), and how far along the
    /// way it lies.
    pub fn along(&self, from: V2, to: V2, now_ms: u64) -> Option<(f32, f32)> {
        let way = geo::sub(to, from);
        let len = geo::norm(way);
        let e = if len > 1e-3 {
            geo::scale(way, 1.0 / len)
        } else {
            return None;
        };
        self.pts
            .iter()
            .filter(|p| now_ms.saturating_sub(p.2) <= KEEP_MS)
            .filter_map(|&(ne, h, _)| {
                let rel = geo::sub(ne, from);
                let along = geo::dot(rel, e);
                let off = (rel[0] * e[1] - rel[1] * e[0]).abs();
                ((0.0..=len).contains(&along) && off <= WAY_HALF_M).then_some((h, along))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
    }

    /// The local frame stepped by `d` (an EKF position reset).
    pub fn shift(&mut self, d: V2) {
        for p in &mut self.pts {
            p.0 = geo::add(p.0, d);
        }
    }
}

impl super::Pilot {
    /// Keep the current scan's overhead returns, levelled at `agl_m` (the
    /// height over the ground the scan was taken at).
    pub(super) fn note_overhead(&mut self, agl_m: Option<f32>, now_ms: u64) {
        let Some(agl) = agl_m else { return };
        let hits: Vec<_> = self
            .obstacles
            .overhead_hits(now_ms)
            .iter()
            .map(|h| (h.ne, agl + h.lift_m - BAND_TOP_M))
            .collect();
        for (ne, fly_under) in hits {
            self.overhead.note(ne, fly_under, now_ms);
        }
    }

    /// The report for the state frame, while a lease goal names a subject.
    pub(super) fn overhead_ahead(&self, now_ms: u64) -> Option<OverheadAhead> {
        let lease = self.auth.lease()?;
        let subject = lease.goal.as_ref()?.subject_ne?;
        let pos = self.pos_ne(now_ms)?;
        let found = self.overhead.along(pos, subject, now_ms);
        Some(OverheadAhead::new(
            lease.id,
            found.map(|f| f.0),
            found.map(|f| f.1),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lowest_leaves_over_the_way_are_reported() {
        let mut log = OverheadLog::default();
        // Over the way 3 m out, and a lower one 2 m off it (not over it).
        log.note([3.0, 0.4], 3.9, 1_000);
        log.note([5.0, 2.0], 3.0, 1_000);
        log.note([6.0, -0.5], 4.1, 1_000);
        let (h, at) = log.along([0.0, 0.0], [8.0, 0.0], 1_500).unwrap();
        assert!((h - 3.9).abs() < 1e-6 && (at - 3.0).abs() < 1e-6);
        // Behind the vehicle or past the subject: not on the way.
        assert!(log.along([4.0, 0.0], [5.5, 0.0], 1_500).is_none());
        // Kept for KEEP_MS, then gone.
        assert!(log.along([0.0, 0.0], [8.0, 0.0], 1_000 + KEEP_MS).is_some());
        assert!(log.along([0.0, 0.0], [8.0, 0.0], 1_001 + KEEP_MS).is_none());
        log.shift([1.0, 0.0]);
        let (_, at) = log.along([0.0, 0.0], [8.0, 0.0], 1_500).unwrap();
        assert!((at - 4.0).abs() < 1e-6);
    }
}
