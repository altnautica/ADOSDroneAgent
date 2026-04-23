"""MCP ota_tools tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register ota_tools tools on the MCP server."""

    @mcp.tool(name="ota.check")
    def ota_check(**kwargs: object) -> dict:
        """Phase 1 stub for ota.check."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="ota.install")
    def ota_install(**kwargs: object) -> dict:
        """Phase 1 stub for ota.install."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="ota.rollback")
    def ota_rollback(**kwargs: object) -> dict:
        """Phase 1 stub for ota.rollback."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
