//! FC frames: arm/disarm, FC mode changes, adoption of an external GUIDED.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavResult};
use ados_protocol::pilot::{ControlMode, Mission, Owner, CLIENT_MODE_TOPIC};
use serde_json::json;

use super::Pilot;
use crate::events::{
    TOPIC_LIDAR, TOPIC_MAP_STATUS, TOPIC_NAV_STATE, TOPIC_OCCUPANCY, TOPIC_POSE, TOPIC_SCAN,
};
use crate::flows::banner;

mod params;

/// A ground station's mode command toward the FC, as the router saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMode {
    /// `SET_MODE` or `DO_SET_MODE` with this custom mode.
    Custom(u32),
    /// `NAV_RETURN_TO_LAUNCH`.
    Rtl,
    /// `NAV_LAND`.
    Land,
}

impl ClientMode {
    /// The router's `client.mode` payload: `{"mode": <custom mode>}`,
    /// `{"mode": "rtl"}` or `{"mode": "land"}`.
    pub fn parse(payload: &serde_json::Value) -> Option<Self> {
        let m = payload.get("mode")?;
        if let Some(n) = m.as_u64() {
            return u32::try_from(n).ok().map(Self::Custom);
        }
        match m.as_str()? {
            "rtl" => Some(Self::Rtl),
            "land" => Some(Self::Land),
            _ => None,
        }
    }
}

