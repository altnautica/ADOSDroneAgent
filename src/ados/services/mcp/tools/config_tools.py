"""MCP config tool handlers.

Wraps /api/config/* via the shim layer.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP

from ..shim import ShimError, get as shim_get, post as shim_post


def register(mcp: FastMCP) -> None:
    """Register config tools on the MCP server."""

    @mcp.tool(name="config.get")
    async def config_get() -> dict:
        """Get the current agent configuration (secrets redacted)."""
        try:
            return await shim_get("config")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="config.set")
    async def config_set(key: str, value: object) -> dict:
        """Set a configuration value by dot-path key."""
        try:
            return await shim_post("config/set", {"key": key, "value": value})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="config.validate")
    async def config_validate() -> dict:
        """Validate current configuration file."""
        try:
            return await shim_get("config/validate")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="config.apply")
    async def config_apply() -> dict:
        """Write current in-memory config to disk and reload."""
        try:
            return await shim_post("config/apply", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="config.reload")
    async def config_reload() -> dict:
        """Reload configuration from disk."""
        try:
            return await shim_post("config/reload", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
