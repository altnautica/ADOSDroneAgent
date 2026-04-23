"""MCP agent identity and health tool handlers.

Safety class: read for all tools.
"""

from __future__ import annotations

import time

from mcp.server.fastmcp import FastMCP

from ados import __version__
from ..shim import ShimError, get as shim_get

_START_TIME = time.time()


def register(mcp: FastMCP) -> None:
    """Register agent tools on the MCP server."""

    @mcp.tool(name="agent.health")
    async def agent_health() -> dict:
        """Return overall agent health: service states, circuit breakers."""
        try:
            return await shim_get("status/full")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="agent.version")
    async def agent_version() -> dict:
        """Return the agent software version."""
        return {"version": __version__}

    @mcp.tool(name="agent.uptime")
    async def agent_uptime() -> dict:
        """Return the agent uptime in seconds."""
        return {"uptime_seconds": round(time.time() - _START_TIME, 1)}

    @mcp.tool(name="agent.identity")
    async def agent_identity() -> dict:
        """Return device identity: device_id, board, tier, hostname."""
        try:
            status = await shim_get("status")
            return {
                "device_id": status.get("device_id"),
                "board": status.get("board"),
                "tier": status.get("tier"),
                "version": __version__,
            }
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="agent.capabilities")
    async def agent_capabilities() -> dict:
        """Return the full capabilities payload: compute, vision, cameras, features."""
        try:
            full = await shim_get("status/full")
            return {"capabilities": full.get("capabilities", {})}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="agent.tier")
    async def agent_tier() -> dict:
        """Return the hardware tier (1-4)."""
        try:
            full = await shim_get("status/full")
            return {"tier": full.get("capabilities", {}).get("tier", 0)}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="agent.board")
    async def agent_board() -> dict:
        """Return the board profile name."""
        try:
            status = await shim_get("status")
            return {"board": status.get("board", "unknown")}
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="agent.feature_flags")
    async def agent_feature_flags() -> dict:
        """Return enabled feature flags."""
        try:
            full = await shim_get("status/full")
            features = full.get("capabilities", {}).get("features", {})
            return {"features": features}
        except ShimError as e:
            return {"status": "error", "message": str(e)}
