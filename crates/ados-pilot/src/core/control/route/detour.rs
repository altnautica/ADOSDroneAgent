//! A blocked mission leg: the immediate skip of a waypoint the planner
//! proves unreachable, and under avoid `bypass` the detour round the leg.
//!
//! Both act while a hold of the mission holds in GUIDED (after BRAKE: an
//! obstacle the FC's flight came too close to, or an infeasible leg ahead)
//! and the straight way from the vehicle to the waypoint meets a seen
//! obstacle inside the margin; never with avoid `off`, on stale obstacle
//! data or on a position the pilot does not vouch for.
//!
//! - The waypoint is skipped at once, recorded as a coverage gap, when it is
//!   proven unreachable ([`Pilot::unreachable`]): it lies inside a seen
//!   obstacle, or the route search over known and unknown space (unknown
//!   counted passable) ends with no way to it within
//!   [`crate::planner::ROUTE_REACH_M`]. A search out of budget is no proof.
//!   The skip counts toward the automatic-skip budget; with the budget
//!   spent no immediate skip is made and the hold waits for the operator.
//! - Otherwise, under `bypass`, the pilot flies its planner (as a goto, so
//!   both planners route it) to the leg's next free point: the first point
//!   of the leg past the blockage that keeps the margin plus
//!   [`DETOUR_EXTRA_M`] with a straight way on to the waypoint that keeps as
//!   much, [`PAST_M`] further on; the waypoint itself when no such point is
//!   on the leg. There it sets the FC's current item to the waypoint
//!   (`MAV_CMD_DO_SET_MISSION_CURRENT`) and puts the FC back in AUTO, which
//!   flies the item from where the vehicle stands.
//! - A detour the planner keeps blocked for [`DETOUR_BLOCKED_MS`], or that
//!   outlasts its budget, holds the mission with a stall (the operator's
//!   choice and the skip countdown), unless a proof skips it at once. An
//!   item gets [`MAX_DETOURS`] detours.

use ados_protocol::pilot::{AvoidMode, AvoidState, MissionDetour, Owner};

use super::super::super::Pilot;
use super::super::stall::Kind;
use super::super::supervisor::wp_arg;
use super::super::HARD_STOP_FRAC;
use crate::flows::banner;
use crate::geo::{self, V2};

/// Detours flown per mission item.
pub const MAX_DETOURS: u32 = 2;
/// A proof is tried at most this often while a hold holds, ms.
const PROOF_EVERY_MS: u64 = 1000;
/// The free point, and the way on from it, keep this much beyond the
/// margin, m.
pub const DETOUR_EXTRA_M: f32 = 0.5;
/// The detour goes this far past the first free point, m.
pub const PAST_M: f32 = 1.0;
/// The detour has arrived within this of its point, m.
pub const ARRIVE_M: f32 = 1.0;
/// A detour whose plan stays blocked this long gives up, ms.
pub const DETOUR_BLOCKED_MS: u64 = 3000;
/// A detour's time budget: this, plus [`DETOUR_MS_PER_M`] per metre of the
/// straight way to its point.
const DETOUR_BASE_MS: u64 = 30_000;
const DETOUR_MS_PER_M: f32 = 2000.0;
/// Samples along a way, m (the grid's cell).
const STEP_M: f32 = crate::obstacles::CELL_M;
/// The route proof's tiles keep this much: the 1 m tiles find every gap at
/// least three tiles wide, so the proof is only made with a margin that
/// needs that much (a narrower gap the pilot could use might read shut).
const PROOF_NEED_M: f32 = 1.0;
const PROOF_MIN_MARGIN_M: f32 = 1.5;
/// A planar map judges a waypoint only this near the flight height, m.
const PROOF_BAND_M: f32 = 0.5;
/// Gap reasons of a skip proven unreachable.
pub const GAP_INSIDE: &str = "unreachable_inside_obstacle";
pub const GAP_NO_PATH: &str = "unreachable_no_path";

/// A detour in flight.
#[derive(Debug, Clone, Copy)]
pub struct Detour {
    pub seq: u32,
    pub target: [f32; 3],
    pub since_ms: u64,
    budget_ms: u64,
    blocked_since: Option<u64>,
}

