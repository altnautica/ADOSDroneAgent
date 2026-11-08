//! What the setup engine knows about the flight controller, refreshed from
//! every frame the FC sends.

use std::collections::{HashMap, VecDeque};

use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavLandedState, MavMessage, MavModeFlag, MavResult, MavSysStatusSensor, MavType, MavVtolState,
};
use ados_protocol::mavlink::MavHeader;
use serde::{Deserialize, Serialize};

/// Flight-controller firmware family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fw {
    ArduPilot,
    Px4,
    Other,
}

impl Fw {
    pub fn as_str(self) -> &'static str {
        match self {
            Fw::ArduPilot => "ardupilot",
            Fw::Px4 => "px4",
            Fw::Other => "other",
        }
    }
}

/// Vehicle class (contract C2b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Multirotor,
    Fixedwing,
    Vtol,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Multirotor => "multirotor",
            Class::Fixedwing => "fixedwing",
            Class::Vtol => "vtol",
        }
    }
}

/// How long a STATUSTEXT stays in the ring.
const TEXT_RING: usize = 64;

/// A sample of body rates for the oscillation check.
#[derive(Debug, Clone, Copy)]
pub struct RateSample {
    pub t_ms: u64,
    pub roll: f32,
    pub pitch: f32,
}

#[derive(Debug, Default, Clone)]
pub struct FcState {
    pub sysid: u8,
    pub compid: u8,
    pub heartbeat_ms: Option<u64>,
    pub autopilot: Option<MavAutopilot>,
    pub mav_type: Option<MavType>,
    pub custom_mode: u32,
    pub armed: bool,
    /// AUTOPILOT_VERSION `uid` (0 = the board reports none).
    pub uid: Option<u64>,
    /// AUTOPILOT_VERSION `uid2` from the raw frame (all zero = none).
    pub uid2: Option<[u8; 18]>,
    pub flight_sw_version: Option<u32>,
    pub board_version: Option<u32>,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    /// When AUTOPILOT_VERSION last arrived.
    pub version_ms: Option<u64>,
    /// `(roll, pitch, yaw)` rad and when.
    pub attitude: Option<([f32; 3], u64)>,
    /// Commanded `(roll, pitch)` rad from ATTITUDE_TARGET or NAV_CONTROLLER_OUTPUT.
    pub att_target: Option<([f32; 2], u64)>,
    pub vibration: Option<([f32; 3], u64)>,
    /// LOCAL_POSITION_NED `(n, e, d, vn, ve, vd)`.
    pub local: Option<([f32; 6], u64)>,
    pub rel_alt_m: Option<f32>,
    /// GLOBAL_POSITION_INT `(lat, lon)` in degE7.
    pub latlon: Option<(i32, i32)>,
    pub airspeed: Option<f32>,
    pub groundspeed: Option<f32>,
    pub climb: Option<f32>,
    pub throttle_pct: Option<u16>,
    /// WIND: direction the wind comes from (deg), speed m/s.
    pub wind: Option<(f32, f32, u64)>,
    pub landed: Option<MavLandedState>,
    pub vtol_state: Option<MavVtolState>,
    pub battery_pct: Option<i8>,
    pub battery_v: Option<f32>,
    /// SYS_STATUS `onboard_control_sensors_present`: the sensors the FC
    /// reports fitted.
    pub sensors_present: Option<MavSysStatusSensor>,
    pub rc: Option<(Vec<u16>, u64)>,
    pub esc_telemetry_ms: Option<u64>,
    /// ACKs by command id: result and arrival time.
    pub acks: HashMap<u32, (MavResult, u64)>,
    /// STATUSTEXT ring: `(sequence, arrival ms, text)`.
    pub texts: VecDeque<(u64, u64, String)>,
    /// Sequence number of the last STATUSTEXT (0 = none yet).
    pub text_seq: u64,
    pub rates: VecDeque<RateSample>,
}

