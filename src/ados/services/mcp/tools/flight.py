"""MCP flight tool handlers.

Calls the agent command endpoint at /api/command via the shim layer.
Safety class: flight_action for all tools.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP

from ..shim import ShimError, post as shim_post


def register(mcp: FastMCP) -> None:
    """Register flight tools on the MCP server."""

    @mcp.tool(name="flight.arm")
    async def flight_arm(simulate: bool = False) -> dict:
        """Arm the vehicle. Set simulate=True for bench testing without props."""
        try:
            result = await shim_post("command", {"cmd": "arm", "args": [1 if simulate else 0]})
            return {"status": "armed", "simulate": simulate, **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.disarm")
    async def flight_disarm(force: bool = False) -> dict:
        """Disarm the vehicle."""
        try:
            result = await shim_post("command", {"cmd": "disarm", "args": [1 if force else 0]})
            return {"status": "disarmed", **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.takeoff")
    async def flight_takeoff(altitude_m: float = 10.0) -> dict:
        """Take off to the specified altitude in meters."""
        try:
            result = await shim_post("command", {"cmd": "takeoff", "args": [altitude_m]})
            return {"status": "taking_off", "altitude_m": altitude_m, **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.land")
    async def flight_land() -> dict:
        """Land at current position."""
        try:
            result = await shim_post("command", {"cmd": "land", "args": []})
            return {"status": "landing", **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.rtl")
    async def flight_rtl() -> dict:
        """Return to launch."""
        try:
            result = await shim_post("command", {"cmd": "rtl", "args": []})
            return {"status": "returning_to_launch", **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.set_mode")
    async def flight_set_mode(mode: str) -> dict:
        """Set flight mode (e.g. GUIDED, LOITER, AUTO, STABILIZE)."""
        try:
            result = await shim_post("command", {"cmd": "mode", "args": [mode]})
            return {"status": "mode_set", "mode": mode, **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.goto")
    async def flight_goto(lat: float, lon: float, alt_m: float = 10.0) -> dict:
        """Fly to lat/lon/alt waypoint."""
        try:
            result = await shim_post("command", {"cmd": "goto", "args": [lat, lon, alt_m]})
            return {"status": "goto_commanded", "lat": lat, "lon": lon, "alt_m": alt_m, **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.orbit")
    async def flight_orbit(radius_m: float = 20.0, velocity_ms: float = 3.0) -> dict:
        """Orbit current position."""
        try:
            result = await shim_post("command", {"cmd": "orbit", "args": [radius_m, velocity_ms]})
            return {"status": "orbiting", "radius_m": radius_m, "velocity_ms": velocity_ms, **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.pause")
    async def flight_pause() -> dict:
        """Pause mission and loiter."""
        try:
            result = await shim_post("command", {"cmd": "pause", "args": []})
            return {"status": "paused", **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.resume")
    async def flight_resume() -> dict:
        """Resume paused mission."""
        try:
            result = await shim_post("command", {"cmd": "resume", "args": []})
            return {"status": "resumed", **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="flight.emergency_stop")
    async def flight_emergency_stop() -> dict:
        """Emergency disarm regardless of flight state."""
        try:
            result = await shim_post("command", {"cmd": "emergency_stop", "args": []})
            return {"status": "emergency_stopped", **result}
        except ShimError as e:
            return {"status": "error", "message": str(e)}
