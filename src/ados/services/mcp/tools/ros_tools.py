"""MCP ROS 2 tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="ros.status")
    async def ros_status() -> dict:
        """Return ROS 2 environment status."""
        try:
            return await shim_get("ros/status")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="ros.list_nodes")
    async def ros_list_nodes() -> dict:
        """List active ROS 2 nodes."""
        try:
            return await shim_get("ros/nodes")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="ros.list_topics")
    async def ros_list_topics() -> dict:
        """List active ROS 2 topics."""
        try:
            return await shim_get("ros/topics")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="ros.start_bag")
    async def ros_start_bag(filename: str = "") -> dict:
        """Start recording a ROS 2 bag file."""
        try:
            return await shim_post("ros/recording/start", {"filename": filename})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="ros.stop_bag")
    async def ros_stop_bag() -> dict:
        """Stop recording a ROS 2 bag file."""
        try:
            return await shim_post("ros/recording/stop", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
