"""MCP mission_tools tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register mission_tools tools on the MCP server."""

    @mcp.tool(name="mission.upload")
    def mission_upload(**kwargs: object) -> dict:
        """Phase 1 stub for mission.upload."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="mission.download")
    def mission_download(**kwargs: object) -> dict:
        """Phase 1 stub for mission.download."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="mission.start")
    def mission_start(**kwargs: object) -> dict:
        """Phase 1 stub for mission.start."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="mission.clear")
    def mission_clear(**kwargs: object) -> dict:
        """Phase 1 stub for mission.clear."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="mission.current_item")
    def mission_current_item(**kwargs: object) -> dict:
        """Phase 1 stub for mission.current_item."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="mission.set_current_item")
    def mission_set_current_item(**kwargs: object) -> dict:
        """Phase 1 stub for mission.set_current_item."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
