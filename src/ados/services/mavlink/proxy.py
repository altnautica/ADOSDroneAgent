"""MAVLink WebSocket relay — binary mode, wire-compatible with SITL bridge."""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

import websockets
from websockets.server import ServerConnection

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.core.config import MavlinkConfig
    from ados.services.mavlink.connection import FCConnection

log = get_logger("mavlink.proxy")


class MavlinkProxy:
    """WebSocket proxy: raw binary MAVLink frames, no JSON, no framing headers.

    Bidirectional:
      FC bytes -> all connected WS clients
      Any WS client bytes -> FC
    """

    def __init__(self, config: MavlinkConfig, fc: FCConnection) -> None:
        self.config = config
        self.fc = fc
        self._clients: set[ServerConnection] = set()
        self._port = 8765

        # Find websocket endpoint port from config
        for ep in config.endpoints:
            if ep.type == "websocket" and ep.enabled:
                self._port = ep.port
                break

    async def run(self) -> None:
        """Start the WebSocket server."""
        server = await websockets.serve(
            self._handle_client,
            "0.0.0.0",
            self._port,
        )
        log.info("ws_proxy_started", port=self._port)

        # Start FC->WS broadcast task
        broadcast_task = asyncio.create_task(self._broadcast_fc_data())

        try:
            await asyncio.Future()  # Run forever
        finally:
            broadcast_task.cancel()
            server.close()
            await server.wait_closed()

    async def _handle_client(self, ws: ServerConnection) -> None:
        """Handle a single WebSocket client connection."""
        addr = ws.remote_address
        log.info("ws_client_connected", addr=str(addr))
        self._clients.add(ws)

        try:
            async for message in ws:
                # Client -> FC: forward raw bytes
                if isinstance(message, bytes):
                    self.fc.send_bytes(message)
                # Ignore text frames
        except websockets.ConnectionClosed:
            pass
        finally:
            self._clients.discard(ws)
            log.info("ws_client_disconnected", addr=str(addr))

    async def _broadcast_fc_data(self) -> None:
        """Subscribe to FC data and broadcast to all WS clients."""
        q = self.fc.subscribe()
        try:
            while True:
                data = await q.get()
                if not self._clients:
                    continue

                # Send to all clients, remove dead ones
                dead = set()
                for ws in self._clients:
                    try:
                        await ws.send(data)
                    except websockets.ConnectionClosed:
                        dead.add(ws)
                self._clients -= dead
        finally:
            self.fc.unsubscribe(q)
