"""Foxglove WebSocket Protocol bridge service entry point.

Run: python -m ados.services.foxglove_bridge
"""

from __future__ import annotations

import asyncio
import sys

from .service import FoxgloveBridgeService


async def main() -> None:
    svc = FoxgloveBridgeService()
    await svc.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
