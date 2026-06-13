"""Command execution routes."""

from __future__ import annotations

import asyncio

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel
from pymavlink.dialects.v20 import common as mavlink2

from ados.api.deps import get_agent_app
from ados.core.ipc import MAVLINK_SOCK, MavlinkIPCClient
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

# Source identity stamped on every command frame: the agent/companion identity
# the MAVLink router uses on its own FC send path (defaults 1/191), so a command
# from this surface looks identical on the wire to one the router sent.
_SOURCE_SYS = 1
_SOURCE_COMP = 191

# Single-vehicle ArduPilot target. The state socket carries no target system, so
# this surface targets the primary autopilot at 1/1.
_TARGET_SYS = 1
_TARGET_COMP = 1

# DO_SET_MODE param1: MAV_MODE_FLAG_CUSTOM_MODE_ENABLED (1), the flag pymavlink's
# set_mode_apm sets so param2 carries a custom_mode.
_CUSTOM_MODE_ENABLED = 1.0

# RTL's custom_mode in the ArduCopter mode table (COPTER_MODE_NUMBERS RTL → 6).
_RTL_CUSTOM_MODE = 6

# Default takeoff altitude in metres when the request carries no args[0].
_DEFAULT_TAKEOFF_ALT_M = 10.0

# ArduCopter flight-mode name → custom_mode. The route resolves a mode name to
# its custom mode here; an unknown name is a 400.
_COPTER_MODE_NUMBERS = {
    "STABILIZE": 0,
    "ACRO": 1,
    "ALT_HOLD": 2,
    "AUTO": 3,
    "GUIDED": 4,
    "LOITER": 5,
    "RTL": 6,
    "CIRCLE": 7,
    "LAND": 9,
    "DRIFT": 11,
    "SPORT": 13,
    "FLIP": 14,
    "AUTOTUNE": 15,
    "POSHOLD": 16,
    "BRAKE": 17,
    "THROW": 18,
    "AVOID_ADSB": 19,
    "GUIDED_NOGPS": 20,
    "SMART_RTL": 21,
    "FLOWHOLD": 22,
    "FOLLOW": 23,
    "ZIGZAG": 24,
    "SYSTEMID": 25,
    "AUTOROTATE": 26,
    "AUTO_RTL": 27,
}


def _encode_command_long(
    command: int, params: tuple[float, float, float, float, float, float, float]
) -> bytes:
    """Encode a COMMAND_LONG to the primary autopilot as a v2 wire frame.

    A fresh encoder per call keeps the monotonic sequence number from being
    shared with other callers; the source identity (1/191) matches the router's
    own FC send path so the frame is wire-identical to a router-sent command.
    """
    encoder = mavlink2.MAVLink(None, srcSystem=_SOURCE_SYS, srcComponent=_SOURCE_COMP)
    encoder.robust_parsing = True
    msg = encoder.command_long_encode(
        _TARGET_SYS,
        _TARGET_COMP,
        command,
        0,  # confirmation
        params[0],
        params[1],
        params[2],
        params[3],
        params[4],
        params[5],
        params[6],
    )
    return msg.pack(encoder)


