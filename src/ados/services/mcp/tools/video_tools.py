"""MCP video_tools tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register video_tools tools on the MCP server."""

    @mcp.tool(name="video.status")
    def video_status(**kwargs: object) -> dict:
        """Phase 1 stub for video.status."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="video.snapshot")
    def video_snapshot(**kwargs: object) -> dict:
        """Phase 1 stub for video.snapshot."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="video.record_start")
    def video_record_start(**kwargs: object) -> dict:
        """Phase 1 stub for video.record_start."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="video.record_stop")
    def video_record_stop(**kwargs: object) -> dict:
        """Phase 1 stub for video.record_stop."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="video.set_bitrate")
    def video_set_bitrate(**kwargs: object) -> dict:
        """Phase 1 stub for video.set_bitrate."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="video.switch_camera")
    def video_switch_camera(**kwargs: object) -> dict:
        """Phase 1 stub for video.switch_camera."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
