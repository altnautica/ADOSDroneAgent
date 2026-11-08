//! A scripted flight controller for the setup engine's tests: an ArduPilot
//! parameter server, the calibration command protocols, motor test, mode
//! changes, arming, a one-axis vertical flight model and an AUTOTUNE script.

use std::collections::{BTreeMap, VecDeque};

use ados_protocol::mavlink::ardupilotmega::*;

use super::out::Effect;
use super::step::PilotInputs;
use super::Engine;

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
}

pub fn ap_copter_params() -> BTreeMap<String, f32> {
    let mut p = BTreeMap::new();
    for (k, v) in [
        ("FRAME_CLASS", 1.0),
        ("FRAME_TYPE", 1.0),
        ("BATT_MONITOR", 4.0),
        ("BATT_CAPACITY", 3300.0),
        ("BATT_VOLT_MULT", 10.1),
        ("ACRO_YAW_P", 4.5),
        ("ATC_ACCEL_P_MAX", 110_000.0),
        ("ATC_ACCEL_R_MAX", 110_000.0),
        ("ATC_ACCEL_Y_MAX", 27_000.0),
        ("ATC_RAT_PIT_FLTD", 20.0),
        ("ATC_RAT_PIT_FLTE", 0.0),
        ("ATC_RAT_PIT_FLTT", 20.0),
        ("ATC_RAT_RLL_FLTD", 20.0),
        ("ATC_RAT_RLL_FLTE", 0.0),
        ("ATC_RAT_RLL_FLTT", 20.0),
        ("ATC_RAT_YAW_FLTD", 0.0),
        ("ATC_RAT_YAW_FLTE", 2.0),
        ("ATC_RAT_YAW_FLTT", 20.0),
        ("ATC_THR_MIX_MAN", 0.1),
        ("ATC_RAT_RLL_P", 0.135),
        ("ATC_RAT_RLL_I", 0.135),
        ("ATC_RAT_RLL_D", 0.0036),
        ("ATC_RAT_PIT_P", 0.135),
        ("ATC_RAT_PIT_I", 0.135),
        ("ATC_RAT_PIT_D", 0.0036),
        ("ATC_RAT_YAW_P", 0.18),
        ("ATC_RAT_YAW_I", 0.018),
        ("ATC_ANG_RLL_P", 4.5),
        ("ATC_ANG_PIT_P", 4.5),
        ("ATC_ANG_YAW_P", 4.5),
        ("INS_ACCEL_FILTER", 20.0),
        ("INS_GYRO_FILTER", 20.0),
        ("INS_HNTCH_ENABLE", 0.0),
        ("MOT_THST_EXPO", 0.65),
        ("MOT_THST_HOVER", 0.35),
        ("MOT_HOVER_LEARN", 2.0),
        ("MOT_BAT_VOLT_MAX", 0.0),
        ("MOT_BAT_VOLT_MIN", 0.0),
        ("MOT_PWM_TYPE", 0.0),
        ("BATT_ARM_VOLT", 0.0),
        ("BATT_CRT_VOLT", 0.0),
        ("BATT_LOW_VOLT", 10.5),
        ("SERVO1_FUNCTION", 33.0),
        ("SERVO2_FUNCTION", 34.0),
        ("SERVO3_FUNCTION", 35.0),
        ("SERVO4_FUNCTION", 36.0),
        ("SERVO_BLH_RVMASK", 0.0),
        ("SERVO_BLH_AUTO", 0.0),
        ("SERVO_BLH_MASK", 0.0),
        ("FLTMODE_CH", 5.0),
        ("FLTMODE1", 0.0),
        ("FLTMODE2", 2.0),
        ("FLTMODE3", 5.0),
        ("FLTMODE4", 5.0),
        ("FLTMODE5", 6.0),
        ("FLTMODE6", 9.0),
        ("MAV_GCS_SYSID", 255.0),
        ("FS_GCS_ENABLE", 0.0),
        ("FS_GCS_TIMEOUT", 5.0),
        ("RC_OVERRIDE_TIME", 3.0),
        ("BATT_FS_LOW_ACT", 0.0),
        ("BATT_FS_CRT_ACT", 0.0),
        ("AUTOTUNE_AXES", 7.0),
        ("AUTOTUNE_AGGR", 0.1),
        ("RC1_MIN", 1100.0),
        ("RC1_MAX", 1900.0),
        ("RC1_TRIM", 1500.0),
    ] {
        p.insert(k.to_string(), v);
    }
    p
}

