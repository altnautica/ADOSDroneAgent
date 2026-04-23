"""MCP config_tools tool handlers.

Safety class: safe_write (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register config_tools tools on the MCP server."""

    @mcp.tool(name="config.get")
    def config_get(**kwargs: object) -> dict:
        """Phase 1 stub for config.get."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="config.set")
    def config_set(**kwargs: object) -> dict:
        """Phase 1 stub for config.set."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="config.validate")
    def config_validate(**kwargs: object) -> dict:
        """Phase 1 stub for config.validate."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="config.apply")
    def config_apply(**kwargs: object) -> dict:
        """Phase 1 stub for config.apply."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="config.reload")
    def config_reload(**kwargs: object) -> dict:
        """Phase 1 stub for config.reload."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
