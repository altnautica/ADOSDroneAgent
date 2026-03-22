"""Standalone scripting engine service.

Runs the text command listeners (UDP + WebSocket), SDK TCP server, and
state broadcaster. Connects to MAVLink IPC for command execution and
state IPC for vehicle telemetry.

Run: python -m ados.services.scripting
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.ipc import MavlinkIPCClient, StateIPCClient
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("scripting_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Connect to MAVLink IPC for sending commands to the FC
    mavlink_client = MavlinkIPCClient()
    try:
        await mavlink_client.connect()
    except ConnectionError:
        log.warning("mavlink_ipc_unavailable", msg="Commands will fail until MAVLink service starts")

    # Connect to state IPC for vehicle telemetry
    state_client = StateIPCClient()
    try:
        await state_client.connect()
    except ConnectionError:
        log.warning("state_ipc_unavailable", msg="State queries will return defaults")

    # Build executor with IPC-backed FC connection and vehicle state
    from ados.services.mavlink.state import VehicleState
    from ados.services.scripting.executor import CommandExecutor
    from ados.services.scripting.safety import SafetyValidator
    from ados.services.scripting.sdk_server import SdkServer
    from ados.services.scripting.text_listener import TextCommandListener

    vehicle_state = VehicleState()
    safety = SafetyValidator(vehicle_state)
    executor = CommandExecutor(mavlink_client, vehicle_state, safety)

    text_listener = TextCommandListener(config.scripting.text_commands, executor)
    sdk_server = SdkServer(executor, port=8892)

    tasks = [
        asyncio.create_task(text_listener.run(), name="text-listener"),
        asyncio.create_task(sdk_server.run(), name="sdk-server"),
    ]

    # State IPC reader to keep vehicle_state updated
    if state_client.connected:
        async def state_sync_loop() -> None:
            """Read state from IPC and update vehicle_state."""
            while not shutdown.is_set():
                state_dict = state_client.state
                if state_dict:
                    vehicle_state.update_from_dict(state_dict)
                await asyncio.sleep(0.1)

        tasks.append(asyncio.create_task(state_sync_loop(), name="state-sync"))

    if mavlink_client.connected:
        tasks.append(asyncio.create_task(mavlink_client.read_loop(), name="mavlink-reader"))

    log.info("scripting_service_ready")

    # Wait for shutdown
    await shutdown.wait()

    log.info("scripting_service_stopping")
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await mavlink_client.disconnect()
    await state_client.disconnect()
    log.info("scripting_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
