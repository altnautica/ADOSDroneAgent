//! Vehicle-state aggregator: turns the MAVLink message stream into the unified
//! JSON snapshot published on the state socket.
//!
//! This is the authority for the snapshot shape: the field set, the
//! message-to-field scaling, and the `to_dict` wire form. The Python side keeps
//! only a passive view of it (`services/mavlink/ipc_state.py`'s
//! `IpcVehicleState`, fed from the state socket); the in-process producer it
//! used to mirror is gone. The runtime extras the service merges onto the
//! snapshot (`fc_connected`, `param_priming`, `param_generation`, ...) are added
//! by the caller via [`VehicleState::to_wire_with`]; this type owns only the
//! vehicle-derived fields so it stays I/O-free and unit-testable.

use std::collections::BTreeMap;

use ados_protocol::flight_modes::{ArduPilotFirmware, MAV_AUTOPILOT_PX4};
use ados_protocol::mavlink::ardupilotmega::{MavMessage, BATTERY_STATUS_DATA};
use serde_json::{json, Value};

/// How many distinct BATTERY_STATUS ids the snapshot tracks. MAVLink allows 256;
/// a real airframe carries one to four packs, and the cap bounds what a
/// misbehaving or spoofed stream can make the snapshot grow to. A new id past the
/// cap is ignored; the packs already tracked keep updating.
pub const MAX_BATTERY_PACKS: usize = 8;

