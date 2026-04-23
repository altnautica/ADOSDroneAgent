"""MCP system_tools tool handlers.

Safety class: destructive (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register system_tools tools on the MCP server."""

    @mcp.tool(name="system.reboot")
    def system_reboot(**kwargs: object) -> dict:
        """Phase 1 stub for system.reboot."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="system.shutdown")
    def system_shutdown(**kwargs: object) -> dict:
        """Phase 1 stub for system.shutdown."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="system.reset_factory")
    def system_reset_factory(**kwargs: object) -> dict:
        """Phase 1 stub for system.reset_factory."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="system.time")
    def system_time(**kwargs: object) -> dict:
        """Phase 1 stub for system.time."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
