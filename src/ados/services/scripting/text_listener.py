"""UDP and WebSocket listeners for Tello-style text commands."""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

import websockets
import websockets.server

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.core.config import TextCommandsConfig
    from ados.services.scripting.executor import CommandExecutor

from ados.services.scripting.text_parser import parse_text_command

log = get_logger("scripting.text_listener")


class _UdpProtocol(asyncio.DatagramProtocol):
    """asyncio datagram protocol that feeds commands into the executor."""

    def __init__(self, executor: CommandExecutor) -> None:
        self._executor = executor
        self._transport: asyncio.DatagramTransport | None = None
        self._loop: asyncio.AbstractEventLoop | None = None

    def connection_made(self, transport: asyncio.DatagramTransport) -> None:  # type: ignore[override]
        self._transport = transport
        self._loop = asyncio.get_running_loop()

    def datagram_received(self, data: bytes, addr: tuple[str, int]) -> None:
        text = data.decode("utf-8", errors="replace").strip()
        if not text:
            return
        log.info("udp_command", text=text, addr=addr)
        if self._loop is not None:
            self._loop.create_task(self._handle(text, addr))

    async def _handle(self, text: str, addr: tuple[str, int]) -> None:
        cmd = parse_text_command(text)
        result = await self._executor.execute(cmd, source="udp")
        if self._transport is not None:
            self._transport.sendto(result.encode("utf-8"), addr)


class TextCommandListener:
    """Listens for Tello-style text commands on UDP and WebSocket.

    UDP port: config.udp_port (default 8889)
    WebSocket port: config.websocket_port (default 8890)
    """

    def __init__(self, config: TextCommandsConfig, executor: CommandExecutor) -> None:
        self._config = config
        self._executor = executor
        self._udp_transport: asyncio.DatagramTransport | None = None

    async def run(self) -> None:
        """Start both UDP and WebSocket listeners concurrently."""
        await asyncio.gather(
            self._run_udp(),
            self._run_ws(),
        )

    async def _run_udp(self) -> None:
        """Start UDP listener."""
        loop = asyncio.get_running_loop()
        transport, _protocol = await loop.create_datagram_endpoint(
            lambda: _UdpProtocol(self._executor),
            local_addr=("0.0.0.0", self._config.udp_port),
        )
        self._udp_transport = transport
        log.info("udp_listener_started", port=self._config.udp_port)
        # Keep running until cancelled
        try:
            while True:
                await asyncio.sleep(3600)
        finally:
            transport.close()

    async def _run_ws(self) -> None:
        """Start WebSocket listener."""
        async def handler(ws: websockets.server.ServerConnection) -> None:
            log.info("ws_client_connected", remote=str(ws.remote_address))
            try:
                async for message in ws:
                    text = message if isinstance(message, str) else message.decode("utf-8")
                    text = text.strip()
                    if not text:
                        continue
                    log.info("ws_command", text=text)
                    cmd = parse_text_command(text)
                    result = await self._executor.execute(cmd, source="websocket")
                    await ws.send(result)
            except websockets.exceptions.ConnectionClosed:
                log.info("ws_client_disconnected")

        server = await websockets.serve(handler, "0.0.0.0", self._config.websocket_port)
        log.info("ws_listener_started", port=self._config.websocket_port)
        try:
            await asyncio.Future()  # run forever
        finally:
            server.close()
            await server.wait_closed()
            log.info("ws_listener_stopped")
