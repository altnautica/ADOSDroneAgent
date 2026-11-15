//! The confined-place profile: the route check of the FC mission against the
//! live occupancy, the hold before an infeasible leg, and the 0.5 m/s cap near
//! the clearance limit. Nothing here acts in an `open` place.
//!
//! The route is the router's mirrored navigation items in the FC's local NED
//! frame. Every leg between consecutive placed items (a take-off without a
//! position starts at home) is sampled every [`SAMPLE_STEP_M`] against the
//! occupancy map ([`crate::obstacles::Obstacles::probe_leg`]): a seen obstacle
//! closer to a sample than the required clearance makes the leg infeasible;
//! unseen space that close leaves it unverified. The required clearance is the
//! effective margin plus max(0.1 m, 3σ), σ being the live horizontal pose
//! uncertainty.
//!
//! σ is the planar scan matcher's `sigma_xy_m` on the `plugin.world-engine.pose`
//! topic. The FC offers no usable figure: ArduCopter never sends
//! LOCAL_POSITION_NED_COV or ODOMETRY (it only receives ODOMETRY), and
//! EKF_STATUS_REPORT's `pos_horiz_variance` is a normalised innovation test
//! ratio, not metres. Without a fresh pose σ the 3σ term is 0.1 m and the route
//! reads at best unverified.
//!
//! In AUTO the FC's waypoint speed is lowered with MAV_CMD_DO_CHANGE_SPEED and
//! restored to its default (`WP_SPD` m/s, or `WPNAV_SPEED` cm/s on older
//! firmware) when the cap ends. ArduCopter denies a speed of zero or less
//! (there is no "back to default" value: `handle_MAV_CMD_DO_CHANGE_SPEED` in
//! `ArduCopter/GCS_MAVLink_Copter.cpp`), and `ModeAuto::init` clears the
//! override on every entry to AUTO, so a cap is re-sent after a re-entry and
//! needs no restore when AUTO ends.

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{wire, Owner, Pinch, PlaceClass, RouteCheck, RouteState, SpeedCap};
use serde_json::{json, Value};

use super::super::Pilot;
use super::stall::{Active, Kind};
use super::supervisor::Resume;
use crate::flows::banner;
use crate::geo::V2;
use crate::modes::copter;
use crate::obstacles::Obstacles;

/// `hold_reason` while the vehicle holds before an infeasible leg.
pub const HOLD_ROUTE_INFEASIBLE: &str = "route-infeasible";
/// `speed_cap.reason` of the confined cap.
pub const REASON_CONFINED_NEAR: &str = "confined-near";
/// The route's sample spacing, metres.
pub const SAMPLE_STEP_M: f32 = 0.25;
/// The 3σ term never goes below this, metres.
pub const MIN_SIGMA_TERM_M: f32 = 0.1;
/// The cap holds while an obstacle is within the required clearance plus this.
pub const NEAR_EXTRA_M: f32 = 1.5;
/// The confined cap, m/s.
pub const CONFINED_CAP_MPS: f32 = 0.5;
/// The cap ends only this much further out than it starts, metres.
const NEAR_RELEASE_M: f32 = 0.25;
/// The route is re-checked this often as cells become seen.
const RECHECK_MS: u64 = 1000;
/// Seen obstacles this far beyond the required clearance still name the
/// tightest point.
const REPORT_EXTRA_M: f32 = 0.5;
/// MAV_CMD_NAV_TAKEOFF.
const CMD_TAKEOFF: u16 = 22;
/// DO_CHANGE_SPEED speed type: ground speed.
const SPEED_TYPE_GROUND: f32 = 1.0;

/// One mirrored navigation item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteItem {
    pub seq: u32,
    pub cmd: u16,
    pub ned: Option<[f32; 3]>,
}

/// The `route` of a mission feed payload ([`crate::router_state::mission_of`]).
pub fn route_items(payload: &Value) -> Vec<RouteItem> {
    let Some(list) = payload.get("route").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|i| {
            let seq = u32::try_from(i.get("seq")?.as_u64()?).ok()?;
            let cmd = u16::try_from(i.get("cmd")?.as_u64()?).ok()?;
            let ned = i.get("ned").and_then(Value::as_array).and_then(|a| {
                let v: Vec<f32> = a
                    .iter()
                    .filter_map(|x| x.as_f64().map(|x| x as f32))
                    .collect();
                (v.len() == 3).then(|| [v[0], v[1], v[2]])
            });
            Some(RouteItem { seq, cmd, ned })
        })
        .collect()
}

