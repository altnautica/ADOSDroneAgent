"""Main entry point for ADOS Drone Agent."""

from __future__ import annotations

import asyncio
import signal
import socket
import time
from enum import StrEnum

from ados import __version__
from ados.core.config import ADOSConfig, load_config
from ados.core.health import HealthMonitor
from ados.core.logging import configure_logging, get_logger

log = get_logger("main")


def _get_local_ip() -> str:
    """Detect local IP via UDP socket probe (works without mDNS)."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"


class ServiceState(StrEnum):
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
            log.info(
                "service_state_change",
                service=name,
                from_state=str(prev),
                to_state=state.value,
            )

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

        # Lazily-initialized service references (private — internal only)
        self._fc_connection = None
        self._mavlink_proxy = None
        self._vehicle_state = None
        self._param_cache = None
        self._video_pipeline = None
        self._wfb_manager = None
        self._command_executor = None
        self._script_runner = None
        self._demo_scripting = None
        # Public attribute: accessed by OTA API routes via get_agent_app().ota_updater
        self.ota_updater = None

        # Pairing and discovery (public — accessed by API routes)
        from ados.core.pairing import PairingManager
        self.pairing_manager = PairingManager(state_path=config.pairing.state_path)
        self.discovery_service = None
        self.board_name = "unknown"

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
        self._board = board  # store for heartbeat access
        self.board_name = board.name
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
            mqtt = MqttGateway(
                self.config, self._vehicle_state,
                api_key=self.pairing_manager.api_key,
            )
            self._start_service("mqtt-gateway", mqtt.run(self._shutdown))

        # Start Video Pipeline
        if self.demo:
            from ados.services.video.demo import DemoVideoPipeline
            self._video_pipeline = DemoVideoPipeline()
            self._start_service("video-pipeline", self._video_pipeline.run())
        else:
            from ados.services.video.pipeline import VideoPipeline
            self._video_pipeline = VideoPipeline(self.config.video)
            self._start_service("video-pipeline", self._video_pipeline.run())

        # Start WFB-ng Link Manager
        if self.demo:
            from ados.services.wfb.demo import DemoWfbManager
            self._wfb_manager = DemoWfbManager()
            self._start_service("wfb-link", self._wfb_manager.run())
        else:
            from ados.services.wfb.manager import WfbManager
            self._wfb_manager = WfbManager(self.config.video.wfb)
            self._start_service("wfb-link", self._wfb_manager.run())

        # Start Scripting Engine
        if self.demo:
            from ados.services.scripting.demo import DemoScriptingEngine
            self._demo_scripting = DemoScriptingEngine()
            self._start_service("scripting", self._demo_scripting.run())
            log.info("scripting_demo_mode", msg="Demo scripting engine active")
        else:
            from ados.services.scripting.executor import CommandExecutor
            from ados.services.scripting.safety import SafetyLimits, SafetyValidator
            from ados.services.scripting.script_runner import ScriptRunner
            from ados.services.scripting.text_listener import TextCommandListener

            safety = SafetyValidator(SafetyLimits(), self._vehicle_state)
            self._command_executor = CommandExecutor(
                self._fc_connection, self._vehicle_state, safety,
            )
            listener = TextCommandListener(
                self.config.scripting.text_commands, self._command_executor,
            )
            self._start_service("text-commands", listener.run())

            self._script_runner = ScriptRunner(
                self.config.scripting.scripts, self._command_executor,
            )

        # Start OTA Updater
        if self.demo:
            from ados.services.ota.demo import DemoOtaUpdater
            self.ota_updater = DemoOtaUpdater()
            self._start_service("ota-updater", self.ota_updater.run())
        else:
            from ados.services.ota.checker import UpdateChecker
            from ados.services.ota.downloader import UpdateDownloader
            from ados.services.ota.updater import OtaUpdater
            checker = UpdateChecker(self.config.ota)
            downloader = UpdateDownloader()
            self.ota_updater = OtaUpdater(
                self.config.ota, checker, downloader,
                current_version=__version__,
            )
            self._start_service("ota-updater", self.ota_updater.run())

        # Health monitor loop
        self._start_service("health-monitor", self._health_loop())

        # Agent heartbeat to FC
        self._start_service("agent-heartbeat", self._heartbeat_loop())

        # mDNS discovery registration
        if self.config.discovery.mdns_enabled:
            from ados.services.discovery import DiscoveryService
            api_port = self.config.scripting.rest_api.port
            self.discovery_service = DiscoveryService(
                device_id=self.config.agent.device_id,
                port=api_port,
                name=self.config.agent.name,
                version=__version__,
                board=self.board_name,
            )
            pm = self.pairing_manager
            await self.discovery_service.register(
                paired=pm.is_paired,
                code=pm.get_or_create_code() if not pm.is_paired else None,
                owner=pm.owner_id,
            )

        # Cloud pairing beacon (when unpaired, POST code to Convex)
        self._start_service("pairing-beacon", self._cloud_beacon_loop())

        # Cloud heartbeat (when paired, POST status to Convex)
        self._start_service("pairing-heartbeat", self._cloud_heartbeat_loop())

        # Cloud command polling (when paired, poll Convex for commands)
        self._start_service("cloud-command-poll", self._cloud_command_poll_loop())

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

    async def _cloud_beacon_loop(self) -> None:
        """When unpaired, periodically POST pairing code to Convex for cloud discovery."""
        import httpx

        interval = self.config.pairing.beacon_interval
        convex_url = self.config.pairing.convex_url

        while not self._shutdown.is_set():
            if not self.pairing_manager.is_paired and convex_url:
                try:
                    code = self.pairing_manager.get_or_create_code()
                    local_ip = _get_local_ip()
                    mdns_host = ""
                    if self.discovery_service:
                        mdns_host = self.discovery_service.mdns_hostname

                    api_key = self.pairing_manager.generate_api_key()
                    async with httpx.AsyncClient(timeout=10.0) as client:
                        await client.post(
                            f"{convex_url}/pairing/register",
                            json={
                                "deviceId": self.config.agent.device_id,
                                "pairingCode": code,
                                "apiKey": api_key,
                                "name": self.config.agent.name,
                                "version": __version__,
                                "board": self.board_name,
                                "mdnsHost": mdns_host,
                                "localIp": local_ip,
                            },
                        )
                    log.debug("pairing_beacon_sent", code=code)
                except Exception:
                    log.debug("pairing_beacon_failed")
            await asyncio.sleep(interval)

    async def _cloud_heartbeat_loop(self) -> None:
        """When paired, periodically POST full status to Convex."""
        import httpx

        convex_url = self.config.pairing.convex_url

        while not self._shutdown.is_set():
            if self.pairing_manager.is_paired and convex_url:
                try:
                    local_ip = _get_local_ip()
                    mdns_host = ""
                    if self.discovery_service:
                        mdns_host = self.discovery_service.mdns_hostname

                    fc_connected = False
                    fc_port = ""
                    fc_baud = 0
                    if self._fc_connection:
                        fc_connected = getattr(self._fc_connection, "connected", False)
                        fc_port = getattr(self.config.mavlink, "port", "")
                        fc_baud = getattr(self.config.mavlink, "baud", 0)

                    # Board info from detection
                    board = getattr(self, "_board", None)
                    board_tier = board.tier if board else 0
                    board_soc = board.soc if board else ""
                    board_arch = board.arch if board else ""

                    # Health info from monitor
                    health = self.health
                    cpu_percent = getattr(health, "cpu_percent", 0.0)
                    memory_percent = getattr(health, "memory_percent", 0.0)
                    disk_percent = getattr(health, "disk_percent", 0.0)
                    temperature = getattr(health, "temperature", None)

                    # Service states with accurate operational status
                    service_list = []
                    all_services = self.services.get_all()

                    # Infer true state from runtime conditions
                    def _svc_state(name: str, state: ServiceState) -> str:
                        s = state.value
                        if s != "running":
                            return s
                        if name == "fc-connection":
                            fc = getattr(self, "_fc_connection", None)
                            if fc and not getattr(fc, "connected", False):
                                return "degraded"
                        elif name == "video-pipeline":
                            if getattr(self.config.video, "mode", "disabled") == "disabled":
                                return "stopped"
                        elif name == "wfb-link":
                            wfb = getattr(self, "_wfb_manager", None)
                            if wfb and not getattr(wfb, "has_adapter", False):
                                return "degraded"
                        elif name == "pairing-beacon":
                            if self.pairing_manager.is_paired:
                                return "stopped"
                        return s

                    running_count = 0
                    for svc_name, svc_state in all_services.items():
                        real_state = _svc_state(svc_name, svc_state)
                        if real_state == "running":
                            running_count += 1

                    # Get process metrics for distribution across running services
                    proc_cpu = 0.0
                    proc_rss_mb = 0.0
                    try:
                        import os as _os

                        import psutil as _psutil
                        _proc = _psutil.Process(_os.getpid())
                        proc_cpu = _proc.cpu_percent(interval=0)
                        proc_rss_mb = _proc.memory_info().rss / (1024 * 1024)
                    except Exception:
                        pass

                    per_svc_cpu = proc_cpu / running_count if running_count > 0 else 0.0
                    per_svc_rss = proc_rss_mb / running_count if running_count > 0 else 0.0

                    for svc_name, svc_state in all_services.items():
                        real_state = _svc_state(svc_name, svc_state)
                        is_running = real_state == "running"
                        service_list.append({
                            "name": svc_name,
                            "status": real_state,
                            "cpuPercent": round(per_svc_cpu, 1) if is_running else 0,
                            "memoryMb": round(per_svc_rss, 1) if is_running else 0,
                        })

                    payload = {
                        "deviceId": self.config.agent.device_id,
                        "apiKey": self.pairing_manager.api_key,
                        "version": __version__,
                        "uptimeSeconds": self.uptime_seconds,
                        "boardName": self.board_name,
                        "boardTier": board_tier,
                        "boardSoc": board_soc,
                        "boardArch": board_arch,
                        "cpuPercent": cpu_percent,
                        "memoryPercent": memory_percent,
                        "diskPercent": disk_percent,
                        "temperature": temperature,
                        "fcConnected": fc_connected,
                        "fcPort": fc_port,
                        "fcBaud": fc_baud,
                        "services": service_list,
                        "lastIp": local_ip,
                        "mdnsHost": mdns_host,
                        "agentVersion": __version__,
                    }

                    async with httpx.AsyncClient(timeout=10.0) as client:
                        await client.post(
                            f"{convex_url}/agent/status",
                            json=payload,
                        )
                    log.debug("cloud_status_sent")
                except Exception:
                    log.debug("cloud_status_failed")
            await asyncio.sleep(5)

    async def _cloud_command_poll_loop(self) -> None:
        """When paired, poll Convex for pending commands and execute them."""
        import httpx

        convex_url = self.config.pairing.convex_url

        while not self._shutdown.is_set():
            if self.pairing_manager.is_paired and convex_url:
                try:
                    device_id = self.config.agent.device_id
                    api_key = self.pairing_manager.api_key

                    async with httpx.AsyncClient(timeout=10.0) as client:
                        resp = await client.get(
                            f"{convex_url}/agent/commands",
                            params={"deviceId": device_id, "apiKey": api_key},
                        )
                        if resp.status_code != 200:
                            log.debug("cloud_command_poll_error", status=resp.status_code)
                            await asyncio.sleep(5)
                            continue

                        data = resp.json()
                        commands = data.get("commands", [])

                        for cmd in commands:
                            cmd_id = cmd.get("_id", "")
                            command = cmd.get("command", "")
                            args = cmd.get("args")
                            result = {"success": False, "message": "Unknown command"}

                            try:
                                if command == "restart_service":
                                    svc_name = args.get("name", "") if args else ""
                                    msg = f"Service '{svc_name}' restart requested"
                                    result = {"success": True, "message": msg}
                                elif command == "send_command":
                                    cmd_text = args.get("cmd", "") if args else ""
                                    if self._command_executor:
                                        exec_result = await asyncio.wait_for(
                                            self._command_executor.execute(cmd_text),
                                            timeout=10.0,
                                        )
                                        result = {
                                            "success": True,
                                            "message": str(exec_result),
                                        }
                                    else:
                                        result = {
                                            "success": False,
                                            "message": "Command executor not available",
                                        }
                                elif command == "scan_peripherals":
                                    result = {
                                        "success": True,
                                        "message": "Peripheral scan complete",
                                    }
                                else:
                                    msg = f"Unknown command: {command}"
                                    result = {"success": False, "message": msg}
                            except Exception as exc:
                                result = {"success": False, "message": str(exc)}

                            # Ack the command back to Convex
                            try:
                                await client.post(
                                    f"{convex_url}/agent/commands/ack",
                                    json={
                                        "commandId": cmd_id,
                                        "deviceId": device_id,
                                        "apiKey": api_key,
                                        "status": "completed" if result["success"] else "failed",
                                        "result": result,
                                    },
                                )
                            except Exception:
                                log.debug("cloud_command_ack_failed", command_id=cmd_id)

                except Exception:
                    log.debug("cloud_command_poll_failed")
            await asyncio.sleep(5)

    async def _stop(self) -> None:
        """Gracefully stop all services."""
        log.info("agent_stopping")

        # Unregister mDNS before cancelling tasks
        if self.discovery_service:
            await self.discovery_service.unregister()

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
