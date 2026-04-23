"""Rerun visualization sink service entry point.

Publishes drone state as Rerun entity paths on port 9876 (gRPC) for
visualization in Rerun viewer, including pose, camera frames,
perception data, and telemetry history.

Run: python -m ados.services.rerun_sink
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
    log.info("rerun_sink_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    _sd_notify(b"READY=1")
    log.info("rerun_sink_ready_stub", note="full implementation in phase 5")

    await shutdown.wait()
    log.info("rerun_sink_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
