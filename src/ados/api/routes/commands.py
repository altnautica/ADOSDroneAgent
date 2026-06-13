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

# Default takeoff altitude in metres when the request carries no args[0].
_DEFAULT_TAKEOFF_ALT_M = 10.0

# ArduPilot vehicle classes the route resolves modes against. The same mode
# NAME maps to a different custom_mode per class (e.g. RTL is 6 on Copter but
# 11 on Plane), so the route MUST know the live vehicle type before it can
# encode a DO_SET_MODE / RTL frame. Resolved from the FC heartbeat's mav_type.
_VEHICLE_COPTER = "copter"
_VEHICLE_PLANE = "plane"
_VEHICLE_ROVER = "rover"
_VEHICLE_SUB = "sub"

# ArduCopter flight-mode name → custom_mode.
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

# ArduPlane flight-mode name → custom_mode (RTL is 11 here, not 6).
_PLANE_MODE_NUMBERS = {
    "MANUAL": 0,
    "CIRCLE": 1,
    "STABILIZE": 2,
    "TRAINING": 3,
    "ACRO": 4,
    "FBWA": 5,
    "FBWB": 6,
    "CRUISE": 7,
    "AUTOTUNE": 8,
    "AUTO": 10,
    "RTL": 11,
    "LOITER": 12,
    "TAKEOFF": 13,
    "AVOID_ADSB": 14,
    "GUIDED": 15,
    "QSTABILIZE": 17,
    "QHOVER": 18,
    "QLOITER": 19,
    "QLAND": 20,
    "QRTL": 21,
    "QAUTOTUNE": 22,
    "QACRO": 23,
    "THERMAL": 24,
    "LOITER_TO_QLAND": 25,
}

# ArduRover flight-mode name → custom_mode.
_ROVER_MODE_NUMBERS = {
    "MANUAL": 0,
    "ACRO": 1,
    "STEERING": 3,
    "HOLD": 4,
    "LOITER": 5,
    "FOLLOW": 6,
    "SIMPLE": 7,
    "AUTO": 10,
    "RTL": 11,
    "SMART_RTL": 12,
    "GUIDED": 15,
    "INITIALISING": 16,
}

# ArduSub flight-mode name → custom_mode (no RTL).
_SUB_MODE_NUMBERS = {
    "STABILIZE": 0,
    "ACRO": 1,
    "ALT_HOLD": 2,
    "AUTO": 3,
    "GUIDED": 4,
    "CIRCLE": 7,
    "SURFACE": 9,
    "POSHOLD": 16,
    "MANUAL": 19,
    "MOTOR_DETECT": 20,
}

_MODE_TABLES: dict[str, dict[str, int]] = {
    _VEHICLE_COPTER: _COPTER_MODE_NUMBERS,
    _VEHICLE_PLANE: _PLANE_MODE_NUMBERS,
    _VEHICLE_ROVER: _ROVER_MODE_NUMBERS,
    _VEHICLE_SUB: _SUB_MODE_NUMBERS,
}

# MAV_TYPE (FC heartbeat) → ArduPilot vehicle class. Anything not mapped is an
# unknown vehicle, for which mode/rtl are refused rather than guessed as Copter.
_MAV_TYPE_TO_VEHICLE: dict[int, str] = {
    mavlink2.MAV_TYPE_QUADROTOR: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_HEXAROTOR: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_OCTOROTOR: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_TRICOPTER: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_HELICOPTER: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_COAXIAL: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_DODECAROTOR: _VEHICLE_COPTER,
    mavlink2.MAV_TYPE_FIXED_WING: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_TILTROTOR: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_QUADROTOR: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_DUOROTOR: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_RESERVED2: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_RESERVED3: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_RESERVED4: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_VTOL_RESERVED5: _VEHICLE_PLANE,
    mavlink2.MAV_TYPE_GROUND_ROVER: _VEHICLE_ROVER,
    mavlink2.MAV_TYPE_SURFACE_BOAT: _VEHICLE_ROVER,
    mavlink2.MAV_TYPE_SUBMARINE: _VEHICLE_SUB,
}


