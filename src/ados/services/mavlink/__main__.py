"""Standalone MAVLink proxy service.

Owns the FC serial connection and broadcasts MAVLink frames to all consumers
via Unix socket IPC. Also exposes WebSocket, TCP, and UDP endpoints for
direct GCS connections.

Run: python -m ados.services.mavlink
"""

from __future__ import annotations

import argparse
import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.ipc import MavlinkIPCServer, StateIPCServer
from ados.core.logging import configure_logging

# Default direct-GCS proxy ports. Overridable on the command line so a second
# instance (the parity harness) can run alongside the first without a clash.
_DEFAULT_TCP_PORT = 5760
_DEFAULT_UDP_PORTS = [14550, 14551]


def _parse_cli_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse the optional CLI overrides.

    All options are optional with defaults that reproduce the production
    behaviour, so the systemd unit (which passes no arguments) is unaffected.
    """
    parser = argparse.ArgumentParser(
        prog="ados.services.mavlink",
        description="ADOS MAVLink proxy service (FC link + IPC + GCS proxies).",
    )
    parser.add_argument(
        "--demo",
        action="store_true",
        help="run a synthetic FC (no serial) for hardware-free testing",
    )
    parser.add_argument(
        "--fc",
        default=None,
        help="override the FC connection string (e.g. tcp:127.0.0.1:5760)",
    )
    parser.add_argument(
        "--ws-port",
        type=int,
        default=None,
        help="override the WebSocket proxy port",
    )
    parser.add_argument(
        "--tcp-port",
        type=int,
        default=None,
        help="override the TCP proxy port",
    )
    parser.add_argument(
        "--udp-ports",
        default=None,
        help="override the UDP proxy ports (comma-separated)",
    )
    return parser.parse_args(argv)


async def main() -> None:
    args = _parse_cli_args()
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("mavlink_service_starting")

    # Apply optional overrides. With no arguments these are all no-ops, so the
    # production service behaves exactly as before.
    if args.fc is not None:
        config.mavlink.serial_port = args.fc
    if args.ws_port is not None:
        _set_ws_port(config, args.ws_port)
    tcp_port = args.tcp_port if args.tcp_port is not None else _DEFAULT_TCP_PORT
    if args.udp_ports is not None:
        udp_ports = [
            int(p) for p in args.udp_ports.split(",") if p.strip()
        ] or list(_DEFAULT_UDP_PORTS)
    else:
        udp_ports = list(_DEFAULT_UDP_PORTS)

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Start IPC servers
    mavlink_ipc = MavlinkIPCServer()
    state_ipc = StateIPCServer()
    await mavlink_ipc.start()
    await state_ipc.start()

    # Start FC connection. In demo mode a synthetic source feeds the same IPC,
    # state, and proxy paths a serial FC would; the serial path is untouched
    # when demo mode is off (the default).
    from ados.services.mavlink.state import VehicleState

    vehicle_state = VehicleState()
    if args.demo:
        from ados.services.mavlink._parity_demo import ParityDemoFC

        fc = ParityDemoFC(vehicle_state)
        log.info("mavlink_service_demo_mode")
    else:
        from ados.services.mavlink.connection import FCConnection

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

    # Send companion computer heartbeat to FC at 1Hz so ArduPilot registers
    # us as a valid GCS component and does not trigger GCS failsafe.
    async def heartbeat_loop() -> None:
        while not shutdown.is_set():
            fc.send_heartbeat()
            await asyncio.sleep(1.0)

    # Periodically publish state to state IPC.
    # Also publish FC connection metadata + service uptime so the API
    # service's /status endpoint can return real values instead of the
    # StandaloneAgent shim's hardcoded `False` and `0.0`. Without this,
    # `ados status` always shows "FC: False / Uptime: 0s" even when
    # the FC is connected.
    import time as _time
    _service_start = _time.monotonic()

    async def state_publish_loop() -> None:
        while not shutdown.is_set():
            payload = vehicle_state.to_dict()
            payload["fc_connected"] = fc.connected
            payload["fc_port"] = fc.port
            payload["fc_baud"] = fc.baud
            payload["service_uptime"] = _time.monotonic() - _service_start
            # Param-sweep state. The API process lives on its own
            # systemd unit and cannot reach this FCConnection directly,
            # so we ferry the priming flags through the same state IPC
            # the rest of the FC snapshot rides on. Tick the deadline
            # check here so the timeout fires regardless of whether
            # /api/params is being polled.
            cached_count = 0
            pc = getattr(vehicle_state, "param_cache", None)
            if pc is not None:
                try:
                    cached_count = pc.count
                except Exception:
                    cached_count = 0
            else:
                cached_count = len(getattr(vehicle_state, "params", {}) or {})
            expected_count = int(getattr(vehicle_state, "param_count", 0) or 0)
            try:
                fc.note_param_progress(cached_count, expected_count)
            except AttributeError:
                pass
            payload["param_priming"] = bool(getattr(fc, "param_priming", False))
            payload["param_sweep_timed_out"] = bool(
                getattr(fc, "param_sweep_timed_out", False)
            )
            payload["param_sweep_send_failed"] = bool(
                getattr(fc, "param_sweep_send_failed", False)
            )
            payload["param_cached_count"] = cached_count
            payload["param_expected_count"] = expected_count
            # Ship the actual params dict too so the API process can serve
            # /api/params without having direct access to the cache. The
            # cache lives in this process; the API service polls the IPC
            # state at 10 Hz so the dashboard sees up-to-date values.
            params_blob: dict[str, float] = {}
            if pc is not None:
                try:
                    params_blob = pc.get_all()
                except Exception:
                    params_blob = {}
            else:
                vs_params = getattr(vehicle_state, "params", None)
                if isinstance(vs_params, dict):
                    params_blob = {
                        k: float(v) for k, v in vs_params.items()
                        if isinstance(v, (int, float))
                    }
            payload["params"] = params_blob
            state_ipc.publish(payload)
            await asyncio.sleep(0.1)  # 10Hz

    # Start proxies
    from ados.services.mavlink.proxy import MavlinkProxy
    from ados.services.mavlink.tcp_proxy import TcpProxy
    from ados.services.mavlink.udp_proxy import UdpProxy

    ws_proxy = MavlinkProxy(config.mavlink, fc)
    tcp_proxy = TcpProxy(fc, port=tcp_port)
    udp_proxies = [UdpProxy(fc, port=p) for p in udp_ports]

    tasks = [
        asyncio.create_task(fc.run(), name="fc-connection"),
        asyncio.create_task(heartbeat_loop(), name="fc-heartbeat"),
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


def _set_ws_port(config, port: int) -> None:
    """Point the WebSocket endpoint at ``port``, adding one if none is present."""
    for ep in config.mavlink.endpoints:
        if ep.type == "websocket":
            ep.port = port
            return
    from ados.core.config.mavlink import EndpointConfig

    config.mavlink.endpoints.append(
        EndpointConfig(type="websocket", port=port, enabled=True)
    )


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
