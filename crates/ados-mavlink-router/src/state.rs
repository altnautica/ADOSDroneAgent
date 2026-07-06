//! Vehicle-state aggregator: turns the MAVLink message stream into the unified
//! JSON snapshot published on the state socket.
//!
//! Mirrors the Python `VehicleState` (services/mavlink/state.py): the same
//! fields, the same message-to-field scaling, and the same `to_dict` wire
//! shape. The runtime extras the service merges onto the snapshot
//! (`fc_connected`, `param_priming`, the `params` blob, ...) are added by the
//! caller via [`VehicleState::to_wire_with`]; this type owns only the
//! vehicle-derived fields so it stays I/O-free and unit-testable.

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use serde_json::{json, Map, Value};

/// ArduCopter `custom_mode` -> mode name.
const COPTER_MODES: &[(u32, &str)] = &[
    (0, "STABILIZE"),
    (1, "ACRO"),
    (2, "ALT_HOLD"),
    (3, "AUTO"),
    (4, "GUIDED"),
    (5, "LOITER"),
    (6, "RTL"),
    (7, "CIRCLE"),
    (9, "LAND"),
    (11, "DRIFT"),
    (13, "SPORT"),
    (14, "FLIP"),
    (15, "AUTOTUNE"),
    (16, "POSHOLD"),
    (17, "BRAKE"),
    (18, "THROW"),
    (19, "AVOID_ADSB"),
    (20, "GUIDED_NOGPS"),
    (21, "SMART_RTL"),
    (22, "FLOWHOLD"),
    (23, "FOLLOW"),
    (24, "ZIGZAG"),
    (25, "SYSTEMID"),
    (26, "AUTOROTATE"),
    (27, "AUTO_RTL"),
];

/// ArduPlane `custom_mode` -> mode name.
const PLANE_MODES: &[(u32, &str)] = &[
    (0, "MANUAL"),
    (1, "CIRCLE"),
    (2, "STABILIZE"),
    (3, "TRAINING"),
    (4, "ACRO"),
    (5, "FBWA"),
    (6, "FBWB"),
    (7, "CRUISE"),
    (8, "AUTOTUNE"),
    (10, "AUTO"),
    (11, "RTL"),
    (12, "LOITER"),
    (14, "AVOID_ADSB"),
    (15, "GUIDED"),
    (17, "QSTABILIZE"),
    (18, "QHOVER"),
    (19, "QLOITER"),
    (20, "QLAND"),
    (21, "QRTL"),
    (22, "QAUTOTUNE"),
    (23, "QACRO"),
    (24, "THERMAL"),
    (25, "LOITER_ALT_QLAND"),
];

/// ArduRover `custom_mode` -> mode name.
const ROVER_MODES: &[(u32, &str)] = &[
    (0, "MANUAL"),
    (1, "ACRO"),
    (3, "STEERING"),
    (4, "HOLD"),
    (5, "LOITER"),
    (6, "FOLLOW"),
    (7, "SIMPLE"),
    (10, "AUTO"),
    (11, "RTL"),
    (12, "SMART_RTL"),
    (15, "GUIDED"),
];

/// PX4 `(main_mode, sub_mode)` -> mode name. PX4 packs the mode into
/// `custom_mode` differently from ArduPilot (a `(main << 16) | (sub << 24)`
/// union, mavros `px4_custom_mode.h`), so it needs its own decode path keyed on
/// `HEARTBEAT.autopilot == MAV_AUTOPILOT_PX4`, not on MAV_TYPE. `sub` is 0 for
/// the manual/assisted modes and non-zero only under AUTO.
const PX4_MODES: &[(u8, u8, &str)] = &[
    (1, 0, "MANUAL"),
    (2, 0, "ALTCTL"),
    (3, 0, "POSCTL"),
    (5, 0, "ACRO"),
    (6, 0, "OFFBOARD"),
    (7, 0, "STABILIZED"),
    (8, 0, "RATTITUDE"),
    (4, 1, "AUTO.READY"),
    (4, 2, "AUTO.TAKEOFF"),
    (4, 3, "AUTO.LOITER"),
    (4, 4, "AUTO.MISSION"),
    (4, 5, "AUTO.RTL"),
    (4, 6, "AUTO.LAND"),
    (4, 7, "AUTO.RTGS"),
    (4, 8, "AUTO.FOLLOW_TARGET"),
    (4, 9, "AUTO.PRECLAND"),
];