def _vehicle_class(mav_type: int) -> str | None:
    """Resolve the FC heartbeat ``mav_type`` to an ArduPilot vehicle class.

    Returns ``None`` for an unmapped / unknown type (mav_type 0 on a fresh
    snapshot before the first heartbeat, or a class the route has no mode table
    for) so the caller refuses mode/rtl rather than guessing Copter.
    """
    return _MAV_TYPE_TO_VEHICLE.get(int(mav_type))


def _resolve_rtl_custom_mode(vehicle: str | None) -> int:
    """The RTL custom_mode for the given vehicle class.

    Refuses with HTTPException(400) when the vehicle is unknown (so RTL is never
    sent as the Copter number against a Plane/Rover) or when the class has no
    RTL mode (Sub). Copter RTL=6, Plane RTL=11, Rover RTL=11.
    """
    if vehicle is None:
        raise HTTPException(
            status_code=400,
            detail="Cannot resolve RTL: FC vehicle type unknown. "
            "Wait for a heartbeat from the flight controller.",
        )
    table = _MODE_TABLES.get(vehicle, {})
    rtl = table.get("RTL")
    if rtl is None:
        raise HTTPException(
            status_code=400,
            detail=f"RTL is not available on this vehicle ({vehicle}).",
        )
    return int(rtl)


def _resolve_mode_number(vehicle: str | None, mode_name: str) -> int:
    """Resolve a mode NAME to its custom_mode for the live vehicle class.

    Refuses with HTTPException(400) when the vehicle is unknown (so a mode is
    never sent against the wrong vehicle's numbering) or when the name is not a
    mode of that vehicle.
    """
    if vehicle is None:
        raise HTTPException(
            status_code=400,
            detail="Cannot set mode: FC vehicle type unknown. "
            "Wait for a heartbeat from the flight controller.",
        )
    table = _MODE_TABLES.get(vehicle, {})
    custom_mode = table.get(mode_name)
    if custom_mode is None:
        raise HTTPException(
            status_code=400,
            detail=f"Unknown mode {mode_name!r} for vehicle {vehicle}.",
        )
    return int(custom_mode)


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


def _build_command_frame(
    cmd: str, args: list[float | str], vehicle: str | None
) -> tuple[bytes, dict]:
    """Build the COMMAND_LONG wire frame + the success body for a named command.

    ``vehicle`` is the live ArduPilot vehicle class (from the FC heartbeat's
    mav_type); ``rtl`` and ``mode`` resolve their custom_mode against it so the
    same mode name maps to the correct number per vehicle (RTL=6 Copter, 11
    Plane/Rover). Raises HTTPException(400) for an unknown command, a bad mode
    argument, or an unknown vehicle on a mode/rtl request. Every command maps to
    a COMMAND_LONG; arm/disarm/land emit {"status","cmd"}, takeoff adds
    "altitude", mode adds "mode".
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
        # DO_SET_MODE with the RTL custom_mode resolved for the live vehicle, the
        # same frame the `mode RTL` path sends, so the shortcut commands
        # Return-to-Launch on a Plane/Rover (RTL=11) just as on a Copter (RTL=6).
        rtl_custom_mode = _resolve_rtl_custom_mode(vehicle)
        frame = _encode_command_long(
            mavlink2.MAV_CMD_DO_SET_MODE,
            (_CUSTOM_MODE_ENABLED, float(rtl_custom_mode), 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        return frame, {"status": "ok", "cmd": "rtl"}

    if cmd == "mode":
        if not args:
            raise HTTPException(status_code=400, detail="Mode name required")
        mode_name = str(args[0]).upper()
        custom_mode = _resolve_mode_number(vehicle, mode_name)
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

    # Resolve the live vehicle class from the router's state snapshot so the
    # mode/rtl frames carry the correct custom_mode for this FC (Copter vs
    # Plane vs Rover vs Sub differ); mav_type 0 (no heartbeat yet) resolves to
    # None and the mode/rtl paths refuse rather than guess Copter.
    snapshot = app.state_ipc_state()
    mav_type = int(snapshot.get("mav_type", 0) or 0)
    vehicle = _vehicle_class(mav_type)

    frame, body = _build_command_frame(cmd, req.args, vehicle)
    await _send_via_mavlink_ipc(frame)
    return body


@router.get("/commands")
async def list_commands():
    """List available commands."""
    return {"commands": SIMPLE_COMMANDS}
