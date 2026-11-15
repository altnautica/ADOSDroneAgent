//! The confined launch into the FC mission: refused unless the whole route,
//! from its first item, was checked and keeps the clearance.

use super::super::super::Pilot;
use super::leg_label;

impl Pilot {
    /// A launch into the mission in a confined place: refused while the
    /// route is infeasible (naming the leg, the point, its clearance and the
    /// clearance needed), or while it was not fully checked
    /// (`route_unchecked`): an item has no position, the mission is longer
    /// than the feed, or the feed may not hold the FC's mission at all.
    pub(in crate::core) fn route_launch_check(&mut self, now_ms: u64) -> Result<(), String> {
        if !self.confined() {
            return Ok(());
        }
        self.route.origin = None;
        self.check_route(now_ms);
        if let Some(leg) = self.route.infeasible().copied() {
            tracing::warn!(
                target: "ados_pilot",
                reason = "route_infeasible",
                leg = %leg_label(leg.from_seq, leg.to_seq),
                ned = ?leg.pinch.map(|(p, _)| p),
                clearance_m = leg.pinch.map(|(_, d)| d),
                required_m = self.route.required,
                "pilot_launch_refused"
            );
            return Err("route_infeasible".into());
        }
        let feed = &self.route.feed;
        if !self.route.unchecked.is_empty() || feed.truncated || feed.stale {
            tracing::warn!(
                target: "ados_pilot",
                reason = "route_unchecked",
                unchecked = ?self.route.unchecked,
                truncated = feed.truncated,
                stale = feed.stale,
                "pilot_launch_refused"
            );
            return Err("route_unchecked".into());
        }
        Ok(())
    }
}
