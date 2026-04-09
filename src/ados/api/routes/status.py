"""Status and telemetry routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados import __version__
from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/status")
async def get_status():
    """Agent status: version, uptime, board, FC connection state.

    DEC-108 Phase E: when running under the multi-process supervisor (the
    normal production path), the API service is a separate process from
    ados-mavlink and has no direct access to the FC connection. The
    `_StandaloneAgent` shim in services/api/__main__.py keeps `_fc_connection`
    as None, which used to make this endpoint return "FC: False / Uptime: 0s"
    forever. Fix: read from the StateIPC client (which subscribes to
    `/run/ados/state.sock` published by ados-mavlink at 10Hz) instead of
    the local `_fc_connection`. The mavlink service now publishes
    `fc_connected`, `fc_port`, `fc_baud`, and `service_uptime` alongside
    the vehicle state dict.
    """
    app = get_agent_app()
    board_info = {}
    try:
        from ados.hal.detect import detect_board
        board = detect_board()
        board_info = board.to_dict()
    except Exception:
        pass

    health_info = app.health.last.to_dict()

    from ados.core.deps import check_video_dependencies
    deps = check_video_dependencies()

    # Read live state from StateIPC if available (multi-process mode), fall
    # back to the in-process FC connection if running as single-process.
    state_client = getattr(app, "_state_client", None)
    state = state_client.state if state_client and state_client.state else {}

    fc_connected = state.get("fc_connected")
    fc_port = state.get("fc_port")
    fc_baud = state.get("fc_baud")
    state_uptime = state.get("service_uptime")

    if fc_connected is None and app._fc_connection is not None:
        # Single-process fallback (e.g. running ados-agent monolithically)
        fc_connected = app._fc_connection.connected
        fc_port = getattr(app._fc_connection, "port", None)
        fc_baud = getattr(app._fc_connection, "baud", None)

    if fc_connected is None:
        fc_connected = False

    # Prefer the mavlink service's uptime when available (it's the actual
    # "agent uptime" the user cares about). Falls back to the API service's
    # own uptime which is 0.0 in the StandaloneAgent shim.
    uptime = state_uptime if state_uptime is not None else app.uptime_seconds

    return {
        "version": __version__,
        "uptime_seconds": uptime,
        "board": board_info,
        "health": health_info,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
        "dependencies": {d.name: d.found for d in deps},
    }


@router.get("/telemetry")
async def get_telemetry():
    """Current vehicle state from VehicleState."""
    app = get_agent_app()
    if app._vehicle_state:
        return app._vehicle_state.to_dict()
    return {}
