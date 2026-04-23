"""MCP system tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="system.reboot")
    async def system_reboot() -> dict:
        """Reboot the SBC. Destructive — confirm required."""
        try:
            return await shim_post("system/reboot", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="system.shutdown")
    async def system_shutdown() -> dict:
        """Shutdown the SBC. Destructive — confirm required."""
        try:
            return await shim_post("system/shutdown", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="system.reset_factory")
    async def system_reset_factory() -> dict:
        """Factory reset the agent. Destructive — confirm required."""
        try:
            return await shim_post("system/reset-factory", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="system.time")
    async def system_time() -> dict:
        """Return current system time and timezone."""
        try:
            return await shim_get("system/time")
        except ShimError as e:
            return {"status": "error", "message": str(e)}
