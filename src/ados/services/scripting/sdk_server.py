"""TCP server for Python SDK connections — newline-delimited JSON protocol."""

from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.services.scripting.text_parser import parse_text_command

if TYPE_CHECKING:
    from ados.services.scripting.executor import CommandExecutor

log = get_logger("scripting.sdk_server")


class SdkServer:
    """TCP server for Python SDK connections.

    Protocol: newline-delimited JSON over TCP.
    Request:  {"cmd": "takeoff", "args": []}
    Response: {"status": "ok"} or {"status": "error", "message": "reason"}
    """

    def __init__(
        self,
        executor: CommandExecutor,
        port: int = 8892,
        max_connections: int = 5,
    ) -> None:
        self._executor = executor
        self._port = port
        self._max_connections = max_connections
        self._active_connections = 0
        self._server: asyncio.Server | None = None

    async def run(self) -> None:
        """Start the TCP server and serve forever."""
        self._server = await asyncio.start_server(
            self._handle_client,
            "0.0.0.0",
            self._port,
        )
        log.info("sdk_server_started", port=self._port, max_connections=self._max_connections)
        async with self._server:
            await self._server.serve_forever()

    async def _handle_client(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> None:
        addr = writer.get_extra_info("peername")

        # Reject connections beyond the limit to avoid exhausting file descriptors
        if self._active_connections >= self._max_connections:
            log.warning("sdk_connection_rejected", addr=addr, active=self._active_connections)
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionError, BrokenPipeError):
                pass
            return

        self._active_connections += 1
        log.info("sdk_client_connected", addr=addr, active=self._active_connections)
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                text = line.decode("utf-8", errors="replace").strip()
                if not text:
                    continue
                response = await self._process_request(text)
                writer.write((json.dumps(response) + "\n").encode("utf-8"))
                await writer.drain()
        except (ConnectionError, asyncio.CancelledError):
            pass
        finally:
            self._active_connections -= 1
            log.info("sdk_client_disconnected", addr=addr, active=self._active_connections)
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionError, BrokenPipeError):
                pass

    async def _process_request(self, text: str) -> dict:
        """Parse a JSON request and execute the command."""
        try:
            req = json.loads(text)
        except json.JSONDecodeError:
            return {"status": "error", "message": "invalid JSON"}

        cmd_str = req.get("cmd", "")
        args = req.get("args", [])

        if not cmd_str:
            return {"status": "error", "message": "missing cmd field"}

        # Build text representation for the parser
        if args:
            arg_str = " ".join(str(a) for a in args)
            full_text = f"{cmd_str} {arg_str}"
        else:
            full_text = cmd_str

        parsed = parse_text_command(full_text)
        result = await self._executor.execute(parsed, source="sdk")

        if result == "ok":
            return {"status": "ok"}
        if result.startswith("error:"):
            return {"status": "error", "message": result[7:].strip()}
        # Query response — return the value
        return {"status": "ok", "value": result}