/// What was tried for one item's blocked leg.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tries {
    seq: Option<u32>,
    detours: u32,
    proof_ms: Option<u64>,
}

/// `p` on the way from `a` to `b` at fraction `t`.
fn lerp(a: V2, b: V2, t: f32) -> V2 {
    geo::add(a, geo::scale(geo::sub(b, a), t))
}

impl Pilot {
    /// Seen clearance at `p` keeps `need` (unseen and off-grid space does).
    fn keeps(&self, p: V2, need: f32) -> bool {
        self.obstacles.grid.clearance(p).is_none_or(|c| c >= need)
    }

    /// Every sample of the straight way from `a` to `b` keeps `need`.
    fn way_keeps(&self, a: V2, b: V2, need: f32) -> bool {
        let n = (geo::dist(a, b) / STEP_M).ceil().max(1.0) as u32;
        (0..=n).all(|k| self.keeps(lerp(a, b, k as f32 / n as f32), need))
    }

    /// The straight way from the vehicle at `pos` to `wp` comes nearer a
    /// seen obstacle than the margin, or than the vehicle is now (beside a
    /// wall inside the margin, a way along it is not blocked).
    fn way_blocked(&self, pos: V2, wp: V2, margin: f32) -> bool {
        let own = self.obstacles.grid.clearance(pos).unwrap_or(f32::INFINITY);
        !self.way_keeps(pos, wp, margin.min(own - 0.1))
    }

    /// The leg's next free point from `pos` on the leg `a` → `wp`: the
    /// first point at or past the vehicle's that keeps `need` with a
    /// straight way on to `wp` that keeps as much, [`PAST_M`] further on.
    fn free_point(&self, a: V2, wp: V2, pos: V2, need: f32) -> Option<V2> {
        let len = geo::dist(a, wp);
        if len < 1e-3 {
            return None;
        }
        let u = geo::scale(geo::sub(wp, a), 1.0 / len);
        let mut t = geo::dot(geo::sub(pos, a), u).clamp(0.0, len);
        while t <= len {
            let p = geo::add(a, geo::scale(u, t));
            if self.keeps(p, need) && self.way_keeps(p, wp, need) {
                return Some(geo::add(a, geo::scale(u, (t + PAST_M).min(len))));
            }
            t += STEP_M;
        }
        None
    }

