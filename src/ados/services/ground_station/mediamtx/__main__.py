"""Ground-station mediamtx service entry point.

Run: python -m ados.services.ground_station.mediamtx
"""

from __future__ import annotations

import asyncio
import sys

from .manager import main

if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
