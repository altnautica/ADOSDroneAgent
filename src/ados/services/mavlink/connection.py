"""Flight controller MAVLink connection manager."""

from __future__ import annotations

import asyncio
import glob
from typing import TYPE_CHECKING

from pymavlink import mavutil

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.core.config import MavlinkConfig
    from ados.services.mavlink.state import VehicleState

log = get_logger("mavlink.connection")

SERIAL_PATTERNS = [
    "/dev/ttyACM*",
    "/dev/ttyAMA*",
    "/dev/ttyUSB*",
    "/dev/ttyS*",
    "/dev/tty.usbmodem*",   # macOS
    "/dev/tty.usbserial*",  # macOS
]

BAUD_CANDIDATES = [921600, 115200, 57600]


def auto_detect_port() -> str | None:
    """Scan common serial port patterns and return the first match."""
    for pattern in SERIAL_PATTERNS:
        matches = sorted(glob.glob(pattern))
        if matches:
            log.info("serial_detected", port=matches[0])
            return matches[0]
    return None


def auto_detect_baud(port: str) -> int:
    """Try baud rates by sending heartbeat and waiting for response."""
    for baud in BAUD_CANDIDATES:
        try:
            conn = mavutil.mavlink_connection(port, baud=baud, autoreconnect=False)
            conn.mav.heartbeat_send(
                mavutil.mavlink.MAV_TYPE_ONBOARD_CONTROLLER,
                mavutil.mavlink.MAV_AUTOPILOT_INVALID,
                0, 0, 0,
            )
            msg = conn.recv_match(type="HEARTBEAT", blocking=True, timeout=3)
            conn.close()
            if msg:
                log.info("baud_detected", baud=baud, port=port)
                return baud
        except Exception as e:
            log.debug("baud_probe_failed", baud=baud, port=port, error=str(e))
    log.warning("baud_detection_failed", port=port, fallback=57600)
    return 57600


def scan_for_fc(ports: list[str] | None = None, timeout: float = 3.0) -> str | None:
    """Probe serial ports for a MAVLink heartbeat and return the first responding port.

    If no ports are given, auto-discovers from SERIAL_PATTERNS. Each port is
    tried at common baud rates (921600, 115200, 57600). Returns the first
    port+baud combination that responds with a HEARTBEAT within `timeout` seconds,
    or None if nothing responds.
    """
    if ports is None:
        ports = []
        for pattern in SERIAL_PATTERNS:
            ports.extend(sorted(glob.glob(pattern)))

    if not ports:
        log.info("scan_no_ports_found")
        return None

    for port in ports:
        for baud in BAUD_CANDIDATES:
            try:
                conn = mavutil.mavlink_connection(
                    port, baud=baud, autoreconnect=False,
                )
                # Send our heartbeat to prompt a response
                conn.mav.heartbeat_send(
                    mavutil.mavlink.MAV_TYPE_ONBOARD_CONTROLLER,
                    mavutil.mavlink.MAV_AUTOPILOT_INVALID,
                    0, 0, 0,
                )
                msg = conn.recv_match(type="HEARTBEAT", blocking=True, timeout=timeout)
                conn.close()
                if msg:
                    log.info("fc_scan_found", port=port, baud=baud)
                    return port
            except Exception as e:
                log.debug("fc_scan_probe_failed", port=port, baud=baud, error=str(e))

    log.info("fc_scan_no_response", ports_tried=len(ports))
    return None


