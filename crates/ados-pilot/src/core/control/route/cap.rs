//! The confined cap: 0.5 m/s while an obstacle is within the required
//! clearance plus [`NEAR_EXTRA_M`] of the vehicle, and in AUTO while an
//! infeasible leg ahead waits for its hold, or along a leg not yet seen or
//! judged (the leg flown now, or any within the stopping reach). The pilot's
//! own setpoints are capped where they are made
//! ([`super::super::super::nav_cap`]); the FC's flights are slowed here.
//!
//! The FC's speed is lowered with MAV_CMD_DO_CHANGE_SPEED in AUTO (the
//! mission) and in RTL and SMART_RTL (a return; ArduCopter's ModeSmartRTL
//! takes the RTL mode's speed change). ArduCopter resets the speed on every
//! mode entry (`wp_and_spline_init` in the mode's init), so a cap is re-sent
//! after a mode change and needs no restore when the mode ends. It is also
//! re-sent whenever the mission moves to another item while capped: a
//! DO_CHANGE_SPEED in the mission runs as the FC reaches the next item and
//! would undo the cap. When the cap ends the FC gets back the speed the
//! mission itself set at or before the current item (its last horizontal
//! DO_CHANGE_SPEED), else its default: `WP_SPD` m/s (`WPNAV_SPEED` cm/s on
//! older firmware), or for RTL `RTL_SPEED_MS` (`RTL_SPEED` cm/s) when set.
//! ArduCopter denies a speed of zero or less, so there is no "back to
//! default" value (`handle_MAV_CMD_DO_CHANGE_SPEED` in
//! `ArduCopter/GCS_MAVLink_Copter.cpp`).

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{RouteState, SpeedCap};

use super::super::super::Pilot;
use super::super::supervisor::Resume;
use crate::modes::copter;

/// `speed_cap.reason` of the confined cap.
pub const REASON_CONFINED_NEAR: &str = "confined-near";
/// The cap holds while an obstacle is within the required clearance plus this.
pub const NEAR_EXTRA_M: f32 = 1.5;
/// The confined cap, m/s.
pub const CONFINED_CAP_MPS: f32 = 0.5;
/// The cap ends only this much further out than it starts, metres.
const NEAR_RELEASE_M: f32 = 0.25;
/// DO_CHANGE_SPEED speed type: ground speed.
const SPEED_TYPE_GROUND: f32 = 1.0;

/// The FC flight the cap lowered: its mode and, in AUTO, the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slowed {
    mode: u32,
    seq: Option<u32>,
}

impl Pilot {
    /// A return the FC flies under the supervisor, not held.
    fn return_flying(&self) -> bool {
        self.supervised() == Some(Resume::Return) && self.sup.hold.is_none()
    }

