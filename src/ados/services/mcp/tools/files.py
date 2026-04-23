"""MCP files tool handlers."""
from __future__ import annotations
from mcp.server.fastmcp import FastMCP
from ..shim import ShimError, get as shim_get, post as shim_post

def register(mcp: FastMCP) -> None:
    @mcp.tool(name="files.list")
    async def files_list(path: str = "/var/ados") -> dict:
        """List files at a path (subject to allowed_roots policy)."""
        try:
            return await shim_get(f"files?path={path}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="files.read")
    async def files_read(path: str) -> dict:
        """Read a text file (subject to allowed_roots policy)."""
        try:
            return await shim_get(f"files/read?path={path}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="files.write")
    async def files_write(path: str, content: str) -> dict:
        """Write content to a file (subject to allowed_roots policy)."""
        try:
            return await shim_post("files/write", {"path": path, "content": content})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="files.delete")
    async def files_delete(path: str) -> dict:
        """Delete a file. Destructive."""
        try:
            return await shim_post("files/delete", {"path": path})
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="files.stat")
    async def files_stat(path: str) -> dict:
        """Return file metadata (size, mtime, type)."""
        try:
            return await shim_get(f"files/stat?path={path}")
        except ShimError as e:
            return {"status": "error", "message": str(e)}

    @mcp.tool(name="files.move")
    async def files_move(src: str, dst: str) -> dict:
        """Move/rename a file."""
        try:
            return await shim_post("files/move", {"src": src, "dst": dst})
        except ShimError as e:
            return {"status": "error", "message": str(e)}
