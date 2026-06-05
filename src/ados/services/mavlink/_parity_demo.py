"""Frame-emitting synthetic flight controller for the parity test harness.

Dev/test only. This module is NOT used by the production demo path (``ados
demo`` via the agent app) or by the systemd-launched MAVLink service. It is
selected solely by the ``--demo`` flag on ``python -m ados.services.mavlink``,
which the side-by-side parity harness (``tools/mavlink-parity/``) uses to run a
hardware-free comparison against the Rust router's demo mode.

``DemoFCConnection`` (demo.py) writes the vehicle state directly and emits no
frames. This source instead builds the same eight MAVLink frames a real FC
streams and pushes them BOTH to the subscriber queues AND through
``VehicleState.update_from_message``, so the frame fan-out and the 10 Hz state
snapshot can be compared frame-for-frame and field-for-field against the Rust
implementation, which drives its state through the same decode path.
"""

from __future__ import annotations

import asyncio
import math
import time
from datetime import datetime, timezone

from pymavlink.dialects.v20 import ardupilotmega as _ap

from ados.core.logging import get_logger
from ados.services.mavlink.state import VehicleState

log = get_logger("parity-demo-fc")

# Flight-path constants, identical to demo.py and the Rust demo source.
_CENTER_LAT = 12.9716
_CENTER_LON = 77.5946
_CIRCLE_RADIUS = 0.001  # ~111 m
_REVOLUTION_PERIOD = 60.0  # seconds per full circle
_BASE_ALT = 50.0
_ALT_OSCILLATION = 3.0
_BANGALORE_ELEVATION = 920.0
_START_BATTERY = 95
_START_VOLTAGE = 25.2
_GROUNDSPEED = 2.0
_AIRSPEED = 2.1
_CURRENT = 4.2
_BATTERY_TEMP_C = 32.5
_THROTTLE = 45

# Vehicle source identity for the synthetic frames (a real ArduPilot autopilot
# is system 1, component 1). The companion heartbeat keeps its own identity.
_DEMO_SYSTEM_ID = 1
_DEMO_COMPONENT_ID = 1

# Window after which a sweep with no progress is reported as timed out (mirrors
# the production FCConnection deadline so the priming/timeout flags compare).
_PARAM_SWEEP_DEADLINE_S = 30.0


