//! Where the leg the FC flies next starts, and the checks before the pilot
//! puts the FC in AUTO.

use super::super::super::Pilot;
use super::Origin;

impl Pilot {
    /// The FC at item `s` still flies the origin's leg: `s` is the first
    /// placed item at or after the origin's item.
    pub(super) fn origin_pending(&self, o: Origin, s: u32) -> bool {
        self.route
            .feed
            .items
            .iter()
            .find(|i| i.seq >= o.to_seq && (i.ned.is_some() || i.here))
            .is_some_and(|i| i.seq == s)
    }

    /// The item the FC flies after item `p` in its own order: the next
    /// navigation item of the feed.
    pub(super) fn next_item(&self, p: u32) -> Option<u32> {
        self.route
            .feed
            .items
            .iter()
            .map(|i| i.seq)
            .filter(|&s| s > p)
            .min()
    }

    /// Item `to_seq` flown from where the vehicle is now, labelled from the
    /// last item before it the FC flew (skipped items left out).
    pub(super) fn origin_here(&self, to_seq: u32, now_ms: u64) -> Option<Origin> {
        let gaps = &self.sup.stall.gaps;
        let from_seq = self
            .route
            .feed
            .items
            .iter()
            .filter(|i| i.seq < to_seq && !gaps.iter().any(|g| g.seq == i.seq))
            .map(|i| i.seq)
            .max()
            .unwrap_or(to_seq);
        let l = self.fc.fresh_local(now_ms)?;
        Some(Origin {
            from_seq,
            to_seq,
            ned: [l.n, l.e, l.d],
        })
    }

    /// AUTO is never commanded on a position the pilot does not vouch for,
    /// nor in a confined place without the fresh local position the leg from
    /// here is checked from.
    pub(in crate::core) fn auto_position_blocked(&self, now_ms: u64) -> bool {
        self.position_unverified(now_ms).is_some()
            || (self.confined() && self.fc.fresh_local(now_ms).is_none())
    }

    /// Before the pilot puts the FC in AUTO at item `to_seq`: the FC will
    /// fly it from where the vehicle is. In a confined place, check the
    /// route from here; `false` when a leg ahead is infeasible (the caller
    /// holds instead). Open places always pass.
    pub(in crate::core) fn confined_auto_clear(&mut self, to_seq: u32, now_ms: u64) -> bool {
        if !self.confined() {
            return true;
        }
        let origin = self.origin_here(to_seq, now_ms);
        self.route.origin = origin;
        self.check_route_from(origin, now_ms);
        self.route.infeasible().is_none()
    }
}
