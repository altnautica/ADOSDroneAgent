"""Tests for the command-to-MAVLink translator."""

from __future__ import annotations

import pytest

from ados.services.mavlink.state import VehicleState
from ados.services.scripting.text_parser import CommandType, ParsedCommand
from ados.services.scripting.translator import translate_command


@pytest.fixture
def state() -> VehicleState:
    s = VehicleState()
    s.heading = 90.0
    s.alt_rel = 50.0
    return s


class TestTranslateCommand:
    """Translation of parsed commands to MAVLink bytes."""

    def test_takeoff_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.TAKEOFF, args=[10.0])
        result = translate_command(cmd, state)
        assert result is not None
        assert isinstance(result, bytes)
        assert len(result) > 0

    def test_land_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.LAND)
        result = translate_command(cmd, state)
        assert result is not None
        assert len(result) > 0

    def test_arm_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.ARM)
        result = translate_command(cmd, state)
        assert result is not None

    def test_disarm_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.DISARM)
        result = translate_command(cmd, state)
        assert result is not None

    def test_emergency_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.EMERGENCY)
        result = translate_command(cmd, state)
        assert result is not None

    def test_forward_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0])
        result = translate_command(cmd, state)
        assert result is not None

    def test_back_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.BACK, args=[50.0])
        result = translate_command(cmd, state)
        assert result is not None

    def test_left_right_returns_bytes(self, state: VehicleState):
        for ct in (CommandType.LEFT, CommandType.RIGHT):
            cmd = ParsedCommand(cmd_type=ct, args=[30.0])
            assert translate_command(cmd, state) is not None

    def test_up_down_returns_bytes(self, state: VehicleState):
        for ct in (CommandType.UP, CommandType.DOWN):
            cmd = ParsedCommand(cmd_type=ct, args=[50.0])
            assert translate_command(cmd, state) is not None

    def test_cw_ccw_returns_bytes(self, state: VehicleState):
        for ct in (CommandType.CW, CommandType.CCW):
            cmd = ParsedCommand(cmd_type=ct, args=[90.0])
            assert translate_command(cmd, state) is not None

    def test_speed_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.SPEED, args=[50.0])
        result = translate_command(cmd, state)
        assert result is not None

    def test_go_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.GO, args=[50.0, 60.0, 70.0, 5.0])
        result = translate_command(cmd, state)
        assert result is not None

    def test_mode_guided_returns_bytes(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.MODE, raw_text="mode guided")
        result = translate_command(cmd, state)
        assert result is not None

    def test_mode_unknown_returns_none(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.MODE, raw_text="mode banana")
        result = translate_command(cmd, state)
        assert result is None

    def test_query_returns_none(self, state: VehicleState):
        for ct in (CommandType.BATTERY_Q, CommandType.SPEED_Q, CommandType.TIME_Q):
            cmd = ParsedCommand(cmd_type=ct)
            assert translate_command(cmd, state) is None

    def test_stop_returns_none(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.STOP)
        assert translate_command(cmd, state) is None

    def test_command_returns_none(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.COMMAND)
        assert translate_command(cmd, state) is None

    def test_takeoff_default_altitude(self, state: VehicleState):
        cmd = ParsedCommand(cmd_type=CommandType.TAKEOFF)
        result = translate_command(cmd, state)
        assert result is not None
