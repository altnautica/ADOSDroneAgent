//! Synthetic flight-controller telemetry for hardware-free runs and the
//! side-by-side parity harness.
//!
//! Reproduces the circular flight the Python demo source generates
//! (services/mavlink/demo.py): a slow circle over a fixed point, a gentle
//! attitude wobble, a draining battery, and a steady GPS lock. The router's
//! demo mode builds these MAVLink frames at 10 Hz and pushes them through the
//! same frame fan-out, vehicle-state, and proxy paths a real serial FC drives,
//! so a ground station can connect with no hardware attached and the parity
//! harness has a deterministic source to compare the two implementations
//! against.

use std::f64::consts::PI;

use ados_protocol::mavlink::ardupilotmega::{
    GpsFixType, MavAutopilot, MavBatteryFunction, MavBatteryType, MavMessage, MavModeFlag,
    MavState, MavSysStatusSensor, MavType, ATTITUDE_DATA, BATTERY_STATUS_DATA,
    GLOBAL_POSITION_INT_DATA, GPS_RAW_INT_DATA, HEARTBEAT_DATA, RC_CHANNELS_DATA, SYS_STATUS_DATA,
    VFR_HUD_DATA,
};

// Flight-path constants, identical to the Python demo source.
const CENTER_LAT: f64 = 12.9716;
const CENTER_LON: f64 = 77.5946;
const CIRCLE_RADIUS: f64 = 0.001; // ~111 m
const REVOLUTION_PERIOD: f64 = 60.0; // seconds per full circle
const BASE_ALT: f64 = 50.0;
const ALT_OSCILLATION: f64 = 3.0;
const BANGALORE_ELEVATION: f64 = 920.0;
const START_BATTERY: f64 = 95.0;
const START_VOLTAGE: f64 = 25.2;
const DEMO_GROUNDSPEED: f64 = 2.0;
const DEMO_AIRSPEED: f64 = 2.1;
const DEMO_CURRENT: f64 = 4.2;
const DEMO_BATTERY_TEMP_C: f64 = 32.5;
const DEMO_THROTTLE: u16 = 45;

/// MAVLink system/component id carried by the synthetic vehicle frames. A real
/// ArduPilot autopilot is system 1, component 1; the companion heartbeat keeps
/// its own (191) identity on a separate send path.
pub const DEMO_SYSTEM_ID: u8 = 1;
pub const DEMO_COMPONENT_ID: u8 = 1;

/// The flight-state targets for one tick, the same values the Python demo writes
/// directly onto the vehicle state. The message builders below encode these into
/// MAVLink frames so the state derived through the normal decode path reproduces
/// them (within the integer field scaling each message uses).
#[derive(Debug, Clone, PartialEq)]
pub struct DemoSample {
    pub lat: f64,
    pub lon: f64,
    pub alt_rel: f64,
    pub alt_msl: f64,
    pub heading: f64,
    pub roll: f64,
    pub pitch: f64,
    pub yaw: f64,
    pub vx: f64,
    pub vy: f64,
    pub vz: f64,
    pub climb: f64,
    pub groundspeed: f64,
    pub airspeed: f64,
    pub battery_remaining: i32,
    pub voltage: f64,
    pub current: f64,
    pub battery_temperature: f64,
    pub throttle: u16,
}

/// Compute the flight-state targets at elapsed time `t` (seconds). Mirrors the
/// arithmetic in services/mavlink/demo.py exactly.
pub fn sample_at(t: f64) -> DemoSample {
    let angle = (2.0 * PI * t) / REVOLUTION_PERIOD;
    let lat = CENTER_LAT + CIRCLE_RADIUS * angle.cos();
    let lon = CENTER_LON + CIRCLE_RADIUS * angle.sin();
    let alt_rel = BASE_ALT + ALT_OSCILLATION * (t * 0.3).sin();
    let alt_msl = alt_rel + BANGALORE_ELEVATION;
    let heading_rad = angle + PI / 2.0;
    let heading = heading_rad.to_degrees().rem_euclid(360.0);
    let roll = 0.05 * (t * 1.1).sin();
    let pitch = 0.05 * (t * 0.9).cos();
    let yaw = heading.to_radians();
    let vx = DEMO_GROUNDSPEED * heading_rad.cos();
    let vy = DEMO_GROUNDSPEED * heading_rad.sin();
    let vz = ALT_OSCILLATION * 0.3 * (t * 0.3).cos();
    let climb = -vz;
    let drain_pct = t / 60.0;
    let battery_remaining = (START_BATTERY - drain_pct).trunc().max(0.0) as i32;
    let voltage = (START_VOLTAGE * (battery_remaining as f64 / 100.0)).max(0.0);
    DemoSample {
        lat,
        lon,
        alt_rel,
        alt_msl,
        heading,
        roll,
        pitch,
        yaw,
        vx,
        vy,
        vz,
        climb,
        groundspeed: DEMO_GROUNDSPEED,
        airspeed: DEMO_AIRSPEED,
        battery_remaining,
        voltage,
        current: DEMO_CURRENT,
        battery_temperature: DEMO_BATTERY_TEMP_C,
        throttle: DEMO_THROTTLE,
    }
}