/// One leg's verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegCheck {
    pub from_seq: u32,
    pub to_seq: u32,
    pub state: RouteState,
    /// The tightest sample with a seen obstacle near it: local NED and the
    /// clearance (m).
    pub pinch: Option<([f32; 3], f32)>,
}

/// A leg to check: `(from_seq, from, to_seq, to)`, local NED.
pub type Leg = (u32, [f32; 3], u32, [f32; 3]);

/// The legs between consecutive placed items. A take-off without a position
/// starts at `home` at the next item's height.
pub fn legs_of(items: &[RouteItem], home: V2) -> Vec<Leg> {
    let mut placed: Vec<(u32, [f32; 3])> = Vec::with_capacity(items.len());
    let mut takeoff: Option<u32> = None;
    for it in items {
        match it.ned {
            Some(p) => {
                if let Some(s) = takeoff.take() {
                    placed.push((s, [home[0], home[1], p[2]]));
                }
                placed.push((it.seq, p));
            }
            None if it.cmd == CMD_TAKEOFF && placed.is_empty() => takeoff = Some(it.seq),
            None => {}
        }
    }
    placed
        .windows(2)
        .map(|w| (w[0].0, w[0].1, w[1].0, w[1].1))
        .collect()
}

/// Check `legs` sampled every `step` m against `obs` with `required` m of
/// clearance.
pub fn check_legs(
    obs: &Obstacles,
    legs: &[Leg],
    step: f32,
    required: f32,
    now_ms: u64,
) -> Vec<LegCheck> {
    legs.iter()
        .map(|&(from_seq, a, to_seq, b)| {
            let probe = obs.probe_leg(
                [a[0], a[1]],
                [b[0], b[1]],
                step,
                required,
                required + REPORT_EXTRA_M,
                now_ms,
            );
            let pinch = probe.nearest.map(|(k, p, d)| {
                let len = (b[0] - a[0]).hypot(b[1] - a[1]);
                let f = if len > 1e-6 {
                    ((k as f32 * step).min(len) / len).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                ([p[0], p[1], a[2] + f * (b[2] - a[2])], d)
            });
            let state = if pinch.is_some_and(|(_, d)| d < required) {
                RouteState::Infeasible
            } else if probe.unseen {
                RouteState::Unverified
            } else {
                RouteState::Feasible
            };
            LegCheck {
                from_seq,
                to_seq,
                state,
                pinch,
            }
        })
        .collect()
}

fn leg_label(from: u32, to: u32) -> String {
    format!("{from}→{to}")
}

fn two_dp(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

/// The confined-place state.
#[derive(Debug, Default)]
pub struct Route {
    items: Vec<RouteItem>,
    /// The last check of the legs still ahead.
    pub legs: Vec<LegCheck>,
    pub required: f32,
    pub margin: f32,
    pub sigma: Option<f32>,
    checked_ms: Option<u64>,
    /// What the last check depended on beyond the map: the current item,
    /// whether the FC flew the mission, and the skipped items.
    checked_key: Option<(Option<u32>, bool, usize)>,
    dirty: bool,
    /// The last logged verdict.
    logged: Option<(RouteState, Option<(u32, u32)>)>,
    /// An obstacle is near the clearance limit (the pilot-flown cap).
    pub near: bool,
    /// The cap in force and why (`near` or `unverified`).
    pub cap: Option<&'static str>,
    /// The FC's AUTO speed is lowered.
    pub fc_slowed: bool,
}

impl Route {
    /// New mirrored items; a change re-checks at once.
    pub fn set_items(&mut self, items: Vec<RouteItem>) {
        if items != self.items {
            self.items = items;
            self.dirty = true;
        }
    }

    fn worst(&self) -> Option<&LegCheck> {
        self.legs.iter().max_by_key(|l| match l.state {
            RouteState::Feasible => 0,
            RouteState::Unverified => 1,
            RouteState::Infeasible => 2,
        })
    }

    /// The overall verdict: the worst leg, and at best unverified while σ is
    /// unknown.
    pub fn state(&self) -> Option<RouteState> {
        let s = self.worst()?.state;
        Some(if s == RouteState::Feasible && self.sigma.is_none() {
            RouteState::Unverified
        } else {
            s
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
        })
    }
}

impl Pilot {
    fn confined(&self) -> bool {
        self.settings.pilot.place_class == PlaceClass::Confined
    }

    /// The clearance the route needs: the effective margin plus
    /// max(0.1, 3σ).
    fn required_clearance(&self, now_ms: u64) -> (f32, f32, Option<f32>) {
        let margin = self.settings.pilot.effective_margin_m();
        let sigma = self.obstacles.pose_sigma(now_ms);
        let term = sigma.map_or(MIN_SIGMA_TERM_M, |s| (3.0 * s).max(MIN_SIGMA_TERM_M));
        (margin + term, margin, sigma)
    }

    /// The FC flies the mission now (any AUTO the pilot watches, launched
    /// by it or by a ground station).
    fn mission_flying(&self) -> bool {
        self.supervised() == Some(Resume::Mission) && self.sup.hold.is_none()
    }

    /// The legs still ahead: from the current item on while the FC flies
    /// the mission, else all; never a leg touching a skipped item (the FC
    /// does not fly it).
    fn legs_ahead(&self) -> Vec<Leg> {
        let gaps = &self.sup.stall.gaps;
        let from = self
            .mission
            .as_ref()
            .filter(|m| matches!(m.state.as_str(), "active" | "paused"))
            .and_then(|m| m.seq);
        legs_of(&self.route.items, self.fc.home_ne())
            .into_iter()
            .filter(|&(f, _, t, _)| !gaps.iter().any(|g| g.seq == f || g.seq == t))
            .filter(|&(_, _, t, _)| from.is_none_or(|s| t >= s))
            .collect()
    }

    /// Check the route now.
    pub(in crate::core) fn check_route(&mut self, now_ms: u64) {
        let (required, margin, sigma) = self.required_clearance(now_ms);
        let legs = self.legs_ahead();
        let checked = check_legs(&self.obstacles, &legs, SAMPLE_STEP_M, required, now_ms);
        let r = &mut self.route;
        r.legs = checked;
        r.required = required;
        r.margin = margin;
        r.sigma = sigma;
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
                    required_m = required,
                    sigma_m = sigma,
                    "pilot_route_check"
                );
            }
        }
    }

    /// Every tick before the supervisor: re-check the route when due, and
    /// work out the cap.
    pub(super) fn confined_tick(&mut self, now_ms: u64) {
        if !self.confined() {
            if self.route.fc_slowed || self.route.cap.is_some() || !self.route.legs.is_empty() {
                self.release_fc_speed();
                self.route.legs.clear();
                self.route.near = false;
                self.route.cap = None;
                self.route.logged = None;
            }
            return;
        }
        let key = (
            self.mission.as_ref().and_then(|m| m.seq),
            self.mission_flying(),
            self.sup.stall.gaps.len(),
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
        }
        self.update_cap(now_ms);
    }

    /// The cap: on while an obstacle is within the required clearance plus
    /// [`NEAR_EXTRA_M`] of the vehicle, and in AUTO along a leg not yet seen.
    fn update_cap(&mut self, now_ms: u64) {
        let airborne = self.fc.armed && !self.fc.landed();
        let (required, _, _) = self.required_clearance(now_ms);
        let clearance = self
            .pos_ne(now_ms)
            .and_then(|p| self.obstacles.grid.clearance(p))
            .filter(|c| c.is_finite());
        let limit = required + NEAR_EXTRA_M;
        let near = airborne
            && clearance.is_some_and(|c| {
                if self.route.near {
                    c <= limit + NEAR_RELEASE_M
                } else {
                    c < limit
                }
            });
        self.route.near = near;
        let seq = self.mission.as_ref().and_then(|m| m.seq);
        let unverified_leg = self.mission_flying()
            && self.route.legs.iter().any(|l| {
                Some(l.to_seq) == seq
                    && (l.state == RouteState::Unverified || self.route.sigma.is_none())
            });
        let cap = if near {
            Some("near")
        } else if unverified_leg {
            Some("unverified")
        } else {
            None
        };
        if cap != self.route.cap {
            tracing::info!(
                target: "ados_pilot",
                on = cap.is_some(),
                reason = cap.or(self.route.cap).unwrap_or_default(),
                clearance_m = clearance,
                required_m = required,
                "pilot_confined_cap"
            );
            self.route.cap = cap;
        }
        self.drive_fc_speed();
    }

    /// Lower the FC's AUTO speed while the cap holds, restore it after.
    fn drive_fc_speed(&mut self) {
        let in_auto = self.fc.custom_mode == copter::AUTO;
        if self.route.fc_slowed && !in_auto {
            // AUTO's next entry starts from the default speed.
            self.route.fc_slowed = false;
        }
        let want = self.route.cap.is_some() && self.mission_flying();
        if want && !self.route.fc_slowed {
            let m = self.tx().command(
                MavCmd::MAV_CMD_DO_CHANGE_SPEED,
                [
                    SPEED_TYPE_GROUND,
                    CONFINED_CAP_MPS,
                    -1.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                ],
            );
            self.mav(m);
            self.route.fc_slowed = true;
        } else if !want && self.route.fc_slowed {
            self.release_fc_speed();
        }
    }

    /// Give the FC its default AUTO speed back.
    fn release_fc_speed(&mut self) {
        if !self.route.fc_slowed {
            return;
        }
        self.route.fc_slowed = false;
        if self.fc.custom_mode != copter::AUTO {
            return;
        }
        match self.default_wp_speed() {
            Some(v) => {
                let m = self.tx().command(
                    MavCmd::MAV_CMD_DO_CHANGE_SPEED,
                    [SPEED_TYPE_GROUND, v, -1.0, 0.0, 0.0, 0.0, 0.0],
                );
                self.mav(m);
            }
            // Unknown: the slow speed stays until AUTO is entered again.
            None => tracing::warn!(target: "ados_pilot", "pilot_confined_speed_default_unknown"),
        }
    }

    /// The FC's default waypoint speed, m/s.
    fn default_wp_speed(&self) -> Option<f32> {
        let p = &self.fc.params;
        p.get("WP_SPD")
            .copied()
            .or_else(|| p.get("WPNAV_SPEED").map(|cms| cms / 100.0))
            .filter(|v| *v > 0.0)
    }

    /// The `route_check` of the published state: only in a confined place
    /// with a mission of at least one leg.
    pub(in crate::core) fn route_check_wire(&self) -> Option<RouteCheck> {
        if self.confined() {
            self.route.wire()
        } else {
            None
        }
    }

    /// The confined cap for setpoints the pilot flies.
    pub(in crate::core) fn confined_near(&self) -> bool {
        self.route.near
    }

    /// The published cap: the confined one while it holds (the pilot's
    /// setpoints near a wall, or the FC's AUTO slowed), else none.
    pub(in crate::core) fn confined_cap_wire(&self, pilot_flown: bool) -> Option<SpeedCap> {
        let on = if pilot_flown {
            self.route.near
        } else {
            self.route.fc_slowed
        };
        on.then(|| SpeedCap::new(REASON_CONFINED_NEAR, CONFINED_CAP_MPS))
    }

    /// While the FC flies the mission: an infeasible leg ahead holds the
    /// vehicle and asks the operator (the stall flow). A leg ending or
    /// starting at the current item counts down to its skip; one further on
    /// holds until the operator answers (skipping the current item would not
    /// remove it).
    pub(super) fn route_hold(&mut self, now_ms: u64) {
        if !self.confined() || !self.mission_flying() || self.owner() != Owner::MissionFc {
            return;
        }
        let Some(leg) = self.route.infeasible().copied() else {
            return;
        };
        let seq = self.mission.as_ref().and_then(|m| m.seq);
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
    /// route is infeasible, naming the leg, the point, its clearance and the
    /// clearance needed.
    pub(in crate::core) fn route_launch_check(&mut self, now_ms: u64) -> Result<(), String> {
        if !self.confined() {
            return Ok(());
        }
        self.check_route(now_ms);
        let Some(leg) = self.route.infeasible().copied() else {
            return Ok(());
        };
        tracing::warn!(
            target: "ados_pilot",
            reason = "route_infeasible",
            leg = %leg_label(leg.from_seq, leg.to_seq),
            ned = ?leg.pinch.map(|(p, _)| p),
            clearance_m = leg.pinch.map(|(_, d)| d),
            required_m = self.route.required,
            "pilot_launch_refused"
        );
        Err("route_infeasible".into())
    }
}
