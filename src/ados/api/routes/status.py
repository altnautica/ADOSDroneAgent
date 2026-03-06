"""Status and telemetry routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados import __version__
from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/status")
async def get_status():
    """Agent status: version, uptime, board, FC connection state."""
    app = get_agent_app()
    board_info = {}
    try:
        from ados.hal.detect import detect_board
        board = detect_board()
        board_info = board.to_dict()
    except Exception:
        pass

    health_info = app.health.last.to_dict()

    return {
        "version": __version__,
        "uptime_seconds": app.uptime_seconds,
        "board": board_info,
        "health": health_info,
        "fc_connected": app._fc_connection.connected if app._fc_connection else False,
        "fc_port": app._fc_connection.port if app._fc_connection else None,
        "fc_baud": app._fc_connection.baud if app._fc_connection else None,
    }


@router.get("/telemetry")
async def get_telemetry():
    """Current vehicle state from VehicleState."""
    app = get_agent_app()
    if app._vehicle_state:
        return app._vehicle_state.to_dict()
    return {}