pub fn ap_plane_params() -> BTreeMap<String, f32> {
    let mut p = BTreeMap::new();
    for (k, v) in [
        ("SERVO1_FUNCTION", 4.0),
        ("SERVO2_FUNCTION", 19.0),
        ("SERVO3_FUNCTION", 70.0),
        ("SERVO4_FUNCTION", 21.0),
        ("SERVO1_REVERSED", 0.0),
        ("SERVO2_REVERSED", 0.0),
        ("SERVO4_REVERSED", 0.0),
        ("SERVO_AUTO_TRIM", 0.0),
        ("Q_ENABLE", 0.0),
        ("ARSPD_TYPE", 0.0),
        ("BATT_MONITOR", 4.0),
        ("BATT_CAPACITY", 5000.0),
        ("BATT_VOLT_MULT", 10.1),
        ("MAV_GCS_SYSID", 255.0),
        ("FS_GCS_ENABL", 0.0),
        ("FS_LONG_TIMEOUT", 5.0),
        ("FS_LONG_ACTN", 0.0),
        ("BATT_FS_LOW_ACT", 0.0),
        ("RC_OVERRIDE_TIME", 3.0),
        ("RLL_RATE_P", 0.08),
        ("PTCH_RATE_P", 0.08),
    ] {
        p.insert(k.to_string(), v);
    }
    p
}

/// Where the fake FC's local origin is, degE7.
const LAT0: i32 = 474_000_000;
const LON0: i32 = 85_000_000;
/// Metres per degE7 of latitude, and the longitude scale at LAT0.
const M_PER_E7: f32 = 0.011_131_95;
const COS_LAT0: f32 = 0.676_9;

fn latlon_of(n: f32, e: f32) -> (i32, i32) {
    (LAT0 + (n / M_PER_E7).round() as i32, LON0 + (e / (M_PER_E7 * COS_LAT0)).round() as i32)
}

fn ne_of(lat: i32, lon: i32) -> (f32, f32) {
    ((lat - LAT0) as f32 * M_PER_E7, (lon - LON0) as f32 * M_PER_E7 * COS_LAT0)
}

fn text(s: &str) -> MavMessage {
    MavMessage::STATUSTEXT(STATUSTEXT_DATA { severity: MavSeverity::MAV_SEVERITY_INFO, text: s.into() })
}

/// A MAVLink 2 AUTOPILOT_VERSION frame carrying the `uid2` extension (the
/// fixed fields at their wire offsets, CRC not checked by the reader).
fn version_frame(uid: u64, uid2: [u8; 18]) -> Vec<u8> {
    let mut payload = vec![0u8; 78];
    payload[8..16].copy_from_slice(&uid.to_le_bytes());
    payload[60..78].copy_from_slice(&uid2);
    while payload.last() == Some(&0) {
        payload.pop();
    }
    let mut f = vec![0xFD, payload.len() as u8, 0, 0, 0, 1, 1, 148, 0, 0];
    f.extend_from_slice(&payload);
    f.extend_from_slice(&[0, 0]);
    f
}

