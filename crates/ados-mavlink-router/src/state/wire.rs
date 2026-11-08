//! The wire snapshot of the vehicle state.

use serde_json::{json, Map, Value};

use super::*;

impl VehicleState {
    /// The vehicle-derived wire snapshot.
    ///
    /// `position_age_ms` is evaluated against the clock at call time, so this is
    /// a snapshot-at-an-instant rather than a pure projection of the struct.
    /// That is the point: the age has to describe the moment the frame is
    /// published, because the consumer that reads it is deciding whether this
    /// node's fix is alive enough to broadcast to the fleet.
    pub fn to_wire(&self) -> Value {
        // `null`, never a number, when no position has ever been decoded. A
        // sentinel like 0 would read as "fixed a moment ago" — the single worst
        // possible reading, since it is the one a freshness gate passes.
        let position_age_ms = self.position_at.map(age_ms);
        // Both blocks are `null` until their message has been decoded once, and
        // carry an age so a reader can gate on freshness, as for the position.
        let ekf = match (self.ekf_flags, self.ekf_at) {
            (Some(flags), Some(at)) => json!({ "flags": flags, "age_ms": age_ms(at) }),
            _ => Value::Null,
        };
        let local_position = match (self.local_ned, self.local_at) {
            (Some([x, y, z, vx, vy, vz]), Some(at)) => json!({
                "x": x, "y": y, "z": z, "vx": vx, "vy": vy, "vz": vz,
                "age_ms": age_ms(at),
            }),
            _ => Value::Null,
        };
        let home = match self.home {
            Some((lat_e7, lon_e7, alt, [x, y, z])) => json!({
                "lat_e7": lat_e7, "lon_e7": lon_e7, "alt_msl_m": alt, "local_ned": [x, y, z],
            }),
            None => Value::Null,
        };
        let gimbal = match self.gimbal {
            Some(g) => json!({
                "pitch_deg": g.pitch_deg, "yaw_deg": g.yaw_deg,
                "earth_frame": g.earth_frame, "age_ms": age_ms(g.at),
            }),
            None => Value::Null,
        };
        let now = std::time::Instant::now();
        let prearm: Vec<&str> = self
            .prearm
            .iter()
            .filter(|(at, _)| now.saturating_duration_since(*at) < PREARM_WINDOW)
            .map(|(_, t)| t.as_str())
            .collect();
        json!({
            "mav_type": self.mav_type,
            "autopilot": self.autopilot,
            // The firmware the banner named ("copter" / "plane" / "rover"), or
            // null until one has been seen. The command route encodes flight
            // modes from this, never from `mav_type` alone.
            "vehicle_firmware": self.firmware.map(ArduPilotFirmware::as_str),
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
                "time_boot_ms": self.attitude_time_boot_ms,
            },
            "battery": {
                "voltage": self.voltage_battery,
                "current": self.current_battery,
                "remaining": self.battery_remaining,
                "temperature": self.battery_temperature,
                "cell_voltages": self.battery_voltages,
            },
            // One entry per BATTERY_STATUS id, ordered by id. `battery` above
            // stays the single-pack view every existing reader projects.
            "batteries": self.battery_packs.values().map(BatteryPack::to_wire).collect::<Vec<_>>(),
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
            // Milliseconds since the last decoded POSITION, or `null` when none
            // has ever arrived. Separate from `last_update`, which every frame
            // refreshes: a vehicle whose GPS died but whose heartbeat continues
            // has a fresh `last_update` and an ever-growing `position_age_ms`,
            // and only the second one catches it.
            "position_age_ms": position_age_ms,
            "ekf": ekf,
            "local_position": local_position,
            "home": home,
            "gimbal": gimbal,
            // The mission state and mirror summary, `null` until the mission
            // mirror has run (see `mission_mirror`).
            "mission": self.mission.clone().unwrap_or(Value::Null),
            // PreArm/Arm STATUSTEXT seen in the last 10 s, oldest first.
            "prearm": prearm,
        })
    }

    /// The full state-socket payload: the vehicle snapshot with the service's
    /// runtime extras merged on top (`fc_connected`, `service_uptime`, the
    /// param-sweep flags, `param_generation`, ...). Extras overwrite vehicle
    /// fields on a key clash.
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

/// Milliseconds since `at` on the monotonic clock, evaluated now.
fn age_ms(at: std::time::Instant) -> u64 {
    std::time::Instant::now()
        .saturating_duration_since(at)
        .as_millis() as u64
}
