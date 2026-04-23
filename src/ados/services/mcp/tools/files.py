"""MCP files tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register files tools on the MCP server."""

    @mcp.tool(name="files.list")
    def files_list(**kwargs: object) -> dict:
        """Phase 1 stub for files.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="files.read")
    def files_read(**kwargs: object) -> dict:
        """Phase 1 stub for files.read."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="files.write")
    def files_write(**kwargs: object) -> dict:
        """Phase 1 stub for files.write."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="files.delete")
    def files_delete(**kwargs: object) -> dict:
        """Phase 1 stub for files.delete."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="files.stat")
    def files_stat(**kwargs: object) -> dict:
        """Phase 1 stub for files.stat."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="files.move")
    def files_move(**kwargs: object) -> dict:
        """Phase 1 stub for files.move."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