def _build_command_frame(cmd: str, args: list[float | str]) -> tuple[bytes, dict]:
    """Build the COMMAND_LONG wire frame + the success body for a named command.

    Raises HTTPException(400) for an unknown command or a bad mode argument. Every
    command maps to a COMMAND_LONG; arm/disarm/land emit {"status","cmd"}, takeoff
    adds "altitude", mode adds "mode".
    """
    if cmd == "arm":
        frame = _encode_command_long(
            mavlink2.MAV_CMD_COMPONENT_ARM_DISARM,
            (1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        return frame, {"status": "ok", "cmd": "arm"}

    if cmd == "disarm":
        frame = _encode_command_long(
            mavlink2.MAV_CMD_COMPONENT_ARM_DISARM,
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        return frame, {"status": "ok", "cmd": "disarm"}

    if cmd == "takeoff":
        alt = _takeoff_altitude(args)
        frame = _encode_command_long(
            mavlink2.MAV_CMD_NAV_TAKEOFF,
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, alt),
        )
        return frame, {"status": "ok", "cmd": "takeoff", "altitude": alt}

    if cmd == "land":
        frame = _encode_command_long(
            mavlink2.MAV_CMD_NAV_LAND,
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        return frame, {"status": "ok", "cmd": "land"}

    if cmd == "rtl":
        # DO_SET_MODE with the RTL custom_mode (6), the same frame the `mode RTL`
        # path sends, so the shortcut commands Return-to-Launch.
        frame = _encode_command_long(
            mavlink2.MAV_CMD_DO_SET_MODE,
            (_CUSTOM_MODE_ENABLED, float(_RTL_CUSTOM_MODE), 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        return frame, {"status": "ok", "cmd": "rtl"}

    if cmd == "mode":
        if not args:
            raise HTTPException(status_code=400, detail="Mode name required")
        mode_name = str(args[0]).upper()
        custom_mode = _COPTER_MODE_NUMBERS.get(mode_name)
        if custom_mode is None:
            raise HTTPException(status_code=400, detail=f"Unknown mode: {mode_name}")
        frame = _encode_command_long(
            mavlink2.MAV_CMD_DO_SET_MODE,
            (_CUSTOM_MODE_ENABLED, float(custom_mode), 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        return frame, {"status": "ok", "cmd": "mode", "mode": mode_name}

    raise HTTPException(status_code=400, detail=f"Unknown command: {cmd}")


def _takeoff_altitude(args: list[float | str]) -> float:
    """The takeoff altitude from args[0], defaulting to 10.0 when absent.

    Mirrors `float(req.args[0]) if req.args else 10.0`: a numeric or stringly-
    typed numeric arg is used, a non-numeric arg falls back to the default.
    """
    if not args:
        return _DEFAULT_TAKEOFF_ALT_M
    try:
        return float(args[0])
    except (TypeError, ValueError):
        return _DEFAULT_TAKEOFF_ALT_M


async def _send_via_mavlink_ipc(frame: bytes) -> None:
    """Open a short-lived IPC client, push the frame to the router, disconnect.

    The MAVLink service owns `/run/ados/mavlink.sock`; a frame written here is
    forwarded to the FC verbatim. An unreachable socket means no live FC link
    from this surface's view → 503, matching the no-connection case. The command
    is never silently dropped.
    """
    ipc = MavlinkIPCClient(sock_path=MAVLINK_SOCK)
    try:
        await ipc.connect(retries=2, delay=0.25)
    except ConnectionError as exc:
        raise HTTPException(status_code=503, detail="No MAVLink connection") from exc
    try:
        ipc.send(frame)
        # Yield once so the kernel buffer ships the frame before disconnect.
        await asyncio.sleep(0)
    finally:
        try:
            await ipc.disconnect()
        except Exception:
            pass


@router.post("/command")
async def execute_command(req: CommandRequest):
    """Execute a text command.

    Gates on the same FC-connected signal `/api/status` reports
    (`app.fc_status().connected`, derived from the state-socket snapshot in the
    multi-process runtime) rather than an in-process pymavlink connection object,
    which the standalone API service never holds. The command becomes a
    COMMAND_LONG frame written to the MAVLink IPC socket the router reads.
    """
    app = get_agent_app()

    if not app.fc_status().connected:
        raise HTTPException(status_code=503, detail="FC not connected")

    cmd = req.cmd.lower()
    log.info("command_received", cmd=cmd, args=req.args)

    frame, body = _build_command_frame(cmd, req.args)
    await _send_via_mavlink_ipc(frame)
    return body


@router.get("/commands")
async def list_commands():
    """List available commands."""
    return {"commands": SIMPLE_COMMANDS}