impl FcState {
    pub fn new() -> Self {
        FcState {
            sysid: 1,
            compid: 1,
            ..Default::default()
        }
    }

    pub fn firmware(&self) -> Option<Fw> {
        Some(match self.autopilot? {
            MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA => Fw::ArduPilot,
            MavAutopilot::MAV_AUTOPILOT_PX4 => Fw::Px4,
            _ => Fw::Other,
        })
    }

    /// `"4.5.7"` from AUTOPILOT_VERSION's packed `flight_sw_version`.
    pub fn version_string(&self) -> Option<String> {
        let v = self.flight_sw_version?;
        Some(format!("{}.{}.{}", (v >> 24) & 0xff, (v >> 16) & 0xff, (v >> 8) & 0xff))
    }

    /// `(major, minor)` of the firmware.
    pub fn version(&self) -> Option<(u32, u32)> {
        let v = self.flight_sw_version?;
        Some(((v >> 24) & 0xff, (v >> 16) & 0xff))
    }

    /// The aircraft identity the setup state is filed under, and where it
    /// came from. `uid2` supersedes `uid` when non-zero (MAVLink
    /// AUTOPILOT_VERSION); with neither, a stable id is derived from the
    /// board and firmware identity (`d` + 16 hex). `None` until
    /// AUTOPILOT_VERSION and a heartbeat arrived.
    pub fn identity(&self) -> Option<(String, IdSource)> {
        if let Some(u2) = self.uid2.filter(|u| u.iter().any(|b| *b != 0)) {
            let hex: String = u2.iter().map(|b| format!("{b:02x}")).collect();
            return Some((hex, IdSource::Uid2));
        }
        if let Some(u) = self.uid.filter(|u| *u != 0) {
            return Some((format!("{u:016x}"), IdSource::Uid));
        }
        self.uid?;
        let mav_type = self.mav_type? as u32;
        let autopilot = self.autopilot? as u32;
        let mut h = Fnv64::new();
        for word in [
            autopilot,
            mav_type,
            self.vendor_id.unwrap_or(0) as u32,
            self.product_id.unwrap_or(0) as u32,
            self.board_version.unwrap_or(0),
            self.flight_sw_version.unwrap_or(0),
            self.sysid as u32,
        ] {
            h.write(&word.to_le_bytes());
        }
        Some((format!("d{:016x}", h.0), IdSource::Derived))
    }

    pub fn link_alive(&self, now_ms: u64) -> bool {
        self.heartbeat_ms.is_some_and(|t| now_ms.saturating_sub(t) < 3000)
    }

    /// An RC or ELRS lane is feeding the FC: RC_CHANNELS with at least four
    /// populated channels seen in the last two seconds.
    pub fn rc_present(&self, now_ms: u64) -> bool {
        match &self.rc {
            Some((ch, t)) => {
                now_ms.saturating_sub(*t) < 2000
                    && ch.len() >= 4
                    && ch.iter().take(4).all(|v| (800..=2200).contains(v))
            }
            None => false,
        }
    }

    pub fn rc_channel(&self, ch: usize) -> Option<u16> {
        let (v, _) = self.rc.as_ref()?;
        v.get(ch.checked_sub(1)?).copied().filter(|p| *p != 0 && *p != u16::MAX)
    }

    pub fn esc_telemetry(&self, now_ms: u64) -> bool {
        self.esc_telemetry_ms.is_some_and(|t| now_ms.saturating_sub(t) < 5000)
    }

    /// The ACK for `cmd` that arrived at or after `since_ms`.
    pub fn ack_since(&self, cmd: u32, since_ms: u64) -> Option<MavResult> {
        self.acks.get(&cmd).filter(|(_, t)| *t >= since_ms).map(|(r, _)| *r)
    }

