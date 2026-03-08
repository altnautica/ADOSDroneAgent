"""Demo scripting engine — accepts all commands, simulates responses."""

from __future__ import annotations

import time
from datetime import datetime, timezone

from ados.core.logging import get_logger
from ados.services.scripting.text_parser import CommandType, ParsedCommand

log = get_logger("scripting.demo")


class DemoScriptingEngine:
    """Simulated scripting engine for demo/test mode.

    Accepts all commands and returns realistic responses.
    Maintains simulated altitude and armed state.
    """

    def __init__(self) -> None:
        self._altitude: float = 0.0
        self._armed: bool = False
        self._speed_cms: float = 100.0  # cm/s
        self._mode: str = "STABILIZE"
        self._start_time: float = time.monotonic()
        self._battery: int = 95
        self._sdk_mode: bool = False
        self._command_log: list[dict] = []

    @property
    def altitude(self) -> float:
        return self._altitude

    @property
    def armed(self) -> bool:
        return self._armed

    @property
    def mode(self) -> str:
        return self._mode

    @property
    def command_log(self) -> list[dict]:
        return list(self._command_log)

    def _log_cmd(self, cmd: ParsedCommand, result: str) -> None:
        self._command_log.append({
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "command": cmd.raw_text or cmd.cmd_type.value,
            "result": result,
        })
        # Keep last 100 entries
        if len(self._command_log) > 100:
            self._command_log = self._command_log[-100:]

    async def execute(self, cmd: ParsedCommand, source: str = "text") -> str:
        """Process a command and return a Tello-style response."""
        ct = cmd.cmd_type

        if ct == CommandType.UNKNOWN:
            result = "error: unknown command"
            self._log_cmd(cmd, result)
            return result

        if ct == CommandType.COMMAND:
            self._sdk_mode = True
            log.info("demo_sdk_mode_enabled", source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Queries
        if ct == CommandType.BATTERY_Q:
            val = str(self._battery)
            self._log_cmd(cmd, val)
            return val
        if ct == CommandType.SPEED_Q:
            val = f"{self._speed_cms / 100.0:.1f}"
            self._log_cmd(cmd, val)
            return val
        if ct == CommandType.HEIGHT_Q:
            val = f"{self._altitude:.1f}"
            self._log_cmd(cmd, val)
            return val
        if ct == CommandType.TIME_Q:
            elapsed = int(time.monotonic() - self._start_time)
            val = str(elapsed)
            self._log_cmd(cmd, val)
            return val

        # Arm/disarm
        if ct == CommandType.ARM:
            self._armed = True
            log.info("demo_armed", source=source)
            self._log_cmd(cmd, "ok")
            return "ok"
        if ct == CommandType.DISARM:
            self._armed = False
            self._altitude = 0.0
            log.info("demo_disarmed", source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Emergency
        if ct == CommandType.EMERGENCY:
            self._armed = False
            self._altitude = 0.0
            log.info("demo_emergency_stop", source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Takeoff
        if ct == CommandType.TAKEOFF:
            self._armed = True
            alt = cmd.args[0] if cmd.args else 100.0  # default 100cm = 1m
            self._altitude = alt / 100.0
            log.info("demo_takeoff", altitude=self._altitude, source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Land
        if ct == CommandType.LAND:
            self._altitude = 0.0
            self._armed = False
            log.info("demo_land", source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Stop
        if ct == CommandType.STOP:
            log.info("demo_stop_hover", source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Vertical movement
        if ct == CommandType.UP and cmd.args:
            self._altitude += cmd.args[0] / 100.0
            log.info("demo_move_up", delta_m=cmd.args[0] / 100.0, alt=self._altitude)
            self._log_cmd(cmd, "ok")
            return "ok"
        if ct == CommandType.DOWN and cmd.args:
            self._altitude = max(0.0, self._altitude - cmd.args[0] / 100.0)
            log.info("demo_move_down", delta_m=cmd.args[0] / 100.0, alt=self._altitude)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Horizontal movement (just log)
        if ct in (
            CommandType.FORWARD, CommandType.BACK,
            CommandType.LEFT, CommandType.RIGHT,
        ):
            dist = cmd.args[0] if cmd.args else 0.0
            log.info("demo_move", direction=ct.value, distance_cm=dist, source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Rotation
        if ct in (CommandType.CW, CommandType.CCW):
            deg = cmd.args[0] if cmd.args else 0.0
            log.info("demo_rotate", direction=ct.value, degrees=deg, source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # Speed
        if ct == CommandType.SPEED:
            self._speed_cms = cmd.args[0] if cmd.args else 100.0
            log.info("demo_speed_set", speed_cms=self._speed_cms, source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        # GO
        if ct == CommandType.GO:
            log.info("demo_go", args=cmd.args, source=source)
            if len(cmd.args) >= 3:
                self._altitude += cmd.args[2] / 100.0
            self._log_cmd(cmd, "ok")
            return "ok"

        # Mode
        if ct == CommandType.MODE:
            parts = cmd.raw_text.strip().split()
            self._mode = parts[1].upper() if len(parts) >= 2 else "STABILIZE"
            log.info("demo_mode_set", mode=self._mode, source=source)
            self._log_cmd(cmd, "ok")
            return "ok"

        self._log_cmd(cmd, "ok")
        return "ok"

    def status(self) -> dict:
        """Return scripting engine status for API."""
        return {
            "demo_mode": True,
            "sdk_mode": self._sdk_mode,
            "altitude": self._altitude,
            "armed": self._armed,
            "mode": self._mode,
            "battery": self._battery,
            "speed_cms": self._speed_cms,
            "commands_executed": len(self._command_log),
        }
