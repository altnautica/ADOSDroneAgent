"""UDP proxy for MAVLink — broadcasts raw MAVLink on UDP ports."""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.services.mavlink.connection import FCConnection

log = get_logger("mavlink.udp")


class _UdpProtocol(asyncio.DatagramProtocol):
    """Track connected UDP clients and forward incoming data to FC."""

    def __init__(self, fc: FCConnection) -> None:
        self.fc = fc
        self.clients: set[tuple[str, int]] = set()
        self.transport: asyncio.DatagramTransport | None = None

    def connection_made(self, transport: asyncio.DatagramTransport) -> None:
        self.transport = transport

    def datagram_received(self, data: bytes, addr: tuple[str, int]) -> None:
        # Track client
        if addr not in self.clients:
            self.clients.add(addr)
            log.info("udp_client_connected", addr=str(addr))
        # Forward to FC
        self.fc.send_bytes(data)

    def broadcast(self, data: bytes) -> None:
        """Send data to all known UDP clients."""
        if not self.transport:
            return
        for addr in self.clients:
            try:
                self.transport.sendto(data, addr)
            except OSError:
                pass


class UdpProxy:
    """UDP proxy that relays raw MAVLink bytes."""

    def __init__(self, fc: FCConnection, port: int = 14550) -> None:
        self.fc = fc
        self.port = port

    async def run(self) -> None:
        """Start the UDP endpoint."""
        loop = asyncio.get_event_loop()
        transport, protocol = await loop.create_datagram_endpoint(
            lambda: _UdpProtocol(self.fc),
            local_addr=("0.0.0.0", self.port),
        )

        log.info("udp_proxy_started", port=self.port)

        # FC -> UDP broadcast
        q = self.fc.subscribe()
        try:
            while True:
                data = await q.get()
                protocol.broadcast(data)
        finally:
            self.fc.unsubscribe(q)
            transport.close()
