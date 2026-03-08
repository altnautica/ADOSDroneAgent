"""Broadcast vehicle state as JSON over UDP at 10 Hz."""

from __future__ import annotations

import asyncio
import json
import socket
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.services.mavlink.state import VehicleState

log = get_logger("scripting.state_broadcast")


class StateBroadcaster:
    """Broadcasts compact vehicle state JSON over UDP at 10 Hz.

    Default broadcast address: 255.255.255.255 on port 8891
    (one above the WebSocket command port).
    """

    def __init__(self, state: VehicleState, port: int = 8891) -> None:
        self._state = state
        self._port = port

    async def run(self) -> None:
        """Main broadcast loop — sends state at 10 Hz."""
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        sock.setblocking(False)

        log.info("state_broadcast_started", port=self._port)

        loop = asyncio.get_running_loop()
        try:
            while True:
                payload = self._build_payload()
                data = json.dumps(payload).encode("utf-8")
                try:
                    await loop.sock_sendto(sock, data, ("255.255.255.255", self._port))
                except OSError:
                    # Network not available, skip this tick
                    pass
                await asyncio.sleep(0.1)
        finally:
            sock.close()

    def _build_payload(self) -> dict:
        """Build compact state dict for broadcast."""
        s = self._state
        return {
            "lat": round(s.lat, 7),
            "lon": round(s.lon, 7),
            "alt": round(s.alt_rel, 2),
            "heading": round(s.heading, 1),
            "speed": round(s.groundspeed, 2),
            "battery": s.battery_remaining,
            "armed": s.armed,
            "mode": s.mode,
        }
