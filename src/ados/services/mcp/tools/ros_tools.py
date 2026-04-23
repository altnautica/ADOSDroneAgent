"""MCP ros_tools tool handlers.

Safety class: read (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register ros_tools tools on the MCP server."""

    @mcp.tool(name="ros.status")
    def ros_status(**kwargs: object) -> dict:
        """Phase 1 stub for ros.status."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="ros.list_nodes")
    def ros_list_nodes(**kwargs: object) -> dict:
        """Phase 1 stub for ros.list_nodes."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="ros.list_topics")
    def ros_list_topics(**kwargs: object) -> dict:
        """Phase 1 stub for ros.list_topics."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="ros.start_bag")
    def ros_start_bag(**kwargs: object) -> dict:
        """Phase 1 stub for ros.start_bag."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="ros.stop_bag")
    def ros_stop_bag(**kwargs: object) -> dict:
        """Phase 1 stub for ros.stop_bag."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
