"""Tests for the safety validator."""

from __future__ import annotations

import pytest

from ados.services.mavlink.state import VehicleState
from ados.services.scripting.safety import SafetyLimits, SafetyValidator
from ados.services.scripting.text_parser import CommandType, ParsedCommand


@pytest.fixture
def state() -> VehicleState:
    s = VehicleState()
    s.battery_remaining = 50
    s.alt_rel = 50.0
    s.lat = 12.9716
    s.lon = 77.5946
    s.gps_fix_type = 3
    s.gps_satellites = 12
    return s


@pytest.fixture
def limits() -> SafetyLimits:
    return SafetyLimits()


@pytest.fixture
def validator(limits: SafetyLimits, state: VehicleState) -> SafetyValidator:
    return SafetyValidator(limits, state)


class TestSafetyValidator:
    """Safety validation for scripting commands."""

    def test_emergency_always_allowed(self, validator: SafetyValidator):
        cmd = ParsedCommand(cmd_type=CommandType.EMERGENCY)
        ok, reason = validator.validate_command(cmd)
        assert ok is True
        assert reason == ""

    def test_land_always_allowed(self, validator: SafetyValidator):
        cmd = ParsedCommand(cmd_type=CommandType.LAND)
        ok, _ = validator.validate_command(cmd)
        assert ok is True

    def test_query_always_allowed(self, validator: SafetyValidator):
        for ct in (CommandType.BATTERY_Q, CommandType.SPEED_Q, CommandType.HEIGHT_Q):
            cmd = ParsedCommand(cmd_type=ct)
            ok, _ = validator.validate_command(cmd)
            assert ok is True

    def test_battery_too_low(self, validator: SafetyValidator, state: VehicleState):
        state.battery_remaining = 10
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0])
        ok, reason = validator.validate_command(cmd)
        assert ok is False
        assert "Battery too low" in reason

    def test_battery_unknown_skips_check(self, validator: SafetyValidator, state: VehicleState):
        state.battery_remaining = -1  # unknown
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0])
        ok, _ = validator.validate_command(cmd)
        assert ok is True

    def test_altitude_too_high(self, validator: SafetyValidator, state: VehicleState):
        state.alt_rel = 115.0
        cmd = ParsedCommand(cmd_type=CommandType.UP, args=[600.0])  # +6m
        ok, reason = validator.validate_command(cmd)
        assert ok is False
        assert "max altitude" in reason

    def test_altitude_too_low(self, validator: SafetyValidator, state: VehicleState):
        state.alt_rel = 1.0
        cmd = ParsedCommand(cmd_type=CommandType.DOWN, args=[100.0])  # -1m
        ok, reason = validator.validate_command(cmd)
        assert ok is False
        assert "min altitude" in reason

    def test_go_altitude_too_high(self, validator: SafetyValidator, state: VehicleState):
        state.alt_rel = 119.0
        cmd = ParsedCommand(cmd_type=CommandType.GO, args=[0, 0, 200, 5])  # +2m
        ok, reason = validator.validate_command(cmd)
        assert ok is False
        assert "max altitude" in reason

    def test_go_altitude_too_low(self, validator: SafetyValidator, state: VehicleState):
        state.alt_rel = 1.0
        cmd = ParsedCommand(cmd_type=CommandType.GO, args=[0, 0, -200, 5])  # -2m
        ok, reason = validator.validate_command(cmd)
        assert ok is False
        assert "min altitude" in reason

    def test_geofence_violation(self, validator: SafetyValidator, state: VehicleState):
        validator.limits.geofence_center_lat = 12.9716
        validator.limits.geofence_center_lon = 77.5946
        validator.limits.geofence_radius_m = 100.0
        # Move drone far away
        state.lat = 13.0
        state.lon = 77.6
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0])
        ok, reason = validator.validate_command(cmd)
        assert ok is False
        assert "geofence" in reason.lower()

    def test_geofence_inactive_when_center_zero(self, validator: SafetyValidator):
        # Default center is 0,0 — geofence should be inactive
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0])
        ok, _ = validator.validate_command(cmd)
        assert ok is True

    def test_valid_forward_command(self, validator: SafetyValidator):
        cmd = ParsedCommand(cmd_type=CommandType.FORWARD, args=[100.0])
        ok, _ = validator.validate_command(cmd)
        assert ok is True

    def test_validate_arm_no_gps(self, validator: SafetyValidator, state: VehicleState):
        state.gps_fix_type = 1  # no 3D fix
        ok, reason = validator.validate_arm()
        assert ok is False
        assert "GPS" in reason

    def test_validate_arm_low_battery(self, validator: SafetyValidator, state: VehicleState):
        state.battery_remaining = 5
        ok, reason = validator.validate_arm()
        assert ok is False
        assert "Battery" in reason

    def test_validate_arm_ok(self, validator: SafetyValidator):
        ok, reason = validator.validate_arm()
        assert ok is True
        assert reason == ""

    def test_stop_always_allowed(self, validator: SafetyValidator, state: VehicleState):
        state.battery_remaining = 5  # low battery
        cmd = ParsedCommand(cmd_type=CommandType.STOP)
        ok, _ = validator.validate_command(cmd)
        assert ok is True

    def test_disarm_always_allowed(self, validator: SafetyValidator, state: VehicleState):
        state.battery_remaining = 5
        cmd = ParsedCommand(cmd_type=CommandType.DISARM)
        ok, _ = validator.validate_command(cmd)
        assert ok is True