/// The unified vehicle state. Only the fields the wire snapshot needs are
/// surfaced by [`VehicleState::to_wire`].
#[derive(Debug, Clone)]
pub struct VehicleState {
    // HEARTBEAT
    pub mav_type: i64,
    pub autopilot: i64,
    pub base_mode: i64,
    pub custom_mode: i64,
    pub system_status: i64,
    pub armed: bool,
    pub mode: String,
    /// The ArduPilot firmware the FC's banner STATUSTEXT named, `None` until
    /// one arrives. Cleared when the heartbeat's autopilot or type changes,
    /// so a swapped FC is never decoded with the previous one's table.
    pub firmware: Option<ArduPilotFirmware>,
    // GLOBAL_POSITION_INT
    pub lat: f64,
    pub lon: f64,
    pub alt_msl: f64,
    pub alt_rel: f64,
    pub vx: f64,
    pub vy: f64,
    pub vz: f64,
    pub heading: f64,
    // ATTITUDE
    pub roll: f64,
    pub pitch: f64,
    pub yaw: f64,
    pub rollspeed: f64,
    pub pitchspeed: f64,
    pub yawspeed: f64,
    /// ATTITUDE `time_boot_ms` of the sample above.
    pub attitude_time_boot_ms: u32,
    // SYS_STATUS. Every reading MAVLink lets an autopilot mark "not measured"
    // is an `Option`: the sentinel (UINT16_MAX mV, -1 cA, -1 %) decodes to
    // `None` and is published as `null`, never as a value a surface would show.
    pub voltage_battery: Option<f64>,
    pub current_battery: Option<f64>,
    pub battery_remaining: Option<i64>,
    pub sensors_health: i64,
    // GPS_RAW_INT
    pub gps_fix_type: i64,
    pub gps_satellites: i64,
    /// `None` when the receiver reports UINT16_MAX (unknown).
    pub gps_eph: Option<f64>,
    pub gps_epv: Option<f64>,
    // VFR_HUD
    pub airspeed: f64,
    pub groundspeed: f64,
    pub climb: f64,
    pub throttle: i64,
    // BATTERY_STATUS
    /// `None` when BATTERY_STATUS reports INT16_MAX (not measured).
    pub battery_temperature: Option<f64>,
    pub battery_voltages: Vec<f64>,
    pub battery_current_consumed: i64,
    pub battery_energy_consumed: i64,
    /// Every battery the FC reports, keyed by BATTERY_STATUS `id`, at most
    /// [`MAX_BATTERY_PACKS`] ids. The single-pack fields above keep tracking
    /// the most recent BATTERY_STATUS of any id.
    pub battery_packs: BTreeMap<u8, BatteryPack>,
    // RC_CHANNELS
    pub rc_channels: Vec<i64>,
    /// `None` when RC_CHANNELS reports UINT8_MAX (invalid/unknown).
    pub rc_rssi: Option<i64>,
    // Timestamps (ISO-8601 UTC strings, supplied by the caller)
    pub last_heartbeat: String,
    pub last_update: String,
    /// When a POSITION was last decoded, monotonic.
    ///
    /// Deliberately separate from `last_update`, which is refreshed by EVERY
    /// frame before the match below and therefore stays fresh while a heartbeat
    /// arrives and the position does not — the exact partial stall a consumer
    /// checking freshness needs to catch. `None` until the first position.
    ///
    /// The `Instant` itself is not serialised — it has no meaning off this
    /// process. [`Self::to_wire`] publishes the derived AGE instead
    /// (`position_age_ms`), which is what an out-of-process consumer actually
    /// needs and which no consumer could compute before: the swarm beacon has to
    /// know how old this node's fix is, because a frozen fix radiated at 2 Hz is
    /// dead-reckoned FORWARD by every other drone in the fleet. An age rather
    /// than a wall-clock stamp so a clock step cannot make a stale fix look
    /// fresh, and so no consumer needs an RFC3339 parser to gate on it.
    pub position_at: Option<std::time::Instant>,
    /// The last EKF_STATUS_REPORT `flags` and when it was decoded, monotonic.
    /// `None` until the FC sends one (PX4 never does). Published as the
    /// `ekf` block with an age, so a surface showing what the EKF navigates
    /// on can tell a live report from a frozen one.
    pub ekf_flags: Option<u16>,
    pub ekf_at: Option<std::time::Instant>,
    /// The last LOCAL_POSITION_NED (x, y, z, vx, vy, vz in metres and m/s)
    /// and when it was decoded. With no GNSS this is the only position the FC
    /// has, so the local-frame minimap draws the vehicle from it.
    pub local_ned: Option<[f64; 6]>,
    pub local_at: Option<std::time::Instant>,
    // The FC-advertised parameter total. The VALUES are not mirrored here: the
    // persistent [`crate::param_cache::ParamCache`] owns them, and this type's
    // copy was never serialised into `to_wire`, so it was a second 700-entry map
    // built one allocation per `PARAM_VALUE` frame for no reader.
    pub param_count: i64,
    /// HOME_POSITION: `(lat_e7, lon_e7, alt_msl_m, [x, y, z])`.
    pub home: Option<(i32, i32, f64, [f64; 3])>,
    /// The camera gimbal attitude, from GIMBAL_DEVICE_ATTITUDE_STATUS or,
    /// when that is absent, MOUNT_ORIENTATION.
    pub gimbal: Option<Gimbal>,
    /// When the last GIMBAL_DEVICE_ATTITUDE_STATUS arrived; MOUNT_ORIENTATION
    /// is used only while this is older than [`GIMBAL_DEVICE_PREFERRED`].
    pub gimbal_device_at: Option<std::time::Instant>,
    /// The last 10 s of PreArm/Arm STATUSTEXT, oldest first.
    pub prearm: std::collections::VecDeque<(std::time::Instant, String)>,
    /// The mission block the mission mirror computes ([`crate::mission_mirror`]);
    /// `None` until it has run once.
    pub mission: Option<Value>,
}

/// How long GIMBAL_DEVICE_ATTITUDE_STATUS stays preferred over
/// MOUNT_ORIENTATION after its last frame.
pub const GIMBAL_DEVICE_PREFERRED: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a PreArm/Arm STATUSTEXT stays in the ring.
pub const PREARM_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
/// Most texts the PreArm ring holds.
pub const PREARM_MAX: usize = 16;

