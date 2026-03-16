"""TCP proxy for MAVLink — serves raw MAVLink on TCP port for MAVProxy/MAVROS."""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.services.mavlink.connection import FCConnection

log = get_logger("mavlink.tcp")


class TcpProxy:
    """TCP server that relays raw MAVLink bytes bidirectionally."""

    def __init__(self, fc: FCConnection, port: int = 5760) -> None:
        self.fc = fc
        self.port = port
        self._clients: set[asyncio.StreamWriter] = set()

    async def run(self) -> None:
        """Start the TCP server."""
        server = await asyncio.start_server(
            self._handle_client, "0.0.0.0", self.port
        )
        log.info("tcp_proxy_started", port=self.port)

        # Start FC->TCP broadcast
        broadcast_task = asyncio.create_task(self._broadcast_fc_data())

        try:
            async with server:
                await server.serve_forever()
        finally:
            broadcast_task.cancel()

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """Handle a single TCP client."""
        addr = writer.get_extra_info("peername")
        log.info("tcp_client_connected", addr=str(addr))
        self._clients.add(writer)

        try:
            while True:
                data = await reader.read(4096)
                if not data:
                    break
                # Client -> FC
                self.fc.send_bytes(data)
        except (ConnectionResetError, asyncio.CancelledError):
            pass
        finally:
            self._clients.discard(writer)
            writer.close()
            log.info("tcp_client_disconnected", addr=str(addr))

    async def _broadcast_fc_data(self) -> None:
        """Forward FC data to all TCP clients."""
        q = self.fc.subscribe()
        try:
            while True:
                data = await q.get()
                dead = set()
                for writer in self._clients:
                    try:
                        writer.write(data)
                        await writer.drain()
                    except (ConnectionResetError, BrokenPipeError):
                        dead.add(writer)
                for w in dead:
                    self._clients.discard(w)
                    w.close()
        finally:
            self.fc.unsubscribe(q)
