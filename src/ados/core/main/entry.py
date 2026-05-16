"""Console-script entry point for ``ados-agent``.

Loads config, configures logging, instantiates :class:`AgentApp`,
installs termination handlers, and runs the asyncio loop until the
shutdown event fires.
"""

from __future__ import annotations

import asyncio

from ados.core.config import load_config
from ados.core.logging import configure_logging

from .app import AgentApp
from .signal_handlers import install_termination_handlers


def main() -> None:
    """Entry point for ``ados-agent``."""
    config = load_config()
    configure_logging(
        level=config.logging.level,
        drone_name=config.agent.name,
        device_id=config.agent.device_id,
    )

    app = AgentApp(config)

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    install_termination_handlers(loop, app)

    try:
        loop.run_until_complete(app.start())
    except KeyboardInterrupt:
        app.request_shutdown()
    finally:
        loop.close()


__all__ = ["main"]
