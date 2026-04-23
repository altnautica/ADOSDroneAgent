"""Assist diagnostics service entry point.

Run: python -m ados.services.assist
"""

from __future__ import annotations

import asyncio
import sys

from .service import AssistService


async def main() -> None:
    svc = AssistService()
    await svc.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