/// A gimbal attitude reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gimbal {
    /// Degrees, negative looks down.
    pub pitch_deg: f64,
    /// Degrees; relative to the vehicle nose unless `earth_frame`.
    pub yaw_deg: f64,
    pub earth_frame: bool,
    pub at: std::time::Instant,
}

impl Default for VehicleState {
    fn default() -> Self {
        Self {
            mav_type: 0,
            autopilot: 0,
            base_mode: 0,
            custom_mode: 0,
            system_status: 0,
            armed: false,
            mode: String::new(),
            firmware: None,
            lat: 0.0,
            lon: 0.0,
            alt_msl: 0.0,
            alt_rel: 0.0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
            heading: 0.0,
            roll: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            rollspeed: 0.0,
            pitchspeed: 0.0,
            yawspeed: 0.0,
            voltage_battery: None,
            current_battery: None,
            battery_remaining: None,
            sensors_health: 0,
            gps_fix_type: 0,
            gps_satellites: 0,
            gps_eph: None,
            gps_epv: None,
            airspeed: 0.0,
            groundspeed: 0.0,
            climb: 0.0,
            throttle: 0,
            battery_temperature: None,
            battery_voltages: Vec::new(),
            battery_current_consumed: 0,
            battery_energy_consumed: 0,
            battery_packs: BTreeMap::new(),
            rc_channels: vec![0; 18],
            rc_rssi: None,
            last_heartbeat: String::new(),
            last_update: String::new(),
            // No position has been seen yet, which a consumer must treat as
            // infinitely stale rather than as "just now".
            position_at: None,
            ekf_flags: None,
            ekf_at: None,
            local_ned: None,
            local_at: None,
            param_count: 0,
            attitude_time_boot_ms: 0,
            home: None,
            gimbal: None,
            gimbal_device_at: None,
            prearm: std::collections::VecDeque::new(),
            mission: None,
        }
    }
}