/// Build the eight MAVLink messages for elapsed time `t` (seconds): HEARTBEAT,
/// GLOBAL_POSITION_INT, ATTITUDE, SYS_STATUS, GPS_RAW_INT, VFR_HUD,
/// BATTERY_STATUS, RC_CHANNELS. The set and the per-message scaling match the
/// fields the Python demo drives onto the vehicle state, so the snapshot the
/// router publishes is shape- and value-compatible with the Python demo's.
pub fn demo_messages(t: f64) -> Vec<MavMessage> {
    let s = sample_at(t);
    let time_boot_ms = (t * 1000.0) as u32;
    let hdg_cdeg = (s.heading * 100.0) as u16;
    let voltages = [0xFFFFu16; 10]; // all "unfilled" so per-cell list stays empty

    vec![
        MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 5, // LOITER
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::from_bits_truncate(209),
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        }),
        MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
            time_boot_ms,
            lat: (s.lat * 1e7) as i32,
            lon: (s.lon * 1e7) as i32,
            alt: (s.alt_msl * 1000.0) as i32,
            relative_alt: (s.alt_rel * 1000.0) as i32,
            vx: (s.vx * 100.0) as i16,
            vy: (s.vy * 100.0) as i16,
            vz: (s.vz * 100.0) as i16,
            hdg: hdg_cdeg,
        }),
        MavMessage::ATTITUDE(ATTITUDE_DATA {
            time_boot_ms,
            roll: s.roll as f32,
            pitch: s.pitch as f32,
            yaw: s.yaw as f32,
            rollspeed: 0.0,
            pitchspeed: 0.0,
            yawspeed: 0.0,
        }),
        MavMessage::SYS_STATUS(SYS_STATUS_DATA {
            onboard_control_sensors_present: MavSysStatusSensor::empty(),
            onboard_control_sensors_enabled: MavSysStatusSensor::empty(),
            onboard_control_sensors_health: MavSysStatusSensor::empty(),
            load: 500,
            voltage_battery: (s.voltage * 1000.0) as u16,
            current_battery: (s.current * 100.0) as i16,
            drop_rate_comm: 0,
            errors_comm: 0,
            errors_count1: 0,
            errors_count2: 0,
            errors_count3: 0,
            errors_count4: 0,
            battery_remaining: s.battery_remaining as i8,
        }),
        MavMessage::GPS_RAW_INT(GPS_RAW_INT_DATA {
            time_usec: (t * 1e6) as u64,
            lat: (s.lat * 1e7) as i32,
            lon: (s.lon * 1e7) as i32,
            alt: (s.alt_msl * 1000.0) as i32,
            eph: 120,
            epv: 180,
            vel: (s.groundspeed * 100.0) as u16,
            cog: hdg_cdeg,
            fix_type: GpsFixType::GPS_FIX_TYPE_3D_FIX,
            satellites_visible: 14,
        }),
        MavMessage::VFR_HUD(VFR_HUD_DATA {
            airspeed: s.airspeed as f32,
            groundspeed: s.groundspeed as f32,
            alt: s.alt_rel as f32,
            climb: s.climb as f32,
            heading: s.heading as i16,
            throttle: s.throttle,
        }),
        MavMessage::BATTERY_STATUS(BATTERY_STATUS_DATA {
            current_consumed: 0,
            energy_consumed: 0,
            temperature: (s.battery_temperature * 100.0) as i16,
            voltages,
            current_battery: (s.current * 100.0) as i16,
            id: 0,
            battery_function: MavBatteryFunction::MAV_BATTERY_FUNCTION_ALL,
            mavtype: MavBatteryType::MAV_BATTERY_TYPE_LIPO,
            battery_remaining: s.battery_remaining as i8,
        }),
        MavMessage::RC_CHANNELS(RC_CHANNELS_DATA {
            time_boot_ms,
            chan1_raw: 1500,
            chan2_raw: 1500,
            chan3_raw: 1500,
            chan4_raw: 1500,
            chan5_raw: 1500,
            chan6_raw: 1500,
            chan7_raw: 1500,
            chan8_raw: 1500,
            chan9_raw: 1500,
            chan10_raw: 1500,
            chan11_raw: 1500,
            chan12_raw: 1500,
            chan13_raw: 1500,
            chan14_raw: 1500,
            chan15_raw: 1500,
            chan16_raw: 1500,
            chan17_raw: 1500,
            chan18_raw: 1500,
            chancount: 18,
            rssi: 200,
        }),
    ]
}

#[cfg(test)]
mod tests;