/// Decode a PX4 `custom_mode` (main in bits 16-23, sub in bits 24-31). Falls
/// back to the main-mode name when the sub-mode is unmapped, then to `MODE_<n>`.
fn px4_mode_name(custom_mode: u32) -> String {
    let main = ((custom_mode >> 16) & 0xff) as u8;
    let sub = ((custom_mode >> 24) & 0xff) as u8;
    if let Some((_, _, name)) = PX4_MODES.iter().find(|(m, s, _)| *m == main && *s == sub) {
        return (*name).to_string();
    }
    // Unmapped sub under a known main: report the main mode.
    if let Some((_, _, name)) = PX4_MODES.iter().find(|(m, s, _)| *m == main && *s == 0) {
        return (*name).to_string();
    }
    format!("MODE_{custom_mode}")
}

/// Select the mode table for a vehicle, then resolve the custom mode. PX4 is
/// decoded by its packed-union scheme (keyed on `autopilot`); ArduPilot is keyed
/// on MAV_TYPE by its wire value (stable across dialect revisions). The plane
/// table covers fixed-wing (1) and the VTOL types (20, 21). An unmapped
/// type/mode falls back to `MODE_<n>`, matching the Python
/// `mode_map.get(type, {}).get(custom_mode, f"MODE_{custom_mode}")`.
fn mode_name(autopilot: i64, mav_type: i64, custom_mode: u32) -> String {
    // MAV_AUTOPILOT_PX4 = 12.
    if autopilot == 12 {
        return px4_mode_name(custom_mode);
    }
    let table: Option<&[(u32, &str)]> = match mav_type {
        // QUADROTOR, HEXAROTOR, OCTOROTOR, HELICOPTER, TRICOPTER, COAXIAL
        2 | 13 | 14 | 4 | 15 | 3 => Some(COPTER_MODES),
        // FIXED_WING, VTOL_QUADROTOR, VTOL_TILTROTOR
        1 | 20 | 21 => Some(PLANE_MODES),
        // GROUND_ROVER, SURFACE_BOAT
        10 | 11 => Some(ROVER_MODES),
        _ => None,
    };
    table
        .and_then(|t| {
            t.iter()
                .find(|(k, _)| *k == custom_mode)
                .map(|(_, v)| v.to_string())
        })
        .unwrap_or_else(|| format!("MODE_{custom_mode}"))
}

/// The unified vehicle state. Field set and defaults mirror the Python
/// dataclass; only the fields the wire snapshot needs are surfaced by
/// [`VehicleState::to_wire`].
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
    // SYS_STATUS
    pub voltage_battery: f64,
    pub current_battery: f64,
    pub battery_remaining: i64,
    pub sensors_health: i64,
    // GPS_RAW_INT
    pub gps_fix_type: i64,
    pub gps_satellites: i64,
    pub gps_eph: f64,
    pub gps_epv: f64,
    // VFR_HUD
    pub airspeed: f64,
    pub groundspeed: f64,
    pub climb: f64,
    pub throttle: i64,
    // BATTERY_STATUS
    pub battery_temperature: f64,
    pub battery_voltages: Vec<f64>,
    pub battery_current_consumed: i64,
    pub battery_energy_consumed: i64,
    // RC_CHANNELS
    pub rc_channels: Vec<i64>,
    pub rc_rssi: i64,
    // Timestamps (ISO-8601 UTC strings, supplied by the caller)
    pub last_heartbeat: String,
    pub last_update: String,
    // Param cache (in-memory mirror)
    pub params: Map<String, Value>,
    pub param_count: i64,
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
            voltage_battery: 0.0,
            current_battery: 0.0,
            battery_remaining: -1,
            sensors_health: 0,
            gps_fix_type: 0,
            gps_satellites: 0,
            gps_eph: 0.0,
            gps_epv: 0.0,
            airspeed: 0.0,
            groundspeed: 0.0,
            climb: 0.0,
            throttle: 0,
            battery_temperature: 0.0,
            battery_voltages: Vec::new(),
            battery_current_consumed: 0,
            battery_energy_consumed: 0,
            rc_channels: vec![0; 18],
            rc_rssi: 0,
            last_heartbeat: String::new(),
            last_update: String::new(),
            params: Map::new(),
            param_count: 0,
        }
    }
}