    /// The cap: on while an obstacle is within the required clearance plus
    /// [`NEAR_EXTRA_M`] of the vehicle, and in AUTO while an infeasible leg
    /// ahead waits for its hold or along a leg not yet seen or judged.
    pub(super) fn update_cap(&mut self, now_ms: u64) {
        let airborne = self.fc.armed && !self.fc.landed();
        let required = self.required_clearance(now_ms).required;
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
        let flying = self.mission_flying();
        // An infeasible leg ahead the hold has not confirmed yet: the FC
        // flies no faster than the hold's reach assumes.
        let pending = flying && self.route.infeasible().is_some();
        // The leg flown now, or any leg within the stopping reach, not fully
        // judged: unseen, off the scan's slice, or not checked at all (an
        // item with no position, past the end of the feed, or σ or the
        // position not vouched for). Until the mission feed names the item
        // there is no leg to judge.
        let unverified_leg = flying
            && seq.is_some()
            && (self
                .route
                .legs
                .iter()
                .find(|l| Some(l.to_seq) == seq)
                .is_none_or(|l| l.state == RouteState::Unverified || !self.route.trusted)
                || self
                    .route
                    .legs
                    .iter()
                    .any(|l| l.state == RouteState::Unverified && self.leg_in_reach(l, now_ms)));
        let cap = if near {
            Some("near")
        } else if pending {
            Some("infeasible")
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

    /// Lower the FC's speed while the cap holds over a mission or a return
    /// it flies, re-send it after a mode change or a new mission item, and
    /// restore the speed after.
    fn drive_fc_speed(&mut self) {
        let mode = self.fc.custom_mode;
        let seq = self.mission.as_ref().and_then(|m| m.seq);
        if self.route.fc_slowed.is_some_and(|s| s.mode != mode) {
            // The new mode started from its own speed.
            self.route.fc_slowed = None;
        }
        let flying = self.mission_flying() || self.return_flying();
        let want = self.route.cap.is_some() && flying;
        let item = (mode == copter::AUTO).then_some(seq).flatten();
        if want && self.route.fc_slowed.is_none_or(|s| s.seq != item) {
            self.send_speed(CONFINED_CAP_MPS);
            self.route.fc_slowed = Some(Slowed { mode, seq: item });
        } else if !want && self.route.fc_slowed.is_some() {
            self.release_fc_speed();
        }
    }

    fn send_speed(&mut self, v: f32) {
        let m = self.tx().command(
            MavCmd::MAV_CMD_DO_CHANGE_SPEED,
            [SPEED_TYPE_GROUND, v, -1.0, 0.0, 0.0, 0.0, 0.0],
        );
        self.mav(m);
    }

    /// Give the FC back the speed it would fly without the cap.
    pub(super) fn release_fc_speed(&mut self) {
        let Some(s) = self.route.fc_slowed.take() else {
            return;
        };
        if self.fc.custom_mode != s.mode {
            return;
        }
        let v = match s.mode {
            copter::AUTO => self.mission_speed(s.seq),
            copter::RTL => self.rtl_speed(),
            _ => self.default_wp_speed(),
        };
        match v {
            Some(v) => self.send_speed(v),
            // Unknown: the slow speed stays until the mode is entered again.
            None => tracing::warn!(target: "ados_pilot", "pilot_confined_speed_default_unknown"),
        }
    }

    /// The speed the mission set at or before item `seq`, else the default.
    fn mission_speed(&self, seq: Option<u32>) -> Option<f32> {
        seq.and_then(|s| {
            self.route
                .feed
                .speeds
                .iter()
                .filter(|(at, _)| *at <= s)
                .max_by_key(|(at, _)| *at)
                .map(|(_, v)| *v)
        })
        .or_else(|| self.default_wp_speed())
    }

    /// RTL's own speed when set, else the default.
    fn rtl_speed(&self) -> Option<f32> {
        let p = &self.fc.params;
        p.get("RTL_SPEED_MS")
            .copied()
            .or_else(|| p.get("RTL_SPEED").map(|cms| cms / 100.0))
            .filter(|v| *v > 0.0)
            .or_else(|| self.default_wp_speed())
    }

    /// The FC's default waypoint speed, m/s.
    fn default_wp_speed(&self) -> Option<f32> {
        let p = &self.fc.params;
        p.get("WP_SPD")
            .copied()
            .or_else(|| p.get("WPNAV_SPEED").map(|cms| cms / 100.0))
            .filter(|v| *v > 0.0)
    }

    /// The confined cap for setpoints the pilot flies.
    pub(in crate::core) fn confined_near(&self) -> bool {
        self.route.near
    }

    /// The published cap: the confined one while it holds (the pilot's
    /// setpoints near a wall, or the FC's flight slowed), else none.
    pub(in crate::core) fn confined_cap_wire(&self, pilot_flown: bool) -> Option<SpeedCap> {
        let on = if pilot_flown {
            self.route.near
        } else {
            self.route.fc_slowed.is_some()
        };
        on.then(|| SpeedCap::new(REASON_CONFINED_NEAR, CONFINED_CAP_MPS))
    }
}