    /// STATUSTEXTs received at or after `since_ms`, oldest first.
    pub fn texts_since(&self, since_ms: u64) -> impl Iterator<Item = &str> {
        self.texts.iter().filter(move |(_, t, _)| *t >= since_ms).map(|(_, _, s)| s.as_str())
    }

    /// Texts with a sequence above `cursor`; returns them and the new cursor.
    pub fn texts_after(&self, cursor: u64) -> (Vec<String>, u64) {
        let v: Vec<String> = self.texts.iter().filter(|(s, _, _)| *s > cursor).map(|(_, _, t)| t.clone()).collect();
        (v, self.text_seq.max(cursor))
    }

    pub fn on_ground(&self) -> bool {
        match self.landed {
            Some(MavLandedState::MAV_LANDED_STATE_ON_GROUND) => true,
            Some(MavLandedState::MAV_LANDED_STATE_UNDEFINED) | None => !self.armed,
            _ => false,
        }
    }

    pub fn ingest(&mut self, header: &MavHeader, msg: &MavMessage, now_ms: u64) {
        match msg {
            MavMessage::HEARTBEAT(h) => {
                // Only the autopilot's own heartbeat (not a GCS, camera, ...).
                if matches!(
                    h.mavtype,
                    MavType::MAV_TYPE_GCS | MavType::MAV_TYPE_ONBOARD_CONTROLLER | MavType::MAV_TYPE_CAMERA
                ) || h.autopilot == MavAutopilot::MAV_AUTOPILOT_INVALID
                {
                    return;
                }
                self.sysid = header.system_id;
                self.compid = header.component_id;
                self.heartbeat_ms = Some(now_ms);
                self.autopilot = Some(h.autopilot);
                self.mav_type = Some(h.mavtype);
                self.custom_mode = h.custom_mode;
                self.armed = h.base_mode.contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED);
            }
            MavMessage::AUTOPILOT_VERSION(v) => {
                self.uid = Some(v.uid);
                self.flight_sw_version = Some(v.flight_sw_version);
                self.board_version = Some(v.board_version);
                self.vendor_id = Some(v.vendor_id);
                self.product_id = Some(v.product_id);
                self.version_ms = Some(now_ms);
            }
            MavMessage::ATTITUDE(a) => {
                self.attitude = Some(([a.roll, a.pitch, a.yaw], now_ms));
                self.rates.push_back(RateSample { t_ms: now_ms, roll: a.rollspeed, pitch: a.pitchspeed });
                while self.rates.front().is_some_and(|s| now_ms.saturating_sub(s.t_ms) > 4000) {
                    self.rates.pop_front();
                }
            }
            MavMessage::ATTITUDE_TARGET(t) => {
                let [w, x, y, z] = t.q;
                let roll = (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y));
                let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin();
                self.att_target = Some(([roll, pitch], now_ms));
            }
            MavMessage::NAV_CONTROLLER_OUTPUT(n) => {
                // Only when no ATTITUDE_TARGET stream is fresh.
                let fresh = self.att_target.is_some_and(|(_, t)| now_ms.saturating_sub(t) < 300);
                if !fresh {
                    self.att_target = Some(([n.nav_roll.to_radians(), n.nav_pitch.to_radians()], now_ms));
                }
            }
            MavMessage::VIBRATION(v) => {
                self.vibration = Some(([v.vibration_x, v.vibration_y, v.vibration_z], now_ms));
            }
            MavMessage::LOCAL_POSITION_NED(p) => {
                self.local = Some(([p.x, p.y, p.z, p.vx, p.vy, p.vz], now_ms));
            }
            MavMessage::GLOBAL_POSITION_INT(g) => {
                self.rel_alt_m = Some(g.relative_alt as f32 / 1000.0);
                self.latlon = Some((g.lat, g.lon));
            }
            MavMessage::VFR_HUD(v) => {
                self.airspeed = Some(v.airspeed);
                self.groundspeed = Some(v.groundspeed);
                self.climb = Some(v.climb);
                self.throttle_pct = Some(v.throttle);
            }
            MavMessage::WIND(w) => {
                self.wind = Some((w.direction, w.speed, now_ms));
            }
            MavMessage::EXTENDED_SYS_STATE(e) => {
                self.landed = Some(e.landed_state);
                self.vtol_state = Some(e.vtol_state);
            }
            MavMessage::SYS_STATUS(s) => {
                self.sensors_present = Some(s.onboard_control_sensors_present);
                if s.battery_remaining >= 0 {
                    self.battery_pct = Some(s.battery_remaining);
                }
                if s.voltage_battery != u16::MAX && s.voltage_battery != 0 {
                    self.battery_v = Some(s.voltage_battery as f32 / 1000.0);
                }
            }
            MavMessage::BATTERY_STATUS(b) => {
                if b.id == 0 && b.battery_remaining >= 0 {
                    self.battery_pct = Some(b.battery_remaining);
                }
            }
            MavMessage::RC_CHANNELS(r) => {
                let all = [
                    r.chan1_raw, r.chan2_raw, r.chan3_raw, r.chan4_raw, r.chan5_raw, r.chan6_raw,
                    r.chan7_raw, r.chan8_raw, r.chan9_raw, r.chan10_raw, r.chan11_raw, r.chan12_raw,
                    r.chan13_raw, r.chan14_raw, r.chan15_raw, r.chan16_raw, r.chan17_raw, r.chan18_raw,
                ];
                let n = (r.chancount as usize).min(all.len());
                self.rc = Some((all[..n].to_vec(), now_ms));
            }
            MavMessage::ESC_TELEMETRY_1_TO_4(_)
            | MavMessage::ESC_TELEMETRY_5_TO_8(_)
            | MavMessage::ESC_TELEMETRY_9_TO_12(_) => {
                self.esc_telemetry_ms = Some(now_ms);
            }
            MavMessage::COMMAND_ACK(a) => {
                self.acks.insert(a.command as u32, (a.result, now_ms));
            }
            MavMessage::STATUSTEXT(s) => {
                let text = s.text.to_str().unwrap_or("").trim_end_matches('\0').to_string();
                self.text_seq += 1;
                self.texts.push_back((self.text_seq, now_ms, text));
                while self.texts.len() > TEXT_RING {
                    self.texts.pop_front();
                }
            }
            _ => {}
        }
    }
}