impl VehicleState {
    /// Apply one MAVLink message. `now_iso` is the current ISO-8601 UTC
    /// timestamp (the caller computes it once per update, as the Python
    /// producer does with `datetime.now(timezone.utc).isoformat()`).
    ///
    /// Returns `Some((name, value, param_type))` when the message was a
    /// `PARAM_VALUE`, so the caller can persist it to the param cache (the
    /// Python producer writes the cache inline; keeping it I/O-free here lets
    /// the caller own persistence).
    pub fn update_from_message(
        &mut self,
        msg: &MavMessage,
        now_iso: &str,
    ) -> Option<(String, f32, i64)> {
        self.last_update = now_iso.to_string();
        match msg {
            MavMessage::HEARTBEAT(m) => {
                self.mav_type = m.mavtype as i64;
                self.autopilot = m.autopilot as i64;
                self.base_mode = m.base_mode.bits() as i64;
                self.custom_mode = m.custom_mode as i64;
                self.system_status = m.system_status as i64;
                self.armed = (m.base_mode.bits() & 128) != 0;
                self.last_heartbeat = now_iso.to_string();
                self.mode = mode_name(m.autopilot as i64, m.mavtype as i64, m.custom_mode);
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
                None
            }
            MavMessage::ATTITUDE(m) => {
                self.roll = m.roll as f64;
                self.pitch = m.pitch as f64;
                self.yaw = m.yaw as f64;
                self.rollspeed = m.rollspeed as f64;
                self.pitchspeed = m.pitchspeed as f64;
                self.yawspeed = m.yawspeed as f64;
                None
            }
            MavMessage::SYS_STATUS(m) => {
                self.voltage_battery = m.voltage_battery as f64 / 1000.0;
                self.current_battery = m.current_battery as f64 / 100.0;
                self.battery_remaining = m.battery_remaining as i64;
                self.sensors_health = m.onboard_control_sensors_health.bits() as i64;
                None
            }
            MavMessage::GPS_RAW_INT(m) => {
                self.gps_fix_type = m.fix_type as i64;
                self.gps_satellites = m.satellites_visible as i64;
                self.gps_eph = m.eph as f64 / 100.0;
                self.gps_epv = m.epv as f64 / 100.0;
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
                self.battery_temperature = if m.temperature != 0x7FFF {
                    m.temperature as f64 / 100.0
                } else {
                    0.0
                };
                self.battery_voltages = m
                    .voltages
                    .iter()
                    .filter(|&&v| v != 0xFFFF)
                    .map(|&v| v as f64 / 1000.0)
                    .collect();
                self.battery_current_consumed = m.current_consumed as i64;
                self.battery_energy_consumed = m.energy_consumed as i64;
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
                self.rc_rssi = m.rssi as i64;
                None
            }
            MavMessage::PARAM_VALUE(m) => {
                let name = param_id_to_string(&m.param_id);
                self.params.insert(name.clone(), json!(m.param_value));
                self.param_count = m.param_count as i64;
                Some((name, m.param_value, m.param_type as i64))
            }
            _ => None,
        }
    }

    /// The vehicle-derived wire snapshot (the Python `to_dict()` shape).
    pub fn to_wire(&self) -> Value {
        json!({
            "mav_type": self.mav_type,
            "autopilot": self.autopilot,
            "armed": self.armed,
            "mode": self.mode,
            "position": {
                "lat": self.lat,
                "lon": self.lon,
                "alt_msl": self.alt_msl,
                "alt_rel": self.alt_rel,
                "heading": self.heading,
            },
            "velocity": {
                "vx": self.vx,
                "vy": self.vy,
                "vz": self.vz,
                "groundspeed": self.groundspeed,
                "airspeed": self.airspeed,
                "climb": self.climb,
            },
            "attitude": {
                "roll": self.roll,
                "pitch": self.pitch,
                "yaw": self.yaw,
            },
            "battery": {
                "voltage": self.voltage_battery,
                "current": self.current_battery,
                "remaining": self.battery_remaining,
                "temperature": self.battery_temperature,
                "cell_voltages": self.battery_voltages,
            },
            "gps": {
                "fix_type": self.gps_fix_type,
                "satellites": self.gps_satellites,
                "eph": self.gps_eph,
                "epv": self.gps_epv,
            },
            "rc": {
                "channels": self.rc_channels,
                "rssi": self.rc_rssi,
            },
            "throttle": self.throttle,
            "last_heartbeat": self.last_heartbeat,
            "last_update": self.last_update,
        })
    }