class FCConnection:
    """Manages the MAVLink connection to the flight controller."""

    def __init__(self, config: MavlinkConfig, state: VehicleState) -> None:
        self.config = config
        self.state = state
        self._conn: mavutil.mavlink_connection | None = None
        self._connected = False
        self._port: str = ""
        self._baud: int = 0
        self._lock = asyncio.Lock()
        self._subscribers: list[asyncio.Queue[bytes]] = []

    @property
    def connected(self) -> bool:
        return self._connected

    @property
    def port(self) -> str:
        return self._port

    @property
    def baud(self) -> int:
        return self._baud

    @property
    def connection(self) -> mavutil.mavlink_connection | None:
        return self._conn

    def subscribe(self) -> asyncio.Queue[bytes]:
        """Subscribe to raw MAVLink bytes from the FC."""
        q: asyncio.Queue[bytes] = asyncio.Queue(maxsize=256)
        self._subscribers.append(q)
        return q

    def unsubscribe(self, q: asyncio.Queue[bytes]) -> None:
        if q in self._subscribers:
            self._subscribers.remove(q)

    def send_bytes(self, data: bytes) -> None:
        """Send raw bytes to the FC."""
        if self._conn:
            try:
                self._conn.write(data)
            except Exception:
                log.warning("fc_write_failed")

    def send_heartbeat(self) -> None:
        """Send companion computer heartbeat to FC."""
        if self._conn:
            self._conn.mav.heartbeat_send(
                mavutil.mavlink.MAV_TYPE_ONBOARD_CONTROLLER,
                mavutil.mavlink.MAV_AUTOPILOT_INVALID,
                0, 0, 0,
            )

    async def _connect(self) -> bool:
        """Establish MAVLink connection."""
        port = self.config.serial_port

        # Check for SITL-style TCP connection string
        if port and (port.startswith("tcp:") or port.startswith("udp:")):
            self._port = port
            self._baud = 0
        else:
            if not port:
                port = auto_detect_port()
                if not port:
                    log.warning("no_serial_port_found")
                    return False
            self._port = port
            self._baud = self.config.baud_rate or auto_detect_baud(port)

        try:
            kwargs = {
                "source_system": self.config.system_id,
                "source_component": self.config.component_id,
                "autoreconnect": True,
            }
            if self._baud:
                kwargs["baud"] = self._baud

            self._conn = mavutil.mavlink_connection(self._port, **kwargs)
            log.info("mavlink_connecting", port=self._port, baud=self._baud)

            # Wait for heartbeat
            msg = self._conn.wait_heartbeat(timeout=10)
            if msg:
                self._connected = True
                log.info(
                    "fc_connected",
                    port=self._port,
                    autopilot=msg.autopilot,
                    mav_type=msg.type,
                )

                # Request data streams
                from ados.services.mavlink.streams import request_data_streams
                request_data_streams(self._conn)

                return True
            else:
                log.warning("heartbeat_timeout", port=self._port)
                return False
        except Exception as e:
            log.error("connection_failed", error=str(e), port=self._port)
            return False

    async def run(self) -> None:
        """Main connection loop with auto-reconnect."""
        backoff = 1.0

        while True:
            if not self._connected:
                ok = await self._connect()
                if not ok:
                    log.info("reconnect_backoff", seconds=backoff)
                    await asyncio.sleep(backoff)
                    backoff = min(backoff * 2, 30.0)
                    continue
                backoff = 1.0

            # Read loop
            try:
                await self._read_loop()
            except Exception as e:
                log.error("read_loop_error", error=str(e))
                self._connected = False
                await asyncio.sleep(1)

    async def _read_loop(self) -> None:
        """Read MAVLink messages from FC and distribute to subscribers."""
        loop = asyncio.get_event_loop()

        while self._connected:
            try:
                # pymavlink is blocking, so run in executor
                msg = await asyncio.wait_for(
                    loop.run_in_executor(None, self._recv_msg),
                    timeout=5.0,
                )
                if msg is None:
                    continue

                # Update vehicle state
                self.state.update_from_message(msg)

                # Get raw bytes and distribute to subscribers
                raw = msg.get_msgbuf()
                if raw:
                    for q in self._subscribers:
                        try:
                            q.put_nowait(bytes(raw))
                        except asyncio.QueueFull:
                            pass  # Drop if subscriber is slow

            except TimeoutError:
                # No message received, check if still connected
                has_port = hasattr(self._conn, 'port')
                if self._conn and (not self._conn.port.closed if has_port else True):
                    continue
                self._connected = False
            except Exception as e:
                log.error("recv_error", error=str(e))
                self._connected = False

    def _recv_msg(self):
        """Blocking receive (runs in executor thread)."""
        if self._conn:
            return self._conn.recv_match(blocking=True, timeout=2)
        return None
