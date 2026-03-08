"""Safety validation for scripting commands — geofence, altitude, battery checks."""

from __future__ import annotations

import math
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from ados.services.scripting.text_parser import CommandType, ParsedCommand

if TYPE_CHECKING:
    from ados.services.mavlink.state import VehicleState


@dataclass
class SafetyLimits:
    """Configurable safety boundaries."""

    max_altitude_m: float = 120.0
    min_altitude_m: float = 0.5
    max_distance_m: float = 500.0
    min_battery_pct: int = 20
    geofence_radius_m: float = 500.0
    geofence_center_lat: float = 0.0
    geofence_center_lon: float = 0.0


# Commands that bypass all safety checks
_ALWAYS_ALLOWED: frozenset[CommandType] = frozenset({
    CommandType.EMERGENCY,
    CommandType.LAND,
    CommandType.COMMAND,
    CommandType.BATTERY_Q,
    CommandType.SPEED_Q,
    CommandType.TIME_Q,
    CommandType.HEIGHT_Q,
    CommandType.STOP,
    CommandType.DISARM,
})


def _haversine_m(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    """Distance in meters between two lat/lon points."""
    r = 6_371_000.0
    phi1 = math.radians(lat1)
    phi2 = math.radians(lat2)
    dphi = math.radians(lat2 - lat1)
    dlam = math.radians(lon2 - lon1)
    a = math.sin(dphi / 2) ** 2 + math.cos(phi1) * math.cos(phi2) * math.sin(dlam / 2) ** 2
    return 2 * r * math.atan2(math.sqrt(a), math.sqrt(1 - a))


class SafetyValidator:
    """Validates commands against safety limits and current vehicle state."""

    def __init__(self, limits: SafetyLimits, state: VehicleState) -> None:
        self.limits = limits
        self.state = state

    def validate_command(self, cmd: ParsedCommand) -> tuple[bool, str]:
        """Check whether a command is safe to execute.

        Returns (True, "") if allowed, or (False, reason) if blocked.
        """
        if cmd.cmd_type in _ALWAYS_ALLOWED:
            return True, ""

        # Battery check
        if 0 <= self.state.battery_remaining < self.limits.min_battery_pct:
            return False, (
                f"Battery too low: {self.state.battery_remaining}%"
                f" (minimum {self.limits.min_battery_pct}%)"
            )

        # Altitude checks for vertical movement
        if cmd.cmd_type == CommandType.UP and cmd.args:
            new_alt = self.state.alt_rel + cmd.args[0] / 100.0
            if new_alt > self.limits.max_altitude_m:
                return False, (
                    f"Would exceed max altitude: {new_alt:.1f}m"
                    f" (limit {self.limits.max_altitude_m}m)"
                )

        if cmd.cmd_type == CommandType.DOWN and cmd.args:
            new_alt = self.state.alt_rel - cmd.args[0] / 100.0
            if new_alt < self.limits.min_altitude_m:
                return False, (
                    f"Would go below min altitude: {new_alt:.1f}m"
                    f" (limit {self.limits.min_altitude_m}m)"
                )

        # GO command altitude check
        if cmd.cmd_type == CommandType.GO and len(cmd.args) >= 3:
            target_alt = self.state.alt_rel + cmd.args[2] / 100.0
            if target_alt > self.limits.max_altitude_m:
                return False, (
                    f"GO target exceeds max altitude: {target_alt:.1f}m"
                    f" (limit {self.limits.max_altitude_m}m)"
                )
            if target_alt < self.limits.min_altitude_m:
                return False, (
                    f"GO target below min altitude: {target_alt:.1f}m"
                    f" (limit {self.limits.min_altitude_m}m)"
                )

        # Geofence check (only if center is set, i.e. not 0/0)
        geofence_active = (
            self.limits.geofence_center_lat != 0.0
            or self.limits.geofence_center_lon != 0.0
        )
        if geofence_active:
            dist = _haversine_m(
                self.state.lat,
                self.state.lon,
                self.limits.geofence_center_lat,
                self.limits.geofence_center_lon,
            )
            if dist > self.limits.geofence_radius_m:
                return False, (
                    f"Outside geofence: {dist:.0f}m from center"
                    f" (radius {self.limits.geofence_radius_m:.0f}m)"
                )

        return True, ""

    def validate_arm(self) -> tuple[bool, str]:
        """Pre-arm safety checks."""
        # GPS fix check (need at least 3D fix)
        if self.state.gps_fix_type < 3:
            return False, f"No GPS 3D fix (fix_type={self.state.gps_fix_type})"

        # Battery check
        if 0 <= self.state.battery_remaining < self.limits.min_battery_pct:
            return False, (
                f"Battery too low for arming: {self.state.battery_remaining}%"
                f" (minimum {self.limits.min_battery_pct}%)"
            )

        return True, ""
