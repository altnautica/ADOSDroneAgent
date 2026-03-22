"""Standalone OTA updater service (oneshot).

Checks GitHub Releases for a newer version, downloads the wheel, verifies
the SHA-256 hash, installs via pip, and optionally restarts the agent
systemd service. Exits after completion.

Run: python -m ados.services.ota
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados import __version__
from ados.core.config import load_config
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("ota_service_starting", current_version=__version__)

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    from ados.services.ota.checker import UpdateChecker
    from ados.services.ota.downloader import UpdateDownloader
    from ados.services.ota.updater import OtaUpdater

    checker = UpdateChecker(config.ota)
    downloader = UpdateDownloader()
    updater = OtaUpdater(config.ota, checker, downloader, current_version=__version__)

    # Step 1: Check for updates
    log.info("ota_checking")
    manifest = await updater.check()

    if not manifest:
        log.info("ota_no_update_available")
        return

    log.info("ota_update_found", version=manifest.version)

    if shutdown.is_set():
        return

    # Step 2: Download and verify
    log.info("ota_downloading", version=manifest.version)
    ok = await updater.download_and_verify()
    if not ok:
        log.error("ota_download_failed", error=updater.error)
        sys.exit(1)

    if shutdown.is_set():
        return

    # Step 3: Install
    log.info("ota_installing", version=manifest.version)
    installed = await updater.install()
    if not installed:
        log.error("ota_install_failed", error=updater.error)
        sys.exit(1)

    log.info("ota_install_complete", version=manifest.version)

    # Step 4: Restart service (Linux only, no-op on macOS)
    await updater.restart_service()

    log.info("ota_service_done")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
