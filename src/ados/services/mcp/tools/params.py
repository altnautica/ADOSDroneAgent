"""MCP params tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register params tools on the MCP server."""

    @mcp.tool(name="params.list")
    def params_list(**kwargs: object) -> dict:
        """Phase 1 stub for params.list."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="params.get")
    def params_get(**kwargs: object) -> dict:
        """Phase 1 stub for params.get."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="params.set")
    def params_set(**kwargs: object) -> dict:
        """Phase 1 stub for params.set."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="params.diff")
    def params_diff(**kwargs: object) -> dict:
        """Phase 1 stub for params.diff."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="params.save_to_flash")
    def params_save_to_flash(**kwargs: object) -> dict:
        """Phase 1 stub for params.save_to_flash."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="params.reset_to_default")
    def params_reset_to_default(**kwargs: object) -> dict:
        """Phase 1 stub for params.reset_to_default."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="params.reset_all_to_default")
    def params_reset_all_to_default(**kwargs: object) -> dict:
        """Phase 1 stub for params.reset_all_to_default."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
