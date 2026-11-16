//! A scripted flight controller for the setup engine's tests: an ArduPilot
//! parameter server, the calibration command protocols, motor test, mode
//! changes, arming, a one-axis vertical flight model and an AUTOTUNE script.

use std::collections::{BTreeMap, VecDeque};

use ados_protocol::mavlink::ardupilotmega::*;

use super::out::Effect;
use super::step::PilotInputs;
use super::Engine;

mod command;
mod frames;
mod params;
mod sim;
use frames::*;
pub use params::*;

pub const TICK: u64 = 20;

/// What the fake FC does with a magnetometer calibration: fitness per attempt.
#[derive(Debug, Clone)]
pub struct Script {
    pub mag_fitness: VecDeque<f32>,
    /// Seconds into AUTOTUNE before "Success".
    pub autotune_s: u64,
    /// The airspeed zero calibration concludes ACCEPTED (else FAILED).
    pub airspeed_cal_ok: bool,
    /// Refuse the AUTOTUNE mode whatever the current mode.
    pub refuse_autotune: bool,
    /// The FC comes back after a reboot (a SITL that is not restarted
    /// stays silent).
    pub reboot_returns: bool,
    /// SYS_STATUS sensors present, when not derived from the parameters.
    pub sensors_present: Option<MavSysStatusSensor>,
    /// A fixed-wing in AUTOTUNE flies this vertical model instead of holding
    /// its height.
    pub plane_tune: Option<PlaneTune>,
    /// The FC applies a PARAM_SET of these names but its PARAM_VALUE reply is
    /// lost: the write lands while the wizard still has it pending.
    pub unacked_params: Vec<String>,
}

/// Vertical speed (m/s) of a fixed-wing in AUTOTUNE: `-sink`, less
/// `bank_loss` per unit of roll stick, plus `pitch_gain` per unit of
/// nose-up stick and `thr_gain` per unit of throttle above `TRIM_THROTTLE`.
#[derive(Debug, Clone, Copy)]
pub struct PlaneTune {
    pub sink: f32,
    pub bank_loss: f32,
    pub pitch_gain: f32,
    pub thr_gain: f32,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            mag_fitness: VecDeque::from(vec![8.0]),
            autotune_s: 3,
            airspeed_cal_ok: true,
            refuse_autotune: false,
            reboot_returns: true,
            sensors_present: None,
            plane_tune: None,
            unacked_params: Vec::new(),
        }
    }
}

pub struct FakeFc {
    pub mav_type: MavType,
    pub params: BTreeMap<String, f32>,
    pub custom_mode: u32,
    pub armed: bool,
    pub alt: f32,
    pub n: f32,
    pub e: f32,
    pub battery_pct: i8,
    pub battery_v: f32,
    pub wind: Option<(f32, f32)>,
    pub rc: Option<Vec<u16>>,
    /// VFR_HUD ground speed (m/s).
    pub groundspeed: f32,
    pub script: Script,
    /// Every frame the engine sent, in order.
    pub sent: Vec<MavMessage>,
    pub effects: Vec<Effect>,
    pub inputs: PilotInputs,
    queue: VecDeque<MavMessage>,
    /// Raw AUTOPILOT_VERSION frames for the engine's `uid2` path.
    raw: VecDeque<Vec<u8>>,
    rebooting_until: Option<u64>,
    accel_pose: Option<u8>,
    mag: Option<(u64, f32)>,
    autotune_since: Option<u64>,
    autotune_success: bool,
    takeoff_alt: Option<f32>,
    last_hb: u64,
    last_tel: u64,
    manual_z: f32,
    /// MANUAL_CONTROL `x` (pitch, > 0 nose down) and `y` (roll), -1..1.
    manual_xy: (f32, f32),
    /// Height at the last telemetry frame, for VFR_HUD climb.
    tel_alt: f32,
    /// The airspeed zero calibration concludes at this time.
    airspeed_cal_until: Option<u64>,
    /// ArduPlane GUIDED's offboard height from GUIDED_CHANGE_ALTITUDE: once
    /// set it wins over a DO_REPOSITION's height.
    offboard_alt: Option<f32>,
    /// QuadPlane in fixed-wing flight (EXTENDED_SYS_STATE `vtol_state` FW),
    /// and a transition under way `(since, to_fw)`.
    pub vtol_fw: bool,
    vtol_trans: Option<(u64, bool)>,
    /// GUIDED target point `(n, e)`: hovered over (VTOL) or loitered round (fixed-wing).
    target_ne: Option<(f32, f32)>,
    pub reboots: u32,
    /// AUTOPILOT_VERSION `uid` and `uid2` (the extension, sent as a raw frame).
    pub uid: u64,
    pub uid2: Option<[u8; 18]>,
    /// An airframe on its skids that flies SET_ATTITUDE_TARGET in GUIDED
    /// (the supervised lift-off); `None`: such targets are ignored.
    pub airframe: Option<crate::liftoff::sim::Airframe>,
    /// The airframe flies the attitude and position targets (GUIDED, armed).
    af_engaged: bool,
    /// PID_TUNING was asked for with SET_MESSAGE_INTERVAL.
    rate_streams: bool,
}

