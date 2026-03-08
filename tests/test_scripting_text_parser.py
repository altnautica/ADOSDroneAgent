"""Tests for the text command parser."""

from __future__ import annotations

import pytest

from ados.services.scripting.text_parser import CommandType, ParsedCommand, parse_text_command


class TestParseTextCommand:
    """Parsing Tello-style text commands."""

    def test_empty_string(self):
        result = parse_text_command("")
        assert result.cmd_type == CommandType.UNKNOWN

    def test_whitespace_only(self):
        result = parse_text_command("   ")
        assert result.cmd_type == CommandType.UNKNOWN

    def test_takeoff(self):
        result = parse_text_command("takeoff")
        assert result.cmd_type == CommandType.TAKEOFF
        assert result.args == []

    def test_land(self):
        result = parse_text_command("land")
        assert result.cmd_type == CommandType.LAND

    def test_command(self):
        result = parse_text_command("command")
        assert result.cmd_type == CommandType.COMMAND

    def test_emergency(self):
        result = parse_text_command("emergency")
        assert result.cmd_type == CommandType.EMERGENCY

    def test_stop(self):
        result = parse_text_command("stop")
        assert result.cmd_type == CommandType.STOP

    def test_arm(self):
        result = parse_text_command("arm")
        assert result.cmd_type == CommandType.ARM

    def test_disarm(self):
        result = parse_text_command("disarm")
        assert result.cmd_type == CommandType.DISARM

    def test_forward_with_distance(self):
        result = parse_text_command("forward 100")
        assert result.cmd_type == CommandType.FORWARD
        assert result.args == [100.0]

    def test_back_with_distance(self):
        result = parse_text_command("back 50")
        assert result.cmd_type == CommandType.BACK
        assert result.args == [50.0]

    def test_left_right(self):
        assert parse_text_command("left 30").cmd_type == CommandType.LEFT
        assert parse_text_command("right 30").cmd_type == CommandType.RIGHT

    def test_up_down(self):
        r = parse_text_command("up 200")
        assert r.cmd_type == CommandType.UP
        assert r.args == [200.0]
        r2 = parse_text_command("down 50")
        assert r2.cmd_type == CommandType.DOWN
        assert r2.args == [50.0]

    def test_cw_ccw(self):
        r = parse_text_command("cw 90")
        assert r.cmd_type == CommandType.CW
        assert r.args == [90.0]
        r2 = parse_text_command("ccw 45")
        assert r2.cmd_type == CommandType.CCW
        assert r2.args == [45.0]

    def test_speed(self):
        r = parse_text_command("speed 50")
        assert r.cmd_type == CommandType.SPEED
        assert r.args == [50.0]

    def test_go_command(self):
        r = parse_text_command("go 50 60 70 5")
        assert r.cmd_type == CommandType.GO
        assert r.args == [50.0, 60.0, 70.0, 5.0]

    def test_go_wrong_arg_count(self):
        r = parse_text_command("go 50 60")
        assert r.cmd_type == CommandType.UNKNOWN

    def test_battery_query(self):
        r = parse_text_command("battery?")
        assert r.cmd_type == CommandType.BATTERY_Q

    def test_speed_query(self):
        r = parse_text_command("speed?")
        assert r.cmd_type == CommandType.SPEED_Q

    def test_time_query(self):
        r = parse_text_command("time?")
        assert r.cmd_type == CommandType.TIME_Q

    def test_height_query(self):
        r = parse_text_command("height?")
        assert r.cmd_type == CommandType.HEIGHT_Q

    def test_mode_with_name(self):
        r = parse_text_command("mode guided")
        assert r.cmd_type == CommandType.MODE
        assert "guided" in r.raw_text

    def test_mode_without_name_is_unknown(self):
        r = parse_text_command("mode")
        assert r.cmd_type == CommandType.UNKNOWN

    def test_case_insensitive(self):
        r = parse_text_command("TAKEOFF")
        assert r.cmd_type == CommandType.TAKEOFF

    def test_leading_trailing_whitespace(self):
        r = parse_text_command("  forward 100  ")
        assert r.cmd_type == CommandType.FORWARD
        assert r.args == [100.0]

    def test_forward_without_distance_is_unknown(self):
        r = parse_text_command("forward")
        assert r.cmd_type == CommandType.UNKNOWN

    def test_forward_with_non_numeric_is_unknown(self):
        r = parse_text_command("forward abc")
        assert r.cmd_type == CommandType.UNKNOWN

    def test_unknown_command(self):
        r = parse_text_command("dance")
        assert r.cmd_type == CommandType.UNKNOWN

    def test_raw_text_preserved(self):
        r = parse_text_command("forward 100")
        assert r.raw_text == "forward 100"

    def test_go_non_numeric_args(self):
        r = parse_text_command("go a b c d")
        assert r.cmd_type == CommandType.UNKNOWN
