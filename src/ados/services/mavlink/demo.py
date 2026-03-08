"""Demo FC connection — simulated telemetry for testing without hardware."""

from __future__ import annotations

import asyncio
import math
import time
from datetime import UTC, datetime

from ados.core.logging import get_logger
from ados.services.mavlink.state import VehicleState

log = get_logger("demo-fc")

# Bangalore center
_CENTER_LAT = 12.9716
_CENTER_LON = 77.5946
_CIRCLE_RADIUS = 0.001  # ~111m
_REVOLUTION_PERIOD = 60.0  # seconds per full circle
_BASE_ALT = 50.0
_ALT_OSCILLATION = 3.0


class DemoFCConnection:
    """Fake flight controller that generates circular flight telemetry."""

    def __init__(self, state: VehicleState) -> None:
        self._state = state
        self._subscribers: list[asyncio.Queue] = []

    # --- Same interface as FCConnection ---

    @property
    def connected(self) -> bool:
        return True

    @property
    def port(self) -> str:
        return "demo"

    @property
    def baud(self) -> int:
        return 0

    @property
    def connection(self):
        return None

    def subscribe(self) -> asyncio.Queue:
        q: asyncio.Queue = asyncio.Queue(maxsize=256)
        self._subscribers.append(q)
        return q

    def unsubscribe(self, q: asyncio.Queue) -> None:
        try:
            self._subscribers.remove(q)
        except ValueError:
            pass

    def send_bytes(self, data: bytes) -> None:
        pass

    def send_heartbeat(self) -> None:
        pass

    # --- Telemetry generation ---

    async def run(self) -> None:
        """Generate fake telemetry at 10 Hz."""
        log.info("demo_start", msg="Demo FC running — circling over Bangalore")
        t0 = time.monotonic()
        start_battery = 95
        start_voltage = 25.2

        while True:
            elapsed = time.monotonic() - t0
            angle = (2.0 * math.pi * elapsed) / _REVOLUTION_PERIOD
            now = datetime.now(UTC).isoformat()

            s = self._state

            # Position — slow circle
            s.lat = _CENTER_LAT + _CIRCLE_RADIUS * math.cos(angle)
            s.lon = _CENTER_LON + _CIRCLE_RADIUS * math.sin(angle)
            s.alt_rel = _BASE_ALT + _ALT_OSCILLATION * math.sin(elapsed * 0.3)
            s.alt_msl = s.alt_rel + 920.0  # Bangalore elevation ~920m

            # Heading follows circle tangent (degrees)
            heading_rad = angle + math.pi / 2.0
            s.heading = math.degrees(heading_rad) % 360.0

            # Attitude — gentle oscillation
            s.roll = 0.05 * math.sin(elapsed * 1.1)
            s.pitch = 0.05 * math.cos(elapsed * 0.9)
            s.yaw = math.radians(s.heading)

            # Velocity
            s.groundspeed = 2.0
            s.airspeed = 2.1
            s.vx = s.groundspeed * math.cos(heading_rad)
            s.vy = s.groundspeed * math.sin(heading_rad)
            s.vz = _ALT_OSCILLATION * 0.3 * math.cos(elapsed * 0.3)
            s.climb = -s.vz

            # Battery — drains 1% per 60s
            drain_pct = elapsed / 60.0
            s.battery_remaining = max(0, int(start_battery - drain_pct))
            s.voltage_battery = max(0.0, start_voltage * (s.battery_remaining / 100.0))
            s.current_battery = 4.2
            s.battery_temperature = 32.5

            # GPS
            s.gps_fix_type = 3
            s.gps_satellites = 14
            s.gps_eph = 1.2
            s.gps_epv = 1.8

            # Mode / arming
            s.mode = "LOITER"
            s.armed = True
            s.mav_type = 2  # MAV_TYPE_QUADROTOR
            s.autopilot = 3  # MAV_AUTOPILOT_ARDUPILOTMEGA
            s.base_mode = 209  # armed + custom + guided + stabilize + manual
            s.custom_mode = 5  # LOITER
            s.system_status = 4  # MAV_STATE_ACTIVE

            # RC
            s.rc_channels = [1500] * 18
            s.rc_rssi = 200
            s.throttle = 45

            # Timestamps
            s.last_heartbeat = now
            s.last_update = now

            await asyncio.sleep(0.1)
