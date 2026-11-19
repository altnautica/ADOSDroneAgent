//! The confined-place profile: the route check of the FC mission against the
//! live occupancy, the hold before an infeasible leg, and the 0.5 m/s cap near
//! the clearance limit ([`cap`]). Nothing here acts in an `open` place.
//!
//! The route is the router's mirrored navigation items in the FC's local NED
//! frame, cut into the legs the FC flies ([`legs::legs_of`]): between
//! consecutive placed items, skipped items left out, and from the vehicle's
//! own position to the item the pilot put the FC in AUTO at (after a skip or
//! a hold ArduPilot flies that item from where the vehicle stands). While a
//! hold of the mission holds, the route is checked as it resumes: from where
//! the vehicle holds to the FC's current item, and on. Each leg is checked
//! against the occupancy map
//! ([`crate::obstacles::Obstacles::probe_leg`]) with a conservative
//! clearance, along the part of it within the planar scan's slice of the
//! vehicle's height ([`legs::check_legs`]): a seen obstacle closer than the
//! required clearance makes the leg infeasible; unseen space that close, or
//! a part of the leg off the slice, leaves it unverified. An item the
//! router cannot place (no position), or a mission longer than the feed
//! carries, leaves the route unverified and named (`unchecked`,
//! `truncated`); a launch into such a route is refused.
//!
//! An infeasible verdict holds the mission once it has stood for
//! [`ROUTE_HOLD_CONFIRM_MS`], or at once when the vehicle could reach the
//! pinch before a hold that waited stopped it; the FC is held to the
//! confined cap meanwhile. A route hold resumes the mission at its current
//! item once the route from where the vehicle holds has read anything but
//! infeasible for [`ROUTE_RESUME_DWELL_MS`]; a route that stays infeasible
//! keeps the hold, its stall countdown and the operator's say.
//!
//! A leg needs from each seen obstacle the effective margin plus
//! max(0.1 m, 3σ), σ the vehicle's horizontal uncertainty relative to that
//! obstacle ([`crate::obstacles::Bar`]): the leg and the map share the frame
//! the pose places the scans in, so drift of the whole frame is no risk, only
//! drift since the obstacle was seen. It comes from the live pose
//! uncertainty, the planar scan matcher's `sigma_xy_m` on the
//! `plugin.navigation.pose` topic, and the σ each map cell turned
//! occupied at: the matcher's own growth between the two, plus the matched
//! pose's σ. A cell of unknown sighting needs the whole σ. The FC offers no
//! usable figure: ArduCopter never sends LOCAL_POSITION_NED_COV or ODOMETRY
//! (it only receives ODOMETRY), and EKF_STATUS_REPORT's `pos_horiz_variance`
//! is a normalised innovation test ratio, not metres. When the pose goes
//! stale the last σ is held for [`SIGMA_HOLD_MS`]; after that it still sets
//! the clearance (missing σ never lowers it) but the route reads at best
//! unverified. With no σ ever the 3σ term is 0.1 m and the route reads at
//! best unverified. So does a position the pilot does not vouch for
//! ([`super::super::position_check`]).

mod cap;
mod clearance;
mod hold;
mod launch;
mod legs;
mod origin;

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{wire, Pinch, PlaceClass, RouteCheck, RouteState};

use super::super::Pilot;
use super::supervisor::Resume;

pub use cap::{Slowed, CONFINED_CAP_MPS};
pub use legs::{check_legs, legs_of, route_of, LegCheck, Origin, RouteFeed, RouteItem};

