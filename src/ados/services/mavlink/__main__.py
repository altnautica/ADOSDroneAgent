"""Standalone MAVLink proxy service.

Owns the FC serial connection and broadcasts MAVLink frames to all consumers
via Unix socket IPC. Also exposes WebSocket, TCP, and UDP endpoints for
direct GCS connections.

Run: python -m ados.services.mavlink
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.ipc import MavlinkIPCServer, StateIPCServer
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("mavlink_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Start IPC servers
    mavlink_ipc = MavlinkIPCServer()
    state_ipc = StateIPCServer()
    await mavlink_ipc.start()
    await state_ipc.start()

    # Start FC connection
    from ados.services.mavlink.connection import FCConnection
    from ados.services.mavlink.state import VehicleState

    vehicle_state = VehicleState()
    fc = FCConnection(config.mavlink, vehicle_state)

    # Subscribe to FC data and broadcast to IPC clients
    fc_queue = fc.subscribe()

    async def ipc_broadcast_loop() -> None:
        """Read MAVLink frames from FC queue and broadcast to IPC clients."""
        while not shutdown.is_set():
            try:
                data = await asyncio.wait_for(fc_queue.get(), timeout=1.0)
                mavlink_ipc.broadcast(data)
            except TimeoutError:
                pass

    # Wire client commands back to FC
    mavlink_ipc.set_command_handler(lambda data: fc.send_bytes(data))

    # Periodically publish state to state IPC
    async def state_publish_loop() -> None:
        while not shutdown.is_set():
            state_ipc.publish(vehicle_state.to_dict())
            await asyncio.sleep(0.1)  # 10Hz

    # Start proxies
    from ados.services.mavlink.proxy import MavlinkProxy
    from ados.services.mavlink.tcp_proxy import TcpProxy
    from ados.services.mavlink.udp_proxy import UdpProxy

    ws_proxy = MavlinkProxy(config.mavlink, fc)
    tcp_proxy = TcpProxy(fc, port=5760)
    udp_proxies = [
        UdpProxy(fc, port=14550),
        UdpProxy(fc, port=14551),
    ]

    tasks = [
        asyncio.create_task(fc.run(), name="fc-connection"),
        asyncio.create_task(ipc_broadcast_loop(), name="ipc-broadcast"),
        asyncio.create_task(ws_proxy.run(), name="ws-proxy"),
        asyncio.create_task(tcp_proxy.run(), name="tcp-proxy"),
        asyncio.create_task(state_publish_loop(), name="state-publish"),
    ]
    for udp in udp_proxies:
        tasks.append(asyncio.create_task(udp.run(), name=f"udp-{udp.port}"))

    log.info("mavlink_service_ready")

    # Wait for shutdown
    await shutdown.wait()

    log.info("mavlink_service_stopping")
    fc.unsubscribe(fc_queue)
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await mavlink_ipc.stop()
    await state_ipc.stop()
    log.info("mavlink_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
