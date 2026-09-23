"""A stand-in for the native router's state and MAVLink sockets.

The router serves both sockets in production; these doubles speak the same
wire so the Python clients can be exercised without it.
"""

from __future__ import annotations

import asyncio
import json
import struct
from pathlib import Path

import msgpack

from ados.core.contracts import contract_version


def encode_state_frame(state: dict) -> bytes:
    """A v2 state frame: 4-byte big-endian length + msgpack ``{"v", "s"}``."""
    body = msgpack.packb({"v": contract_version("state.v2"), "s": state}, use_bin_type=True)
    return struct.pack("!I", len(body)) + body


def encode_state_frame_v1(state: dict) -> bytes:
    """A legacy v1 state frame: newline-terminated JSON."""
    return json.dumps(state).encode() + b"\n"


class StateSocketServer:
    """Serves v2 state frames: the latest snapshot on connect, then each publish."""

    def __init__(self, sock_path: Path) -> None:
        self._sock_path = sock_path
        self._server: asyncio.AbstractServer | None = None
        self._writers: set[asyncio.StreamWriter] = set()
        self._last: dict | None = None

    async def start(self) -> None:
        self._server = await asyncio.start_unix_server(self._handle, path=str(self._sock_path))

    async def stop(self) -> None:
        for writer in list(self._writers):
            writer.close()
        self._writers.clear()
        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
        self._sock_path.unlink(missing_ok=True)

    def publish(self, state: dict) -> None:
        self._last = state
        frame = encode_state_frame(state)
        for writer in list(self._writers):
            writer.write(frame)

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self._writers.add(writer)
        if self._last is not None:
            writer.write(encode_state_frame(self._last))
        try:
            await reader.read()  # until the client goes away
        except (ConnectionResetError, OSError):
            pass
        finally:
            self._writers.discard(writer)
            writer.close()


class MavlinkSocketServer:
    """Collects the length-prefixed frames clients send over the MAVLink socket."""

    def __init__(self, sock_path: Path) -> None:
        self._sock_path = sock_path
        self._server: asyncio.AbstractServer | None = None
        self.received: list[bytes] = []

    async def start(self) -> None:
        self._server = await asyncio.start_unix_server(self._handle, path=str(self._sock_path))

    async def stop(self) -> None:
        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
        self._sock_path.unlink(missing_ok=True)

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while True:
                (length,) = struct.unpack("!I", await reader.readexactly(4))
                self.received.append(await reader.readexactly(length))
        except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
            pass
        finally:
            writer.close()
