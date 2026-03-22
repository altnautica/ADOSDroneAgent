"""Standalone cloud relay service.

Handles MQTT telemetry publishing, Convex HTTP heartbeat, and cloud command
polling. Reads vehicle state from the state IPC socket.

Run: python -m ados.services.cloud
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.ipc import StateIPCClient
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("cloud_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Connect to state IPC to get telemetry
    state_client = StateIPCClient()
    try:
        await state_client.connect()
    except ConnectionError:
        log.warning("state_ipc_unavailable", msg="Running without telemetry")

    # Start MQTT gateway
    from ados.services.mqtt.gateway import MqttGateway
    from ados.core.pairing import PairingManager

    pairing = PairingManager(state_path=config.pairing.state_path)

    mqtt = MqttGateway(config, state_client)

    tasks = []

    # MQTT telemetry publishing
    tasks.append(asyncio.create_task(mqtt.run(shutdown), name="mqtt-gateway"))

    # State IPC reading
    if state_client.connected:
        tasks.append(asyncio.create_task(state_client.read_loop(), name="state-reader"))

    # Cloud heartbeat (Convex HTTP)
    async def heartbeat_loop() -> None:
        import httpx

        convex_url = config.pairing.convex_url
        while not shutdown.is_set():
            if pairing.is_paired and convex_url:
                try:
                    payload = {
                        "deviceId": config.agent.device_id,
                        "apiKey": pairing.api_key,
                        "version": "0.3.0",
                        "uptimeSeconds": 0,
                        # State from IPC
                        **_build_status_from_state(state_client.state),
                    }
                    async with httpx.AsyncClient(timeout=10.0) as client:
                        await client.post(f"{convex_url}/agent/status", json=payload)
                except Exception:
                    log.debug("cloud_heartbeat_failed")
            await asyncio.sleep(5)

    tasks.append(asyncio.create_task(heartbeat_loop(), name="heartbeat"))

    # Cloud command polling
    async def command_poll_loop() -> None:
        import httpx

        convex_url = config.pairing.convex_url
        while not shutdown.is_set():
            if pairing.is_paired and convex_url:
                try:
                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.get(
                            f"{convex_url}/agent/commands",
                            params={
                                "deviceId": config.agent.device_id,
                                "apiKey": pairing.api_key,
                            },
                        )
                        if resp.status_code == 200:
                            commands = resp.json()
                            for cmd in commands:
                                log.info("cloud_command_received", command=cmd)
                                # TODO: execute command via MAVLink IPC
                except Exception:
                    log.debug("cloud_command_poll_failed")
            await asyncio.sleep(5)

    tasks.append(asyncio.create_task(command_poll_loop(), name="command-poll"))

    log.info("cloud_service_ready")
    await shutdown.wait()

    log.info("cloud_service_stopping")
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await state_client.disconnect()
    log.info("cloud_service_stopped")


def _build_status_from_state(state: dict) -> dict:
    """Extract cloud heartbeat fields from VehicleState dict."""
    return {
        "cpuPercent": state.get("cpu_percent", 0),
        "memoryPercent": state.get("memory_percent", 0),
        "diskPercent": state.get("disk_percent", 0),
        "temperature": state.get("temperature"),
        "fcConnected": state.get("fc_connected", False),
        "fcPort": state.get("fc_port", ""),
        "fcBaud": state.get("fc_baud", 0),
    }


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
