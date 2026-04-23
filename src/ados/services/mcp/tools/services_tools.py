"""MCP services_tools tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register services_tools tools on the MCP server."""

    @mcp.tool(name="services.list")
    def services_list(**kwargs: object) -> dict:
        """Phase 1 stub for services.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="services.status")
    def services_status(**kwargs: object) -> dict:
        """Phase 1 stub for services.status."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="services.start")
    def services_start(**kwargs: object) -> dict:
        """Phase 1 stub for services.start."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="services.stop")
    def services_stop(**kwargs: object) -> dict:
        """Phase 1 stub for services.stop."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="services.restart")
    def services_restart(**kwargs: object) -> dict:
        """Phase 1 stub for services.restart."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="services.logs")
    def services_logs(**kwargs: object) -> dict:
        """Phase 1 stub for services.logs."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