    /// The full state-socket payload: the vehicle snapshot with the service's
    /// runtime extras merged on top (`fc_connected`, `service_uptime`, the
    /// param-sweep flags, the `params` blob, ...). The Python producer builds
    /// this in `__main__.py`; the merge order is the same (extras overwrite).
    pub fn to_wire_with(&self, extras: &Map<String, Value>) -> Value {
        let mut wire = self.to_wire();
        if let Value::Object(map) = &mut wire {
            for (k, v) in extras {
                map.insert(k.clone(), v.clone());
            }
        }
        wire
    }
}

/// Trim the trailing NUL padding from a fixed-width MAVLink `param_id` field,
/// matching Python's `msg.param_id.rstrip("\x00")`.
fn param_id_to_string(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::{
        GpsFixType, MavAutopilot, MavBatteryFunction, MavBatteryType, MavModeFlag, MavState,
        MavSysStatusSensor, MavType, BATTERY_STATUS_DATA, GLOBAL_POSITION_INT_DATA,
        GPS_RAW_INT_DATA, HEARTBEAT_DATA, PARAM_VALUE_DATA, RC_CHANNELS_DATA, SYS_STATUS_DATA,
        VFR_HUD_DATA,
    };
    use ados_protocol::mavlink::MavMessage;

    const TS: &str = "2026-05-28T15:28:23.880948+00:00";

    fn heartbeat(mavtype: MavType, custom_mode: u32, armed: bool) -> MavMessage {
        let base_mode = if armed {
            MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
        } else {
            MavModeFlag::empty()
        };
        MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode,
            mavtype,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode,
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        })
    }

    #[test]
    fn heartbeat_sets_mode_armed_and_type() {
        let mut s = VehicleState::default();
        let r = s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 4, true), TS);
        assert!(r.is_none());
        assert_eq!(s.mode, "GUIDED");
        assert!(s.armed);
        assert_eq!(s.last_heartbeat, TS);
        assert_eq!(s.last_update, TS);
        assert_eq!(
            s.autopilot,
            MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA as i64
        );
    }

    #[test]
    fn unknown_mode_falls_back_to_mode_number() {
        let mut s = VehicleState::default();
        // custom_mode 99 is not in the copter table.
        s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 99, false), TS);
        assert_eq!(s.mode, "MODE_99");
        assert!(!s.armed);
    }

    fn heartbeat_px4(mavtype: MavType, custom_mode: u32) -> MavMessage {
        MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode,
            mavtype,
            autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        })
    }

    /// PX4 packs the mode differently (main bits 16-23, sub bits 24-31), so an
    /// AUTO submode must decode via the PX4 path, not the ArduCopter table.
    fn px4_custom(main: u32, sub: u32) -> u32 {
        (sub << 24) | (main << 16)
    }

    #[test]
    fn px4_heartbeat_decodes_base_and_auto_submodes() {
        let mut s = VehicleState::default();
        // POSCTL (main=3, sub=0).
        s.update_from_message(
            &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(3, 0)),
            TS,
        );
        assert_eq!(s.mode, "POSCTL");
        // AUTO.MISSION (main=4, sub=4) -> 0x04040000, wrong on the copter table.
        s.update_from_message(
            &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(4, 4)),
            TS,
        );
        assert_eq!(s.mode, "AUTO.MISSION");
        // AUTO.RTL (main=4, sub=5).
        s.update_from_message(
            &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(4, 5)),
            TS,
        );
        assert_eq!(s.mode, "AUTO.RTL");
        // A PX4 fixed-wing still uses the PX4 path (keyed on autopilot, not type).
        s.update_from_message(
            &heartbeat_px4(MavType::MAV_TYPE_FIXED_WING, px4_custom(4, 4)),
            TS,
        );
        assert_eq!(s.mode, "AUTO.MISSION");
    }

    #[test]
    fn px4_unmapped_submode_falls_back_to_mode_number() {
        let mut s = VehicleState::default();
        // AUTO main (4) with an unmapped sub (99). There is no (4, 0) entry to
        // fall back to, so the mode degrades to MODE_<custom_mode>.
        s.update_from_message(
            &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(4, 99)),
            TS,
        );
        assert_eq!(s.mode, format!("MODE_{}", px4_custom(4, 99)));
    }

    #[test]
    fn plane_and_rover_tables_select_correctly() {
        let mut s = VehicleState::default();
        s.update_from_message(&heartbeat(MavType::MAV_TYPE_FIXED_WING, 5, false), TS);
        assert_eq!(s.mode, "FBWA");
        s.update_from_message(&heartbeat(MavType::MAV_TYPE_GROUND_ROVER, 4, false), TS);
        assert_eq!(s.mode, "HOLD");
    }

    #[test]
    fn global_position_scaling_matches_python() {
        let mut s = VehicleState::default();
        s.update_from_message(
            &MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
                time_boot_ms: 0,
                lat: 129_716_000,
                lon: 775_946_000,
                alt: 120_000,
                relative_alt: 50_000,
                vx: 250,
                vy: -100,
                vz: 30,
                hdg: 18_000,
            }),
            TS,
        );
        assert!((s.lat - 12.9716).abs() < 1e-9);
        assert!((s.lon - 77.5946).abs() < 1e-9);
        assert!((s.alt_msl - 120.0).abs() < 1e-9);
        assert!((s.alt_rel - 50.0).abs() < 1e-9);
        assert!((s.vx - 2.5).abs() < 1e-9);
        assert!((s.heading - 180.0).abs() < 1e-9);
    }

    #[test]
    fn battery_status_filters_unfilled_cells_and_temp_sentinel() {
        let mut s = VehicleState::default();
        let mut voltages = [0xFFFFu16; 10];
        voltages[0] = 4200;
        voltages[1] = 4180;
        s.update_from_message(
            &MavMessage::BATTERY_STATUS(BATTERY_STATUS_DATA {
                current_consumed: 1500,
                energy_consumed: 9000,
                temperature: 0x7FFF,
                voltages,
                current_battery: 0,
                id: 0,
                battery_function: MavBatteryFunction::MAV_BATTERY_FUNCTION_ALL,
                mavtype: MavBatteryType::MAV_BATTERY_TYPE_LIPO,
                battery_remaining: 0,
            }),
            TS,
        );
        assert_eq!(s.battery_voltages, vec![4.2, 4.18]);
        assert_eq!(s.battery_temperature, 0.0);
        assert_eq!(s.battery_current_consumed, 1500);
    }

    #[test]
    fn param_value_returns_persist_tuple_and_trims_nulls() {
        let mut s = VehicleState::default();
        let mut param_id = [0u8; 16];
        param_id[..5].copy_from_slice(b"WPNAV");
        let r = s.update_from_message(
            &MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
                param_value: 1234.5,
                param_count: 700,
                param_index: 1,
                param_id,
                param_type:
                    ados_protocol::mavlink::ardupilotmega::MavParamType::MAV_PARAM_TYPE_REAL32,
            }),
            TS,
        );
        let (name, value, _ptype) = r.expect("PARAM_VALUE yields a persist tuple");
        assert_eq!(name, "WPNAV");
        assert_eq!(value, 1234.5);
        assert_eq!(s.param_count, 700);
        assert_eq!(s.params.get("WPNAV"), Some(&json!(1234.5f32)));
    }

    #[test]
    fn to_wire_has_the_python_to_dict_shape() {
        let mut s = VehicleState::default();
        s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, false), TS);
        let wire = s.to_wire();
        // Top-level keys present, nested groups shaped like to_dict().
        for key in [
            "mav_type",
            "autopilot",
            "armed",
            "mode",
            "position",
            "velocity",
            "attitude",
            "battery",
            "gps",
            "rc",
            "throttle",
            "last_heartbeat",
            "last_update",
        ] {
            assert!(wire.get(key).is_some(), "missing wire key {key}");
        }
        assert_eq!(wire["mode"], json!("STABILIZE"));
        assert!(wire["battery"].get("cell_voltages").is_some());
        assert!(
            wire["attitude"].get("rollspeed").is_none(),
            "to_dict omits rollspeed"
        );
        assert_eq!(wire["rc"]["channels"].as_array().unwrap().len(), 18);
    }

    #[test]
    fn to_wire_with_merges_runtime_extras() {
        let s = VehicleState::default();
        let mut extras = Map::new();
        extras.insert("fc_connected".into(), json!(true));
        extras.insert("param_priming".into(), json!(false));
        extras.insert("service_uptime".into(), json!(12.5));
        let wire = s.to_wire_with(&extras);
        assert_eq!(wire["fc_connected"], json!(true));
        assert_eq!(wire["param_priming"], json!(false));
        assert_eq!(wire["service_uptime"], json!(12.5));
        // Base fields still present.
        assert!(wire.get("position").is_some());
    }

    #[test]
    fn sys_status_and_gps_and_vfr_scaling() {
        let mut s = VehicleState::default();
        s.update_from_message(
            &MavMessage::SYS_STATUS(SYS_STATUS_DATA {
                onboard_control_sensors_present: MavSysStatusSensor::empty(),
                onboard_control_sensors_enabled: MavSysStatusSensor::empty(),
                onboard_control_sensors_health: MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_GYRO,
                load: 500,
                voltage_battery: 16400,
                current_battery: 250,
                drop_rate_comm: 0,
                errors_comm: 0,
                errors_count1: 0,
                errors_count2: 0,
                errors_count3: 0,
                errors_count4: 0,
                battery_remaining: 87,
            }),
            TS,
        );
        assert!((s.voltage_battery - 16.4).abs() < 1e-9);
        assert!((s.current_battery - 2.5).abs() < 1e-9);
        assert_eq!(s.battery_remaining, 87);

        s.update_from_message(
            &MavMessage::GPS_RAW_INT(GPS_RAW_INT_DATA {
                time_usec: 0,
                lat: 0,
                lon: 0,
                alt: 0,
                eph: 150,
                epv: 200,
                vel: 0,
                cog: 0,
                fix_type: GpsFixType::GPS_FIX_TYPE_3D_FIX,
                satellites_visible: 14,
            }),
            TS,
        );
        assert_eq!(s.gps_satellites, 14);
        assert!((s.gps_eph - 1.5).abs() < 1e-9);
        assert_eq!(s.gps_fix_type, GpsFixType::GPS_FIX_TYPE_3D_FIX as i64);

        s.update_from_message(
            &MavMessage::VFR_HUD(VFR_HUD_DATA {
                airspeed: 12.0,
                groundspeed: 11.0,
                alt: 100.0,
                climb: 1.5,
                heading: 90,
                throttle: 55,
            }),
            TS,
        );
        assert_eq!(s.throttle, 55);
        assert!((s.groundspeed - 11.0).abs() < 1e-9);
    }

    #[test]
    fn rc_channels_capture_all_18_and_rssi() {
        let mut s = VehicleState::default();
        s.update_from_message(
            &MavMessage::RC_CHANNELS(RC_CHANNELS_DATA {
                time_boot_ms: 0,
                chan1_raw: 1500,
                chan2_raw: 1501,
                chan3_raw: 1000,
                chan4_raw: 1500,
                chan5_raw: 1,
                chan6_raw: 2,
                chan7_raw: 3,
                chan8_raw: 4,
                chan9_raw: 5,
                chan10_raw: 6,
                chan11_raw: 7,
                chan12_raw: 8,
                chan13_raw: 9,
                chan14_raw: 10,
                chan15_raw: 11,
                chan16_raw: 12,
                chan17_raw: 13,
                chan18_raw: 14,
                chancount: 18,
                rssi: 200,
            }),
            TS,
        );
        assert_eq!(s.rc_channels.len(), 18);
        assert_eq!(s.rc_channels[0], 1500);
        assert_eq!(s.rc_channels[17], 14);
        assert_eq!(s.rc_rssi, 200);
    }
}
