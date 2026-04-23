"""MCP mission tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="mission.download")
    async def mission_download() -> dict:
        """Download the current mission from the FC."""
        try:
            return await shim_get("mission")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="mission.upload")
    async def mission_upload(waypoints: list) -> dict:
        """Upload a list of waypoints to the FC."""
        try:
            return await shim_post("mission/upload", {"waypoints": waypoints})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="mission.start")
    async def mission_start() -> dict:
        """Start the currently loaded mission."""
        try:
            return await shim_post("mission/start", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="mission.clear")
    async def mission_clear() -> dict:
        """Clear the mission from the FC."""
        try:
            return await shim_post("mission/clear", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="mission.current_item")
    async def mission_current_item() -> dict:
        """Get the current mission item index."""
        try:
            return await shim_get("mission/current_item")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="mission.set_current_item")
    async def mission_set_current_item(index: int) -> dict:
        """Jump to a specific mission item."""
        try:
            return await shim_post("mission/current_item", {"index": index})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
