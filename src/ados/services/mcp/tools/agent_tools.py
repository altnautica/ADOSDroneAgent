"""MCP agent_tools tool handlers.

Safety class: read (default for this group; some tools may vary).
All handlers are stubs returning not_implemented status.
Full implementation ships in Phase 2.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP


def register(mcp: FastMCP) -> None:
    """Register agent_tools tools on the MCP server."""

    @mcp.tool(name="agent.health")
    def agent_health(**kwargs: object) -> dict:
        """Phase 1 stub for agent.health."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.version")
    def agent_version(**kwargs: object) -> dict:
        """Phase 1 stub for agent.version."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.tier")
    def agent_tier(**kwargs: object) -> dict:
        """Phase 1 stub for agent.tier."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.board")
    def agent_board(**kwargs: object) -> dict:
        """Phase 1 stub for agent.board."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.capabilities")
    def agent_capabilities(**kwargs: object) -> dict:
        """Phase 1 stub for agent.capabilities."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.identity")
    def agent_identity(**kwargs: object) -> dict:
        """Phase 1 stub for agent.identity."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.uptime")
    def agent_uptime(**kwargs: object) -> dict:
        """Phase 1 stub for agent.uptime."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}

    @mcp.tool(name="agent.feature_flags")
    def agent_feature_flags(**kwargs: object) -> dict:
        """Phase 1 stub for agent.feature_flags."""
        return {"status": "not_implemented", "message": "full implementation in phase 2"}
