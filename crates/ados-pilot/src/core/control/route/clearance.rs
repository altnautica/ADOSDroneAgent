//! The clearance the route needs: per obstacle, from the pose's σ relative
//! to when the obstacle was seen ([`Bar`]).

use super::super::super::Pilot;
use super::{MIN_SIGMA_TERM_M, SIGMA_CAP_M, SIGMA_HOLD_MS, SIGMA_SETTLE_MS};
use crate::obstacles::Bar;

/// The clearance the route needs now.
#[derive(Debug, Clone, Copy)]
pub struct Clearance {
    /// Each seen obstacle's required clearance.
    pub bar: Bar,
    /// The most any obstacle needs ([`Bar::most`]).
    pub required: f32,
    pub margin: f32,
    /// σ as published: fresh or held, `None` once it aged out.
    pub sigma: Option<f32>,
    /// σ is fresh or held and the position is vouched for.
    pub trusted: bool,
}

impl Pilot {
    /// Keep the last σ with the time its pose arrived: the hold counts from
    /// the pose, not from when the check last looked.
    pub(super) fn note_sigma(&mut self) {
        if let Some(s) = self.obstacles.last_pose_sigma() {
            self.route.sigma_seen = Some(s);
        }
    }

    /// The clearance the route needs: each seen obstacle the effective
    /// margin plus max(0.1, 3σ), σ relative to it ([`Bar`]) from the pose's
    /// σ fresh, held, or the last one seen.
    pub(super) fn required_clearance(&self, now_ms: u64) -> Clearance {
        let margin = self.settings.pilot.effective_margin_m();
        let seen = self.route.sigma_seen;
        let held = seen.filter(|(_, at)| now_ms.saturating_sub(*at) <= SIGMA_HOLD_MS);
        // Relative to the map only while the match holds the pose to it: a
        // pose the matcher calls lost, one the pilot does not vouch for, one
        // whose σ rose within SIGMA_SETTLE_MS (a jump, a ride on the prior),
        // or one at the matcher's σ cap (unmatched for good, its σ no longer
        // able to rise) counts its whole σ against every obstacle.
        let matched = !self.obstacles.pose_lost()
            && self.position_unverified(now_ms).is_none()
            && self.obstacles.sigma_settled(now_ms, SIGMA_SETTLE_MS)
            && seen.is_none_or(|(s, _)| s < SIGMA_CAP_M);
        let bar = Bar {
            margin,
            min_term: MIN_SIGMA_TERM_M,
            sigma: seen.map(|(s, _)| s),
            base: self.obstacles.sigma_low().filter(|_| matched),
        };
        Clearance {
            bar,
            required: bar.most(),
            margin,
            sigma: held.map(|(s, _)| s),
            trusted: held.is_some() && self.position_unverified(now_ms).is_none(),
        }
    }
}
