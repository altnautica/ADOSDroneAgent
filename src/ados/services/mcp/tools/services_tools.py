"""MCP services tool handlers.

Wraps /api/services/* via the shim layer.
"""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP

from ..shim import ShimError, get as shim_get, post as shim_post


def register(mcp: FastMCP) -> None:
    """Register services tools on the MCP server."""

    @mcp.tool(name="services.list")
    async def services_list() -> dict:
        """List all agent services and their current states."""
        try:
            return await shim_get("services")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="services.status")
    async def services_status(name: str) -> dict:
        """Get status of a specific service (e.g. ados-video)."""
        try:
            return await shim_get(f"services/{name}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="services.start")
    async def services_start(name: str) -> dict:
        """Start a stopped service."""
        try:
            return await shim_post(f"services/{name}/start", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="services.stop")
    async def services_stop(name: str) -> dict:
        """Stop a running service."""
        try:
            return await shim_post(f"services/{name}/stop", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="services.restart")
    async def services_restart(name: str) -> dict:
        """Restart a service."""
        try:
            return await shim_post(f"services/{name}/restart", {})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="services.logs")
    async def services_logs(name: str, lines: int = 50) -> dict:
        """Get recent journal log lines for a service."""
        try:
            return await shim_get(f"services/{name}/logs?lines={lines}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}
