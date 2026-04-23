"""MCP OTA tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="ota.check")
    async def ota_check() -> dict:
        """Check for available agent updates."""
        try:
            return await shim_get("ota/check")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="ota.install")
    async def ota_install() -> dict:
        """Install the latest available agent update."""
        try:
            return await shim_post("ota/install", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="ota.rollback")
    async def ota_rollback() -> dict:
        """Roll back to the previous agent version. Destructive."""
        try:
            return await shim_post("ota/rollback", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
