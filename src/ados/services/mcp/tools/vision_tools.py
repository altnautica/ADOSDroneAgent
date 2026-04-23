"""MCP vision tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="vision.list_models")
    async def vision_list_models() -> dict:
        """List installed vision models."""
        try:
            return await shim_get("vision/models")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="vision.set_model")
    async def vision_set_model(model_name: str) -> dict:
        """Set the active vision model."""
        try:
            return await shim_post("vision/model", {"name": model_name})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="vision.detect_now")
    async def vision_detect_now() -> dict:
        """Run one detection inference on the current frame."""
        try:
            return await shim_post("vision/detect", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="vision.start_tracker")
    async def vision_start_tracker(behavior: str = "follow_me") -> dict:
        """Start a vision behavior (e.g. follow_me, orbit, spotlight)."""
        try:
            return await shim_post("vision/behavior/start", {"behavior": behavior})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="vision.stop_tracker")
    async def vision_stop_tracker() -> dict:
        """Stop any active vision behavior."""
        try:
            return await shim_post("vision/behavior/stop", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
