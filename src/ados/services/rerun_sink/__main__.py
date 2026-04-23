"""Rerun visualization sink service entry point.

Run: python -m ados.services.rerun_sink
"""

from __future__ import annotations

import asyncio
import sys

from .service import RerunSinkService


async def main() -> None:
    svc = RerunSinkService()
    await svc.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
