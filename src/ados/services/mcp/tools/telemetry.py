"""MCP telemetry tool handlers.

Reads state from /api/status or /api/telemetry via the shim layer.
Safety class: read for all tools.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP

from ..shim import ShimError, get as shim_get


def register(mcp: FastMCP) -> None:
    """Register telemetry tools on the MCP server."""

    @mcp.tool(name="telemetry.snapshot")
    async def telemetry_snapshot() -> dict:
        """Return a full telemetry snapshot: attitude, GPS, battery, mode, FC state."""
        try:
            return await shim_get("telemetry")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="telemetry.battery")
    async def telemetry_battery() -> dict:
        """Return battery voltage, current, remaining percent, and cell count."""
        try:
            status = await shim_get("status/full")
            batt = status.get("telemetry", {}).get("battery", {})
            return {"battery": batt}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="telemetry.gps")
    async def telemetry_gps() -> dict:
        """Return GPS fix type, satellite count, latitude, longitude, altitude."""
        try:
            status = await shim_get("status/full")
            gps = status.get("telemetry", {}).get("gps", {})
            pos = status.get("telemetry", {}).get("position", {})
            return {"gps": gps, "position": pos}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="telemetry.attitude")
    async def telemetry_attitude() -> dict:
        """Return roll, pitch, yaw, and angular velocities."""
        try:
            status = await shim_get("status/full")
            att = status.get("telemetry", {}).get("attitude", {})
            return {"attitude": att}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="telemetry.history")
    async def telemetry_history(seconds: int = 60) -> dict:
        """Return recent telemetry history (up to seconds back)."""
        try:
            return await shim_get(f"telemetry?history={seconds}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}
