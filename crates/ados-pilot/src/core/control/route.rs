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
//! The required clearance is the effective margin plus max(0.1 m, 3σ), σ
//! being the live horizontal pose uncertainty: the planar scan matcher's
//! `sigma_xy_m` on the `plugin.world-engine.pose` topic. The FC offers no
//! usable figure: ArduCopter never sends LOCAL_POSITION_NED_COV or ODOMETRY
//! (it only receives ODOMETRY), and EKF_STATUS_REPORT's `pos_horiz_variance`
//! is a normalised innovation test ratio, not metres. When the pose goes
//! stale the last σ is held for [`SIGMA_HOLD_MS`]; after that it still sets
//! the clearance (missing σ never lowers it) but the route reads at best
//! unverified. With no σ ever the 3σ term is 0.1 m and the route reads at
//! best unverified. So does a position the pilot does not vouch for
//! ([`super::super::position_check`]).

mod cap;
mod launch;
mod legs;

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{wire, Owner, Pinch, PlaceClass, RouteCheck, RouteState};
use serde_json::json;

use super::super::Pilot;
use super::stall::{Active, Kind};
use super::supervisor::Resume;
use super::BRAKE_LAG_S;
use crate::flows::banner;
use crate::geo::{self, V2};

pub use cap::{Slowed, CONFINED_CAP_MPS};
#[cfg(test)]
pub use legs::RouteItem;
pub use legs::{check_legs, legs_of, route_of, LegCheck, Origin, RouteFeed};

/// `hold_reason` while the vehicle holds before an infeasible leg.
pub const HOLD_ROUTE_INFEASIBLE: &str = "route-infeasible";
/// The 3σ term never goes below this, metres.
pub const MIN_SIGMA_TERM_M: f32 = 0.1;
/// A stale pose's last σ still counts this long, ms.
pub const SIGMA_HOLD_MS: u64 = 5000;
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

/// The clearance the route needs now.
#[derive(Debug, Clone, Copy)]
pub struct Clearance {
    pub required: f32,
    pub margin: f32,
    /// σ as published: fresh or held, `None` once it aged out.
    pub sigma: Option<f32>,
    /// σ is fresh or held and the position is vouched for.
    pub trusted: bool,
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
    pub required: f32,
    pub margin: f32,
    pub sigma: Option<f32>,
    pub trusted: bool,
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
    /// The cap in force and why (`near`, `infeasible` or `unverified`).
    pub cap: Option<&'static str>,
    /// The FC flight the cap slowed.
    pub fc_slowed: Option<Slowed>,
}

impl Route {
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