/// `hold_reason` while the vehicle holds before an infeasible leg.
pub const HOLD_ROUTE_INFEASIBLE: &str = "route-infeasible";
/// The 3σ term never goes below this, metres.
pub const MIN_SIGMA_TERM_M: f32 = 0.1;
/// A stale pose's last σ still counts this long, ms.
pub const SIGMA_HOLD_MS: u64 = 5000;
/// After σ rises the whole σ counts against every obstacle for this long,
/// ms: 20 scans at 10 Hz. A pose that jumps rides the prior and raises σ on
/// the scan it jumps (a room flight's pose broke with σ rising that scan,
/// 0.05 to 0.096, then on nearly every scan for seconds), and the FC's own
/// velocity is no independent check: it fuses that pose. The relative σ
/// returns only once the scans have matched it for 2 s.
pub const SIGMA_SETTLE_MS: u64 = 2000;
/// The planar scan matcher's σ stops growing here, metres (scan2d
/// `SIGMA_MAX_M`): a pose at it has ridden unmatched past what σ can show.
pub const SIGMA_CAP_M: f32 = 5.0;
/// The route is re-checked this often as cells become seen.
const RECHECK_MS: u64 = 1000;
/// An infeasible verdict holds the mission only once every check has read
/// it for this long: two checks `RECHECK_MS` apart, the second on a map ten
/// more scans (10 Hz) have updated, so one sample (a cell mapped from the
/// floor before the climb re-maps it, a scan the brake's pitch tilted) never
/// holds. Waiting is safe: meanwhile the FC is held to the 0.5 m/s confined
/// cap, and a pinch the vehicle could reach before it stopped holds at once
/// (`Pilot::stop_reach`): at the cap the wait, the FC's 0.8 s response and
/// the stop cover 0.94 m (at the normal profile's 3 m/s²).
pub const ROUTE_HOLD_CONFIRM_MS: u64 = 1000;
/// A route hold resumes the mission once every check has read the route
/// from where the vehicle holds as anything but infeasible for this long:
/// four checks, three re-checks past the first clear one (an obstacle hold
/// resumes after as long clear). A pinch that reads infeasible on one check
/// in four keeps the hold: three times the confirmation, so a flickering
/// pinch holds rather than resumes.
pub const ROUTE_RESUME_DWELL_MS: u64 = 3000;

fn leg_label(from: u32, to: u32) -> String {
    format!("{from}→{to}")
}