fn ack(cmd: MavCmd, result: MavResult) -> MavMessage {
    MavMessage::COMMAND_ACK(COMMAND_ACK_DATA { command: cmd, result })
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
            airspeed_cal_until: None,
            offboard_alt: None,
            vtol_fw: false,
            vtol_trans: None,
            target_ne: None,
            reboots: 0,
            uid: 0x00C0_FFEE_1234_5678,
            uid2: None,
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
                let name = r.param_id.to_str().unwrap_or("").trim_end_matches('\0').to_string();
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
                let name = s.param_id.to_str().unwrap_or("").trim_end_matches('\0').to_string();
                if self.params.contains_key(&name) {
                    self.params.insert(name.clone(), s.param_value);
                    let i = self.params.keys().position(|k| *k == name).unwrap_or(0) as u16;
                    let v = self.param_value(&name, i);
                    self.queue.push_back(v);
                }
            }
            MavMessage::MANUAL_CONTROL(mc) => {
                self.manual_z = mc.z as f32 / 1000.0;
            }
            MavMessage::COMMAND_INT(c) => {
                self.queue.push_back(ack(c.command, MavResult::MAV_RESULT_ACCEPTED));
                if c.command == MavCmd::MAV_CMD_DO_REPOSITION {
                    self.custom_mode = 15;
                    self.takeoff_alt = Some(self.offboard_alt.unwrap_or(c.z));
                    self.target_ne = Some(if c.x == 0 && c.y == 0 { (self.n, self.e) } else { ne_of(c.x, c.y) });
                    // ArduPlane: a GUIDED point restarts navigation as fixed-wing;
                    // with Q_GUIDED_MODE 0 nothing hands it back to the VTOL
                    // controller, so a hovering QuadPlane transitions.
                    if self.is_vtol() && self.alt > 1.0 && !self.vtol_fw && self.vtol_trans.is_none() && self.params.get("Q_GUIDED_MODE") == Some(&0.0) {
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
            _ => {}
        }
    }

    fn command(&mut self, c: &COMMAND_LONG_DATA, now: u64) {
        use MavCmd::*;
        let mut result = MavResult::MAV_RESULT_ACCEPTED;
        match c.command {
            MAV_CMD_REQUEST_MESSAGE if c.param1 as u32 == 148 => {
                if let Some(u2) = self.uid2 {
                    self.raw.push_back(version_frame(self.uid, u2));
                }
                self.queue.push_back(MavMessage::AUTOPILOT_VERSION(AUTOPILOT_VERSION_DATA {
                    uid: self.uid,
                    flight_sw_version: (4 << 24) | (6 << 16) | (2 << 8),
                    ..Default::default()
                }));
            }
            MAV_CMD_PREFLIGHT_CALIBRATION => {
                if c.param5 == 1.0 {
                    self.accel_pose = Some(1);
                    self.queue.push_back(text("Place vehicle level and press any key."));
                    self.queue.push_back(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                        command: MAV_CMD_ACCELCAL_VEHICLE_POS,
                        param1: 1.0,
                        ..Default::default()
                    }));
                } else if c.param3 == 1.0 {
                    // ArduPilot: ground pressure, then the airspeed zero as an
                    // in-progress task when a sensor is enabled.
                    if self.params.get("ARSPD_TYPE").is_some_and(|t| *t != 0.0) {
                        self.airspeed_cal_until = Some(now + 2000);
                        result = MavResult::MAV_RESULT_IN_PROGRESS;
                    }
                } else if c.param6 != 0.0 {
                    // ArduPilot handles no p6 calibration (compass-motor or airspeed).
                    result = MavResult::MAV_RESULT_UNSUPPORTED;
                }
            }
            MAV_CMD_ACCELCAL_VEHICLE_POS => {
                if let Some(p) = self.accel_pose {
                    if c.param1 as u8 == p {
                        let next = p + 1;
                        if next > 6 {
                            self.accel_pose = None;
                            self.queue.push_back(text("Calibration successful"));
                            self.queue.push_back(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                                command: MAV_CMD_ACCELCAL_VEHICLE_POS,
                                param1: 16.0,
                                ..Default::default()
                            }));
                        } else {
                            self.accel_pose = Some(next);
                            self.queue.push_back(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                                command: MAV_CMD_ACCELCAL_VEHICLE_POS,
                                param1: next as f32,
                                ..Default::default()
                            }));
                        }
                    }
                }
                return;
            }
            MAV_CMD_DO_START_MAG_CAL => {
                let fit = self.script.mag_fitness.pop_front().unwrap_or(8.0);
                self.mag = Some((now, fit));
            }
            MAV_CMD_DO_CANCEL_MAG_CAL => self.mag = None,
            MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN => {
                self.reboots += 1;
                self.rebooting_until = Some(if self.script.reboot_returns { now + 3500 } else { u64::MAX });
            }
            MAV_CMD_DO_SET_MODE => {
                let want = c.param2 as u32;
                // ArduCopter enters AUTOTUNE only from a mode that allows it
                // (STABILIZE 0, ALT_HOLD 2, LOITER 5, POSHOLD 16).
                let allowed = matches!(self.custom_mode, 0 | 2 | 5 | 16) && !self.script.refuse_autotune;
                if self.is_copter() && !self.is_vtol() && want == self.mode_autotune() && !allowed {
                    self.queue.push_back(text("Mode change to Autotune failed: init failed"));
                    result = MavResult::MAV_RESULT_FAILED;
                } else {
                    self.custom_mode = want;
                    if self.is_vtol() {
                        if (17..=22).contains(&want) && (self.vtol_fw || self.vtol_trans.is_some_and(|(_, fw)| fw)) {
                            // A Q mode from fixed-wing flight transitions back and hovers where it is.
                            self.vtol_trans = Some((now, false));
                            self.target_ne = None;
                        } else if want == 15 {
                            self.target_ne = Some((self.n, self.e));
                        }
                    }
                    if self.custom_mode == self.mode_autotune() && self.armed {
                        self.autotune_since = Some(now);
                        self.autotune_success = false;
                    }
                }
            }
            MAV_CMD_COMPONENT_ARM_DISARM => {
                if c.param1 == 1.0 {
                    self.armed = true;
                    if !self.is_copter() && self.custom_mode == 13 {
                        self.takeoff_alt = Some(self.params.get("TKOFF_ALT").copied().unwrap_or(50.0));
                    }
                } else if self.alt < 0.2 || c.param2 == 21196.0 {
                    if self.custom_mode == self.mode_autotune() && self.autotune_success {
                        let axes = self.params.get("AUTOTUNE_AXES").copied().unwrap_or(3.0) as u32;
                        let mut names = Vec::new();
                        if axes & 1 != 0 {
                            names.push("Roll");
                            *self.params.get_mut("ATC_RAT_RLL_P").unwrap() *= 1.4;
                            *self.params.get_mut("ATC_ANG_RLL_P").unwrap() *= 1.2;
                        }
                        if axes & 2 != 0 {
                            names.push("Pitch");
                            *self.params.get_mut("ATC_RAT_PIT_P").unwrap() *= 1.3;
                        }
                        if axes & 4 != 0 {
                            names.push("Yaw");
                            *self.params.get_mut("ATC_RAT_YAW_P").unwrap() *= 1.5;
                        }
                        self.queue.push_back(text(&format!("AutoTune: Saved gains for {}", names.join(" "))));
                    }
                    self.armed = false;
                    self.autotune_since = None;
                    self.vtol_fw = false;
                    self.vtol_trans = None;
                    self.target_ne = None;
                } else {
                    result = MavResult::MAV_RESULT_DENIED;
                }
            }
            MAV_CMD_NAV_TAKEOFF => {
                if self.armed {
                    self.takeoff_alt = Some(c.param7);
                } else {
                    result = MavResult::MAV_RESULT_DENIED;
                }
            }
            _ => {}
        }
        self.queue.push_back(ack(c.command, result));
    }

    fn telemetry(&mut self, now: u64) {
        if now.saturating_sub(self.last_hb) >= 1000 {
            self.last_hb = now;
            let base = if self.armed { MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED } else { MavModeFlag::empty() };
            self.queue.push_back(MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: self.custom_mode,
                mavtype: self.mav_type,
                autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
                base_mode: base | MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED,
                system_status: MavState::MAV_STATE_STANDBY,
                mavlink_version: 3,
            }));
            // ArduPlane reports the differential-pressure sensor present when one is enabled.
            let derived = if self.params.get("ARSPD_TYPE").is_some_and(|t| *t != 0.0) {
                MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_DIFFERENTIAL_PRESSURE
            } else {
                MavSysStatusSensor::empty()
            };
            self.queue.push_back(MavMessage::SYS_STATUS(SYS_STATUS_DATA {
                onboard_control_sensors_present: self.script.sensors_present.unwrap_or(derived),
                voltage_battery: (self.battery_v * 1000.0) as u16,
                battery_remaining: self.battery_pct,
                ..Default::default()
            }));
            if let Some((dir, spd)) = self.wind {
                self.queue.push_back(MavMessage::WIND(WIND_DATA { direction: dir, speed: spd, speed_z: 0.0 }));
            }
            if let Some(rc) = &self.rc {
                let mut d = RC_CHANNELS_DATA { chancount: rc.len() as u8, ..Default::default() };
                let ch = [
                    &mut d.chan1_raw, &mut d.chan2_raw, &mut d.chan3_raw, &mut d.chan4_raw, &mut d.chan5_raw,
                    &mut d.chan6_raw, &mut d.chan7_raw, &mut d.chan8_raw,
                ];
                for (slot, v) in ch.into_iter().zip(rc.iter()) {
                    *slot = *v;
                }
                self.queue.push_back(MavMessage::RC_CHANNELS(d));
            }
        }
        if now.saturating_sub(self.last_tel) >= 100 {
            self.last_tel = now;
            let landed = if self.alt < 0.1 { MavLandedState::MAV_LANDED_STATE_ON_GROUND } else { MavLandedState::MAV_LANDED_STATE_IN_AIR };
            let vtol_state = match (self.is_vtol(), self.vtol_trans, self.vtol_fw) {
                (false, _, _) => MavVtolState::MAV_VTOL_STATE_UNDEFINED,
                (true, Some((_, true)), _) => MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_FW,
                (true, Some((_, false)), _) => MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_MC,
                (true, None, true) => MavVtolState::MAV_VTOL_STATE_FW,
                (true, None, false) => MavVtolState::MAV_VTOL_STATE_MC,
            };
            self.queue.push_back(MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA { vtol_state, landed_state: landed }));
            let (lat, lon) = latlon_of(self.n, self.e);
            self.queue.push_back(MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
                relative_alt: (self.alt * 1000.0) as i32,
                lat,
                lon,
                ..Default::default()
            }));
            self.queue.push_back(MavMessage::LOCAL_POSITION_NED(LOCAL_POSITION_NED_DATA {
                x: self.n,
                y: self.e,
                z: -self.alt,
                ..Default::default()
            }));
            self.queue.push_back(MavMessage::ATTITUDE(ATTITUDE_DATA::default()));
            self.queue.push_back(MavMessage::VFR_HUD(VFR_HUD_DATA { throttle: 40, ..Default::default() }));
            self.queue.push_back(MavMessage::VIBRATION(VIBRATION_DATA { vibration_x: 8.0, vibration_y: 9.0, vibration_z: 12.0, ..Default::default() }));
        }
    }

    fn physics(&mut self, now: u64) {
        self.mag_tick(now);
        if self.airspeed_cal_until.is_some_and(|t| now >= t) {
            self.airspeed_cal_until = None;
            let r = if self.script.airspeed_cal_ok { MavResult::MAV_RESULT_ACCEPTED } else { MavResult::MAV_RESULT_FAILED };
            self.queue.push_back(ack(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, r));
        } else if self.airspeed_cal_until.is_some() && now % 1000 < TICK {
            self.queue.push_back(ack(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, MavResult::MAV_RESULT_IN_PROGRESS));
        }
        let dt = TICK as f32 / 1000.0;
        if !self.armed {
            self.alt = 0.0;
            return;
        }
        self.vtol_physics(now, dt);
        // A QuadPlane lands in QLAND once it hovers again.
        let land = self.custom_mode == self.mode_land() && !self.vtol_fw && self.vtol_trans.is_none();
        let autotune = self.custom_mode == self.mode_autotune();
        if land || (autotune && self.is_copter() && self.manual_z < 0.3) {
            self.alt = (self.alt - 1.5 * dt).max(0.0);
        } else if let Some(t) = self.takeoff_alt {
            if self.alt < t {
                self.alt = (self.alt + 2.0 * dt).min(t);
            } else if !autotune {
                // An idle-throttle plane (THR_MAX 0, the TECS sink stages) sinks fast.
                let sink = if self.params.get("THR_MAX").is_some_and(|t| *t == 0.0) { 5.0 } else { 2.0 };
                self.alt = (self.alt - sink * dt).max(t);
            }
        }
        if let Some(since) = self.autotune_since {
            let el = now - since;
            if (1000..1000 + TICK).contains(&el) {
                self.queue.push_back(text("AutoTune: Started"));
            }
            if (2000..2000 + TICK).contains(&el) {
                self.queue.push_back(text("AutoTune: Testing"));
            }
            if el >= self.script.autotune_s * 1000 && !self.autotune_success {
                self.autotune_success = true;
                self.queue.push_back(text("AutoTune: Success"));
            }
        }
    }

    /// QuadPlane transitions (2 s) and horizontal flight: fixed-wing circles
    /// the GUIDED point (or where it went fixed-wing) at 60 m and 15 m/s; a
    /// VTOL in GUIDED with Q_GUIDED_MODE 1 flies to its point at 1 m/s.
    fn vtol_physics(&mut self, now: u64, dt: f32) {
        if !self.is_vtol() || self.alt < 0.5 {
            return;
        }
        if let Some((since, to_fw)) = self.vtol_trans {
            if now.saturating_sub(since) >= 2000 {
                self.vtol_trans = None;
                self.vtol_fw = to_fw;
            }
            return;
        }
        if self.vtol_fw {
            let (tn, te) = *self.target_ne.get_or_insert((self.n, self.e));
            let (dn, de) = (self.n - tn, self.e - te);
            let d = (dn * dn + de * de).sqrt();
            let (un, ue) = if d > 0.1 { (dn / d, de / d) } else { (1.0, 0.0) };
            // Tangent plus a pull onto the 60 m circle.
            let (vn, ve) = (-ue + (60.0 - d) / 20.0 * un, un + (60.0 - d) / 20.0 * ue);
            let v = (vn * vn + ve * ve).sqrt().max(1e-3);
            self.n += 15.0 * vn / v * dt;
            self.e += 15.0 * ve / v * dt;
        } else if self.custom_mode == 15 && self.params.get("Q_GUIDED_MODE") == Some(&1.0) {
            if let Some((tn, te)) = self.target_ne {
                let (dn, de) = (tn - self.n, te - self.e);
                let d = (dn * dn + de * de).sqrt();
                if d > 0.01 {
                    let step = (1.0 * dt).min(d);
                    self.n += dn / d * step;
                    self.e += de / d * step;
                }
            }
        }
    }

    fn mag_tick(&mut self, now: u64) {
        if let Some((start, fit)) = self.mag {
            let pct = ((now - start) / 50).min(100) as u8;
            self.queue.push_back(MavMessage::MAG_CAL_PROGRESS(MAG_CAL_PROGRESS_DATA {
                compass_id: 0,
                cal_mask: 1,
                cal_status: MagCalStatus::MAG_CAL_RUNNING_STEP_TWO,
                completion_pct: pct,
                ..Default::default()
            }));
            if pct >= 100 {
                self.mag = None;
                self.queue.push_back(MavMessage::MAG_CAL_REPORT(MAG_CAL_REPORT_DATA {
                    fitness: fit,
                    compass_id: 0,
                    cal_mask: 1,
                    cal_status: MagCalStatus::MAG_CAL_SUCCESS,
                    autosaved: 1,
                    ..Default::default()
                }));
            }
        }
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