    /// The tightest seen point over the legs ahead.
    fn pinch(&self) -> Option<(&LegCheck, [f32; 3], f32)> {
        self.legs
            .iter()
            .filter_map(|l| l.pinch.map(|(p, d)| (l, p, d)))
            .min_by(|a, b| a.2.total_cmp(&b.2))
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
                required_m,
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
    fn confined(&self) -> bool {
        self.settings.pilot.place_class == PlaceClass::Confined
    }

    /// Keep the last σ with the time its pose arrived: the hold counts from
    /// the pose, not from when the check last looked.
    fn note_sigma(&mut self) {
        if let Some(s) = self.obstacles.last_pose_sigma() {
            self.route.sigma_seen = Some(s);
        }
    }

    /// The clearance the route needs: the effective margin plus
    /// max(0.1, 3σ), σ fresh, held, or the last one seen.
    fn required_clearance(&self, now_ms: u64) -> Clearance {
        let margin = self.settings.pilot.effective_margin_m();
        let seen = self.route.sigma_seen;
        let held = seen.filter(|(_, at)| now_ms.saturating_sub(*at) <= SIGMA_HOLD_MS);
        let term = seen.map_or(MIN_SIGMA_TERM_M, |(s, _)| (3.0 * s).max(MIN_SIGMA_TERM_M));
        Clearance {
            required: margin + term,
            margin,
            sigma: held.map(|(s, _)| s),
            trusted: held.is_some() && self.position_unverified(now_ms).is_none(),
        }
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
        let gaps = &self.sup.stall.gaps;
        let ahead = legs_of(
            &self.route.feed.items,
            self.fc.home_ne(),
            &|s| gaps.iter().any(|g| g.seq == s),
            self.held_mission_seq().or(self.mission_seq()),
            origin.as_ref(),
        );
        let plane_d = self.fc.fresh_local(now_ms).map(|l| l.d);
        let checked = check_legs(&self.obstacles, &ahead.legs, c.required, plane_d, now_ms);
        let r = &mut self.route;
        r.legs = checked;
        r.unchecked = ahead.unchecked;
        r.required = c.required;
        r.margin = c.margin;
        r.sigma = c.sigma;
        r.trusted = c.trusted;
        r.checked_ms = Some(now_ms);
        r.dirty = false;
        let state = r.state();
        if state == Some(RouteState::Infeasible) {
            r.infeasible_since.get_or_insert(now_ms);
            r.clear_since = None;
        } else {
            r.infeasible_since = None;
            r.clear_since.get_or_insert(now_ms);
        }
        let pinch = r.pinch().map(|(l, p, d)| ((l.from_seq, l.to_seq), p, d));
        let key = state.map(|s| (s, pinch.map(|(k, _, _)| k)));
        if key != r.logged {
            r.logged = key;
            if let Some(s) = state {
                let (leg, ned, clearance_m) = match pinch {
                    Some(((f, t), p, d)) => (Some(leg_label(f, t)), Some(p), Some(d)),
                    None => (None, None, None),
                };
                tracing::info!(
                    target: "ados_pilot",
                    state = s.as_str(),
                    leg = leg.as_deref(),
                    ned = ?ned,
                    clearance_m,
                    required_m = c.required,
                    sigma_m = c.sigma,
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

    /// The FC at item `s` still flies the origin's leg: `s` is the first
    /// placed item at or after the origin's item.
    fn origin_pending(&self, o: Origin, s: u32) -> bool {
        self.route
            .feed
            .items
            .iter()
            .find(|i| i.seq >= o.to_seq && (i.ned.is_some() || i.here))
            .is_some_and(|i| i.seq == s)
    }

    /// The item the FC flies after item `p` in its own order: the next
    /// navigation item of the feed.
    fn next_item(&self, p: u32) -> Option<u32> {
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
    fn origin_here(&self, to_seq: u32, now_ms: u64) -> Option<Origin> {
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

    /// The `route_check` of the published state: only in a confined place
    /// with a mission route.
    pub(in crate::core) fn route_check_wire(&self) -> Option<RouteCheck> {
        if self.confined() {
            self.route.wire()
        } else {
            None
        }
    }

    /// Where the vehicle is and how far it could fly before it stopped were
    /// a hold to wait out [`ROUTE_HOLD_CONFIRM_MS`]: at the faster of its
    /// speed now and the confined cap the wait holds the FC to, for the wait
    /// and the FC's response (`BRAKE_LAG_S`), then braking at the profile's
    /// horizontal acceleration. `None` without a fresh position.
    fn stop_reach(&self, now_ms: u64) -> Option<(V2, f32)> {
        let l = self.fc.fresh_local(now_ms)?;
        let v = l.vn.hypot(l.ve).max(CONFINED_CAP_MPS);
        let a = self.limits_now(now_ms).a_h;
        let t = ROUTE_HOLD_CONFIRM_MS as f32 / 1000.0 + BRAKE_LAG_S;
        Some(([l.n, l.e], v * t + v * v / (2.0 * a)))
    }

    /// The vehicle could come within the required clearance of `p` (local
    /// NE) before a hold that waited stopped it. The straight line is never
    /// longer than the route there, and a leg comes within the required
    /// clearance of an obstacle at most that far before its point nearest
    /// that obstacle; `p` is the first such point of a tight stretch, so a
    /// stretch that narrows toward its pinch counts from where it starts.
    /// Without a position everything is in reach.
    fn in_reach(&self, p: V2, now_ms: u64) -> bool {
        self.stop_reach(now_ms)
            .is_none_or(|(pos, stop_m)| geo::dist(pos, p) - self.route.required <= stop_m)
    }

    /// Some point of the leg `l` is in reach ([`Self::in_reach`]).
    pub(super) fn leg_in_reach(&self, l: &LegCheck, now_ms: u64) -> bool {
        let Some(pos) = self.fc.fresh_local(now_ms).map(|l| [l.n, l.e]) else {
            return true;
        };
        let (a, b) = ([l.from[0], l.from[1]], [l.to[0], l.to[1]]);
        let d = geo::sub(b, a);
        let len2 = geo::dot(d, d);
        let t = if len2 > 1e-12 {
            (geo::dot(geo::sub(pos, a), d) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.in_reach(geo::add(a, geo::scale(d, t)), now_ms)
    }

    /// While the FC flies the mission: an infeasible leg ahead holds the
    /// vehicle and asks the operator (the stall flow) once every check has
    /// read it for [`ROUTE_HOLD_CONFIRM_MS`], or at once when the vehicle
    /// could reach where the leg turns tight before a hold that waited
    /// stopped it.
    pub(super) fn route_hold(&mut self, now_ms: u64) {
        if !self.confined() || !self.mission_flying() || self.owner() != Owner::MissionFc {
            return;
        }
        let Some(leg) = self.route.infeasible().copied() else {
            return;
        };
        let near = leg
            .tight
            .or(leg.pinch.map(|(p, _)| [p[0], p[1]]))
            .is_none_or(|p| self.in_reach(p, now_ms));
        if near || self.route.infeasible_for(ROUTE_HOLD_CONFIRM_MS) {
            let seq = self.mission.as_ref().and_then(|m| m.seq);
            self.hold_route_infeasible(seq, now_ms);
        }
    }

    /// A route hold may resume: every check has read the route from where
    /// the vehicle holds as anything but infeasible for
    /// [`ROUTE_RESUME_DWELL_MS`].
    pub(super) fn route_resume_due(&self) -> bool {
        self.route.clear_for(ROUTE_RESUME_DWELL_MS)
    }

    /// Hold before the first infeasible leg with the current item `seq`.
    /// A leg ending or starting at `seq` counts down to its skip (within the
    /// automatic-skip budget); one further on holds until the operator
    /// answers (skipping `seq` would not remove it). A stall already asking
    /// about `seq` the same way keeps its countdown. The hold resumes when
    /// the route clears ([`Self::route_resume_due`]).
    pub(super) fn hold_route_infeasible(&mut self, seq: Option<u32>, now_ms: u64) {
        let Some(leg) = self.route.infeasible().copied() else {
            return;
        };
        let touches = seq.is_some_and(|s| leg.from_seq == s || leg.to_seq == s);
        let deadline_ms = if touches {
            self.mission_skip_deadline(now_ms)
        } else {
            None
        };
        let clearance = leg.pinch.map_or(0.0, |(_, d)| d);
        let required = self.route.required;
        self.supervise_hold(Resume::Mission, HOLD_ROUTE_INFEASIBLE, now_ms);
        if let Some(h) = &mut self.sup.hold {
            h.route = true;
        }
        let st = &mut self.sup.stall;
        let asked = st.active.as_ref().is_some_and(|a| {
            a.kind == Kind::Mission && a.seq == seq && a.deadline_ms.is_some() == deadline_ms.is_some()
        });
        if !asked {
            st.active = Some(Active {
                kind: Kind::Mission,
                seq,
                since_ms: now_ms,
                deadline_ms,
            });
        }
        let deadline_ms = st.active.as_ref().and_then(|a| a.deadline_ms);
        let label = leg_label(leg.from_seq, leg.to_seq);
        self.banner = Some(banner(
            "pilot.banner.route_infeasible",
            &[
                ("leg", json!(label)),
                ("clearance_m", json!(two_dp(clearance))),
                ("required_m", json!(two_dp(required))),
            ],
            now_ms,
        ));
        tracing::warn!(
            target: "ados_pilot",
            leg = %label,
            ned = ?leg.pinch.map(|(p, _)| p),
            clearance_m = clearance,
            required_m = required,
            seq,
            infeasible_ms = self.route.infeasible_since.map(|t| now_ms.saturating_sub(t)),
            skip_in_s = deadline_ms.map(|d| d.saturating_sub(now_ms) / 1000),
            "pilot_route_infeasible_hold"
        );
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
