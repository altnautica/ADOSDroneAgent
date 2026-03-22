"""Standalone WFB-ng link manager service.

Detects compatible WiFi adapters, sets monitor mode, and manages wfb_tx/wfb_rx
subprocesses with auto-restart and link quality monitoring.

Run: python -m ados.services.wfb
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("wfb_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    from ados.services.wfb.manager import WfbManager

    manager = WfbManager(config.video.wfb)

    # Run the WFB manager (handles adapter detection, monitor mode, process lifecycle)
    manager_task = asyncio.create_task(manager.run(), name="wfb-manager")

    log.info("wfb_service_ready")

    # Wait for shutdown
    await shutdown.wait()

    log.info("wfb_service_stopping")
    manager_task.cancel()
    await asyncio.gather(manager_task, return_exceptions=True)
    await manager.stop()
    log.info("wfb_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
