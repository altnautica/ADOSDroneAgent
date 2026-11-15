//! The confined-place profile: the route check of the FC mission against the
//! live occupancy, the hold before an infeasible leg, and the 0.5 m/s cap near
//! the clearance limit ([`cap`]). Nothing here acts in an `open` place.
//!
//! The route is the router's mirrored navigation items in the FC's local NED
//! frame, cut into the legs the FC flies ([`legs::legs_of`]): between
//! consecutive placed items, skipped items left out, and from the vehicle's
//! own position to the item the pilot put the FC in AUTO at (after a skip or
//! a hold ArduPilot flies that item from where the vehicle stands). Each leg
//! is checked against the occupancy map
//! ([`crate::obstacles::Obstacles::probe_leg`]) with a conservative
//! clearance: a seen obstacle closer than the required clearance makes the
//! leg infeasible; unseen space that close leaves it unverified. An item the
//! router cannot place (no position), or a mission longer than the feed
//! carries, leaves the route unverified and named (`unchecked`, `truncated`);
//! a launch into such a route is refused.
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
mod legs;

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{wire, Owner, Pinch, PlaceClass, RouteCheck, RouteState};
use serde_json::json;

use super::super::Pilot;
use super::stall::{Active, Kind};
use super::supervisor::Resume;
use crate::flows::banner;

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
    /// Where the FC flies the item it was put in AUTO at from.
    pub origin: Option<Origin>,
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
    /// What the last check depended on beyond the map: the current item,
    /// whether the FC flew the mission, the skipped items and the origin.
    checked_key: Option<(Option<u32>, bool, usize, Option<Origin>)>,
    dirty: bool,
    /// The last logged verdict.
    logged: Option<(RouteState, Option<(u32, u32)>)>,
    /// An obstacle is near the clearance limit (the pilot-flown cap).
    pub near: bool,
    /// The cap in force and why (`near` or `unverified`).
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
    /// mission longer than the feed, or no leg at all.
    pub fn incomplete(&self) -> bool {
        !self.unchecked.is_empty() || self.feed.truncated || self.legs.is_empty()
    }

    /// The overall verdict: the worst leg; at best unverified while the
    /// route is incomplete or σ or the position is not vouched for. `None`
    /// with no mission route.
    pub fn state(&self) -> Option<RouteState> {
        if self.feed.items.is_empty() {
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

    /// Check the route ahead now: from the current item on while the
    /// mission runs (from the origin when the pilot put the FC in AUTO),
    /// else all of it; never through a skipped item.
    pub(in crate::core) fn check_route(&mut self, now_ms: u64) {
        self.note_sigma();
        let c = self.required_clearance(now_ms);
        let gaps = &self.sup.stall.gaps;
        let ahead = legs_of(
            &self.route.feed.items,
            self.fc.home_ne(),
            &|s| gaps.iter().any(|g| g.seq == s),
            self.mission_seq(),
            self.route.origin.as_ref(),
        );
        let checked = check_legs(&self.obstacles, &ahead.legs, c.required, now_ms);
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
            return;
        }
        // The origin served its item once the FC moved past it.
        let seq = self.mission_seq();
        if let Some(o) = self.route.origin {
            if seq.is_none_or(|s| s > o.to_seq && !self.origin_pending(o, s)) {
                self.route.origin = None;
            }
        }
        let key = (
            seq,
            self.mission_flying(),
            self.sup.stall.gaps.len(),
            self.route.origin,
        );
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

    /// Before the pilot puts the FC in AUTO at item `to_seq`: the FC will
    /// fly it from where the vehicle is. In a confined place, check the
    /// route from here; `false` when a leg ahead is infeasible (the caller
    /// holds instead). Open places always pass.
    pub(in crate::core) fn confined_auto_clear(&mut self, to_seq: u32, now_ms: u64) -> bool {
        if !self.confined() {
            return true;
        }
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
        self.route.origin = self.fc.fresh_local(now_ms).map(|l| Origin {
            from_seq,
            to_seq,
            ned: [l.n, l.e, l.d],
        });
        self.check_route(now_ms);
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

    /// While the FC flies the mission: an infeasible leg ahead holds the
    /// vehicle and asks the operator (the stall flow).
    pub(super) fn route_hold(&mut self, now_ms: u64) {
        if !self.confined() || !self.mission_flying() || self.owner() != Owner::MissionFc {
            return;
        }
        if self.route.infeasible().is_some() {
            let seq = self.mission.as_ref().and_then(|m| m.seq);
            self.hold_route_infeasible(seq, now_ms);
        }
    }

    /// Hold before the first infeasible leg with the current item `seq`.
    /// A leg ending or starting at `seq` counts down to its skip (within the
    /// automatic-skip budget); one further on holds until the operator
    /// answers (skipping `seq` would not remove it).
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
        self.sup.stall.active = Some(Active {
            kind: Kind::Mission,
            seq,
            since_ms: now_ms,
            deadline_ms,
        });
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
            skip_in_s = deadline_ms.map(|d| d.saturating_sub(now_ms) / 1000),
            "pilot_route_infeasible_hold"
        );
    }

    /// A launch into the mission in a confined place: refused while the
    /// route is infeasible (naming the leg, the point, its clearance and the
    /// clearance needed), or while an item has no position or the mission
    /// is longer than the feed (`route_unchecked`, naming them).
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
        if !self.route.unchecked.is_empty() || self.route.feed.truncated {
            tracing::warn!(
                target: "ados_pilot",
                reason = "route_unchecked",
                unchecked = ?self.route.unchecked,
                truncated = self.route.feed.truncated,
                "pilot_launch_refused"
            );
            return Err("route_unchecked".into());
        }
        Ok(())
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
