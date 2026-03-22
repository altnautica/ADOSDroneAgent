"""Standalone mDNS discovery service.

Registers the ADOS agent on the local network via zeroconf so that GCS
clients can find it automatically. Keeps the registration alive until shutdown.

Run: python -m ados.services.discovery
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados import __version__
from ados.core.config import load_config
from ados.core.logging import configure_logging
from ados.core.pairing import PairingManager


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("discovery_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    from ados.services.discovery import DiscoveryService

    pairing = PairingManager(state_path=config.pairing.state_path)
    api_port = config.scripting.rest_api.port

    discovery = DiscoveryService(
        device_id=config.agent.device_id,
        port=api_port,
        name=config.agent.name,
        version=__version__,
    )

    # Register mDNS service
    await discovery.register(
        paired=pairing.is_paired,
        code=getattr(pairing, "pairing_code", None),
        owner=getattr(pairing, "owner_id", None),
    )

    log.info("discovery_service_ready", hostname=discovery.mdns_hostname)

    # Keep running until shutdown, periodically updating TXT records
    while not shutdown.is_set():
        try:
            await asyncio.wait_for(shutdown.wait(), timeout=30.0)
        except TimeoutError:
            # Refresh TXT records (pairing state may have changed)
            await discovery.update_txt(
                paired=pairing.is_paired,
                code=getattr(pairing, "pairing_code", None),
                owner=getattr(pairing, "owner_id", None),
            )

    log.info("discovery_service_stopping")
    await discovery.unregister()
    log.info("discovery_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
