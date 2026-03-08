"""Command executor — queue, priority, rate limiting, safety validation."""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import StrEnum
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.services.scripting.safety import SafetyValidator
from ados.services.scripting.text_parser import CommandType, ParsedCommand
from ados.services.scripting.translator import translate_command

if TYPE_CHECKING:
    from ados.services.mavlink.state import VehicleState

log = get_logger("scripting.executor")


class CommandPriority(StrEnum):
    """Priority levels for queued commands."""

    EMERGENCY = "emergency"
    HIGH = "high"
    NORMAL = "normal"
    LOW = "low"


# Numeric sort key (lower = higher priority)
_PRIORITY_ORDER: dict[CommandPriority, int] = {
    CommandPriority.EMERGENCY: 0,
    CommandPriority.HIGH: 1,
    CommandPriority.NORMAL: 2,
    CommandPriority.LOW: 3,
}


@dataclass
class QueuedCommand:
    """A command waiting for execution."""

    parsed_command: ParsedCommand
    priority: CommandPriority
    timestamp: float = field(default_factory=time.monotonic)
    source: str = "text"

    @property
    def sort_key(self) -> tuple[int, float]:
        return _PRIORITY_ORDER.get(self.priority, 2), self.timestamp


@dataclass
class CommandLogEntry:
    """Record of an executed command."""

    timestamp: str
    command: str
    source: str
    result: str


class CommandExecutor:
    """Validates, translates, and sends commands to the flight controller.

    Supports priority ordering and rate limiting (max 10 commands/sec).
    """

    MAX_RATE: float = 10.0  # commands per second
    LOG_SIZE: int = 100

    def __init__(
        self,
        fc_connection: object,
        vehicle_state: VehicleState,
        safety: SafetyValidator,
    ) -> None:
        self._fc = fc_connection
        self._state = vehicle_state
        self._safety = safety
        self._last_cmd_time: float = 0.0
        self._lock = asyncio.Lock()
        self._sdk_mode: bool = False
        self._command_log: list[CommandLogEntry] = []

    @property
    def command_log(self) -> list[CommandLogEntry]:
        return list(self._command_log)

    def _record(self, raw: str, source: str, result: str) -> None:
        now = datetime.now(timezone.utc).isoformat()
        entry = CommandLogEntry(timestamp=now, command=raw, source=source, result=result)
        self._command_log.append(entry)
        if len(self._command_log) > self.LOG_SIZE:
            self._command_log = self._command_log[-self.LOG_SIZE:]

    def _resolve_priority(self, cmd: ParsedCommand) -> CommandPriority:
        """Determine priority from command type."""
        if cmd.cmd_type == CommandType.EMERGENCY:
            return CommandPriority.EMERGENCY
        if cmd.cmd_type in (CommandType.LAND, CommandType.STOP):
            return CommandPriority.HIGH
        return CommandPriority.NORMAL

    async def execute(self, cmd: ParsedCommand, source: str = "text") -> str:
        """Validate, translate, and send a command. Returns response string.

        Responses follow Tello protocol: "ok", "error: <reason>", or a value.
        """
        async with self._lock:
            return await self._execute_locked(cmd, source)

    async def _execute_locked(self, cmd: ParsedCommand, source: str) -> str:
        priority = self._resolve_priority(cmd)

        # Rate limiting (skip for emergency)
        if priority != CommandPriority.EMERGENCY:
            now = time.monotonic()
            elapsed = now - self._last_cmd_time
            min_interval = 1.0 / self.MAX_RATE
            if elapsed < min_interval:
                await asyncio.sleep(min_interval - elapsed)

        self._last_cmd_time = time.monotonic()

        # "command" enables SDK mode
        if cmd.cmd_type == CommandType.COMMAND:
            self._sdk_mode = True
            log.info("sdk_mode_enabled", source=source)
            self._record(cmd.raw_text, source, "ok")
            return "ok"

        # Query commands — return state values
        query_result = self._handle_query(cmd)
        if query_result is not None:
            self._record(cmd.raw_text, source, query_result)
            return query_result

        # Stop — no MAVLink needed, just acknowledge
        if cmd.cmd_type == CommandType.STOP:
            log.info("stop_command", source=source)
            self._record(cmd.raw_text, source, "ok")
            return "ok"

        # Arm pre-check
        if cmd.cmd_type == CommandType.ARM:
            ok, reason = self._safety.validate_arm()
            if not ok:
                result = f"error: {reason}"
                self._record(cmd.raw_text, source, result)
                log.warning("arm_rejected", reason=reason, source=source)
                return result

        # Safety validation
        ok, reason = self._safety.validate_command(cmd)
        if not ok:
            result = f"error: {reason}"
            self._record(cmd.raw_text, source, result)
            log.warning("command_rejected", cmd=cmd.cmd_type.value, reason=reason, source=source)
            return result

        # Translate to MAVLink bytes
        mavlink_bytes = translate_command(cmd, self._state)
        if mavlink_bytes is None:
            # Unknown mode or untranslatable command
            if cmd.cmd_type == CommandType.MODE:
                result = "error: unknown mode"
                self._record(cmd.raw_text, source, result)
                return result
            result = "error: cannot translate command"
            self._record(cmd.raw_text, source, result)
            return result

        # Send to FC
        if not getattr(self._fc, "connected", False):
            result = "error: FC not connected"
            self._record(cmd.raw_text, source, result)
            return result

        self._fc.send_bytes(mavlink_bytes)
        log.info(
            "command_sent",
            cmd=cmd.cmd_type.value,
            source=source,
            priority=priority.value,
        )
        self._record(cmd.raw_text, source, "ok")
        return "ok"

    def _handle_query(self, cmd: ParsedCommand) -> str | None:
        """Handle query commands by reading vehicle state. Returns None if not a query."""
        if cmd.cmd_type == CommandType.BATTERY_Q:
            return str(self._state.battery_remaining)
        if cmd.cmd_type == CommandType.SPEED_Q:
            return f"{self._state.groundspeed:.1f}"
        if cmd.cmd_type == CommandType.HEIGHT_Q:
            return f"{self._state.alt_rel:.1f}"
        if cmd.cmd_type == CommandType.TIME_Q:
            return "0"
        return None
