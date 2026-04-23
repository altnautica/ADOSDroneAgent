"""MCP vision_tools tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register vision_tools tools on the MCP server."""

    @mcp.tool(name="vision.list_models")
    def vision_list_models(**kwargs: object) -> dict:
        """Phase 1 stub for vision.list_models."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="vision.set_model")
    def vision_set_model(**kwargs: object) -> dict:
        """Phase 1 stub for vision.set_model."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="vision.detect_now")
    def vision_detect_now(**kwargs: object) -> dict:
        """Phase 1 stub for vision.detect_now."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="vision.start_tracker")
    def vision_start_tracker(**kwargs: object) -> dict:
        """Phase 1 stub for vision.start_tracker."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="vision.stop_tracker")
    def vision_stop_tracker(**kwargs: object) -> dict:
        """Phase 1 stub for vision.stop_tracker."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
