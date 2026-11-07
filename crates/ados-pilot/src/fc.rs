//! The flight controller as the pilot sees it: telemetry state decoded from
//! the MAVLink socket, and the frames the pilot sends back.

use std::collections::HashMap;

use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavCmd, MavFrame, MavLandedState, MavMessage, MavModeFlag, MavParamType,
    MavState, MavType, MavVtolState, PositionTargetTypemask, COMMAND_INT_DATA, COMMAND_LONG_DATA,
    HEARTBEAT_DATA, MANUAL_CONTROL_DATA, PARAM_REQUEST_READ_DATA, PARAM_SET_DATA,
    SET_POSITION_TARGET_LOCAL_NED_DATA,
};
use ados_protocol::pilot::VehicleClass;

use crate::fc_profile::Firmware;

/// Velocity plus yaw rate (C2 "moving").
pub const MASK_VELOCITY: u16 = 1479;
/// Position plus velocity plus yaw (C2 "holding", follow goals).
pub const MASK_POS_VEL_YAW: u16 = 2496;

/// Telemetry older than this is not used for control.
pub const FRESH_MS: u64 = 1000;
/// Downward rangefinder readings are trusted below this.
pub const AGL_RANGEFINDER_MAX_M: f32 = 8.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stamped<T> {
    pub v: T,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Attitude {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub yaw_rate: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LocalPos {
    pub n: f32,
    pub e: f32,
    pub d: f32,
    pub vn: f32,
    pub ve: f32,
    pub vd: f32,
}

/// Everything decoded from FC traffic.
#[derive(Debug, Clone, Default)]
pub struct FcState {
    pub sysid: u8,
    pub compid: u8,
    pub firmware: Option<Firmware>,
    pub mav_type: u32,
    pub custom_mode: u32,
    pub armed: bool,
    pub heartbeat_ms: u64,
    pub mode_changed_ms: u64,
    pub attitude: Option<Stamped<Attitude>>,
    pub local: Option<Stamped<LocalPos>>,
    pub rangefinder: Option<Stamped<f32>>,
    pub landed_state: Option<MavLandedState>,
    pub vtol_state: Option<MavVtolState>,
    pub home_local: Option<[f32; 3]>,
    pub battery_pct: Option<f32>,
    pub params: HashMap<String, f32>,
    pub rc: Option<Stamped<[u16; 18]>>,
}

impl FcState {
    /// Fold one FC frame in. Returns `Some(prev_mode)` when the custom mode
    /// changed.
    pub fn update(&mut self, sysid: u8, compid: u8, msg: &MavMessage, now_ms: u64) -> Option<u32> {
        match msg {
            MavMessage::HEARTBEAT(h) => {
                if h.autopilot == MavAutopilot::MAV_AUTOPILOT_INVALID
                    || h.mavtype == MavType::MAV_TYPE_GCS
                {
                    return None;
                }
                self.sysid = sysid;
                self.compid = compid;
                self.firmware = match h.autopilot {
                    MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA => Some(Firmware::Ardupilot),
                    MavAutopilot::MAV_AUTOPILOT_PX4 => Some(Firmware::Px4),
                    _ => None,
                };
                self.mav_type = h.mavtype as u32;
                self.armed = h
                    .base_mode
                    .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED);
                self.heartbeat_ms = now_ms;
                if self.custom_mode != h.custom_mode || self.mode_changed_ms == 0 {
                    let prev = self.custom_mode;
                    self.custom_mode = h.custom_mode;
                    self.mode_changed_ms = now_ms;
                    return Some(prev);
                }
            }
            MavMessage::ATTITUDE(a) => {
                let v = Attitude {
                    roll: a.roll,
                    pitch: a.pitch,
                    yaw: a.yaw,
                    yaw_rate: a.yawspeed,
                };
                self.attitude = Some(Stamped { v, at_ms: now_ms });
            }
            MavMessage::LOCAL_POSITION_NED(p) => {
                let v = LocalPos {
                    n: p.x,
                    e: p.y,
                    d: p.z,
                    vn: p.vx,
                    ve: p.vy,
                    vd: p.vz,
                };
                self.local = Some(Stamped { v, at_ms: now_ms });
            }
            MavMessage::DISTANCE_SENSOR(d) => {
                use ados_protocol::mavlink::ardupilotmega::MavSensorOrientation as O;
                if d.orientation == O::MAV_SENSOR_ROTATION_PITCH_270 {
                    let m = f32::from(d.current_distance) / 100.0;
                    let valid = d.current_distance >= d.min_distance
                        && d.current_distance <= d.max_distance;
                    if valid {
                        self.rangefinder = Some(Stamped {
                            v: m,
                            at_ms: now_ms,
                        });
                    }
                }
            }
            MavMessage::EXTENDED_SYS_STATE(x) => {
                self.landed_state = Some(x.landed_state);
                self.vtol_state = Some(x.vtol_state);
            }
            MavMessage::HOME_POSITION(h) => self.home_local = Some([h.x, h.y, h.z]),
            MavMessage::SYS_STATUS(s) if s.battery_remaining >= 0 => {
                self.battery_pct = Some(f32::from(s.battery_remaining));
            }
            MavMessage::PARAM_VALUE(p) => {
                let raw: Vec<u8> = p.param_id.iter().copied().take_while(|&b| b != 0).collect();
                let end = raw.len();
                if let Ok(name) = std::str::from_utf8(&raw[..end]) {
                    self.params.insert(name.to_owned(), p.param_value);
                }
            }
            MavMessage::RC_CHANNELS(r) => {
                let v = [
                    r.chan1_raw,
                    r.chan2_raw,
                    r.chan3_raw,
                    r.chan4_raw,
                    r.chan5_raw,
                    r.chan6_raw,
                    r.chan7_raw,
                    r.chan8_raw,
                    r.chan9_raw,
                    r.chan10_raw,
                    r.chan11_raw,
                    r.chan12_raw,
                    r.chan13_raw,
                    r.chan14_raw,
                    r.chan15_raw,
                    r.chan16_raw,
                    r.chan17_raw,
                    r.chan18_raw,
                ];
                self.rc = Some(Stamped { v, at_ms: now_ms });
            }
            _ => {}
        }
        None
    }

    pub fn linked(&self, now_ms: u64) -> bool {
        self.heartbeat_ms > 0 && now_ms.saturating_sub(self.heartbeat_ms) < 3000
    }

    /// Vehicle class from HEARTBEAT type and `Q_ENABLE`.
    pub fn vehicle_class(&self) -> VehicleClass {
        match self.mav_type {
            1 => {
                if self.params.get("Q_ENABLE").copied().unwrap_or(0.0) > 0.5 {
                    VehicleClass::Vtol
                } else {
                    VehicleClass::Fixedwing
                }
            }
            19..=24 => VehicleClass::Vtol,
            _ => VehicleClass::Multirotor,
        }
    }

    /// True while a VTOL is in forward flight.
    pub fn vtol_forward(&self) -> bool {
        self.vehicle_class() == VehicleClass::Vtol
            && self.vtol_state == Some(MavVtolState::MAV_VTOL_STATE_FW)
    }

    pub fn vtol_transitioning(&self) -> bool {
        matches!(
            self.vtol_state,
            Some(
                MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_FW
                    | MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_MC
            )
        )
    }

    pub fn fresh_local(&self, now_ms: u64) -> Option<LocalPos> {
        self.local
            .filter(|s| now_ms.saturating_sub(s.at_ms) < FRESH_MS)
            .map(|s| s.v)
    }

    pub fn fresh_attitude(&self, now_ms: u64) -> Option<Attitude> {
        self.attitude
            .filter(|s| now_ms.saturating_sub(s.at_ms) < FRESH_MS)
            .map(|s| s.v)
    }

    /// Rangefinder AGL while it is fresh and under the trusted range.
    pub fn rangefinder_agl(&self, now_ms: u64) -> Option<f32> {
        self.rangefinder
            .filter(|s| now_ms.saturating_sub(s.at_ms) < 500 && s.v < AGL_RANGEFINDER_MAX_M)
            .map(|s| s.v)
    }

    /// Height above the take-off point from the EKF (home-relative).
    pub fn ekf_height(&self, now_ms: u64) -> Option<f32> {
        let p = self.fresh_local(now_ms)?;
        let home_d = self.home_local.map(|h| h[2]).unwrap_or(0.0);
        Some(home_d - p.d)
    }

    pub fn landed(&self) -> bool {
        match self.landed_state {
            Some(MavLandedState::MAV_LANDED_STATE_ON_GROUND) => true,
            Some(MavLandedState::MAV_LANDED_STATE_UNDEFINED) | None => !self.armed,
            Some(_) => false,
        }
    }

    pub fn home_ne(&self) -> [f32; 2] {
        self.home_local.map(|h| [h[0], h[1]]).unwrap_or([0.0, 0.0])
    }
}

/// Frames the pilot sends, addressed to the FC it learned.
#[derive(Debug, Clone, Copy)]
pub struct Tx {
    pub sysid: u8,
    pub compid: u8,
}

impl Tx {
    pub fn setpoint(
        &self,
        mask: u16,
        pos: [f32; 3],
        vel: [f32; 3],
        yaw: f32,
        yaw_rate: f32,
    ) -> MavMessage {
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(SET_POSITION_TARGET_LOCAL_NED_DATA {
            time_boot_ms: 0,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            vx: vel[0],
            vy: vel[1],
            vz: vel[2],
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw,
            yaw_rate,
            type_mask: PositionTargetTypemask::from_bits_truncate(mask),
            target_system: self.sysid,
            target_component: self.compid,
            coordinate_frame: MavFrame::MAV_FRAME_LOCAL_NED,
        })
    }

    pub fn command(&self, command: MavCmd, p: [f32; 7]) -> MavMessage {
        MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            param5: p[4],
            param6: p[5],
            param7: p[6],
            command,
            target_system: self.sysid,
            target_component: self.compid,
            confirmation: 0,
        })
    }

    pub fn command_int(&self, command: MavCmd, p: [f32; 4], z: f32, frame: MavFrame) -> MavMessage {
        MavMessage::COMMAND_INT(COMMAND_INT_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            x: 0,
            y: 0,
            z,
            command,
            target_system: self.sysid,
            target_component: self.compid,
            frame,
            current: 0,
            autocontinue: 0,
        })
    }

    pub fn set_mode(&self, custom_mode: u32) -> MavMessage {
        let flag = MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED.bits() as f32;
        self.command(
            MavCmd::MAV_CMD_DO_SET_MODE,
            [flag, custom_mode as f32, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
    }

    pub fn arm(&self, arm: bool, force: bool) -> MavMessage {
        let p2 = if force { 21196.0 } else { 0.0 };
        let p1 = if arm { 1.0 } else { 0.0 };
        self.command(
            MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            [p1, p2, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
    }

    pub fn manual_control(&self, axes: [f32; 4]) -> MavMessage {
        let s = |v: f32| (v.clamp(-1.0, 1.0) * 1000.0) as i16;
        MavMessage::MANUAL_CONTROL(MANUAL_CONTROL_DATA {
            x: s(axes[1]),
            y: s(axes[0]),
            z: ((axes[2].clamp(-1.0, 1.0) + 1.0) * 500.0) as i16,
            r: s(axes[3]),
            buttons: 0,
            target: self.sysid,
        })
    }

    pub fn param_request(&self, name: &str) -> MavMessage {
        MavMessage::PARAM_REQUEST_READ(PARAM_REQUEST_READ_DATA {
            param_index: -1,
            target_system: self.sysid,
            target_component: self.compid,
            param_id: param_id(name).into(),
        })
    }

    pub fn param_set(&self, name: &str, value: f32) -> MavMessage {
        MavMessage::PARAM_SET(PARAM_SET_DATA {
            param_value: value,
            target_system: self.sysid,
            target_component: self.compid,
            param_id: param_id(name).into(),
            param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
        })
    }
}

fn param_id(name: &str) -> [u8; 16] {
    let mut id = [0u8; 16];
    let n = name.len().min(16);
    id[..n].copy_from_slice(&name.as_bytes()[..n]);
    id
}

/// The pilot's own 2 Hz heartbeat (sysid/compid stamped by the encoder).
pub fn heartbeat() -> MavMessage {
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_ONBOARD_CONTROLLER,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    })
}
