"""World Model memory service entry point.

Ingests detections from the Vision Engine, stores observations with
CLIP embeddings in SQLite+sqlite-vec, runs background entity merging,
and exposes a REST query API at /api/memory/*.

Run: python -m ados.services.memory
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
    log.info("memory_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    _sd_notify(b"READY=1")
    log.info("memory_service_ready_stub", note="full implementation in phase 3")

    await shutdown.wait()
    log.info("memory_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
