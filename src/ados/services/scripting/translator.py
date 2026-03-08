"""Translate parsed commands to MAVLink binary for the flight controller."""

from __future__ import annotations

import math
from typing import TYPE_CHECKING

from pymavlink import mavutil
from pymavlink.dialects.v20 import common as mavlink2

from ados.services.scripting.text_parser import CommandType, ParsedCommand

if TYPE_CHECKING:
    from ados.services.mavlink.state import VehicleState

# Shared MAVLink encoder instance for generating bytes
_mav = mavlink2.MAVLink(None, srcSystem=1, srcComponent=191)
_mav.robust_parsing = True

# ArduCopter mode name -> custom_mode number
_MODE_MAP: dict[str, int] = {
    "STABILIZE": 0, "ACRO": 1, "ALT_HOLD": 2, "AUTO": 3,
    "GUIDED": 4, "LOITER": 5, "RTL": 6, "CIRCLE": 7,
    "LAND": 9, "DRIFT": 11, "SPORT": 13, "FLIP": 14,
    "AUTOTUNE": 15, "POSHOLD": 16, "BRAKE": 17, "THROW": 18,
    "SMART_RTL": 21, "FLOWHOLD": 22, "FOLLOW": 23, "ZIGZAG": 24,
}

# Target system/component (FC defaults)
_TARGET_SYS = 1
_TARGET_COMP = 1


def _cmd_long_bytes(command: int, *params: float) -> bytes:
    """Build a COMMAND_LONG MAVLink message and return its wire bytes."""
    p = list(params) + [0.0] * (7 - len(params))
    msg = _mav.command_long_encode(
        _TARGET_SYS,
        _TARGET_COMP,
        command,
        0,  # confirmation
        p[0], p[1], p[2], p[3], p[4], p[5], p[6],
    )
    return msg.pack(_mav)


def _set_position_ned_bytes(
    north: float,
    east: float,
    down: float,
    speed: float = 1.0,
) -> bytes:
    """Build SET_POSITION_TARGET_LOCAL_NED for relative movement."""
    # type_mask: position + velocity control
    # bits: 0b0000_1111_1111_1000 = use position only
    type_mask = 0x0FF8
    msg = _mav.set_position_target_local_ned_encode(
        0,  # time_boot_ms
        _TARGET_SYS,
        _TARGET_COMP,
        mavutil.mavlink.MAV_FRAME_BODY_OFFSET_NED,
        type_mask,
        north,
        east,
        down,
        speed if north != 0 or east != 0 else 0.0,  # vx
        speed if north != 0 or east != 0 else 0.0,  # vy
        0.0,  # vz
        0, 0, 0,  # afx, afy, afz
        0, 0,  # yaw, yaw_rate
    )
    return msg.pack(_mav)


def translate_command(cmd: ParsedCommand, state: VehicleState) -> bytes | None:
    """Convert a ParsedCommand into MAVLink bytes to send to the FC.

    Returns None for query commands (those are answered from vehicle state).
    """
    ct = cmd.cmd_type

    # --- Queries return None (handled by executor) ---
    if ct in (
        CommandType.BATTERY_Q,
        CommandType.SPEED_Q,
        CommandType.TIME_Q,
        CommandType.HEIGHT_Q,
        CommandType.COMMAND,
        CommandType.STOP,
    ):
        return None

    # --- Takeoff ---
    if ct == CommandType.TAKEOFF:
        alt = cmd.args[0] if cmd.args else 10.0
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_NAV_TAKEOFF,
            0, 0, 0, 0, 0, 0, alt,
        )

    # --- Land ---
    if ct == CommandType.LAND:
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_NAV_LAND,
            0, 0, 0, 0, 0, 0, 0,
        )

    # --- Arm ---
    if ct == CommandType.ARM:
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_COMPONENT_ARM_DISARM,
            1, 0,
        )

    # --- Disarm ---
    if ct == CommandType.DISARM:
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_COMPONENT_ARM_DISARM,
            0, 0,
        )

    # --- Emergency (force disarm) ---
    if ct == CommandType.EMERGENCY:
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_COMPONENT_ARM_DISARM,
            0, 21196,  # magic force-disarm value
        )

    # --- Mode change ---
    if ct == CommandType.MODE:
        parts = cmd.raw_text.strip().split()
        mode_name = parts[1].upper() if len(parts) >= 2 else ""
        mode_num = _MODE_MAP.get(mode_name)
        if mode_num is None:
            return None
        msg = _mav.set_mode_encode(
            _TARGET_SYS,
            mavutil.mavlink.MAV_MODE_FLAG_CUSTOM_MODE_ENABLED,
            mode_num,
        )
        return msg.pack(_mav)

    # --- Speed ---
    if ct == CommandType.SPEED:
        speed_ms = cmd.args[0] / 100.0 if cmd.args else 1.0
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_DO_CHANGE_SPEED,
            0, speed_ms, -1,
        )

    # --- Movement commands (body-frame NED offsets) ---
    dist_m = cmd.args[0] / 100.0 if cmd.args else 0.0

    if ct == CommandType.FORWARD:
        return _set_position_ned_bytes(dist_m, 0, 0)
    if ct == CommandType.BACK:
        return _set_position_ned_bytes(-dist_m, 0, 0)
    if ct == CommandType.LEFT:
        return _set_position_ned_bytes(0, -dist_m, 0)
    if ct == CommandType.RIGHT:
        return _set_position_ned_bytes(0, dist_m, 0)
    if ct == CommandType.UP:
        return _set_position_ned_bytes(0, 0, -dist_m)  # NED: up = negative D
    if ct == CommandType.DOWN:
        return _set_position_ned_bytes(0, 0, dist_m)

    # --- Rotation ---
    if ct in (CommandType.CW, CommandType.CCW):
        deg = cmd.args[0] if cmd.args else 0.0
        if ct == CommandType.CCW:
            deg = -deg
        target_yaw = (state.heading + deg) % 360.0
        return _cmd_long_bytes(
            mavutil.mavlink.MAV_CMD_CONDITION_YAW,
            abs(deg), 25, 1 if deg >= 0 else -1, 0,
        )

    # --- GO x y z speed ---
    if ct == CommandType.GO and len(cmd.args) >= 4:
        x_m = cmd.args[0] / 100.0
        y_m = cmd.args[1] / 100.0
        z_m = cmd.args[2] / 100.0
        spd = cmd.args[3] / 100.0
        return _set_position_ned_bytes(x_m, y_m, -z_m, spd)

    return None
