"""MCP server service entry point.

Exposes the drone's full control surface as an MCP server — Tools,
Resources, and Prompts accessible from any MCP-capable client over
HTTP+SSE on :8090, a Unix socket at /run/ados/mcp.sock, or stdio.

Run: python -m ados.services.mcp
"""

from __future__ import annotations

import asyncio
import sys

from .service import McpService


async def main() -> None:
    svc = McpService()
    await svc.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
