"""MCP server service entry point.

Exposes the drone's full control surface as an MCP server — Tools,
Resources, and Prompts accessible from any MCP-capable client over
HTTP+SSE on :8090, a Unix socket at /run/ados/mcp.sock, or stdio.

Run: python -m ados.services.mcp
"""

from __future__ import annotations

import asyncio
import os
import signal
import socket
import sys

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging

log = structlog.get_logger()


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        sock.sendto(message, addr)
        sock.close()
    except OSError:
        pass


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log.info("mcp_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Stub: service not yet implemented — signal ready and wait for shutdown.
    _sd_notify(b"READY=1")
    log.info("mcp_service_ready_stub", note="full implementation in phase 1")

    await shutdown.wait()
    log.info("mcp_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
