"""Main entry point for ADOS Drone Agent."""

from __future__ import annotations

import asyncio
import signal
import time
from enum import Enum

from ados import __version__
from ados.core.config import ADOSConfig, load_config
from ados.core.health import HealthMonitor
from ados.core.logging import configure_logging, get_logger

log = get_logger("main")


class ServiceState(str, Enum):
    """Lifecycle states for managed services."""

    STOPPED = "stopped"
    STARTING = "starting"
    RUNNING = "running"
    DEGRADED = "degraded"
    FAILED = "failed"


class ServiceTracker:
    """Tracks state and transitions for all agent services."""

    def __init__(self) -> None:
        self._states: dict[str, ServiceState] = {}
        self._transitions: dict[str, list[tuple[float, ServiceState]]] = {}

    def set_state(self, name: str, state: ServiceState) -> None:
        """Transition a service to a new state, recording the timestamp."""
        prev = self._states.get(name)
        self._states[name] = state

        if name not in self._transitions:
            self._transitions[name] = []
        self._transitions[name].append((time.monotonic(), state))

        if prev != state:
            log.info("service_state_change", service=name, from_state=str(prev), to_state=state.value)

    def get_state(self, name: str) -> ServiceState:
        """Get the current state of a service."""
        return self._states.get(name, ServiceState.STOPPED)

    def get_all(self) -> dict[str, ServiceState]:
        """Return a copy of all service states."""
        return dict(self._states)

    def get_transitions(self, name: str) -> list[tuple[float, ServiceState]]:
        """Return recorded state transitions for a given service."""
        return list(self._transitions.get(name, []))

    def to_dict(self) -> dict[str, dict]:
        """Serialize all service states for the REST API."""
        result: dict[str, dict] = {}
        for name, state in self._states.items():
            transitions = self._transitions.get(name, [])
            last_transition = transitions[-1][0] if transitions else 0
            result[name] = {
                "state": state.value,
                "last_transition": last_transition,
                "transition_count": len(transitions),
            }
        return result


class AgentApp:
    """Main application orchestrator. Runs all services as asyncio tasks."""

    def __init__(self, config: ADOSConfig, demo: bool = False) -> None:
        self.config = config
        self.demo = demo
        self.health = HealthMonitor()
        self.services = ServiceTracker()
        self._shutdown = asyncio.Event()
        self._start_time = time.monotonic()
        self._tasks: list[asyncio.Task] = []

        # Lazily-initialized service references
        self._fc_connection = None
        self._mavlink_proxy = None
        self._vehicle_state = None
        self._param_cache = None

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

        # Initialize parameter cache and wire into VehicleState
        from ados.services.mavlink.param_cache import ParamCache
        self._param_cache = ParamCache()
        self._param_cache.load()
        self._vehicle_state.param_cache = self._param_cache

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
        self._start_service("fc-connection", self._fc_connection.run())

        # Start MAVLink WebSocket proxy
        from ados.services.mavlink.proxy import MavlinkProxy
        self._mavlink_proxy = MavlinkProxy(
            self.config.mavlink,
            self._fc_connection,
        )
        self._start_service("mavlink-ws-proxy", self._mavlink_proxy.run())

        # Start TCP proxy
        from ados.services.mavlink.tcp_proxy import TcpProxy
        tcp_proxy = TcpProxy(self._fc_connection, port=5760)
        self._start_service("mavlink-tcp-proxy", tcp_proxy.run())

        # Start UDP proxy
        from ados.services.mavlink.udp_proxy import UdpProxy
        for udp_port in (14550, 14551):
            udp_proxy = UdpProxy(self._fc_connection, port=udp_port)
            self._start_service(f"mavlink-udp-{udp_port}", udp_proxy.run())

        # Start REST API
        from ados.api.server import create_api_task
        api_task = create_api_task(self)
        self._start_service("rest-api", api_task)

        # Start MQTT gateway if enabled
        if self.config.server.mode != "disabled":
            from ados.services.mqtt.gateway import MqttGateway
            mqtt = MqttGateway(self.config, self._vehicle_state)
            self._start_service("mqtt-gateway", mqtt.run(self._shutdown))

        # Health monitor loop
        self._start_service("health-monitor", self._health_loop())

        # Agent heartbeat to FC
        self._start_service("agent-heartbeat", self._heartbeat_loop())

        # Notify systemd
        self.health.sd_notify_ready()

        log.info("agent_started", services=len(self._tasks))

        # Wait for shutdown signal
        await self._shutdown.wait()
        await self._stop()

    def _start_service(self, name: str, coro) -> None:
        """Create a tracked asyncio task for a service."""
        self.services.set_state(name, ServiceState.STARTING)

        async def _wrapper():
            try:
                self.services.set_state(name, ServiceState.RUNNING)
                await coro
            except asyncio.CancelledError:
                self.services.set_state(name, ServiceState.STOPPED)
                raise
            except Exception as exc:
                log.error("service_failed", service=name, error=str(exc))
                self.services.set_state(name, ServiceState.FAILED)

        task = asyncio.create_task(_wrapper(), name=name)
        self._tasks.append(task)

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
            name = task.get_name()
            self.services.set_state(name, ServiceState.STOPPED)
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
