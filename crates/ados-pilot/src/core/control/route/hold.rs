//! The route hold: when an infeasible leg ahead holds the mission, and when
//! the hold may resume.

use serde_json::json;

use super::super::super::Pilot;
use super::super::stall::{Active, Kind};
use super::super::supervisor::Resume;
use super::super::BRAKE_LAG_S;
use super::{
    leg_label, two_dp, LegCheck, CONFINED_CAP_MPS, HOLD_ROUTE_INFEASIBLE, ROUTE_HOLD_CONFIRM_MS,
    ROUTE_RESUME_DWELL_MS,
};
use crate::flows::banner;
use crate::geo::{self, V2};
use ados_protocol::pilot::{AvoidMode, Owner};

/// How far a vehicle at `v` m/s flies before a hold that waited stopped it:
/// for [`ROUTE_HOLD_CONFIRM_MS`] (as long as a pinch can wait for the next
/// check, `RECHECK_MS`, before any check sees it) and the FC's response
/// (`BRAKE_LAG_S`) at `v`, then braking at `a` m/s².
pub(super) fn stop_m(v: f32, a: f32) -> f32 {
    let t = ROUTE_HOLD_CONFIRM_MS as f32 / 1000.0 + BRAKE_LAG_S;
    v * t + v * v / (2.0 * a)
}

/// The fastest speed whose [`stop_m`] fits in `d` metres (0 when none
/// does): the positive root of `v·T + v²/2a = d`.
pub(super) fn speed_stopping_within(d: f32, a: f32) -> f32 {
    let t = ROUTE_HOLD_CONFIRM_MS as f32 / 1000.0 + BRAKE_LAG_S;
    if d <= 0.0 {
        return 0.0;
    }
    a * (-t + (t * t + 2.0 * d / a).sqrt())
}

impl Pilot {
    /// Where the vehicle is and how far it could fly before it stopped were
    /// a hold to wait out [`ROUTE_HOLD_CONFIRM_MS`] ([`stop_m`]): at the
    /// faster of its speed now and the confined cap the wait holds the FC
    /// to, braking at the profile's horizontal acceleration. `None` without
    /// a fresh position.
    fn stop_reach(&self, now_ms: u64) -> Option<(V2, f32)> {
        let l = self.fc.fresh_local(now_ms)?;
        let v = l.vn.hypot(l.ve).max(CONFINED_CAP_MPS);
        Some(([l.n, l.e], stop_m(v, self.limits_now(now_ms).a_h)))
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
    pub(in crate::core) fn leg_in_reach(&self, l: &LegCheck, now_ms: u64) -> bool {
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
    /// stopped it. In a confined place always; in an open one under avoid
    /// `bypass`, where the detour round the leg follows the hold
    /// ([`super::detour`]).
    pub(in crate::core) fn route_hold(&mut self, now_ms: u64) {
        let bypass = self.settings.pilot.avoid == AvoidMode::Bypass;
        if !(self.confined() || bypass)
            || !self.mission_flying()
            || self.owner() != Owner::MissionFc
        {
            return;
        }
        let Some(leg) = self.route.infeasible().copied() else {
            return;
        };
        // Where the leg turns tight, or its pinch: the tight stretch may
        // start behind the vehicle while the pinch lies ahead.
        let pinch = leg.pinch.map(|(p, _)| [p[0], p[1]]);
        let near = (leg.tight.is_none() && pinch.is_none())
            || [leg.tight, pinch]
                .into_iter()
                .flatten()
                .any(|p| self.in_reach(p, now_ms));
        // While σ settles every obstacle counts the whole σ, so a leg the
        // match keeps clear can read tight for those 2 s: only a pinch in
        // reach holds then (the FC is held to the cap meanwhile); the
        // confirmation counts once a check has been made with σ settled.
        if near || (self.route.settled && self.route.infeasible_for(ROUTE_HOLD_CONFIRM_MS)) {
            let seq = self.mission.as_ref().and_then(|m| m.seq);
            self.hold_route_infeasible(seq, now_ms);
        }
    }

    /// A route hold may resume: every check has read the route from where
    /// the vehicle holds as anything but infeasible for
    /// [`ROUTE_RESUME_DWELL_MS`].
    pub(in crate::core) fn route_resume_due(&self) -> bool {
        self.route.clear_for(ROUTE_RESUME_DWELL_MS)
    }

    /// Hold before the first infeasible leg with the current item `seq`.
    /// A leg ending or starting at `seq` counts down to its skip (within the
    /// automatic-skip budget); one further on holds until the operator
    /// answers (skipping `seq` would not remove it). A stall already asking
    /// about `seq` the same way keeps its countdown. The hold resumes when
    /// the route clears ([`Self::route_resume_due`]).
    pub(in crate::core) fn hold_route_infeasible(&mut self, seq: Option<u32>, now_ms: u64) {
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
        let required = leg.required;
        self.supervise_hold(Resume::Mission, HOLD_ROUTE_INFEASIBLE, now_ms);
        if let Some(h) = &mut self.sup.hold {
            h.route = true;
        }
        let return_ms = if deadline_ms.is_none() {
            self.operator_wait_return(now_ms)
        } else {
            None
        };
        let st = &mut self.sup.stall;
        let asked = st.active.as_ref().is_some_and(|a| {
            a.kind == Kind::Mission
                && a.seq == seq
                && a.deadline_ms.is_some() == deadline_ms.is_some()
        });
        if !asked {
            st.active = Some(Active {
                kind: Kind::Mission,
                seq,
                since_ms: now_ms,
                deadline_ms,
                return_ms,
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
}
