"""MCP video tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="video.status")
    async def video_status() -> dict:
        """Return video pipeline status."""
        try:
            return await shim_get("video")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="video.snapshot")
    async def video_snapshot() -> dict:
        """Capture a JPEG snapshot. Returns a URL."""
        try:
            return await shim_post("video/snapshot", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="video.record_start")
    async def video_record_start(filename: str = "") -> dict:
        """Start recording video."""
        try:
            return await shim_post("video/record/start", {"filename": filename})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="video.record_stop")
    async def video_record_stop() -> dict:
        """Stop recording and return file path."""
        try:
            return await shim_post("video/record/stop", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="video.set_bitrate")
    async def video_set_bitrate(bitrate_kbps: int) -> dict:
        """Set encode bitrate in kbps."""
        try:
            return await shim_post("video/bitrate", {"bitrate_kbps": bitrate_kbps})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="video.switch_camera")
    async def video_switch_camera(device: str) -> dict:
        """Switch to a different camera device."""
        try:
            return await shim_post("video/camera", {"device": device})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
