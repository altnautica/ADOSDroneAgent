"""MCP flight tool handlers.

Safety class: flight_action (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register flight tools on the MCP server."""

    @mcp.tool(name="flight.arm")
    def flight_arm(**kwargs: object) -> dict:
        """Phase 1 stub for flight.arm."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.disarm")
    def flight_disarm(**kwargs: object) -> dict:
        """Phase 1 stub for flight.disarm."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.takeoff")
    def flight_takeoff(**kwargs: object) -> dict:
        """Phase 1 stub for flight.takeoff."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.land")
    def flight_land(**kwargs: object) -> dict:
        """Phase 1 stub for flight.land."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.rtl")
    def flight_rtl(**kwargs: object) -> dict:
        """Phase 1 stub for flight.rtl."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.set_mode")
    def flight_set_mode(**kwargs: object) -> dict:
        """Phase 1 stub for flight.set_mode."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.goto")
    def flight_goto(**kwargs: object) -> dict:
        """Phase 1 stub for flight.goto."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.orbit")
    def flight_orbit(**kwargs: object) -> dict:
        """Phase 1 stub for flight.orbit."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.pause")
    def flight_pause(**kwargs: object) -> dict:
        """Phase 1 stub for flight.pause."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.resume")
    def flight_resume(**kwargs: object) -> dict:
        """Phase 1 stub for flight.resume."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="flight.emergency_stop")
    def flight_emergency_stop(**kwargs: object) -> dict:
        """Phase 1 stub for flight.emergency_stop."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
