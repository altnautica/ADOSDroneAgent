"""Command execution routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger

log = get_logger("api.commands")

router = APIRouter()


class CommandRequest(BaseModel):
    cmd: str
    args: list[float | str] = []


SIMPLE_COMMANDS = {
    "arm": "Arm the vehicle",
    "disarm": "Disarm the vehicle",
    "takeoff": "Takeoff to altitude (args: [altitude_m])",
    "land": "Land at current position",
    "rtl": "Return to launch",
    "mode": "Set flight mode (args: [mode_name])",
}


@router.post("/command")
async def execute_command(req: CommandRequest):
    """Execute a text command."""
    app = get_agent_app()
    fc = app._fc_connection

    if not fc or not fc.connected:
        raise HTTPException(status_code=503, detail="FC not connected")

    conn = fc.connection
    if not conn:
        raise HTTPException(status_code=503, detail="No MAVLink connection")

    cmd = req.cmd.lower()
    log.info("command_received", cmd=cmd, args=req.args)

    from pymavlink import mavutil

    if cmd == "arm":
        conn.arducopter_arm()
        return {"status": "ok", "cmd": "arm"}

    elif cmd == "disarm":
        conn.arducopter_disarm()
        return {"status": "ok", "cmd": "disarm"}

    elif cmd == "takeoff":
        alt = float(req.args[0]) if req.args else 10.0
        conn.mav.command_long_send(
            conn.target_system, conn.target_component,
            mavutil.mavlink.MAV_CMD_NAV_TAKEOFF,
            0, 0, 0, 0, 0, 0, 0, alt,
        )
        return {"status": "ok", "cmd": "takeoff", "altitude": alt}

    elif cmd == "land":
        conn.mav.command_long_send(
            conn.target_system, conn.target_component,
            mavutil.mavlink.MAV_CMD_NAV_LAND,
            0, 0, 0, 0, 0, 0, 0, 0,
        )
        return {"status": "ok", "cmd": "land"}

    elif cmd == "rtl":
        conn.mav.set_mode_apm(mavutil.mavlink.MAV_MODE_FLAG_CUSTOM_MODE_ENABLED, 6)
        return {"status": "ok", "cmd": "rtl"}

    elif cmd == "mode":
        if not req.args:
            raise HTTPException(status_code=400, detail="Mode name required")
        mode_name = str(req.args[0]).upper()
        mode_map = conn.mode_mapping()
        if mode_name not in mode_map:
            raise HTTPException(status_code=400, detail=f"Unknown mode: {mode_name}")
        conn.set_mode(mode_map[mode_name])
        return {"status": "ok", "cmd": "mode", "mode": mode_name}

    else:
        raise HTTPException(status_code=400, detail=f"Unknown command: {cmd}")


@router.get("/commands")
async def list_commands():
    """List available commands."""
    return {"commands": SIMPLE_COMMANDS}
