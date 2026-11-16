//! The flight controller as the pilot sees it: telemetry state decoded from
//! the MAVLink socket, and the frames the pilot sends back.

use std::collections::{HashMap, VecDeque};

use ados_protocol::mavlink::ardupilotmega::{
    EkfStatusFlags, MavAutopilot, MavLandedState, MavMessage, MavModeFlag, MavType, MavVtolState,
};
use ados_protocol::pilot::VehicleClass;

use crate::fc_profile::Firmware;

mod ekf;
pub mod extnav;
pub mod rates;
mod tx;
pub use ekf::EkfPosition;
pub use tx::{heartbeat, Tx};

/// Velocity plus yaw rate (C2 "moving").
pub const MASK_VELOCITY: u16 = 1479;
/// Position plus velocity plus yaw (C2 "holding", stale-obstacle holds).
pub const MASK_POS_VEL_YAW: u16 = 2496;
/// Position, velocity and acceleration plus yaw: the shaped setpoints of a
/// skill goal or a goto. ArduCopter's guided mode flies the acceleration as a
/// feed-forward (`set_pos_vel_accel_NED_m`).
pub const MASK_POS_VEL_ACC_YAW: u16 = 2048;

/// Telemetry older than this is not used for control.
pub const FRESH_MS: u64 = 1000;
/// Downward rangefinder readings are trusted below this.
pub const AGL_RANGEFINDER_MAX_M: f32 = 8.0;
/// FC STATUSTEXT kept for surfaces: MAV_SEVERITY_WARNING (4) or worse.
pub const STATUSTEXT_MIN_SEVERITY: u8 = 4;
/// How many such lines the ring keeps.
pub const STATUSTEXT_RING: usize = 8;
/// How long a line stays on the pilot kind.
pub const STATUSTEXT_MAX_AGE_MS: u64 = 30_000;

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

/// One OPTICAL_FLOW from the FC: the rotation-compensated flow rate (rad/s;
/// ArduPilot sends rad/s in the `flow_comp_m_*` fields), its quality (0 =
/// none) and the range it used (m, negative while unknown).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Flow {
    pub comp: [f32; 2],
    pub quality: u8,
    pub ground_m: f32,
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

