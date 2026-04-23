"""MCP parameter tool handlers.

Wraps /api/params/* via the shim layer.
Safety class: read for reads, safe_write for writes.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP

from ..shim import ShimError, get as shim_get, post as shim_post


def register(mcp: FastMCP) -> None:
    """Register params tools on the MCP server."""

    @mcp.tool(name="params.list")
    async def params_list() -> dict:
        """List all FC parameters."""
        try:
            return await shim_get("params")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="params.get")
    async def params_get(name: str) -> dict:
        """Get a single FC parameter by name."""
        try:
            return await shim_get(f"params/{name}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="params.set")
    async def params_set(name: str, value: float) -> dict:
        """Set a FC parameter. ArduPilot auto-saves to EEPROM on PARAM_SET."""
        try:
            return await shim_post(f"params/{name}", {"value": value})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="params.diff")
    async def params_diff() -> dict:
        """Return parameters that differ from their default values."""
        try:
            return await shim_get("params/diff")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="params.save_to_flash")
    async def params_save_to_flash() -> dict:
        """Trigger MAV_CMD_PREFLIGHT_STORAGE (belt-and-suspenders flash write)."""
        try:
            return await shim_post("params/save", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="params.reset_to_default")
    async def params_reset_to_default(name: str) -> dict:
        """Reset a single parameter to its default value."""
        try:
            return await shim_post(f"params/{name}/reset", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="params.reset_all_to_default")
    async def params_reset_all_to_default() -> dict:
        """Reset ALL parameters to defaults. Destructive — confirm required."""
        try:
            return await shim_post("params/reset-all", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