impl FakeFc {
    pub fn new(mav_type: MavType, params: BTreeMap<String, f32>) -> Self {
        FakeFc {
            mav_type,
            params,
            custom_mode: 0,
            armed: false,
            alt: 0.0,
            n: 0.0,
            e: 0.0,
            battery_pct: 95,
            battery_v: 16.0,
            wind: Some((90.0, 2.0)),
            rc: None,
            groundspeed: 0.0,
            script: Script::default(),
            sent: Vec::new(),
            effects: Vec::new(),
            inputs: PilotInputs::default(),
            queue: VecDeque::new(),
            raw: VecDeque::new(),
            rebooting_until: None,
            accel_pose: None,
            mag: None,
            autotune_since: None,
            autotune_success: false,
            takeoff_alt: None,
            last_hb: 0,
            last_tel: 0,
            manual_z: 0.5,
            manual_xy: (0.0, 0.0),
            tel_alt: 0.0,
            airspeed_cal_until: None,
            offboard_alt: None,
            vtol_fw: false,
            vtol_trans: None,
            target_ne: None,
            reboots: 0,
            uid: 0x00C0_FFEE_1234_5678,
            uid2: None,
            airframe: None,
            af_engaged: false,
            rate_streams: false,
        }
    }

    fn is_copter(&self) -> bool {
        self.mav_type != MavType::MAV_TYPE_FIXED_WING
    }

    fn mode_land(&self) -> u32 {
        if self.is_vtol() {
            20
        } else if self.is_copter() {
            9
        } else {
            11
        }
    }

    fn is_vtol(&self) -> bool {
        self.params.get("Q_ENABLE").is_some_and(|q| *q > 0.0)
    }

    fn mode_autotune(&self) -> u32 {
        if self.is_vtol() {
            22
        } else if self.is_copter() {
            15
        } else {
            8
        }
    }