    /// The waypoint `wp` is proven unreachable from `pos` at down `d`: the
    /// gap reason, or `None`. Inside an obstacle: nearer a seen one than
    /// the hard stop (where the pilot never flies) less the σ term. No path:
    /// the route search over known and unknown space ends without one
    /// (never on a search out of budget, beyond the reach, or with the
    /// vehicle itself inside a tile's clearance). A planar map proves
    /// nothing about another height.
    fn unreachable(&mut self, pos: V2, wp: [f32; 3], d: f32, now_ms: u64) -> Option<&'static str> {
        if (wp[2] - d).abs() > PROOF_BAND_M {
            return None;
        }
        let at = [wp[0], wp[1]];
        let margin = self.margin();
        let c = self.required_clearance(now_ms);
        let inside = HARD_STOP_FRAC * margin - (c.required - c.margin);
        let grid = &self.obstacles.grid;
        if grid.clearance(at).is_some_and(|c| c < inside) {
            return Some(GAP_INSIDE);
        }
        if margin < PROOF_MIN_MARGIN_M || grid.clearance(pos).is_some_and(|c| c < PROOF_NEED_M) {
            return None;
        }
        let r = &mut self.route.prover;
        (r.connects(grid, pos, at, PROOF_NEED_M, now_ms) == Some(false)).then_some(GAP_NO_PATH)
    }

    /// Skip item `seq` at once as proven unreachable (`reason`), within the
    /// automatic-skip budget; `false` when the budget is spent.
    fn skip_unreachable(&mut self, seq: u32, reason: &'static str, now_ms: u64) -> bool {
        if self.mission_skip_deadline(now_ms).is_none() {
            return false;
        }
        tracing::warn!(target: "ados_pilot", seq, reason, "pilot_mission_unreachable");
        let key = if reason == GAP_INSIDE {
            "pilot.banner.mission_wp_inside_obstacle"
        } else {
            "pilot.banner.mission_wp_no_path"
        };
        self.skip_item(seq, "unreachable", reason.to_owned(), key, now_ms);
        true
    }

    /// While a hold of the mission holds in GUIDED: skip a waypoint proven
    /// unreachable, or under `bypass` fly the detour round its blocked leg.
    /// `true` when it took the vehicle.
    pub(in crate::core) fn bypass_blocked_leg(&mut self, now_ms: u64) -> bool {
        let avoid = self.settings.pilot.avoid;
        if avoid == AvoidMode::Off
            || self.obstacle_data_state(now_ms).is_some()
            || self.auto_position_blocked(now_ms)
        {
            return false;
        }
        let (Some(seq), Some(wp), Some(l)) = (
            self.mission.as_ref().and_then(|m| m.seq),
            self.mission_target_ned(),
            self.fc.fresh_local(now_ms),
        ) else {
            return false;
        };
        let tries = &mut self.route.tries;
        if tries.seq != Some(seq) {
            *tries = Tries {
                seq: Some(seq),
                ..Tries::default()
            };
        }
        if tries
            .proof_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < PROOF_EVERY_MS)
        {
            return false;
        }
        tries.proof_ms = Some(now_ms);
        let pos = [l.n, l.e];
        let at = [wp[0], wp[1]];
        let margin = self.margin();
        if !self.way_blocked(pos, at, margin) {
            return false;
        }
        if let Some(reason) = self.unreachable(pos, wp, l.d, now_ms) {
            if self.skip_unreachable(seq, reason, now_ms) {
                return true;
            }
        }
        if avoid != AvoidMode::Bypass || self.route.tries.detours >= MAX_DETOURS {
            return false;
        }
        // The leg the FC flies to the item: from the item before it (or
        // from where it was put in AUTO), else from here.
        let from = self
            .route
            .legs
            .iter()
            .find(|g| g.to_seq == seq)
            .map_or(pos, |g| [g.from[0], g.from[1]]);
        let target = self
            .free_point(from, at, pos, margin + DETOUR_EXTRA_M)
            .or_else(|| self.keeps(at, margin).then_some(at));
        let Some(t) = target else {
            return false;
        };
        self.start_detour(seq, [t[0], t[1], l.d], geo::dist(pos, t), now_ms)
    }

    /// Fly the planner to `target` for item `seq` (a goto): the hold and
    /// any stall end. `false` when the goto is refused.
    fn start_detour(&mut self, seq: u32, target: [f32; 3], dist_m: f32, now_ms: u64) -> bool {
        self.end_supervision();
        self.start_goto(target, now_ms);
        if self.owner() != Owner::Goto {
            return false;
        }
        self.route.tries.detours += 1;
        self.route.detour = Some(Detour {
            seq,
            target,
            since_ms: now_ms,
            budget_ms: DETOUR_BASE_MS + (dist_m * DETOUR_MS_PER_M) as u64,
            blocked_since: None,
        });
        self.banner = Some(banner(
            "pilot.banner.mission_detour",
            &[("wp", wp_arg(Some(seq)))],
            now_ms,
        ));
        tracing::info!(target: "ados_pilot", seq, ?target, "pilot_mission_detour");
        true
    }

    /// Every tick: a detour that arrived resumes the mission; one the
    /// planner keeps blocked or that outlasts its budget holds the mission
    /// with a stall (or skips a waypoint proven unreachable); one someone
    /// else took ends (an operator's stick takeover leaves the mission
    /// paused for `resume`).
    pub(in crate::core) fn detour_tick(&mut self, now_ms: u64) {
        let Some(d) = self.route.detour else {
            return;
        };
        let pos = self.fc.fresh_local(now_ms).map(|l| [l.n, l.e]);
        let arrived = pos.is_some_and(|p| geo::dist(p, [d.target[0], d.target[1]]) < ARRIVE_M);
        let ours = self.owner() == Owner::Goto && self.goto == Some(d.target);
        if !self.fc.armed || (!ours && !arrived) {
            self.route.detour = None;
            if self.fc.armed && self.owner() == Owner::Pilot {
                self.mission_paused = true;
            }
            tracing::info!(target: "ados_pilot", seq = d.seq, owner = ?self.owner(), "pilot_mission_detour_ended");
            return;
        }
        if arrived {
            self.resume_after_detour(d, now_ms);
            return;
        }
        let blocked = self.plan.state == AvoidState::Blocked && self.plan.target.is_none();
        let blocked_since = blocked.then(|| d.blocked_since.unwrap_or(now_ms));
        if let Some(x) = &mut self.route.detour {
            x.blocked_since = blocked_since;
        }
        let stuck = blocked_since.is_some_and(|t| now_ms.saturating_sub(t) >= DETOUR_BLOCKED_MS);
        if stuck || now_ms.saturating_sub(d.since_ms) >= d.budget_ms {
            self.give_up_detour(d, pos, now_ms);
        }
    }

    /// The detour arrived: the FC's current item is the waypoint again and
    /// AUTO flies it from here, after the checks any return to AUTO makes.
    fn resume_after_detour(&mut self, d: Detour, now_ms: u64) {
        self.route.detour = None;
        self.goto = None;
        self.drop_goal_ref();
        tracing::info!(target: "ados_pilot", seq = d.seq, "pilot_mission_detour_resume");
        self.set_mission_current(d.seq);
        if self.auto_position_blocked(now_ms) {
            self.supervise_hold(
                super::super::supervisor::Resume::Mission,
                super::super::super::position_check::HOLD_POSITION_UNVERIFIED,
                now_ms,
            );
            return;
        }
        if !self.confined_auto_clear(d.seq, now_ms) {
            self.hold_route_infeasible(Some(d.seq), now_ms);
            return;
        }
        let auto = self.family().auto();
        self.set_mode(auto, now_ms);
        self.set_owner(Owner::MissionFc, "mission_detour");
        self.mission_paused = false;
        self.banner = Some(banner(
            "pilot.banner.mission_detour_resumed",
            &[("wp", wp_arg(Some(d.seq)))],
            now_ms,
        ));
    }

    /// The detour found no way: a waypoint proven unreachable is skipped at
    /// once; anything else holds the mission with a stall.
    fn give_up_detour(&mut self, d: Detour, pos: Option<V2>, now_ms: u64) {
        self.route.detour = None;
        self.goto = None;
        tracing::warn!(
            target: "ados_pilot",
            seq = d.seq,
            flown_s = now_ms.saturating_sub(d.since_ms) / 1000,
            "pilot_mission_detour_failed"
        );
        let wp = self
            .mission_target_ned()
            .filter(|_| self.mission_seq_is(d.seq));
        if let (Some(wp), Some(p)) = (wp, pos) {
            let fresh = self.obstacle_data_state(now_ms).is_none();
            let d_now = self.fc.fresh_local(now_ms).map_or(wp[2], |l| l.d);
            if fresh && !self.auto_position_blocked(now_ms) {
                if let Some(reason) = self.unreachable(p, wp, d_now, now_ms) {
                    if self.skip_unreachable(d.seq, reason, now_ms) {
                        return;
                    }
                }
            }
        }
        let dist = match (wp, pos) {
            (Some(w), Some(p)) => geo::dist(p, [w[0], w[1]]),
            _ => 0.0,
        };
        self.raise_stall(Kind::Mission, Some(d.seq), dist, 0.0, now_ms);
    }

    /// The FC's current item is `seq`.
    fn mission_seq_is(&self, seq: u32) -> bool {
        self.mission.as_ref().and_then(|m| m.seq) == Some(seq)
    }

    /// The `mission_detour` of the published state.
    pub(in crate::core) fn mission_detour_wire(&self) -> Option<MissionDetour> {
        self.route.detour.map(|d| MissionDetour {
            seq: d.seq,
            target_ned: d.target.map(f64::from),
            since_ms: d.since_ms + self.epoch_offset_ms,
        })
    }
}
