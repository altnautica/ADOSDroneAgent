"""Standalone REST API service.

Runs the FastAPI server with uvicorn, connecting to state IPC for live
telemetry data on status endpoints.

Run: python -m ados.services.api
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog
import uvicorn

from ados.core.config import load_config
from ados.core.ipc import StateIPCClient
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("api_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Connect to state IPC for telemetry data
    state_client = StateIPCClient()
    try:
        await state_client.connect()
    except ConnectionError:
        log.warning("state_ipc_unavailable", msg="Running without live telemetry")

    # Build a minimal AgentApp-like shim so create_app() can function
    # without the full process supervisor.
    from ados.api.server import create_app

    from ados.core.pairing import PairingManager
    from ados.core.main import ServiceTracker

    class _StandaloneAgent:
        """Minimal stand-in for AgentApp when running API standalone."""

        def __init__(self, cfg, state):
            self.config = cfg
            self._state_client = state
            self.pairing_manager = PairingManager(state_path=cfg.pairing.state_path)
            self.services = ServiceTracker()
            self._tasks = []
            self._fc_connection = None
            self._video_pipeline = None
            self._wfb_manager = None
            self._command_executor = None
            self._script_runner = None
            self.ota_updater = None
            self.discovery_service = None
            self.board_name = "unknown"
            self.health = type("Health", (), {
                "cpu_percent": 0.0, "memory_percent": 0.0,
                "disk_percent": 0.0, "temperature": None,
            })()
            self.demo = False

        @property
        def uptime_seconds(self) -> float:
            return 0.0

    agent_shim = _StandaloneAgent(config, state_client)
    app = create_app(agent_shim)

    api_config = config.scripting.rest_api
    uvi_config = uvicorn.Config(
        app,
        host=api_config.host,
        port=api_config.port,
        log_level="warning",
        access_log=False,
    )
    server = uvicorn.Server(uvi_config)

    tasks = [
        asyncio.create_task(server.serve(), name="uvicorn"),
    ]

    if state_client.connected:
        tasks.append(asyncio.create_task(state_client.read_loop(), name="state-reader"))

    log.info("api_service_ready", host=api_config.host, port=api_config.port)

    # Wait for shutdown signal
    await shutdown.wait()

    log.info("api_service_stopping")
    server.should_exit = True
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await state_client.disconnect()
    log.info("api_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