    fn param_value(&self, name: &str, idx: u16) -> MavMessage {
        MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
            param_value: self.params[name],
            param_count: self.params.len() as u16,
            param_index: idx,
            param_id: name.into(),
            param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
        })
    }

    fn handle(&mut self, m: &MavMessage, now: u64) {
        if self.rebooting_until.is_some() {
            return;
        }
        match m {
            MavMessage::PARAM_REQUEST_LIST(_) => {
                let names: Vec<String> = self.params.keys().cloned().collect();
                for (i, n) in names.iter().enumerate() {
                    let v = self.param_value(n, i as u16);
                    self.queue.push_back(v);
                }
            }
            MavMessage::PARAM_REQUEST_READ(r) => {
                let name = r
                    .param_id
                    .to_str()
                    .unwrap_or("")
                    .trim_end_matches('\0')
                    .to_string();
                if r.param_index >= 0 {
                    if let Some(n) = self.params.keys().nth(r.param_index as usize).cloned() {
                        let v = self.param_value(&n, r.param_index as u16);
                        self.queue.push_back(v);
                    }
                } else if self.params.contains_key(&name) {
                    let i = self.params.keys().position(|k| *k == name).unwrap_or(0) as u16;
                    let v = self.param_value(&name, i);
                    self.queue.push_back(v);
                }
            }
            MavMessage::PARAM_SET(s) => {
                let name = s
                    .param_id
                    .to_str()
                    .unwrap_or("")
                    .trim_end_matches('\0')
                    .to_string();
                if self.params.contains_key(&name) {
                    self.params.insert(name.clone(), s.param_value);
                    if !self.script.unacked_params.contains(&name) {
                        let i = self.params.keys().position(|k| *k == name).unwrap_or(0) as u16;
                        let v = self.param_value(&name, i);
                        self.queue.push_back(v);
                    }
                }
            }
            MavMessage::MANUAL_CONTROL(mc) => {
                self.manual_z = mc.z as f32 / 1000.0;
                self.manual_xy = (mc.x as f32 / 1000.0, mc.y as f32 / 1000.0);
            }
            MavMessage::COMMAND_INT(c) => {
                self.queue
                    .push_back(ack(c.command, MavResult::MAV_RESULT_ACCEPTED));
                if c.command == MavCmd::MAV_CMD_DO_REPOSITION {
                    self.custom_mode = 15;
                    self.takeoff_alt = Some(self.offboard_alt.unwrap_or(c.z));
                    self.target_ne = Some(if c.x == 0 && c.y == 0 {
                        (self.n, self.e)
                    } else {
                        ne_of(c.x, c.y)
                    });
                    // ArduPlane: a GUIDED point restarts navigation as fixed-wing;
                    // with Q_GUIDED_MODE 0 nothing hands it back to the VTOL
                    // controller, so a hovering QuadPlane transitions.
                    if self.is_vtol()
                        && self.alt > 1.0
                        && !self.vtol_fw
                        && self.vtol_trans.is_none()
                        && self.params.get("Q_GUIDED_MODE") == Some(&0.0)
                    {
                        self.vtol_trans = Some((now, true));
                    }
                }
                if c.command == MavCmd::MAV_CMD_DO_CHANGE_ALTITUDE {
                    // The GUIDED target's height only (relative frame): the point stays.
                    self.takeoff_alt = Some(c.param1);
                }
                if c.command == MavCmd::MAV_CMD_GUIDED_CHANGE_ALTITUDE {
                    self.offboard_alt = Some(c.z);
                    self.takeoff_alt = Some(c.z);
                }
            }
            MavMessage::COMMAND_LONG(c) => self.command(c, now),
            MavMessage::SET_ATTITUDE_TARGET(t) => {
                if let Some(af) = self.airframe.as_mut() {
                    let q = t.q;
                    let roll = (2.0 * (q[0] * q[1] + q[2] * q[3]))
                        .atan2(1.0 - 2.0 * (q[1] * q[1] + q[2] * q[2]));
                    let pitch = (2.0 * (q[0] * q[2] - q[3] * q[1])).clamp(-1.0, 1.0).asin();
                    af.target = Some(([roll, pitch, af.att[2]], t.thrust));
                    self.af_engaged = true;
                }
            }
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) if self.af_engaged => {
                if let Some(af) = self.airframe.as_mut() {
                    af.goal_h = Some(-s.z);
                }
            }
            _ => {}
        }
    }

    /// The FC sends a STATUSTEXT.
    pub fn push_text(&mut self, s: &str) {
        self.queue.push_back(text(s));
    }

    /// The FC changes a parameter on its own (an autotune saving an axis) and
    /// says so with a PARAM_VALUE.
    pub fn set_param_notify(&mut self, name: &str, v: f32) {
        self.params.insert(name.to_string(), v);
        let i = self.params.keys().position(|k| k == name).unwrap_or(0) as u16;
        let m = self.param_value(name, i);
        self.queue.push_back(m);
    }

    /// One 20 ms step: FC frames into the engine, engine tick, engine
    /// frames into the FC.
    pub fn step(&mut self, e: &mut Engine, now: u64) {
        if let Some(until) = self.rebooting_until {
            if now >= until {
                self.rebooting_until = None;
                self.last_hb = 0;
            } else {
                e.tick(now);
                self.drain(e, now);
                return;
            }
        }
        self.physics(now);
        self.telemetry(now);
        while let Some(f) = self.raw.pop_front() {
            e.on_version_frame(&f);
        }
        while let Some(m) = self.queue.pop_front() {
            e.on_fc(1, 1, &m, now);
        }
        e.set_inputs(self.inputs.clone());
        e.tick(now);
        self.drain(e, now);
    }

    fn drain(&mut self, e: &mut Engine, now: u64) {
        let msgs = e.outbox().take_msgs();
        for eff in e.outbox().take_effects() {
            match eff {
                Effect::EnterTuning => e.tuning_result(true, now),
                Effect::ExitTuning { .. } => e.set_pilot_tuning(false),
                _ => {}
            }
            self.effects.push(eff);
        }
        for m in msgs {
            self.handle(&m, now);
            self.sent.push(m);
        }
    }
}
