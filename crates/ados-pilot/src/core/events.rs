//! FC frames: arm/disarm, FC mode changes, adoption of an external GUIDED.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavResult};
use ados_protocol::pilot::{ControlMode, Mission, Owner, CLIENT_MODE_TOPIC};
use serde_json::json;

use super::Pilot;
use crate::flows::banner;

/// FC parameters the pilot reads.
const EXTRA_PARAMS: &[&str] = &[
    "Q_ENABLE",
    "FLTMODE_CH",
    "BATT_LOW_MAH",
    "BATT_CAPACITY",
    "TECS_CLMB_MAX",
    "TECS_SINK_MAX",
    "AIRSPEED_MIN",
    "AIRSPEED_MAX",
    "AIRSPEED_CRUISE",
    "TKOFF_ALT",
    // The landing speed: `LAND_SPD_MS` (m/s), `LAND_SPEED` (cm/s) before it.
    "LAND_SPD_MS",
    "LAND_SPEED",
    // The EKF's horizontal position source per source set (3 = GPS): the
    // GNSS-denied speed cap.
    "EK3_SRC1_POSXY",
    "EK3_SRC2_POSXY",
    "EK3_SRC3_POSXY",
];
/// Re-request missing parameters this often.
const PARAM_RETRY_MS: u64 = 15_000;

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
            "plugin.world-engine.scan" => {
                let Some(pos) = self.pos_ne(now_ms) else {
                    return;
                };
                let att = self.fc.fresh_attitude(now_ms).unwrap_or_default();
                let level = crate::obstacles::Level {
                    roll: att.roll,
                    pitch: att.pitch,
                    yaw: att.yaw,
                    agl: self.agl(now_ms),
                };
                self.obstacles.ingest_scan(payload, level, pos, now_ms);
            }
            "plugin.world-engine.occupancy" => self.obstacles.ingest_occupancy(payload),
            "plugin.world-engine.pose" => {
                let pos = self.pos_ne(now_ms);
                self.obstacles.ingest_pose(payload, pos);
            }
            "mission" => {
                let m = <Mission as serde::Deserialize>::deserialize(payload).ok();
                self.on_mission(m, now_ms);
            }
            "nav.state" => {
                // The FC keeps a source set until told otherwise: a report
                // naming none leaves the last one.
                let set = payload
                    .get("active_set")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| u8::try_from(v).ok());
                if let Some(set) = set {
                    self.nav_set = set;
                }
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
        if let MavMessage::COMMAND_ACK(a) = msg {
            if sysid == self.fc.sysid && compid == self.fc.compid {
                if a.command == MavCmd::MAV_CMD_DO_REPOSITION
                    && a.result == MavResult::MAV_RESULT_ACCEPTED
                {
                    self.on_fc_reposition(now_ms);
                }
                self.take_ack(a, now_ms);
            }
            return;
        }
        if !matches!(msg, MavMessage::HEARTBEAT(_)) || self.fc.heartbeat_ms != now_ms {
            return;
        }
        if first || now_ms.saturating_sub(self.params_ms) >= PARAM_RETRY_MS {
            self.request_params(now_ms);
        }
        if was_armed && !self.fc.armed {
            self.on_disarmed();
            return;
        }
        if !was_armed && self.fc.armed {
            self.touchdown = Default::default();
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

    fn request_params(&mut self, now_ms: u64) {
        self.params_ms = now_ms;
        let Some(fw) = self.fc.firmware else { return };
        let class = self.fc.vehicle_class();
        let names: Vec<&str> = crate::fc_profile::all_names(fw, class)
            .chain(EXTRA_PARAMS.iter().copied())
            .collect();
        for n in names {
            if !self.fc.params.contains_key(n) {
                let m = self.tx().param_request(n);
                self.mav(m);
            }
        }
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
        self.hold_reason = None;
        self.fc_failsafe = false;
        self.failsafe_cause = None;
        if self
            .launch
            .as_ref()
            .is_some_and(|l| l.step != crate::flows::LaunchStep::Arm)
        {
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
        if self.owner() == Owner::Tuning {
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
        } else {
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
