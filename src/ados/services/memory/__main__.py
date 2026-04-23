"""World Model memory service entry point.

Run: python -m ados.services.memory
"""

from __future__ import annotations

import asyncio
import sys

from .service import MemoryService


async def main() -> None:
    svc = MemoryService()
    await svc.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