fn two_dp(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

/// The confined-place state.
#[derive(Debug, Default)]
pub struct Route {
    feed: RouteFeed,
    /// Where the FC flies the item it was put in AUTO at, or jumped to,
    /// from (by the pilot or a ground station).
    pub origin: Option<Origin>,
    /// Whether the FC flew the mission, and its item, at the last tick.
    flown: Option<(bool, Option<u32>)>,
    /// The last check of the legs still ahead.
    pub legs: Vec<LegCheck>,
    /// Items ahead with no position.
    pub unchecked: Vec<u32>,
    /// The most any obstacle needs ([`crate::obstacles::Bar::most`]); each
    /// leg holds the clearance its pinch needs ([`LegCheck::required`]).
    pub required: f32,
    pub margin: f32,
    pub sigma: Option<f32>,
    pub trusted: bool,
    /// The last check was made with σ settled ([`SIGMA_SETTLE_MS`]).
    pub settled: bool,
    /// The last fresh σ and when it arrived.
    sigma_seen: Option<(f32, u64)>,
    checked_ms: Option<u64>,
    /// The first check of the current run of infeasible verdicts.
    infeasible_since: Option<u64>,
    /// The first check of the current run of verdicts that are not
    /// infeasible.
    clear_since: Option<u64>,
    /// What the last check depended on beyond the map: the current item,
    /// whether the FC flew the mission, the skipped items and the origin.
    checked_key: Option<(Option<u32>, bool, usize, Option<Origin>)>,
    dirty: bool,
    /// The last logged verdict.
    logged: Option<(RouteState, Option<(u32, u32)>)>,
    /// An obstacle is near the clearance limit (the pilot-flown cap).
    pub near: bool,
    /// The cap in force and why (`near`, `infeasible`, `sloped` or
    /// `unverified`).
    pub cap: Option<&'static str>,
    /// The speed the cap holds the FC to, m/s.
    pub cap_mps: f32,
    /// The FC flight the cap slowed.
    pub fc_slowed: Option<Slowed>,
}

impl Route {
    /// The mission feed's route as last received.
    pub fn feed(&self) -> &RouteFeed {
        &self.feed
    }

    /// A new mission feed; a different route re-checks at once and forgets
    /// the origin.
    pub fn set_feed(&mut self, feed: RouteFeed) {
        if feed != self.feed {
            if feed.items != self.feed.items {
                self.origin = None;
            }
            self.feed = feed;
            self.dirty = true;
        }
    }

    fn worst(&self) -> Option<RouteState> {
        self.legs.iter().map(|l| l.state).max_by_key(|s| match s {
            RouteState::Feasible => 0,
            RouteState::Unverified => 1,
            RouteState::Infeasible => 2,
        })
    }

    /// The route ahead was not fully checked: an item with no position, a
    /// mission longer than the feed, a feed that may not hold the FC's
    /// mission, or no leg at all.
    pub fn incomplete(&self) -> bool {
        !self.unchecked.is_empty() || self.feed.truncated || self.feed.stale || self.legs.is_empty()
    }

    /// The overall verdict: the worst leg; at best unverified while the
    /// route is incomplete or σ or the position is not vouched for. `None`
    /// with no mission.
    pub fn state(&self) -> Option<RouteState> {
        if self.feed.items.is_empty() && !self.feed.stale {
            return None;
        }
        Some(match self.worst() {
            Some(RouteState::Infeasible) => RouteState::Infeasible,
            Some(RouteState::Feasible) if self.trusted && !self.incomplete() => {
                RouteState::Feasible
            }
            _ => RouteState::Unverified,
        })
    }

    /// The tightest seen point over the legs ahead: the least clearance past
    /// what its obstacle needs.
    fn pinch(&self) -> Option<(&LegCheck, [f32; 3], f32)> {
        self.legs
            .iter()
            .filter_map(|l| l.pinch.map(|(p, d)| (l, p, d)))
            .min_by(|a, b| (a.2 - a.0.required).total_cmp(&(b.2 - b.0.required)))
    }

    /// The first infeasible leg ahead.
    pub fn infeasible(&self) -> Option<&LegCheck> {
        self.legs.iter().find(|l| l.state == RouteState::Infeasible)
    }

    /// Every check has read the route infeasible for at least `ms`.
    pub fn infeasible_for(&self, ms: u64) -> bool {
        Self::held_for(self.infeasible_since, self.checked_ms, ms)
    }

    /// Every check has read the route as anything but infeasible for at
    /// least `ms`.
    pub fn clear_for(&self, ms: u64) -> bool {
        Self::held_for(self.clear_since, self.checked_ms, ms)
    }

    fn held_for(since: Option<u64>, last: Option<u64>, ms: u64) -> bool {
        since
            .zip(last)
            .is_some_and(|(s, l)| l.saturating_sub(s) >= ms)
    }

    /// The `route_check` of the published state.
    pub fn wire(&self) -> Option<RouteCheck> {
        let state = self.state()?;
        let required_m = wire(self.required);
        Some(RouteCheck {
            state,
            required_m,
            margin_m: wire(self.margin),
            sigma_m: self.sigma.map(wire),
            pinch: self.pinch().map(|(l, p, d)| Pinch {
                from_seq: l.from_seq,
                to_seq: l.to_seq,
                ned: p.map(wire),
                clearance_m: wire(d),
                required_m: wire(l.required),
            }),
            unverified_legs: self
                .legs
                .iter()
                .filter(|l| l.state == RouteState::Unverified)
                .map(|l| l.to_seq)
                .collect(),
            unchecked: self.unchecked.clone(),
            truncated: self.feed.truncated,
            stale: self.feed.stale,
        })
    }
}

impl Pilot {
    pub(super) fn confined(&self) -> bool {
        self.settings.pilot.place_class == PlaceClass::Confined
    }

    /// The FC flies the mission now (any AUTO the pilot watches, launched
    /// by it or by a ground station).
    fn mission_flying(&self) -> bool {
        self.supervised() == Some(Resume::Mission) && self.sup.hold.is_none()
    }

    /// The current item while the mission runs.
    fn mission_seq(&self) -> Option<u32> {
        self.mission
            .as_ref()
            .filter(|m| matches!(m.state.as_str(), "active" | "paused"))
            .and_then(|m| m.seq)
    }

    /// The FC's current item while a supervisor hold of the mission holds:
    /// the item it resumes at.
    fn held_mission_seq(&self) -> Option<u32> {
        self.sup
            .hold
            .as_ref()
            .filter(|h| h.resume == Resume::Mission)?;
        self.mission.as_ref().and_then(|m| m.seq)
    }

    /// Check the route ahead now: from the current item on while the
    /// mission runs (from the origin when the pilot put the FC in AUTO),
    /// from where the vehicle holds to the item a held mission resumes at,
    /// else all of it; never through a skipped item.
    pub(in crate::core) fn check_route(&mut self, now_ms: u64) {
        let origin = match self.held_mission_seq() {
            Some(s) => self.origin_here(s, now_ms),
            None => self.route.origin,
        };
        self.check_route_from(origin, now_ms);
    }

    /// [`Self::check_route`] with the legs flown from `origin`, each judged
    /// in the scan's slice of the vehicle's height now.
    fn check_route_from(&mut self, origin: Option<Origin>, now_ms: u64) {
        self.note_sigma();
        let c = self.required_clearance(now_ms);
        let settled = self.obstacles.sigma_settled(now_ms, SIGMA_SETTLE_MS);
        let gaps = &self.sup.stall.gaps;
        let ahead = legs_of(
            &self.route.feed.items,
            self.fc.home_ne(),
            &|s| gaps.iter().any(|g| g.seq == s),
            self.held_mission_seq().or(self.mission_seq()),
            origin.as_ref(),
        );
        let plane_d = self.fc.fresh_local(now_ms).map(|l| l.d);
        let fresh = self.obstacle_data_state(now_ms).is_none();
        let checked = check_legs(&self.obstacles, &ahead.legs, &c.bar, plane_d, now_ms);
        let r = &mut self.route;
        r.legs = checked;
        r.unchecked = ahead.unchecked;
        r.required = c.required;
        r.margin = c.margin;
        r.sigma = c.sigma;
        r.trusted = c.trusted;
        r.settled = settled;
        r.checked_ms = Some(now_ms);
        r.dirty = false;
        let state = r.state();
        if state == Some(RouteState::Infeasible) {
            r.infeasible_since.get_or_insert(now_ms);
            r.clear_since = None;
        } else {
            r.infeasible_since = None;
            // Only checks on fresh obstacle data count toward a resume: a
            // stale scan shows none of the pinches it found.
            if fresh {
                r.clear_since.get_or_insert(now_ms);
            } else {
                r.clear_since = None;
            }
        }
        let pinch = r
            .pinch()
            .map(|(l, p, d)| ((l.from_seq, l.to_seq), p, d, l.required));
        let key = state.map(|s| (s, pinch.map(|(k, _, _, _)| k)));
        if key != r.logged {
            r.logged = key;
            if let Some(s) = state {
                let (leg, ned, clearance_m, pinch_required_m) = match pinch {
                    Some(((f, t), p, d, need)) => {
                        (Some(leg_label(f, t)), Some(p), Some(d), Some(need))
                    }
                    None => (None, None, None, None),
                };
                tracing::info!(
                    target: "ados_pilot",
                    state = s.as_str(),
                    leg = leg.as_deref(),
                    ned = ?ned,
                    clearance_m,
                    pinch_required_m,
                    required_m = c.required,
                    sigma_m = c.sigma,
                    sigma_low_m = c.bar.base,
                    unchecked = ?r.unchecked,
                    truncated = r.feed.truncated,
                    "pilot_route_check"
                );
            }
        }
    }

    /// Every tick before the supervisor: re-check the route when due, and
    /// work out the cap.
    pub(super) fn confined_tick(&mut self, now_ms: u64) {
        if !self.confined() {
            if self.route.fc_slowed.is_some()
                || self.route.cap.is_some()
                || !self.route.legs.is_empty()
            {
                self.release_fc_speed();
                self.route.legs.clear();
                self.route.unchecked.clear();
                self.route.near = false;
                self.route.cap = None;
                self.route.logged = None;
            }
            self.route.origin = None;
            self.route.flown = None;
            self.route.infeasible_since = None;
            self.route.clear_since = None;
            return;
        }
        // The origin served its item once the FC moved past it.
        let seq = self.mission_seq();
        if let Some(o) = self.route.origin {
            if seq.is_none_or(|s| s > o.to_seq && !self.origin_pending(o, s)) {
                self.route.origin = None;
            }
        }
        // The FC flies the current item from where the vehicle is when it
        // enters AUTO or jumps to an item other than the next (whoever
        // commanded it); item to item it flies from the item before.
        let flying = self.mission_flying();
        let last = self.route.flown.replace((flying, seq));
        if let (true, Some(s)) = (flying, seq) {
            let entered = last.is_none_or(|(was, p)| !was || p.is_none());
            let jumped = last
                .and_then(|(_, p)| p)
                .is_some_and(|p| s != p && Some(s) != self.next_item(p));
            // A skipped item is a feed still behind the pilot's own skip:
            // the FC already flies the next item from the origin the skip
            // stored.
            let skipped = self.sup.stall.gaps.iter().any(|g| g.seq == s);
            if (entered || jumped) && !skipped {
                if let Some(o) = self.origin_here(s, now_ms) {
                    self.route.origin = Some(o);
                }
            }
        }
        let key = (seq, flying, self.sup.stall.gaps.len(), self.route.origin);
        let due = self.route.dirty
            || self.route.checked_key != Some(key)
            || self
                .route
                .checked_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= RECHECK_MS);
        if due {
            self.route.checked_key = Some(key);
            self.check_route(now_ms);
        } else {
            self.note_sigma();
        }
        self.update_cap(now_ms);
    }

    /// The `route_check` of the published state: only in a confined place
    /// with a mission route.
    pub(in crate::core) fn route_check_wire(&self) -> Option<RouteCheck> {
        if self.confined() {
            self.route.wire()
        } else {
            None
        }
    }

    /// Set the FC's current mission item (the command form also answers
    /// with an ack; MISSION_SET_CURRENT is deprecated).
    pub(super) fn set_mission_current(&mut self, seq: u32) {
        let m = self.tx().command(
            MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT,
            [seq as f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        self.mav(m);
    }
}
