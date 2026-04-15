"""Systemd service wrapper for the Peripheral Manager.

Wave 3 behavior: load the registry on startup, honor SIGHUP to reload,
and sit idle until shutdown. The registry is consumed in-process by
the REST API, but the supervised service is still useful because it
gives operators a clean ``systemctl reload ados-peripherals`` path for
refreshing YAML manifests without bouncing the API.

Future waves will extend this service with active transport probing
(udev monitoring for USB hot-plug, pyserial enumeration, zeroconf
watchers, BLE scans) and a state-IPC publisher so the GCS can render
live connection dots.

Run: ``python -m ados.services.peripherals.service``.
"""

from __future__ import annotations

import asyncio
import signal

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging
from ados.services.peripherals.registry import get_peripheral_registry


async def main() -> None:
    """Service entrypoint."""
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger("peripherals.service")
    log.info("peripherals_service_starting")

    registry = get_peripheral_registry()
    log.info(
        "peripherals_registry_ready",
        count=len(registry.list()),
    )

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()

    def _on_sighup() -> None:
        count = registry.reload()
        log.info("peripherals_sighup_reloaded", count=count)

    def _on_shutdown() -> None:
        shutdown.set()

    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, _on_shutdown)
    loop.add_signal_handler(signal.SIGHUP, _on_sighup)

    try:
        await shutdown.wait()
    finally:
        log.info("peripherals_service_stopped")


if __name__ == "__main__":
    asyncio.run(main())
