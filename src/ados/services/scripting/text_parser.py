"""Parse Tello-style text commands into structured representations."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import StrEnum


class CommandType(StrEnum):
    """All supported text command types."""

    COMMAND = "command"
    TAKEOFF = "takeoff"
    LAND = "land"
    UP = "up"
    DOWN = "down"
    LEFT = "left"
    RIGHT = "right"
    FORWARD = "forward"
    BACK = "back"
    CW = "cw"
    CCW = "ccw"
    SPEED = "speed"
    STOP = "stop"
    EMERGENCY = "emergency"
    GO = "go"
    BATTERY_Q = "battery?"
    SPEED_Q = "speed?"
    TIME_Q = "time?"
    HEIGHT_Q = "height?"
    MODE = "mode"
    ARM = "arm"
    DISARM = "disarm"
    UNKNOWN = "unknown"


@dataclass
class ParsedCommand:
    """Result of parsing a text command."""

    cmd_type: CommandType
    args: list[float] = field(default_factory=list)
    raw_text: str = ""


# Commands that take exactly one numeric argument (distance in cm or degrees)
_SINGLE_ARG_CMDS: dict[str, CommandType] = {
    "up": CommandType.UP,
    "down": CommandType.DOWN,
    "left": CommandType.LEFT,
    "right": CommandType.RIGHT,
    "forward": CommandType.FORWARD,
    "back": CommandType.BACK,
    "cw": CommandType.CW,
    "ccw": CommandType.CCW,
    "speed": CommandType.SPEED,
}

# Zero-argument commands
_ZERO_ARG_CMDS: dict[str, CommandType] = {
    "command": CommandType.COMMAND,
    "takeoff": CommandType.TAKEOFF,
    "land": CommandType.LAND,
    "stop": CommandType.STOP,
    "emergency": CommandType.EMERGENCY,
    "arm": CommandType.ARM,
    "disarm": CommandType.DISARM,
}

# Query commands (end with ?)
_QUERY_CMDS: dict[str, CommandType] = {
    "battery?": CommandType.BATTERY_Q,
    "speed?": CommandType.SPEED_Q,
    "time?": CommandType.TIME_Q,
    "height?": CommandType.HEIGHT_Q,
}


def parse_text_command(text: str) -> ParsedCommand:
    """Parse a raw text command string into a ParsedCommand.

    Examples:
        "takeoff"       -> ParsedCommand(TAKEOFF, [])
        "forward 100"   -> ParsedCommand(FORWARD, [100.0])
        "go 50 60 70 5" -> ParsedCommand(GO, [50.0, 60.0, 70.0, 5.0])
        "battery?"      -> ParsedCommand(BATTERY_Q, [])
        "mode guided"   -> ParsedCommand(MODE, [])  (mode name stored via raw_text)
    """
    cleaned = text.strip().lower()
    if not cleaned:
        return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)

    parts = cleaned.split()
    word = parts[0]

    # Query commands (exact match including ?)
    if cleaned in _QUERY_CMDS:
        return ParsedCommand(cmd_type=_QUERY_CMDS[cleaned], raw_text=text)

    # Zero-arg commands
    if word in _ZERO_ARG_CMDS:
        return ParsedCommand(cmd_type=_ZERO_ARG_CMDS[word], raw_text=text)

    # Single-arg movement/rotation commands
    if word in _SINGLE_ARG_CMDS:
        if len(parts) < 2:
            return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)
        try:
            val = float(parts[1])
        except ValueError:
            return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)
        return ParsedCommand(cmd_type=_SINGLE_ARG_CMDS[word], args=[val], raw_text=text)

    # "go X Y Z speed" — needs exactly 4 args
    if word == "go":
        if len(parts) != 5:
            return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)
        try:
            args = [float(p) for p in parts[1:5]]
        except ValueError:
            return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)
        return ParsedCommand(cmd_type=CommandType.GO, args=args, raw_text=text)

    # "mode <name>" — mode name kept in raw_text for downstream extraction
    if word == "mode":
        if len(parts) < 2:
            return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)
        return ParsedCommand(cmd_type=CommandType.MODE, raw_text=text)

    return ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text=text)