impl Pilot {
    /// Topics from the plugin event export, the router's state socket
    /// (`mission`) or a `feed` relay.
    pub fn on_feed(&mut self, topic: &str, payload: &serde_json::Value, now_ms: u64) {
        match topic {
            TOPIC_SCAN => {
                let returns = crate::obstacles::scan_returns(payload);
                self.lidar.on_scan(returns, now_ms);
                let Some(ned) = self.pos(now_ms) else {
                    return;
                };
                let att = self.fc.fresh_attitude(now_ms).unwrap_or_default();
                let tilt = [att.roll, att.pitch];
                if returns == 0 && self.empty_scan_unbelievable(ned, tilt) {
                    // Not open air: keep the last scan's obstacles and let
                    // them age to stale rather than read this one as free.
                    return;
                }
                let level = crate::obstacles::Level {
                    roll: att.roll,
                    pitch: att.pitch,
                    yaw: att.yaw,
                    agl: self.agl(now_ms),
                };
                // The widest of the brake's per-direction trust bounds on a
                // standing obstacle's closing speed (`scan_closing_bound`, a
                // direction along the FC's velocity now); the widest bound
                // without a finite one.
                let closing_max = self
                    .fc
                    .fresh_local(now_ms)
                    .map(|l| super::control::scan_closing_bound_max(l.vn.hypot(l.ve)))
                    .filter(|v| v.is_finite())
                    .unwrap_or(crate::obstacles::SCAN_CLOSING_MAX_MPS);
                self.obstacles
                    .ingest_scan(payload, level, [ned[0], ned[1]], closing_max, now_ms);
                if self.obstacles.scan_age_ms(now_ms) == Some(0) {
                    match self.obstacles.nearest_surface() {
                        Some((range_m, ne, extent_m)) => {
                            self.lidar.on_surface(ned, tilt, range_m, ne, extent_m)
                        }
                        None => self.lidar.on_no_surface(),
                    }
                }
            }
            TOPIC_LIDAR => self.lidar.on_status(payload, now_ms),
            TOPIC_MAP_STATUS => self.nav_imu.on_status(payload, now_ms),
            TOPIC_OCCUPANCY => self.obstacles.ingest_occupancy(payload),
            TOPIC_POSE => {
                let pos = self.pos_ne(now_ms);
                self.obstacles.ingest_pose(payload, pos, now_ms);
            }
            "mission" => {
                let m = <Mission as serde::Deserialize>::deserialize(payload).ok();
                self.route.set_feed(super::control::route_of(payload));
                self.on_mission(m, now_ms);
            }
            TOPIC_NAV_STATE => {
                // The FC keeps a source set until told otherwise: a report
                // naming none leaves the last one.
                let set = payload
                    .get("active_set")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| u8::try_from(v).ok());
                if let Some(set) = set {
                    if set != self.nav_set && self.fc.armed {
                        self.fc
                            .pos_reset
                            .cue(crate::fc::pos_reset::ResetCause::SourceSet, now_ms);
                        // It may name a step already seen.
                        self.check_pos_reset(now_ms);
                    }
                    self.nav_set = set;
                }
                if let Some(n) = super::position_check::ExtNav::parse(payload, now_ms) {
                    self.extnav = Some(n);
                }
                // Whether the lidar pose is flown on: the `nav_imu` and
                // `nav_heading` severity.
                self.nav_imu.on_nav_state(payload);
            }
            CLIENT_MODE_TOPIC => {
                if let Some(c) = ClientMode::parse(payload) {
                    self.client_mode = Some((c, now_ms));
                }
            }
            _ => {}
        }
    }

    /// Fold one frame from the FC in.
    pub fn on_fc(&mut self, sysid: u8, compid: u8, msg: &MavMessage, now_ms: u64) {
        let was_armed = self.fc.armed;
        let first = self.fc.heartbeat_ms == 0;
        let changed = self.fc.update(sysid, compid, msg, now_ms);
        self.check_pos_reset(now_ms);
        if matches!(msg, MavMessage::OPTICAL_FLOW(_))
            && sysid == self.fc.sysid
            && compid == self.fc.compid
        {
            self.on_flow(now_ms);
        }
        if let MavMessage::COMMAND_ACK(a) = msg {
            if sysid == self.fc.sysid && compid == self.fc.compid {
                if a.command == MavCmd::MAV_CMD_DO_REPOSITION
                    && a.result == MavResult::MAV_RESULT_ACCEPTED
                {
                    self.on_fc_reposition(now_ms);
                }
                if a.command == MavCmd::MAV_CMD_DO_SET_MODE
                    && a.result != MavResult::MAV_RESULT_ACCEPTED
                {
                    self.mode_refused(now_ms);
                }
                self.take_ack(a, now_ms);
            }
            return;
        }
        if !matches!(msg, MavMessage::HEARTBEAT(_)) || self.fc.heartbeat_ms != now_ms {
            return;
        }
        self.request_params(now_ms);
        if was_armed && !self.fc.armed {
            self.on_disarmed();
            return;
        }
        if !was_armed && self.fc.armed {
            self.touchdown = Default::default();
            self.sup.stall.new_flight();
            self.lidar.new_flight();
            self.failsafe
                .instances
                .armed(now_ms, crate::link_watch::ARM_LIVE_MS);
        }
        if !was_armed && self.fc.armed && self.owner() == Owner::Idle && !self.in_guided() {
            self.set_owner(Owner::FcMode, "fc-mode");
        }
        if let Some(prev) = changed {
            if !first || self.fc.armed {
                self.on_mode_change(prev, now_ms);
            }
        }
        self.check_take(now_ms, false);
    }

    fn on_disarmed(&mut self) {
        self.take_disarmed();
        // The setup engine holds `tuning` across the whole air wizard, landings
        // between its flight steps included, and hands it back itself.
        if self.owner() != Owner::Tuning {
            self.set_owner(Owner::Idle, "disarmed");
        }
        self.failsafe.reset_link();
        self.landing = false;
        self.touchdown = Default::default();
        self.goto = None;
        self.swarm = None;
        self.mission_paused = false;
        self.rth_deadline_ms = None;
        self.low_batt_cancelled = false;
        self.low_batt_action = None;
        self.hold_reason = None;
        self.fc_failsafe = false;
        self.failsafe_cause = None;
        self.end_supervision();
        self.drop_return();
        // A lift-off that stopped on the ground disarmed it on purpose: the
        // launch stays a few seconds more so every surface shows why.
        if self.launch.as_ref().is_some_and(|l| {
            l.step != crate::flows::LaunchStep::Arm
                && !l
                    .supervised
                    .as_ref()
                    .and_then(|s| s.liftoff.as_ref())
                    .is_some_and(|lo| lo.stopped().is_some())
        }) {
            self.launch = None;
        }
    }

    /// The FC accepted a reposition. Under `ados` the router takes those to
    /// the pilot as a goto, so this is a ground station flying the FC
    /// directly, outside `ados`. It is already in GUIDED, so no mode change
    /// tells the pilot: whoever flies through the pilot (its hold, a skill,
    /// a goto, the swarm) yields here, and nothing is streamed over the
    /// FC's target from now on.
    fn on_fc_reposition(&mut self, now_ms: u64) {
        if self.control_mode() == ControlMode::Ados
            || !self.fc.armed
            || !self.in_guided()
            || !matches!(
                self.owner(),
                Owner::Pilot | Owner::Skill | Owner::Goto | Owner::Swarm
            )
        {
            return;
        }
        self.set_owner(Owner::FcMode, "fc-reposition");
        self.landing = false;
        self.goto = None;
        self.swarm = None;
        let name = self.family().mode_name(self.fc.custom_mode);
        self.banner = Some(banner(
            "pilot.banner.fc_control",
            &[("mode", json!(name))],
            now_ms,
        ));
    }

    /// A ground station asked the FC for `mode` just before it changed.
    fn mode_is_clients(&self, mode: u32, now_ms: u64) -> bool {
        let fam = self.family();
        self.client_mode.is_some_and(|(c, t)| {
            now_ms.saturating_sub(t) < super::CLIENT_MODE_WINDOW_MS
                && match c {
                    ClientMode::Custom(m) => m == mode,
                    ClientMode::Rtl => fam.is_rtl(mode),
                    ClientMode::Land => fam.is_land(mode),
                }
        })
    }

    /// The stable code for who changed the FC's mode when no operator did
    /// (`pilot.banner.cause.<code>`): `rc_or_failsafe` while the FC reports
    /// RC input, `fc` otherwise.
    fn fc_mode_cause(&self) -> &'static str {
        if self.fc.rc.is_some() {
            "rc_or_failsafe"
        } else {
            "fc"
        }
    }

    fn on_mode_change(&mut self, prev: u32, now_ms: u64) {
        let fam = self.family();
        let mode = self.fc.custom_mode;
        self.fc_failsafe = false;
        self.failsafe_cause = None;
        // The supervisor's own BRAKE on the way to its GUIDED hold: the
        // pilot keeps the vehicle.
        if mode == crate::modes::copter::BRAKE
            && self.sup.hold.is_some()
            && self.mode_is_ours(mode, now_ms)
        {
            return;
        }
        if self.owner() == Owner::Tuning {
            // The setup engine flies its own modes. A return, land or brake it
            // did not command is still the FC's failsafe: recorded here so
            // leaving the wizard cannot fly over it.
            if self.fc.armed {
                let operator =
                    self.mode_is_ours(mode, now_ms) || self.mode_is_clients(mode, now_ms);
                self.fc_failsafe = !operator && fam.is_failsafe_mode(mode);
                self.failsafe_cause = self.fc_failsafe.then(|| self.fc_mode_cause());
            }
            return;
        }
        if fam.is_autotune(mode) {
            return;
        }
        if mode == fam.guided() {
            let ours = self.mode_is_ours(mode, now_ms);
            if !ours && self.control_mode() != ControlMode::Ados {
                // `fc_manual` / `elrs`: a ground station's GUIDED (a reposition,
                // a guided goto) is the FC's to fly. Adopting it would hold the
                // vehicle where it is instead of flying to the point.
                if self.fc.armed && self.owner() != Owner::FcMode {
                    self.set_owner(Owner::FcMode, "fc-mode");
                }
                return;
            }
            if self.fc.armed
                && !ours
                && matches!(self.owner(), Owner::FcMode | Owner::Idle | Owner::MissionFc)
            {
                self.adopt(now_ms);
            }
            return;
        }
        // An operator's change: the pilot's own, or a ground station's mode
        // command (Mode sheet, RTH, land) the router saw.
        let operator = self.mode_is_ours(mode, now_ms) || self.mode_is_clients(mode, now_ms);
        // Copter AUTO_RTL nobody commanded is a failsafe action (Auto
        // DO_LAND_START or RTL), not a mission the operator flies: the FC
        // owns it below, so Take control and the stick preempt cannot leave it.
        if fam.is_auto(mode) && (operator || !fam.is_failsafe_mode(mode)) {
            if self.owner() != Owner::MissionFc {
                self.set_owner(Owner::MissionFc, "fc-mode");
            }
            self.mission_paused = false;
            return;
        }
        if !self.fc.armed {
            return;
        }
        // Any other mode: the FC flies.
        // A return, land or brake nobody commanded: an RC switch or an FC
        // failsafe (radio, battery, EKF).
        self.fc_failsafe = !operator && fam.is_failsafe_mode(mode);
        self.failsafe_cause = self.fc_failsafe.then(|| self.fc_mode_cause());
        // A failsafe return the supervisor held and resumed is still the FC's
        // failsafe.
        if let Some(cause) = self
            .sup
            .restore_failsafe
            .take()
            .filter(|_| fam.is_rtl(mode))
        {
            self.fc_failsafe = true;
            self.failsafe_cause = Some(cause);
        }
        if self.owner() != Owner::FcMode {
            self.set_owner(Owner::FcMode, "fc-mode");
        }
        if self.launch.is_some()
            && !(fam == crate::modes::Family::Plane && mode == crate::modes::plane::TAKEOFF)
        {
            self.launch = None;
        }
        if !operator && prev != mode {
            let name = fam.mode_name(mode);
            let cause = self.fc_mode_cause();
            self.banner = Some(banner(
                "pilot.banner.fc_mode_change",
                &[("mode", json!(name)), ("cause", json!(cause))],
                now_ms,
            ));
        } else if !(self.low_batt_action.is_some() && self.mode_is_ours(mode, now_ms))
            && !self
                .banner
                .as_ref()
                .is_some_and(|b| b.text_key == "pilot.banner.return_land_fallback")
        {
            // The pilot's own low-battery action, or its landing after a
            // refused return, keeps its banner: it says why the FC changed
            // mode.
            let name = fam.mode_name(mode);
            self.banner = Some(banner(
                "pilot.banner.fc_control",
                &[("mode", json!(name))],
                now_ms,
            ));
        }
    }

    /// Someone other than the pilot put the armed FC in GUIDED: take over.
    fn adopt(&mut self, now_ms: u64) {
        let source = if self.rc_at_ados_position() {
            "rc"
        } else {
            "gcs"
        };
        self.set_owner(Owner::Pilot, "adopted");
        self.engage_pilot(now_ms);
        self.banner = Some(banner(
            "pilot.banner.ados_engaged",
            &[("source", json!(source))],
            now_ms,
        ));
    }

    /// True when the flight-mode channel sits at the slot labelled ADOS.
    fn rc_at_ados_position(&self) -> bool {
        let slot = self.settings.control.rc_ados_position;
        if slot == 0 {
            return false;
        }
        let ch = self.fc.params.get("FLTMODE_CH").copied().unwrap_or(5.0) as usize;
        let Some(rc) = self.fc.rc else { return false };
        let Some(&pwm) = rc.v.get(ch.saturating_sub(1)) else {
            return false;
        };
        flight_mode_slot(pwm) == slot
    }
}

/// ArduPilot's flight-mode slot for a channel PWM.
pub fn flight_mode_slot(pwm: u16) -> u8 {
    match pwm {
        0..=1230 => 1,
        1231..=1360 => 2,
        1361..=1490 => 3,
        1491..=1620 => 4,
        1621..=1749 => 5,
        _ => 6,
    }
}