/// Where the aircraft identity came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdSource {
    Uid,
    Uid2,
    /// No hardware id: a hash of autopilot, vehicle type, vendor, product,
    /// board and firmware version and system id. Stable across reboots; a
    /// firmware update or a sysid change makes it a new aircraft.
    Derived,
}

/// FNV-1a 64: a fixed hash, so a derived id is the same on every build.
struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Fnv64(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// `uid2` from a raw AUTOPILOT_VERSION frame (MAVLink 2 only: the field is an
/// extension). Wire order sorts fields by size: capabilities, uid (u64),
/// four u32, vendor and product (u16), three 8-byte arrays, then the
/// extension `uid2[18]` at offset 60. MAVLink 2 trims trailing zero bytes.
pub fn uid2_from_frame(frame: &[u8]) -> Option<[u8; 18]> {
    const HEADER: usize = 10;
    const MSG_ID: u32 = 148;
    const UID2_AT: usize = 60;
    if frame.len() < HEADER || frame[0] != 0xFD {
        return None;
    }
    let len = frame[1] as usize;
    let id = u32::from_le_bytes([frame[7], frame[8], frame[9], 0]);
    if id != MSG_ID || frame.len() < HEADER + len {
        return None;
    }
    let payload = &frame[HEADER..HEADER + len];
    let mut out = [0u8; 18];
    if payload.len() > UID2_AT {
        let src = &payload[UID2_AT..payload.len().min(UID2_AT + 18)];
        out[..src.len()].copy_from_slice(src);
    }
    Some(out)
}
