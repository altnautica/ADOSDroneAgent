"""Main entry point for ADOS Drone Agent."""

from __future__ import annotations

import asyncio
import signal
import time

from ados import __version__
from ados.core.config import ADOSConfig, load_config
from ados.core.health import HealthMonitor
from ados.core.logging import configure_logging, get_logger

log = get_logger("main")


class AgentApp:
    """Main application orchestrator. Runs all services as asyncio tasks."""

    def __init__(self, config: ADOSConfig, demo: bool = False) -> None:
        self.config = config
        self.demo = demo
        self.health = HealthMonitor()
        self._shutdown = asyncio.Event()
        self._start_time = time.monotonic()
        self._tasks: list[asyncio.Task] = []

        # Lazily-initialized service references
        self._fc_connection = None
        self._mavlink_proxy = None
        self._vehicle_state = None

    @property
    def uptime_seconds(self) -> float:
        return time.monotonic() - self._start_time

    async def start(self) -> None:
        """Start all agent services."""
        log.info(
            "agent_starting",
            version=__version__,
            device_id=self.config.agent.device_id,
            name=self.config.agent.name,
        )

        # Detect board
        from ados.hal.detect import detect_board
        board = detect_board()
        log.info("board_info", name=board.name, tier=board.tier, ram_mb=board.ram_mb)

        # Set tier from detection if auto
        if self.config.agent.tier == "auto":
            self.config.agent.tier = f"tier{board.tier}"

        # Initialize MAVLink connection
        from ados.services.mavlink.state import VehicleState
        self._vehicle_state = VehicleState()

        if self.demo:
            log.info("demo_mode", msg="DEMO MODE — simulated telemetry, no real FC")
            from ados.services.mavlink.demo import DemoFCConnection
            self._fc_connection = DemoFCConnection(self._vehicle_state)
        else:
            from ados.services.mavlink.connection import FCConnection
            self._fc_connection = FCConnection(
                self.config.mavlink,
                self._vehicle_state,
            )

        # Start FC connection task
        self._tasks.append(asyncio.create_task(
            self._fc_connection.run(), name="fc-connection"
        ))

        # Start MAVLink WebSocket proxy
        from ados.services.mavlink.proxy import MavlinkProxy
        self._mavlink_proxy = MavlinkProxy(
            self.config.mavlink,
            self._fc_connection,
        )
        self._tasks.append(asyncio.create_task(
            self._mavlink_proxy.run(), name="mavlink-ws-proxy"
        ))

        # Start TCP proxy
        from ados.services.mavlink.tcp_proxy import TcpProxy
        tcp_proxy = TcpProxy(self._fc_connection, port=5760)
        self._tasks.append(asyncio.create_task(
            tcp_proxy.run(), name="mavlink-tcp-proxy"
        ))

        # Start UDP proxy
        from ados.services.mavlink.udp_proxy import UdpProxy
        for udp_port in (14550, 14551):
            udp_proxy = UdpProxy(self._fc_connection, port=udp_port)
            self._tasks.append(asyncio.create_task(
                udp_proxy.run(), name=f"mavlink-udp-{udp_port}"
            ))

        # Start REST API
        from ados.api.server import create_api_task
        api_task = create_api_task(self)
        self._tasks.append(asyncio.create_task(api_task, name="rest-api"))

        # Start MQTT gateway if enabled
        if self.config.server.mode != "disabled":
            from ados.services.mqtt.gateway import MqttGateway
            mqtt = MqttGateway(self.config, self._vehicle_state)
            self._tasks.append(asyncio.create_task(
                mqtt.run(self._shutdown), name="mqtt-gateway"
            ))

        # Health monitor loop
        self._tasks.append(asyncio.create_task(
            self._health_loop(), name="health-monitor"
        ))

        # Agent heartbeat to FC
        self._tasks.append(asyncio.create_task(
            self._heartbeat_loop(), name="agent-heartbeat"
        ))

        # Notify systemd
        self.health.sd_notify_ready()

        log.info("agent_started", services=len(self._tasks))

        # Wait for shutdown signal
        await self._shutdown.wait()
        await self._stop()

    async def _health_loop(self) -> None:
        """Periodically check system health."""
        while not self._shutdown.is_set():
            self.health.check_system()
            self.health.sd_notify_watchdog()
            await asyncio.sleep(5)

    async def _heartbeat_loop(self) -> None:
        """Send HEARTBEAT as MAV_TYPE_ONBOARD_CONTROLLER at 1Hz."""
        while not self._shutdown.is_set():
            if self._fc_connection and self._fc_connection.connected:
                try:
                    self._fc_connection.send_heartbeat()
                except Exception:
                    log.debug("heartbeat_send_failed")
            await asyncio.sleep(1)

    async def _stop(self) -> None:
        """Gracefully stop all services."""
        log.info("agent_stopping")
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        log.info("agent_stopped")

    def request_shutdown(self) -> None:
        """Signal the agent to shut down."""
        self._shutdown.set()


def main() -> None:
    """Entry point for ados-agent."""
    config = load_config()
    configure_logging(
        level=config.logging.level,
        drone_name=config.agent.name,
        device_id=config.agent.device_id,
    )

    app = AgentApp(config)

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    # Handle signals
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, app.request_shutdown)

    try:
        loop.run_until_complete(app.start())
    except KeyboardInterrupt:
        app.request_shutdown()
    finally:
        loop.close()


if __name__ == "__main__":
    main()
