"""Vehicle state aggregator — parses MAVLink messages into a unified state."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone

from pymavlink import mavutil

# ArduCopter custom_mode -> mode name mapping
_COPTER_MODES: dict[int, str] = {
    0: "STABILIZE", 1: "ACRO", 2: "ALT_HOLD", 3: "AUTO",
    4: "GUIDED", 5: "LOITER", 6: "RTL", 7: "CIRCLE",
    9: "LAND", 11: "DRIFT", 13: "SPORT", 14: "FLIP",
    15: "AUTOTUNE", 16: "POSHOLD", 17: "BRAKE", 18: "THROW",
    19: "AVOID_ADSB", 20: "GUIDED_NOGPS", 21: "SMART_RTL",
    22: "FLOWHOLD", 23: "FOLLOW", 24: "ZIGZAG", 25: "SYSTEMID",
    26: "AUTOROTATE", 27: "AUTO_RTL",
}

# ArduPlane custom_mode -> mode name mapping
_PLANE_MODES: dict[int, str] = {
    0: "MANUAL", 1: "CIRCLE", 2: "STABILIZE", 3: "TRAINING",
    4: "ACRO", 5: "FBWA", 6: "FBWB", 7: "CRUISE",
    8: "AUTOTUNE", 10: "AUTO", 11: "RTL", 12: "LOITER",
    14: "AVOID_ADSB", 15: "GUIDED", 17: "QSTABILIZE",
    18: "QHOVER", 19: "QLOITER", 20: "QLAND", 21: "QRTL",
    22: "QAUTOTUNE", 23: "QACRO", 24: "THERMAL",
    25: "LOITER_ALT_QLAND",
}

# ArduRover custom_mode -> mode name mapping
_ROVER_MODES: dict[int, str] = {
    0: "MANUAL", 1: "ACRO", 3: "STEERING", 4: "HOLD",
    5: "LOITER", 6: "FOLLOW", 7: "SIMPLE",
    10: "AUTO", 11: "RTL", 12: "SMART_RTL", 15: "GUIDED",
}

# MAV_TYPE -> mode map
_MODE_MAPS: dict[int, dict[int, str]] = {
    mavutil.mavlink.MAV_TYPE_QUADROTOR: _COPTER_MODES,
    mavutil.mavlink.MAV_TYPE_HEXAROTOR: _COPTER_MODES,
    mavutil.mavlink.MAV_TYPE_OCTOROTOR: _COPTER_MODES,
    mavutil.mavlink.MAV_TYPE_HELICOPTER: _COPTER_MODES,
    mavutil.mavlink.MAV_TYPE_TRICOPTER: _COPTER_MODES,
    mavutil.mavlink.MAV_TYPE_COAXIAL: _COPTER_MODES,
    mavutil.mavlink.MAV_TYPE_FIXED_WING: _PLANE_MODES,
    mavutil.mavlink.MAV_TYPE_VTOL_QUADROTOR: _PLANE_MODES,
    mavutil.mavlink.MAV_TYPE_VTOL_TILTROTOR: _PLANE_MODES,
    mavutil.mavlink.MAV_TYPE_GROUND_ROVER: _ROVER_MODES,
    mavutil.mavlink.MAV_TYPE_SURFACE_BOAT: _ROVER_MODES,
}


@dataclass
class VehicleState:
    # HEARTBEAT
    mav_type: int = 0
    autopilot: int = 0
    base_mode: int = 0
    custom_mode: int = 0
    system_status: int = 0
    armed: bool = False
    mode: str = ""

    # GLOBAL_POSITION_INT
    lat: float = 0.0
    lon: float = 0.0
    alt_msl: float = 0.0
    alt_rel: float = 0.0
    vx: float = 0.0
    vy: float = 0.0
    vz: float = 0.0
    heading: float = 0.0

    # ATTITUDE
    roll: float = 0.0
    pitch: float = 0.0
    yaw: float = 0.0
    rollspeed: float = 0.0
    pitchspeed: float = 0.0
    yawspeed: float = 0.0

    # SYS_STATUS
    voltage_battery: float = 0.0
    current_battery: float = 0.0
    battery_remaining: int = -1
    sensors_health: int = 0

    # GPS_RAW_INT
    gps_fix_type: int = 0
    gps_satellites: int = 0
    gps_eph: float = 0.0
    gps_epv: float = 0.0

    # VFR_HUD
    airspeed: float = 0.0
    groundspeed: float = 0.0
    climb: float = 0.0
    throttle: int = 0

    # BATTERY_STATUS
    battery_temperature: float = 0.0
    battery_voltages: list[float] = field(default_factory=list)
    battery_current_consumed: int = 0
    battery_energy_consumed: int = 0

    # RC_CHANNELS
    rc_channels: list[int] = field(default_factory=lambda: [0] * 18)
    rc_rssi: int = 0

    # Timestamps
    last_heartbeat: str = ""
    last_update: str = ""

    # Param cache (in-memory)
    params: dict[str, float] = field(default_factory=dict)
    param_count: int = 0

    # Optional persistent param cache (set externally by AgentApp)
    param_cache: object = field(default=None, repr=False)

    def update_from_message(self, msg) -> None:
        """Update state from a pymavlink message."""
        msg_type = msg.get_type()
        now = datetime.now(timezone.utc).isoformat()
        self.last_update = now

        if msg_type == "HEARTBEAT":
            self.mav_type = msg.type
            self.autopilot = msg.autopilot
            self.base_mode = msg.base_mode
            self.custom_mode = msg.custom_mode
            self.system_status = msg.system_status
            self.armed = bool(msg.base_mode & 128)
            self.last_heartbeat = now
            # Map custom_mode to human-readable mode name
            mode_map = _MODE_MAPS.get(msg.type, {})
            self.mode = mode_map.get(msg.custom_mode, f"MODE_{msg.custom_mode}")

        elif msg_type == "GLOBAL_POSITION_INT":
            self.lat = msg.lat / 1e7
            self.lon = msg.lon / 1e7
            self.alt_msl = msg.alt / 1000.0
            self.alt_rel = msg.relative_alt / 1000.0
            self.vx = msg.vx / 100.0
            self.vy = msg.vy / 100.0
            self.vz = msg.vz / 100.0
            self.heading = msg.hdg / 100.0

        elif msg_type == "ATTITUDE":
            self.roll = msg.roll
            self.pitch = msg.pitch
            self.yaw = msg.yaw
            self.rollspeed = msg.rollspeed
            self.pitchspeed = msg.pitchspeed
            self.yawspeed = msg.yawspeed

        elif msg_type == "SYS_STATUS":
            self.voltage_battery = msg.voltage_battery / 1000.0
            self.current_battery = msg.current_battery / 100.0
            self.battery_remaining = msg.battery_remaining
            self.sensors_health = msg.onboard_control_sensors_health

        elif msg_type == "GPS_RAW_INT":
            self.gps_fix_type = msg.fix_type
            self.gps_satellites = msg.satellites_visible
            self.gps_eph = msg.eph / 100.0
            self.gps_epv = msg.epv / 100.0

        elif msg_type == "VFR_HUD":
            self.airspeed = msg.airspeed
            self.groundspeed = msg.groundspeed
            self.climb = msg.climb
            self.throttle = msg.throttle

        elif msg_type == "BATTERY_STATUS":
            self.battery_temperature = msg.temperature / 100.0 if msg.temperature != 0x7FFF else 0
            self.battery_voltages = [v / 1000.0 for v in msg.voltages if v != 0xFFFF]
            self.battery_current_consumed = msg.current_consumed
            self.battery_energy_consumed = msg.energy_consumed

        elif msg_type == "RC_CHANNELS":
            self.rc_channels = [
                msg.chan1_raw, msg.chan2_raw, msg.chan3_raw, msg.chan4_raw,
                msg.chan5_raw, msg.chan6_raw, msg.chan7_raw, msg.chan8_raw,
                msg.chan9_raw, msg.chan10_raw, msg.chan11_raw, msg.chan12_raw,
                msg.chan13_raw, msg.chan14_raw, msg.chan15_raw, msg.chan16_raw,
                msg.chan17_raw, msg.chan18_raw,
            ]
            self.rc_rssi = msg.rssi

        elif msg_type == "PARAM_VALUE":
            param_name = msg.param_id.rstrip("\x00")
            self.params[param_name] = msg.param_value
            self.param_count = msg.param_count
            # Persist to ParamCache if wired
            if self.param_cache is not None:
                self.param_cache.set(param_name, msg.param_value, getattr(msg, "param_type", 0))

    def update_from_dict(self, d: dict) -> None:
        """Update fields from a dict (e.g., from IPC state snapshot)."""
        if "armed" in d:
            self.armed = d["armed"]
        if "mode" in d:
            self.mode = d["mode"]
        if "mav_type" in d:
            self.mav_type = d["mav_type"]
        pos = d.get("position", {})
        if pos:
            self.lat = pos.get("lat", self.lat)
            self.lon = pos.get("lon", self.lon)
            self.alt_msl = pos.get("alt_msl", self.alt_msl)
            self.alt_rel = pos.get("alt_rel", self.alt_rel)
            self.heading = pos.get("heading", self.heading)
        vel = d.get("velocity", {})
        if vel:
            self.groundspeed = vel.get("groundspeed", self.groundspeed)
            self.airspeed = vel.get("airspeed", self.airspeed)
            self.climb = vel.get("climb", self.climb)
        att = d.get("attitude", {})
        if att:
            self.roll = att.get("roll", self.roll)
            self.pitch = att.get("pitch", self.pitch)
            self.yaw = att.get("yaw", self.yaw)
        bat = d.get("battery", {})
        if bat:
            self.voltage_battery = bat.get("voltage", self.voltage_battery)
            self.current_battery = bat.get("current", self.current_battery)
            self.battery_remaining = bat.get("remaining", self.battery_remaining)
        gps = d.get("gps", {})
        if gps:
            self.gps_fix_type = gps.get("fix_type", self.gps_fix_type)
            self.gps_satellites = gps.get("satellites", self.gps_satellites)
        if "last_heartbeat" in d:
            self.last_heartbeat = d["last_heartbeat"]
        if "last_update" in d:
            self.last_update = d["last_update"]
        # Cloud/system fields (not part of MAVLink, but used by cloud heartbeat)
        if "fc_connected" in d:
            self.fc_connected = d.get("fc_connected", False)

    def to_dict(self) -> dict:
        """Serialize to dictionary for REST API."""
        return {
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
        }
