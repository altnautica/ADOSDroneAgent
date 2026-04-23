"""Survey photogrammetry service entry point.

Run: python -m ados.services.survey
"""

from __future__ import annotations

import asyncio
import sys

from .service import SurveyService


async def main() -> None:
    svc = SurveyService()
    await svc.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