impl VehicleState {
    /// Apply one MAVLink message. `now_iso` is the current ISO-8601 UTC
    /// timestamp (the caller computes it once per update).
    ///
    /// Returns `Some((name, value, param_type))` when the message was a
    /// `PARAM_VALUE`, so the caller can persist it to the param cache (keeping
    /// it I/O-free here lets the caller own persistence). The value is decoded for the vehicle's
    /// firmware: a PX4 integer parameter arrives as its integer bytes in the
    /// float field, and its float reading would be garbage.
    pub fn update_from_message(
        &mut self,
        msg: &MavMessage,
        now_iso: &str,
    ) -> Option<(String, f64, i64)> {
        self.last_update = now_iso.to_string();
        match msg {
            MavMessage::HEARTBEAT(m) => {
                let (autopilot, mav_type) = (m.autopilot as i64, m.mavtype as i64);
                // A different autopilot, or a type only another firmware
                // reports, is a different FC: forget the identification. A
                // multirotor type contradicts nothing (a QuadPlane reports one).
                let contradicted = match ArduPilotFirmware::from_mav_type(mav_type) {
                    Some(ArduPilotFirmware::Copter) | None => false,
                    Some(fw) => self.firmware.is_some_and(|known| known != fw),
                };
                if !self.last_heartbeat.is_empty() && (autopilot != self.autopilot || contradicted)
                {
                    self.firmware = None;
                }
                self.mav_type = mav_type;
                self.autopilot = autopilot;
                self.base_mode = m.base_mode.bits() as i64;
                self.custom_mode = m.custom_mode as i64;
                self.system_status = m.system_status as i64;
                self.armed = (m.base_mode.bits() & 128) != 0;
                self.last_heartbeat = now_iso.to_string();
                self.mode = mode_name(autopilot, self.firmware, mav_type, m.custom_mode);
                None
            }
            MavMessage::STATUSTEXT(m) => {
                let end = m.text.iter().position(|&b| b == 0).unwrap_or(m.text.len());
                let text = String::from_utf8_lossy(&m.text[..end]);
                if text.starts_with("PreArm:") || text.starts_with("Arm:") {
                    let now = std::time::Instant::now();
                    self.prearm.push_back((now, text.trim().to_owned()));
                    self.prune_prearm(now);
                }
                if let Some(fw) = ArduPilotFirmware::from_banner(&text) {
                    self.firmware = Some(fw);
                    if !self.last_heartbeat.is_empty() {
                        self.mode = mode_name(
                            self.autopilot,
                            self.firmware,
                            self.mav_type,
                            self.custom_mode as u32,
                        );
                    }
                }
                None
            }
            MavMessage::GLOBAL_POSITION_INT(m) => {
                self.lat = m.lat as f64 / 1e7;
                self.lon = m.lon as f64 / 1e7;
                self.alt_msl = m.alt as f64 / 1000.0;
                self.alt_rel = m.relative_alt as f64 / 1000.0;
                self.vx = m.vx as f64 / 100.0;
                self.vy = m.vy as f64 / 100.0;
                self.vz = m.vz as f64 / 100.0;
                self.heading = m.hdg as f64 / 100.0;
                self.position_at = Some(std::time::Instant::now());
                None
            }
            MavMessage::ATTITUDE(m) => {
                self.roll = m.roll as f64;
                self.pitch = m.pitch as f64;
                self.yaw = m.yaw as f64;
                self.rollspeed = m.rollspeed as f64;
                self.pitchspeed = m.pitchspeed as f64;
                self.yawspeed = m.yawspeed as f64;
                self.attitude_time_boot_ms = m.time_boot_ms;
                None
            }
            MavMessage::HOME_POSITION(m) => {
                self.home = Some((
                    m.latitude,
                    m.longitude,
                    m.altitude as f64 / 1000.0,
                    [m.x as f64, m.y as f64, m.z as f64],
                ));
                None
            }
            MavMessage::GIMBAL_DEVICE_ATTITUDE_STATUS(m) => {
                let now = std::time::Instant::now();
                let (pitch, yaw) = quat_pitch_yaw_deg(m.q);
                let bits = m.flags.bits();
                // YAW_IN_EARTH_FRAME (64), or YAW_LOCK (16) without
                // YAW_IN_VEHICLE_FRAME (32).
                let earth = bits & 64 != 0 || (bits & 16 != 0 && bits & 32 == 0);
                self.gimbal = Some(Gimbal {
                    pitch_deg: pitch,
                    yaw_deg: yaw,
                    earth_frame: earth,
                    at: now,
                });
                self.gimbal_device_at = Some(now);
                None
            }
            // Legacy gimbals still report attitude only through this message.
            #[allow(deprecated)]
            MavMessage::MOUNT_ORIENTATION(m) => {
                let now = std::time::Instant::now();
                let device_fresh = self
                    .gimbal_device_at
                    .is_some_and(|at| now.saturating_duration_since(at) < GIMBAL_DEVICE_PREFERRED);
                if !device_fresh {
                    // MOUNT_ORIENTATION's yaw is relative to the vehicle.
                    self.gimbal = Some(Gimbal {
                        pitch_deg: m.pitch as f64,
                        yaw_deg: m.yaw as f64,
                        earth_frame: false,
                        at: now,
                    });
                }
                None
            }
            MavMessage::SYS_STATUS(m) => {
                self.voltage_battery =
                    (m.voltage_battery != u16::MAX).then(|| m.voltage_battery as f64 / 1000.0);
                self.current_battery =
                    (m.current_battery != -1).then(|| m.current_battery as f64 / 100.0);
                self.battery_remaining =
                    (m.battery_remaining != -1).then_some(m.battery_remaining as i64);
                self.sensors_health = m.onboard_control_sensors_health.bits() as i64;
                None
            }
            MavMessage::GPS_RAW_INT(m) => {
                self.gps_fix_type = m.fix_type as i64;
                self.gps_satellites = m.satellites_visible as i64;
                self.gps_eph = (m.eph != u16::MAX).then(|| m.eph as f64 / 100.0);
                self.gps_epv = (m.epv != u16::MAX).then(|| m.epv as f64 / 100.0);
                None
            }
            MavMessage::EKF_STATUS_REPORT(m) => {
                self.ekf_flags = Some(m.flags.bits());
                self.ekf_at = Some(std::time::Instant::now());
                None
            }
            MavMessage::LOCAL_POSITION_NED(m) => {
                self.local_ned = Some([
                    m.x as f64,
                    m.y as f64,
                    m.z as f64,
                    m.vx as f64,
                    m.vy as f64,
                    m.vz as f64,
                ]);
                self.local_at = Some(std::time::Instant::now());
                None
            }
            MavMessage::VFR_HUD(m) => {
                self.airspeed = m.airspeed as f64;
                self.groundspeed = m.groundspeed as f64;
                self.climb = m.climb as f64;
                self.throttle = m.throttle as i64;
                None
            }
            MavMessage::BATTERY_STATUS(m) => {
                self.battery_temperature =
                    (m.temperature != i16::MAX).then(|| m.temperature as f64 / 100.0);
                self.battery_voltages = cell_voltages(&m.voltages);
                self.battery_current_consumed = m.current_consumed as i64;
                self.battery_energy_consumed = m.energy_consumed as i64;
                if self.battery_packs.contains_key(&m.id)
                    || self.battery_packs.len() < MAX_BATTERY_PACKS
                {
                    self.battery_packs.insert(m.id, BatteryPack::from_status(m));
                }
                None
            }
            MavMessage::RC_CHANNELS(m) => {
                self.rc_channels = vec![
                    m.chan1_raw as i64,
                    m.chan2_raw as i64,
                    m.chan3_raw as i64,
                    m.chan4_raw as i64,
                    m.chan5_raw as i64,
                    m.chan6_raw as i64,
                    m.chan7_raw as i64,
                    m.chan8_raw as i64,
                    m.chan9_raw as i64,
                    m.chan10_raw as i64,
                    m.chan11_raw as i64,
                    m.chan12_raw as i64,
                    m.chan13_raw as i64,
                    m.chan14_raw as i64,
                    m.chan15_raw as i64,
                    m.chan16_raw as i64,
                    m.chan17_raw as i64,
                    m.chan18_raw as i64,
                ];
                self.rc_rssi = (m.rssi != u8::MAX).then_some(m.rssi as i64);
                None
            }
            MavMessage::PARAM_VALUE(m) => {
                let name = param_id_to_string(&m.param_id[..]);
                self.param_count = m.param_count as i64;
                let param_type = m.param_type as u8;
                let value = ados_protocol::param_codec::decode(
                    m.param_value,
                    param_type,
                    ados_protocol::param_codec::uses_bytewise(self.autopilot),
                );
                Some((name, value, i64::from(param_type)))
            }
            _ => None,
        }
    }

    /// Drop PreArm texts older than the window and bound the ring.
    fn prune_prearm(&mut self, now: std::time::Instant) {
        while self
            .prearm
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= PREARM_WINDOW)
            || self.prearm.len() > PREARM_MAX
        {
            self.prearm.pop_front();
        }
    }
}

/// Pitch and yaw in degrees from a `[w, x, y, z]` quaternion (aerospace
/// ZYX order), as GIMBAL_DEVICE_ATTITUDE_STATUS carries it.
pub fn quat_pitch_yaw_deg(q: [f32; 4]) -> (f64, f64) {
    let [w, x, y, z] = q.map(f64::from);
    let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin();
    let yaw = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
    (pitch.to_degrees(), yaw.to_degrees())
}

/// Trim the trailing NUL padding from a fixed-width MAVLink `param_id` field,
/// matching Python's `msg.param_id.rstrip("\x00")`.
fn param_id_to_string(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

mod battery;
pub use battery::*;
mod modes;
pub use modes::*;
mod source;
pub use source::*;
mod wire;

#[cfg(test)]
mod tests;
