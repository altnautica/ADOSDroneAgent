"""MCP telemetry tool handlers.

Safety class: read (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register telemetry tools on the MCP server."""

    @mcp.tool(name="telemetry.snapshot")
    def telemetry_snapshot(**kwargs: object) -> dict:
        """Phase 1 stub for telemetry.snapshot."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="telemetry.battery")
    def telemetry_battery(**kwargs: object) -> dict:
        """Phase 1 stub for telemetry.battery."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="telemetry.gps")
    def telemetry_gps(**kwargs: object) -> dict:
        """Phase 1 stub for telemetry.gps."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="telemetry.attitude")
    def telemetry_attitude(**kwargs: object) -> dict:
        """Phase 1 stub for telemetry.attitude."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="telemetry.history")
    def telemetry_history(**kwargs: object) -> dict:
        """Phase 1 stub for telemetry.history."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
