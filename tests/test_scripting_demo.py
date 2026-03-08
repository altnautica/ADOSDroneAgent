"""Tests for the demo scripting engine."""

from __future__ import annotations

import pytest

from ados.services.scripting.demo import DemoScriptingEngine
from ados.services.scripting.text_parser import CommandType, ParsedCommand


@pytest.fixture
def demo() -> DemoScriptingEngine:
    return DemoScriptingEngine()


class TestDemoScriptingEngine:
    """Demo mode command simulation."""

    @pytest.mark.asyncio
    async def test_command_enables_sdk_mode(self, demo: DemoScriptingEngine):
        cmd = ParsedCommand(cmd_type=CommandType.COMMAND, raw_text="command")
        result = await demo.execute(cmd)
        assert result == "ok"

    @pytest.mark.asyncio
    async def test_takeoff_sets_altitude(self, demo: DemoScriptingEngine):
        cmd = ParsedCommand(cmd_type=CommandType.TAKEOFF, args=[200.0], raw_text="takeoff")
        result = await demo.execute(cmd)
        assert result == "ok"
        assert demo.altitude == 2.0  # 200cm = 2m
        assert demo.armed is True

    @pytest.mark.asyncio
    async def test_land_resets(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.TAKEOFF, args=[100.0]))
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.LAND))
        assert result == "ok"
        assert demo.altitude == 0.0
        assert demo.armed is False

    @pytest.mark.asyncio
    async def test_arm_disarm(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.ARM))
        assert demo.armed is True
        await demo.execute(ParsedCommand(cmd_type=CommandType.DISARM))
        assert demo.armed is False

    @pytest.mark.asyncio
    async def test_emergency_disarms(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.ARM))
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.EMERGENCY))
        assert result == "ok"
        assert demo.armed is False

    @pytest.mark.asyncio
    async def test_up_down(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.TAKEOFF, args=[100.0]))
        await demo.execute(ParsedCommand(cmd_type=CommandType.UP, args=[50.0]))
        assert demo.altitude == pytest.approx(1.5)
        await demo.execute(ParsedCommand(cmd_type=CommandType.DOWN, args=[25.0]))
        assert demo.altitude == pytest.approx(1.25)

    @pytest.mark.asyncio
    async def test_movement_commands(self, demo: DemoScriptingEngine):
        for ct in (CommandType.FORWARD, CommandType.BACK, CommandType.LEFT, CommandType.RIGHT):
            cmd = ParsedCommand(cmd_type=ct, args=[100.0], raw_text=f"{ct.value} 100")
            result = await demo.execute(cmd)
            assert result == "ok"

    @pytest.mark.asyncio
    async def test_rotation(self, demo: DemoScriptingEngine):
        for ct in (CommandType.CW, CommandType.CCW):
            cmd = ParsedCommand(cmd_type=ct, args=[90.0])
            result = await demo.execute(cmd)
            assert result == "ok"

    @pytest.mark.asyncio
    async def test_speed_set(self, demo: DemoScriptingEngine):
        cmd = ParsedCommand(cmd_type=CommandType.SPEED, args=[200.0])
        result = await demo.execute(cmd)
        assert result == "ok"

    @pytest.mark.asyncio
    async def test_battery_query(self, demo: DemoScriptingEngine):
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.BATTERY_Q))
        assert result == "95"

    @pytest.mark.asyncio
    async def test_speed_query(self, demo: DemoScriptingEngine):
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.SPEED_Q))
        assert result == "1.0"  # default 100 cm/s = 1.0 m/s

    @pytest.mark.asyncio
    async def test_height_query(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.TAKEOFF, args=[300.0]))
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.HEIGHT_Q))
        assert result == "3.0"

    @pytest.mark.asyncio
    async def test_time_query(self, demo: DemoScriptingEngine):
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.TIME_Q))
        assert int(result) >= 0

    @pytest.mark.asyncio
    async def test_mode_change(self, demo: DemoScriptingEngine):
        cmd = ParsedCommand(cmd_type=CommandType.MODE, raw_text="mode guided")
        result = await demo.execute(cmd)
        assert result == "ok"
        assert demo.mode == "GUIDED"

    @pytest.mark.asyncio
    async def test_go_command(self, demo: DemoScriptingEngine):
        cmd = ParsedCommand(cmd_type=CommandType.GO, args=[50, 60, 70, 5])
        result = await demo.execute(cmd)
        assert result == "ok"
        assert demo.altitude == pytest.approx(0.7)

    @pytest.mark.asyncio
    async def test_unknown_command_error(self, demo: DemoScriptingEngine):
        cmd = ParsedCommand(cmd_type=CommandType.UNKNOWN, raw_text="dance")
        result = await demo.execute(cmd)
        assert result.startswith("error")

    @pytest.mark.asyncio
    async def test_command_log_tracked(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.TAKEOFF, raw_text="takeoff"))
        await demo.execute(ParsedCommand(cmd_type=CommandType.LAND, raw_text="land"))
        assert len(demo.command_log) == 2

    @pytest.mark.asyncio
    async def test_status(self, demo: DemoScriptingEngine):
        status = demo.status()
        assert status["demo_mode"] is True
        assert "altitude" in status
        assert "armed" in status
        assert "battery" in status

    @pytest.mark.asyncio
    async def test_stop_command(self, demo: DemoScriptingEngine):
        result = await demo.execute(ParsedCommand(cmd_type=CommandType.STOP))
        assert result == "ok"

    @pytest.mark.asyncio
    async def test_down_does_not_go_negative(self, demo: DemoScriptingEngine):
        await demo.execute(ParsedCommand(cmd_type=CommandType.DOWN, args=[500.0]))
        assert demo.altitude >= 0.0
