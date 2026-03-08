"""Tests for the command executor."""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from ados.services.mavlink.state import VehicleState
from ados.services.scripting.executor import CommandExecutor
from ados.services.scripting.safety import SafetyLimits, SafetyValidator
from ados.services.scripting.text_parser import CommandType, ParsedCommand


@pytest.fixture
def state() -> VehicleState:
    s = VehicleState()
    s.battery_remaining = 80
    s.alt_rel = 50.0
    s.groundspeed = 5.0
    s.gps_fix_type = 3
    s.gps_satellites = 12
    return s


@pytest.fixture
def mock_fc():
    fc = MagicMock()
    fc.connected = True
    fc.send_bytes = MagicMock()
    return fc


@pytest.fixture
def executor(mock_fc, state: VehicleState) -> CommandExecutor:
    limits = SafetyLimits()
    safety = SafetyValidator(limits, state)
    return CommandExecutor(mock_fc, state, safety)


class TestCommandExecutor:
    """Command execution with safety, translation, and logging."""

    @pytest.mark.asyncio
    async def test_command_enables_sdk_mode(self, executor: CommandExecutor):
        cmd = ParsedCommand(cmd_type=CommandType.COMMAND, raw_text="command")
        result = await executor.execute(cmd)
        assert result == "ok"

    @pytest.mark.asyncio
    async def test_battery_query(self, executor: CommandExecutor, state: VehicleState):
        state.battery_remaining = 75
        cmd = ParsedCommand(cmd_type=CommandType.BATTERY_Q, raw_text="battery?")
        result = await executor.execute(cmd)
        assert result == "75"

    @pytest.mark.asyncio
    async def test_speed_query(self, executor: CommandExecutor, state: VehicleState):
        state.groundspeed = 3.5
        cmd = ParsedCommand(cmd_type=CommandType.SPEED_Q, raw_text="speed?")
        result = await executor.execute(cmd)
        assert result == "3.5"

    @pytest.mark.asyncio
    async def test_height_query(self, executor: CommandExecutor, state: VehicleState):
        state.alt_rel = 42.7
        cmd = ParsedCommand(cmd_type=CommandType.HEIGHT_Q, raw_text="height?")
        result = await executor.execute(cmd)
        assert result == "42.7"

    @pytest.mark.asyncio
    async def test_stop_returns_ok(self, executor: CommandExecutor):
        cmd = ParsedCommand(cmd_type=CommandType.STOP, raw_text="stop")
        result = await executor.execute(cmd)
        assert result == "ok"

    @pytest.mark.asyncio
    async def test_takeoff_sends_bytes(self, executor: CommandExecutor, mock_fc):
        cmd = ParsedCommand(cmd_type=CommandType.TAKEOFF, raw_text="takeoff")
        result = await executor.execute(cmd)
        assert result == "ok"
        mock_fc.send_bytes.assert_called_once()

    @pytest.mark.asyncio
    async def test_forward_sends_bytes(self, executor: CommandExecutor, mock_fc):
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0], raw_text="forward 100")
        result = await executor.execute(cmd)
        assert result == "ok"
        mock_fc.send_bytes.assert_called_once()

    @pytest.mark.asyncio
    async def test_safety_rejects_low_battery(
        self, executor: CommandExecutor, state: VehicleState
    ):
        state.battery_remaining = 5
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0], raw_text="forward 100")
        result = await executor.execute(cmd)
        assert result.startswith("error:")
        assert "Battery" in result

    @pytest.mark.asyncio
    async def test_arm_rejected_no_gps(self, executor: CommandExecutor, state: VehicleState):
        state.gps_fix_type = 1
        cmd = ParsedCommand(cmd_type=CommandType.ARM, raw_text="arm")
        result = await executor.execute(cmd)
        assert result.startswith("error:")
        assert "GPS" in result

    @pytest.mark.asyncio
    async def test_arm_success(self, executor: CommandExecutor, mock_fc):
        cmd = ParsedCommand(cmd_type=CommandType.ARM, raw_text="arm")
        result = await executor.execute(cmd)
        assert result == "ok"
        mock_fc.send_bytes.assert_called_once()

    @pytest.mark.asyncio
    async def test_fc_disconnected(self, executor: CommandExecutor, mock_fc):
        mock_fc.connected = False
        cmd = ParsedCommand(cmd_type=CommandType.TAKEOFF, raw_text="takeoff")
        result = await executor.execute(cmd)
        assert "FC not connected" in result

    @pytest.mark.asyncio
    async def test_unknown_mode(self, executor: CommandExecutor):
        cmd = ParsedCommand(cmd_type=CommandType.MODE, raw_text="mode banana")
        result = await executor.execute(cmd)
        assert "error" in result

    @pytest.mark.asyncio
    async def test_command_log_recorded(self, executor: CommandExecutor):
        cmd = ParsedCommand(cmd_type=CommandType.COMMAND, raw_text="command")
        await executor.execute(cmd)
        assert len(executor.command_log) == 1
        assert executor.command_log[0].command == "command"

    @pytest.mark.asyncio
    async def test_time_query(self, executor: CommandExecutor):
        cmd = ParsedCommand(cmd_type=CommandType.TIME_Q, raw_text="time?")
        result = await executor.execute(cmd)
        assert result == "0"