/// An FC boot time this far behind the last one means the FC rebooted
/// (slack for frames reordered on the link).
pub const REBOOT_BACKSTEP_MS: u32 = 5_000;

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
    /// Home latitude and longitude in degrees (HOME_POSITION).
    pub home_global: Option<[f64; 2]>,
    /// The newest FC STATUSTEXT at WARNING or worse, oldest first.
    pub statustext: VecDeque<Stamped<(u8, String)>>,
    pub battery_pct: Option<f32>,
    pub params: HashMap<String, f32>,
    /// GPS_RAW_INT `fix_type` (3 = 3-D fix), `None` until one arrives.
    pub gps_fix: Option<Stamped<u8>>,
    pub rc: Option<Stamped<[u16; 18]>>,
    /// OPTICAL_FLOW from the FC itself.
    pub flow_track: crate::sensor_health::FlowTrack,
    /// The downward DISTANCE_SENSOR from the FC itself.
    pub range_track: crate::sensor_health::RangeTrack,
    /// The FC's own EKF_STATUS_REPORT, `None` until one arrives.
    pub ekf: Option<Stamped<EkfPosition>>,
    /// The FC's OPTICAL_FLOW.
    pub flow: Option<Stamped<Flow>>,
    /// The EKF's start on external navigation and the stillness since.
    pub extnav: extnav::ExtNavStart,
    /// The newest FC `time_boot_ms` (ATTITUDE).
    pub boot_ms: u32,
    /// Counts FC reboots seen (boot time running backwards, or a new sysid).
    pub boot_epoch: u32,
    /// The FC's roll and pitch rate loop and its motor outputs.
    pub rates: rates::Rates,
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
                if self.sysid != 0 && self.sysid != sysid {
                    self.boot_epoch += 1;
                    self.rc = None;
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
                if self.from_fc(sysid, compid) {
                    if a.time_boot_ms.saturating_add(REBOOT_BACKSTEP_MS) < self.boot_ms {
                        self.boot_epoch += 1;
                        // A rebooted FC's RC frames start over: the last
                        // boot's channels say nothing about a receiver now.
                        self.rc = None;
                    }
                    self.boot_ms = a.time_boot_ms;
                }
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
                self.extnav.on_local(p.vx, p.vy, now_ms);
            }
            // The FC re-emits its flow module's flow and range; a bridge may
            // also forward the module's own raw frames (its own sysid), which
            // are not what the EKF fuses, so only the FC's count.
            MavMessage::OPTICAL_FLOW(f) if self.from_fc(sysid, compid) => {
                self.flow_track.on_msg(f.quality, now_ms);
                let v = Flow {
                    comp: [f.flow_comp_m_x, f.flow_comp_m_y],
                    quality: f.quality,
                    ground_m: f.ground_distance,
                };
                self.flow = Some(Stamped { v, at_ms: now_ms });
            }
            MavMessage::DISTANCE_SENSOR(d) if self.from_fc(sysid, compid) => {
                use ados_protocol::mavlink::ardupilotmega::MavSensorOrientation as O;
                if d.orientation == O::MAV_SENSOR_ROTATION_PITCH_270 {
                    self.range_track.on_msg(
                        d.current_distance,
                        d.min_distance,
                        d.max_distance,
                        now_ms,
                    );
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
            MavMessage::EKF_STATUS_REPORT(e) if self.from_fc(sysid, compid) => {
                self.ekf = Some(Stamped {
                    v: EkfPosition {
                        horiz_abs: e.flags.contains(EkfStatusFlags::EKF_POS_HORIZ_ABS),
                        pred_horiz_abs: e.flags.contains(EkfStatusFlags::EKF_PRED_POS_HORIZ_ABS),
                        horiz_rel: e.flags.contains(EkfStatusFlags::EKF_POS_HORIZ_REL),
                        pred_horiz_rel: e.flags.contains(EkfStatusFlags::EKF_PRED_POS_HORIZ_REL),
                        const_pos_mode: e.flags.contains(EkfStatusFlags::EKF_CONST_POS_MODE),
                        horiz_variance: e.pos_horiz_variance,
                    },
                    at_ms: now_ms,
                });
            }
            MavMessage::EXTENDED_SYS_STATE(x) => {
                self.landed_state = Some(x.landed_state);
                self.vtol_state = Some(x.vtol_state);
            }
            MavMessage::HOME_POSITION(h) => {
                self.home_local = Some([h.x, h.y, h.z]);
                self.home_global =
                    Some([f64::from(h.latitude) / 1e7, f64::from(h.longitude) / 1e7]);
            }
            MavMessage::STATUSTEXT(s) => {
                let end = s.text.iter().position(|&b| b == 0).unwrap_or(s.text.len());
                let text = String::from_utf8_lossy(&s.text[..end]).trim().to_owned();
                if self.from_fc(sysid, compid) {
                    self.extnav.on_text(&text, now_ms);
                }
                if !text.is_empty() && (s.severity as u8) <= STATUSTEXT_MIN_SEVERITY {
                    self.push_statustext(s.severity as u8, text, now_ms);
                }
            }
            MavMessage::SYS_STATUS(s) if s.battery_remaining >= 0 => {
                self.battery_pct = Some(f32::from(s.battery_remaining));
            }
            MavMessage::GPS_RAW_INT(g) => {
                self.gps_fix = Some(Stamped {
                    v: g.fix_type as u8,
                    at_ms: now_ms,
                });
            }
            MavMessage::PARAM_VALUE(p) => {
                let raw: Vec<u8> = p.param_id.iter().copied().take_while(|&b| b != 0).collect();
                let end = raw.len();
                if let Ok(name) = std::str::from_utf8(&raw[..end]) {
                    self.params.insert(name.to_owned(), p.param_value);
                }
            }
            // A frame with no channel carries no receiver: the FC repeats
            // its last values once RC input stops.
            MavMessage::RC_CHANNELS(r) if r.chancount > 0 => {
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
            MavMessage::PID_TUNING(p) if self.from_fc(sysid, compid) => {
                self.rates.on_pid(p, now_ms);
            }
            MavMessage::SERVO_OUTPUT_RAW(s) if self.from_fc(sysid, compid) => {
                self.rates.on_servo(s, now_ms);
            }
            _ => {}
        }
        None
    }

    /// Add a line to the STATUSTEXT ring the surfaces read: an FC line, or
    /// one of the pilot's own announcements (a low-battery action).
    pub fn push_statustext(&mut self, severity: u8, text: String, now_ms: u64) {
        if self.statustext.len() == STATUSTEXT_RING {
            self.statustext.pop_front();
        }
        self.statustext.push_back(Stamped {
            v: (severity, text),
            at_ms: now_ms,
        });
    }

    pub fn linked(&self, now_ms: u64) -> bool {
        self.heartbeat_ms > 0 && now_ms.saturating_sub(self.heartbeat_ms) < 3000
    }

    /// The frame comes from the autopilot whose heartbeat the pilot follows.
    pub fn from_fc(&self, sysid: u8, compid: u8) -> bool {
        self.heartbeat_ms > 0 && sysid == self.sysid && compid == self.compid
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
