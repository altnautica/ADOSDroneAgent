"""Standalone health monitor service.

Collects CPU, memory, disk, and temperature via psutil every 5 seconds
and writes a JSON snapshot to /run/ados/health.json for other services to read.

Run: python -m ados.services.health
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
from pathlib import Path

import structlog

from ados.core.config import load_config
from ados.core.health import HealthMonitor
from ados.core.logging import configure_logging

HEALTH_FILE = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "health.json"
COLLECT_INTERVAL = 5.0


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("health_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    monitor = HealthMonitor()

    # Ensure the run directory exists
    HEALTH_FILE.parent.mkdir(parents=True, exist_ok=True)

    # Notify systemd we are ready
    monitor.sd_notify_ready()

    log.info("health_service_ready", output=str(HEALTH_FILE))

    while not shutdown.is_set():
        health = monitor.check_system()

        # Write atomically via tmp + rename
        tmp_path = HEALTH_FILE.with_suffix(".tmp")
        try:
            tmp_path.write_text(json.dumps(health.to_dict(), indent=2))
            os.replace(str(tmp_path), str(HEALTH_FILE))
        except OSError as e:
            log.warning("health_write_failed", error=str(e))

        # Send systemd watchdog ping
        monitor.sd_notify_watchdog()

        # Wait for next interval or shutdown
        try:
            await asyncio.wait_for(shutdown.wait(), timeout=COLLECT_INTERVAL)
        except TimeoutError:
            pass

    # Clean up health file on shutdown
    try:
        HEALTH_FILE.unlink(missing_ok=True)
    except OSError:
        pass

    log.info("health_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