class ParityDemoFC:
    """A flight-controller stand-in that streams synthetic telemetry frames.

    Implements the subset of the ``FCConnection`` interface the standalone
    MAVLink service uses: ``subscribe``/``unsubscribe``, ``send_bytes``,
    ``send_heartbeat``, ``connected``/``port``/``baud``, ``run``, and the
    param-sweep flags read by the state-publish loop.
    """

    def __init__(self, state: VehicleState) -> None:
        self._state = state
        self._subscribers: list[asyncio.Queue[bytes]] = []
        # Encoder for the vehicle frames. file=None: we pack to bytes rather
        # than write to a transport.
        self._mav = _ap.MAVLink(
            None, srcSystem=_DEMO_SYSTEM_ID, srcComponent=_DEMO_COMPONENT_ID
        )
        self._mav.seq = 0
        # Commands a proxy/IPC client sends "to the FC" land here, so the
        # harness can confirm a proxy forwarded a command even with no real FC.
        self.commands_received: list[bytes] = []
        # Param-sweep priming state (mirrors FCConnection so the flags compare).
        self._param_sweep_at: float = 0.0
        self._param_priming: bool = False
        self._param_sweep_timed_out: bool = False
        self._param_sweep_send_failed: bool = False

    # --- FCConnection-compatible surface ---

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

    def subscribe(self) -> asyncio.Queue[bytes]:
        q: asyncio.Queue[bytes] = asyncio.Queue(maxsize=256)
        self._subscribers.append(q)
        return q

    def unsubscribe(self, q: asyncio.Queue[bytes]) -> None:
        try:
            self._subscribers.remove(q)
        except ValueError:
            pass

    def send_bytes(self, data: bytes) -> None:
        # No real FC to write to; record the bytes so the harness can verify a
        # proxy forwarded a command toward the FC.
        self.commands_received.append(bytes(data))

    def send_heartbeat(self) -> None:
        # The companion heartbeat goes agent->FC; with no FC it is a no-op,
        # matching the Rust router's demo behaviour (its writer is absent).
        pass

    @property
    def param_priming(self) -> bool:
        return self._param_priming

    @property
    def param_sweep_timed_out(self) -> bool:
        return self._param_sweep_timed_out

    @property
    def param_sweep_send_failed(self) -> bool:
        return self._param_sweep_send_failed

    def note_param_progress(self, cached: int, expected: int) -> None:
        """Clear priming when the cache catches up, else time out at the deadline."""
        if expected > 0 and cached >= expected:
            self._param_priming = False
            self._param_sweep_timed_out = False
            return
        if self._param_priming and self._param_sweep_at:
            elapsed = time.monotonic() - self._param_sweep_at
            if elapsed >= _PARAM_SWEEP_DEADLINE_S and cached == 0:
                self._param_priming = False
                self._param_sweep_timed_out = True

    # --- Telemetry generation ---

    def _build_messages(self, t: float) -> list:
        """Build the eight telemetry messages for elapsed time ``t`` (seconds)."""
        angle = (2.0 * math.pi * t) / _REVOLUTION_PERIOD
        lat = _CENTER_LAT + _CIRCLE_RADIUS * math.cos(angle)
        lon = _CENTER_LON + _CIRCLE_RADIUS * math.sin(angle)
        alt_rel = _BASE_ALT + _ALT_OSCILLATION * math.sin(t * 0.3)
        alt_msl = alt_rel + _BANGALORE_ELEVATION
        heading_rad = angle + math.pi / 2.0
        heading = math.degrees(heading_rad) % 360.0
        roll = 0.05 * math.sin(t * 1.1)
        pitch = 0.05 * math.cos(t * 0.9)
        yaw = math.radians(heading)
        vx = _GROUNDSPEED * math.cos(heading_rad)
        vy = _GROUNDSPEED * math.sin(heading_rad)
        vz = _ALT_OSCILLATION * 0.3 * math.cos(t * 0.3)
        climb = -vz
        drain_pct = t / 60.0
        remaining = max(0, int(_START_BATTERY - drain_pct))
        voltage = max(0.0, _START_VOLTAGE * (remaining / 100.0))

        time_boot_ms = int(t * 1000.0)
        hdg_cdeg = int(heading * 100.0)
        m = self._mav

        return [
            m.heartbeat_encode(
                _ap.MAV_TYPE_QUADROTOR,
                _ap.MAV_AUTOPILOT_ARDUPILOTMEGA,
                209,  # base_mode
                5,  # custom_mode -> LOITER
                _ap.MAV_STATE_ACTIVE,
                3,  # mavlink_version
            ),
            m.global_position_int_encode(
                time_boot_ms,
                int(lat * 1e7),
                int(lon * 1e7),
                int(alt_msl * 1000.0),
                int(alt_rel * 1000.0),
                int(vx * 100.0),
                int(vy * 100.0),
                int(vz * 100.0),
                hdg_cdeg,
            ),
            m.attitude_encode(time_boot_ms, roll, pitch, yaw, 0.0, 0.0, 0.0),
            m.sys_status_encode(
                0,  # sensors present
                0,  # sensors enabled
                0,  # sensors health
                500,  # load
                int(voltage * 1000.0),
                int(_CURRENT * 100.0),
                remaining,
                0,  # drop_rate_comm
                0,  # errors_comm
                0,
                0,
                0,
                0,
            ),
            m.gps_raw_int_encode(
                int(t * 1e6),
                3,  # fix_type 3D
                int(lat * 1e7),
                int(lon * 1e7),
                int(alt_msl * 1000.0),
                120,  # eph
                180,  # epv
                int(_GROUNDSPEED * 100.0),
                hdg_cdeg,
                14,  # satellites
            ),
            m.vfr_hud_encode(
                _AIRSPEED,
                _GROUNDSPEED,
                int(heading),
                _THROTTLE,
                alt_rel,
                climb,
            ),
            m.battery_status_encode(
                0,  # id
                _ap.MAV_BATTERY_FUNCTION_ALL,
                _ap.MAV_BATTERY_TYPE_LIPO,
                int(_BATTERY_TEMP_C * 100.0),
                [0xFFFF] * 10,  # cell voltages: all "unfilled"
                int(_CURRENT * 100.0),
                0,  # current_consumed
                0,  # energy_consumed
                remaining,
            ),
            m.rc_channels_encode(
                time_boot_ms,
                18,  # chancount
                *([1500] * 18),
                200,  # rssi
            ),
        ]

    async def run(self) -> None:
        """Generate the eight telemetry frames at 10 Hz."""
        log.info("parity_demo_start", msg="Parity demo FC — circling over Bangalore")
        # A sweep is "in flight" for the run's lifetime (no FC to answer it),
        # matching the Rust router which flips priming on its first sweep tick.
        self._param_priming = True
        self._param_sweep_at = time.monotonic()
        t0 = time.monotonic()
        while True:
            t = time.monotonic() - t0
            now = datetime.now(timezone.utc).isoformat()
            for msg in self._build_messages(t):
                buf = bytes(msg.pack(self._mav))
                for q in self._subscribers:
                    try:
                        q.put_nowait(buf)
                    except asyncio.QueueFull:
                        pass
                self._state.update_from_message(msg)
            self._state.last_update = now
            await asyncio.sleep(0.1)
