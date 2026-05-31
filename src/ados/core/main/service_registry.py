"""Service registry — what the agent spawns at startup, in order.

Extracted from the original monolithic ``AgentApp.start()``. The
registry knows the order things come up in, what's gated on demo
mode, what's gated on a non-disabled cloud server, and what's
profile-specific (drone vs ground-station).

``register_services(app)`` is called once from ``AgentApp.start()``
and returns when every service has been kicked off. Each service is
attached to the asyncio loop via ``app._start_service(name, coro)``.
The actual readiness-wait + shutdown wiring stay on the AgentApp.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados import __version__
from ados.core.logging import get_logger

if TYPE_CHECKING:
    from .app import AgentApp

log = get_logger("main")


async def register_services(app: AgentApp) -> None:  # noqa: C901
    """Spawn every long-running agent service and wire its task into the supervisor."""
    log.info(
        "agent_starting",
        version=__version__,
        device_id=app.config.agent.device_id,
        name=app.config.agent.name,
    )

    # Detect board
    from ados.hal.detect import detect_board
    board = detect_board()
    app._board = board  # store for heartbeat access
    app.board_name = board.name
    log.info("board_info", name=board.name, tier=board.tier, ram_mb=board.ram_mb)

    # Set tier from detection if auto
    if app.config.agent.tier == "auto":
        app.config.agent.tier = f"tier{board.tier}"

    # Initialize model manager (vision model registry + cache)
    from ados.services.vision.model_manager import ModelManager

    # Load raw board profile YAML so the model manager can pick variants
    # sized for the detected NPU.
    board_profile_dict: dict = {}
    try:
        import yaml as _yaml

        from ados.hal.detect import BOARDS_DIR

        if BOARDS_DIR.is_dir():
            model_lower = board.model.lower()
            for yaml_file in sorted(BOARDS_DIR.glob("*.yaml")):
                with open(yaml_file) as _f:
                    raw = _yaml.safe_load(_f)
                    if not raw:
                        continue
                    for pattern in raw.get("model_patterns", []):
                        if pattern.lower() in model_lower:
                            board_profile_dict = raw
                            break
                if board_profile_dict:
                    break
    except Exception:
        pass

    npu_tops = board_profile_dict.get("compute", {}).get("npu_tops", 0)
    app.model_manager = ModelManager(app.config.vision, npu_tops=npu_tops)

    # Initialize MAVLink connection
    from ados.services.mavlink.state import VehicleState
    app._vehicle_state = VehicleState()

    # Initialize parameter cache and wire into VehicleState
    from ados.services.mavlink.param_cache import ParamCache
    app._param_cache = ParamCache()
    app._param_cache.load()
    app._vehicle_state.param_cache = app._param_cache

    if app.demo:
        log.info("demo_mode", msg="DEMO MODE — simulated telemetry, no real FC")
        from ados.services.mavlink.demo import DemoFCConnection
        app._fc_connection = DemoFCConnection(app._vehicle_state)
    else:
        from ados.services.mavlink.connection import FCConnection
        app._fc_connection = FCConnection(
            app.config.mavlink,
            app._vehicle_state,
        )

    # Start FC connection task
    app._start_service("fc-connection", app._fc_connection.run())

    # Start MAVLink WebSocket proxy
    from ados.services.mavlink.proxy import MavlinkProxy
    app._mavlink_proxy = MavlinkProxy(
        app.config.mavlink,
        app._fc_connection,
    )
    app._start_service("mavlink-ws-proxy", app._mavlink_proxy.run())

    # Start TCP proxy
    from ados.services.mavlink.tcp_proxy import TcpProxy
    tcp_proxy = TcpProxy(app._fc_connection, port=5760)
    app._start_service("mavlink-tcp-proxy", tcp_proxy.run())

    # Start UDP proxy
    from ados.services.mavlink.udp_proxy import UdpProxy
    for udp_port in (14550, 14551):
        udp_proxy = UdpProxy(app._fc_connection, port=udp_port)
        app._start_service(f"mavlink-udp-{udp_port}", udp_proxy.run())

    # Start REST API
    from ados.api.server import create_api_task
    api_task = create_api_task(app)
    app._start_service("rest-api", api_task)

    # Start MQTT gateway if enabled
    if app.config.server.mode != "disabled":
        from ados.services.mqtt.gateway import MqttGateway
        mqtt = MqttGateway(
            app.config, app._vehicle_state,
            api_key=app.pairing_manager.api_key,
        )
        app._start_service("mqtt-gateway", mqtt.run(app._shutdown))

    # Start Video Pipeline
    if app.demo:
        from ados.services.video.demo import DemoVideoPipeline
        app._video_pipeline = DemoVideoPipeline()
        app._start_service("video-pipeline", app._video_pipeline.run())
    else:
        from ados.services.video.pipeline import VideoPipeline
        app._video_pipeline = VideoPipeline(app.config.video)
        app._start_service("video-pipeline", app._video_pipeline.run())

    # WFB-ng radio link.
    #
    # The drone-side transmit chain — adapter selection + monitor mode,
    # the wfb_tx/wfb_rx process group, the TX-health + video-recvq
    # watchdogs, the frequency-hop loop, the adaptive bitrate/FEC
    # controller — runs as its own systemd unit (a compiled binary), not
    # an in-process asyncio task. So the agent process spawns nothing for
    # the real radio here; the API + heartbeat read the link snapshot
    # cross-process from /run/ados/wfb-stats.json instead.
    #
    # Demo mode is orthogonal: it has no real radio to own, so it keeps
    # its in-process synthetic manager that writes the same stats sidecar.
    if app.demo:
        from ados.services.wfb.demo import DemoWfbManager
        app._wfb_manager = DemoWfbManager()
        app._start_service("wfb-link", app._wfb_manager.run())

    # Start Scripting Engine
    if app.demo:
        from ados.services.scripting.demo import DemoScriptingEngine
        app._demo_scripting = DemoScriptingEngine()
        app._start_service("scripting", app._demo_scripting.run())
        log.info("scripting_demo_mode", msg="Demo scripting engine active")
    else:
        from ados.services.scripting.executor import CommandExecutor
        from ados.services.scripting.safety import SafetyLimits, SafetyValidator
        from ados.services.scripting.script_runner import ScriptRunner
        from ados.services.scripting.text_listener import TextCommandListener

        safety = SafetyValidator(SafetyLimits(), app._vehicle_state)
        app._command_executor = CommandExecutor(
            app._fc_connection, app._vehicle_state, safety,
        )
        listener = TextCommandListener(
            app.config.scripting.text_commands, app._command_executor,
        )
        app._start_service("text-commands", listener.run())

        app._script_runner = ScriptRunner(
            app.config.scripting.scripts, app._command_executor,
        )

    # Start OTA Updater
    if app.demo:
        from ados.services.ota.demo import DemoOtaUpdater
        app.ota_updater = DemoOtaUpdater()
        app._start_service("ota-updater", app.ota_updater.run())
    else:
        from ados.services.ota.checker import UpdateChecker
        from ados.services.ota.downloader import UpdateDownloader
        from ados.services.ota.updater import OtaUpdater
        checker = UpdateChecker(app.config.ota)
        downloader = UpdateDownloader()
        app.ota_updater = OtaUpdater(
            app.config.ota, checker, downloader,
            current_version=__version__,
        )
        app._start_service("ota-updater", app.ota_updater.run())

    # Health monitor loop
    app._start_service("health-monitor", app._health_loop())

    # Agent heartbeat to FC
    app._start_service("agent-heartbeat", app._heartbeat_loop())

    # mDNS discovery registration
    if app.config.discovery.mdns_enabled:
        from ados.core.profile import current_profile_and_role
        from ados.services.discovery import DiscoveryService
        api_port = app.config.scripting.rest_api.port
        app.discovery_service = DiscoveryService(
            device_id=app.config.agent.device_id,
            port=api_port,
            name=app.config.agent.name,
            version=__version__,
            board=app.board_name,
        )
        pm = app.pairing_manager
        profile, role = current_profile_and_role(app.config)
        await app.discovery_service.register(
            paired=pm.is_paired,
            code=pm.get_or_create_code() if not pm.is_paired else None,
            owner=pm.owner_id,
            profile=profile,
            role=role,
        )

    if app._single_process_cloud_enabled():
        # Cloud pairing beacon (when unpaired, POST code to Convex)
        app._start_service("pairing-beacon", app._cloud_beacon_loop())

        # Cloud heartbeat (when paired, POST status to Convex)
        app._start_service("pairing-heartbeat", app._cloud_heartbeat_loop())

        # Cloud command polling (when paired, poll Convex for commands)
        app._start_service("cloud-command-poll", app._cloud_command_poll_loop())
    else:
        log.info(
            "single_process_cloud_disabled",
            reason="cloud runs through managed service runtime",
        )


__all__ = ["register_services"]
